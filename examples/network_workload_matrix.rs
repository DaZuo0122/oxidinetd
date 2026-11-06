//! End-to-end HTTP, HTTPS, and optional iperf3 comparison of direct,
//! Tokio async-connect proxy, and blocking-pool-connect proxy paths.
//! Build the `oi` binary with the same `tcp-profile` feature before running.

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const REQUEST_LEN: usize = b"GET /00001024 HTTP/1.1\r\nHost: localhost\r\n\r\n".len();
const RESPONSE_HEADER_LEN: usize =
    b"HTTP/1.1 200 OK\r\nContent-Length: 00001024\r\nConnection: keep-alive\r\n\r\n".len();
const MAX_BODY: usize = 64 * 1024;

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct HttpServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn tls_config() -> Result<(Arc<ServerConfig>, Arc<ClientConfig>)> {
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(vec!["localhost".into()])?;
    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(cert_der.clone())?;
    let server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((Arc::new(server), Arc::new(client)))
}

fn start_http_server(tls: Option<Arc<ServerConfig>>) -> Result<HttpServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let body = Arc::new(vec![b'X'; MAX_BODY]);
    let handle = std::thread::spawn(move || {
        for accepted in listener.incoming() {
            if stopped.load(Ordering::Acquire) {
                break;
            }
            let Ok(stream) = accepted else { continue };
            let tls = tls.clone();
            let body = body.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                if let Some(config) = tls {
                    if let Ok(conn) = ServerConnection::new(config) {
                        serve_http(StreamOwned::new(conn, stream), &body);
                    }
                } else {
                    serve_http(stream, &body);
                }
            });
        }
    });
    Ok(HttpServer {
        addr,
        stop,
        handle: Some(handle),
    })
}

fn serve_http<S: Read + Write>(mut stream: S, body: &[u8]) {
    let mut request = [0u8; REQUEST_LEN];
    loop {
        if stream.read_exact(&mut request).is_err() {
            break;
        }
        if &request[..5] != b"GET /" || &request[13..] != b" HTTP/1.1\r\nHost: localhost\r\n\r\n" {
            break;
        }
        let Ok(size) = std::str::from_utf8(&request[5..13])
            .unwrap_or("")
            .parse::<usize>()
        else {
            break;
        };
        if size > body.len() {
            break;
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {size:08}\r\nConnection: keep-alive\r\n\r\n"
        );
        if stream.write_all(header.as_bytes()).is_err() || stream.write_all(&body[..size]).is_err()
        {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
    }
}

fn connect_http(
    addr: SocketAddr,
    tls: Option<&Arc<ClientConfig>>,
) -> Result<Box<dyn ReadWrite + Send>> {
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    if let Some(config) = tls {
        let conn = ClientConnection::new(config.clone(), ServerName::try_from("localhost")?)?;
        Ok(Box::new(StreamOwned::new(conn, tcp)))
    } else {
        Ok(Box::new(tcp))
    }
}

fn one_request(stream: &mut dyn ReadWrite, size: usize, body: &mut [u8]) -> Result<()> {
    let request = format!("GET /{size:08} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut header = [0u8; RESPONSE_HEADER_LEN];
    stream.read_exact(&mut header)?;
    let expected =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {size:08}\r\nConnection: keep-alive\r\n\r\n");
    if header != expected.as_bytes() {
        return Err("invalid HTTP response header".into());
    }
    stream.read_exact(&mut body[..size])?;
    if body[..size].iter().any(|&byte| byte != b'X') {
        return Err("invalid HTTP response body".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct HttpCase {
    name: &'static str,
    tls: bool,
    keep_alive: bool,
    bytes: usize,
    clients: usize,
    iterations: usize,
}

struct HttpResult {
    case: &'static str,
    mode: &'static str,
    clients: usize,
    count: usize,
    p50: u64,
    p95: u64,
    p99: u64,
    max: u64,
    requests_per_second: f64,
}

fn run_http_case(
    case: HttpCase,
    mode: &'static str,
    addr: SocketAddr,
    tls: Option<Arc<ClientConfig>>,
) -> Result<HttpResult> {
    let barrier = Arc::new(Barrier::new(case.clients + 1));
    let mut handles = Vec::new();
    for _ in 0..case.clients {
        let barrier = barrier.clone();
        let tls = tls.clone();
        handles.push(std::thread::spawn(move || -> Result<Vec<u64>> {
            let mut body = vec![0; case.bytes];
            let setup = (|| -> Result<Option<Box<dyn ReadWrite + Send>>> {
                if case.keep_alive {
                    let mut stream = connect_http(addr, tls.as_ref())?;
                    one_request(&mut *stream, case.bytes, &mut body)?;
                    Ok(Some(stream))
                } else {
                    Ok(None)
                }
            })();
            barrier.wait();
            let mut persistent = setup?;
            let mut values = Vec::with_capacity(case.iterations);
            for _ in 0..case.iterations {
                let started = Instant::now();
                if let Some(stream) = persistent.as_mut() {
                    one_request(&mut **stream, case.bytes, &mut body)?;
                } else {
                    let mut stream = connect_http(addr, tls.as_ref())?;
                    one_request(&mut *stream, case.bytes, &mut body)?;
                }
                values.push(started.elapsed().as_micros() as u64);
            }
            Ok(values)
        }));
    }
    barrier.wait();
    let started = Instant::now();
    let mut values = Vec::new();
    for handle in handles {
        values.extend(handle.join().map_err(|_| "HTTP client thread panicked")??);
    }
    let elapsed = started.elapsed().as_secs_f64();
    values.sort_unstable();
    let count = values.len();
    Ok(HttpResult {
        case: case.name,
        mode,
        clients: case.clients,
        count,
        p50: values[count / 2],
        p95: values[count * 95 / 100],
        p99: values[count * 99 / 100],
        max: values[count - 1],
        requests_per_second: count as f64 / elapsed,
    })
}

fn reserve_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn write_config(path: &Path, rules: &[(u16, u16)]) -> Result<()> {
    let mut text = String::new();
    for &(bind, backend) in rules {
        text.push_str(&format!("[[forwarding_rules]]\nbind_address = \"127.0.0.1\"\nbind_port = {bind}\nconnect_address = \"127.0.0.1\"\nconnect_port = {backend}\nprotocol = \"tcp\"\n\n"));
    }
    fs::write(path, text)?;
    Ok(())
}

fn wait_for_port(port: u16, child: &mut Child) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..100 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("proxy exited before port {port} was ready: {status}").into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!("proxy port {port} did not become ready").into())
}

fn start_proxy(binary: &Path, config: &Path, pool: bool, first_port: u16) -> Result<ChildGuard> {
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.env_remove("OI_PROFILE_SYNC_BACKEND_CONNECT");
    if pool {
        command.env("OI_PROFILE_BLOCKING_POOL_CONNECT", "1");
    } else {
        command.env_remove("OI_PROFILE_BLOCKING_POOL_CONNECT");
    }
    let mut child = ChildGuard(command.spawn()?);
    wait_for_port(first_port, &mut child.0)?;
    Ok(child)
}

fn iperf_result(binary: &Path, port: u16, parallel: usize) -> Result<f64> {
    let output = Command::new(binary)
        .args(["-c", "127.0.0.1", "-p"])
        .arg(port.to_string())
        .args(["-t", "3", "-P"])
        .arg(parallel.to_string())
        .arg("-J")
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "iperf3 client failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let result: Value = serde_json::from_slice(&output.stdout)?;
    result["end"]["sum_received"]["bits_per_second"]
        .as_f64()
        .map(|bps| bps / 1_000_000_000.0)
        .ok_or_else(|| "iperf3 JSON has no end.sum_received.bits_per_second".into())
}

fn proxy_binary() -> Result<PathBuf> {
    let release = std::env::current_exe()?
        .parent()
        .ok_or("example has no parent directory")?
        .parent()
        .ok_or("example has no release directory")?
        .to_path_buf();
    let binary = release.join(if cfg!(windows) { "oi.exe" } else { "oi" });
    if !binary.is_file() {
        return Err(format!(
            "missing {}; build --bin oi with --features tcp-profile",
            binary.display()
        )
        .into());
    }
    Ok(binary)
}

fn main() -> Result<()> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/network-workload-matrix.md".into());
    let binary = proxy_binary()?;
    let (tls_server, tls_client) = tls_config()?;
    let http = start_http_server(None)?;
    let https = start_http_server(Some(tls_server))?;
    let iperf = std::env::var_os("IPERF3").map(PathBuf::from);
    let iperf_port = if iperf.is_some() {
        Some(reserve_port()?)
    } else {
        None
    };
    let _iperf_server = if let (Some(path), Some(port)) = (&iperf, iperf_port) {
        let child = Command::new(path)
            .arg("-s")
            .arg("-p")
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::sleep(Duration::from_millis(400));
        Some(ChildGuard(child))
    } else {
        None
    };

    let async_http = reserve_port()?;
    let async_https = reserve_port()?;
    let async_iperf = if iperf.is_some() {
        Some(reserve_port()?)
    } else {
        None
    };
    let pool_http = reserve_port()?;
    let pool_https = reserve_port()?;
    let pool_iperf = if iperf.is_some() {
        Some(reserve_port()?)
    } else {
        None
    };
    let temporary = tempfile::tempdir()?;
    let async_config = temporary.path().join("async.toml");
    let pool_config = temporary.path().join("pool.toml");
    let mut async_rules = vec![
        (async_http, http.addr.port()),
        (async_https, https.addr.port()),
    ];
    let mut pool_rules = vec![
        (pool_http, http.addr.port()),
        (pool_https, https.addr.port()),
    ];
    if let (Some(backend), Some(a), Some(p)) = (iperf_port, async_iperf, pool_iperf) {
        async_rules.push((a, backend));
        pool_rules.push((p, backend));
    }
    write_config(&async_config, &async_rules)?;
    write_config(&pool_config, &pool_rules)?;
    let _async_proxy = start_proxy(&binary, &async_config, false, async_http)?;
    let _pool_proxy = start_proxy(&binary, &pool_config, true, pool_http)?;

    let cases = [
        HttpCase {
            name: "HTTP fresh 1 KiB",
            tls: false,
            keep_alive: false,
            bytes: 1024,
            clients: 1,
            iterations: 120,
        },
        HttpCase {
            name: "HTTP fresh 1 KiB",
            tls: false,
            keep_alive: false,
            bytes: 1024,
            clients: 8,
            iterations: 80,
        },
        HttpCase {
            name: "HTTPS fresh 1 KiB",
            tls: true,
            keep_alive: false,
            bytes: 1024,
            clients: 1,
            iterations: 100,
        },
        HttpCase {
            name: "HTTPS fresh 1 KiB",
            tls: true,
            keep_alive: false,
            bytes: 1024,
            clients: 8,
            iterations: 60,
        },
        HttpCase {
            name: "HTTP keepalive 64 KiB",
            tls: false,
            keep_alive: true,
            bytes: 65536,
            clients: 1,
            iterations: 160,
        },
        HttpCase {
            name: "HTTP keepalive 64 KiB",
            tls: false,
            keep_alive: true,
            bytes: 65536,
            clients: 8,
            iterations: 100,
        },
        HttpCase {
            name: "HTTPS keepalive 64 KiB",
            tls: true,
            keep_alive: true,
            bytes: 65536,
            clients: 1,
            iterations: 120,
        },
        HttpCase {
            name: "HTTPS keepalive 64 KiB",
            tls: true,
            keep_alive: true,
            bytes: 65536,
            clients: 8,
            iterations: 80,
        },
    ];
    let mut results = Vec::new();
    for (case_index, case) in cases.into_iter().enumerate() {
        let backend = if case.tls { https.addr } else { http.addr };
        let async_addr = SocketAddr::from((
            [127, 0, 0, 1],
            if case.tls { async_https } else { async_http },
        ));
        let pool_addr = SocketAddr::from((
            [127, 0, 0, 1],
            if case.tls { pool_https } else { pool_http },
        ));
        let tls = case.tls.then(|| tls_client.clone());
        let mut modes = [
            ("direct", backend),
            ("async", async_addr),
            ("blocking-pool", pool_addr),
        ];
        modes.rotate_left(case_index % 3);
        for (mode, addr) in modes {
            eprintln!("{} / {} / {} clients", case.name, mode, case.clients);
            results.push(run_http_case(case, mode, addr, tls.clone())?);
        }
    }
    let mut iperf_results = Vec::new();
    if let (Some(path), Some(backend), Some(a), Some(p)) =
        (&iperf, iperf_port, async_iperf, pool_iperf)
    {
        for (case_index, parallel) in [1, 4].into_iter().enumerate() {
            let mut modes = [("direct", backend), ("async", a), ("blocking-pool", p)];
            modes.rotate_left(case_index % 3);
            for (mode, port) in modes {
                eprintln!("iperf3 / {} / {} streams", mode, parallel);
                iperf_results.push((parallel, mode, iperf_result(path, port, parallel)?));
            }
        }
    }
    let mut report = String::from("# HTTP, HTTPS, and iperf3 workload matrix\n\n");
    report.push_str("Modes: direct backend, actual oi proxy with Tokio async backend connect, and actual oi proxy with feature-gated `spawn_blocking` backend connect. All traffic uses 127.0.0.1. HTTP and HTTPS clients use blocking TCP connect in every mode; HTTPS uses TLS 1.3 through the proxy. Fresh cases include connection and TLS handshake; keepalive cases exclude setup with one warmup request per client. Each proxy runs in a separate process.\n\n");
    report.push_str("| Workload | Clients | Mode | Requests | p50 (µs) | p95 (µs) | p99 (µs) | Max (µs) | Requests/s |\n| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for row in results {
        report.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.1} |\n",
            row.case,
            row.clients,
            row.mode,
            row.count,
            row.p50,
            row.p95,
            row.p99,
            row.max,
            row.requests_per_second
        ));
    }
    if iperf.is_some() {
        report.push_str("\niperf3 receiver throughput (Gb/s; higher is better):\n\n| Streams | Mode | Gb/s |\n| ---: | --- | ---: |\n");
        for (parallel, mode, gbps) in iperf_results {
            report.push_str(&format!("| {parallel} | {mode} | {gbps:.2} |\n"));
        }
    } else {
        report.push_str(
            "\niperf3 skipped: set the `IPERF3` environment variable to its executable path.\n",
        );
    }
    report.push_str("\nThese are local single-run measurements. Compare repeated runs and other machines before changing the default backend-connect strategy.\n");
    if let Some(parent) = Path::new(&output).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, report)?;
    println!("Wrote {output}");
    Ok(())
}
