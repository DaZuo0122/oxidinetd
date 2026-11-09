//! TCP relay benchmark: repeated download/upload reversals on one connection.
//! Build the production `oi` binary and this example with matching features.

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
#[cfg(windows)]
const MODES: [&str; 5] = ["adaptive", "fixed-8k", "fixed-16k", "fixed-32k", "rio"];
#[cfg(target_os = "linux")]
const MODES: [&str; 2] = ["adaptive", "splice"];
#[cfg(not(any(windows, target_os = "linux")))]
const MODES: [&str; 1] = ["adaptive"];

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    std::thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else { continue };
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                let _ = stream.set_nodelay(true);
                let block = [0xA5; 64 * 1024];
                let mut incoming = [0; 64 * 1024];
                loop {
                    let mut header = [0; 5];
                    if stream.read_exact(&mut header).is_err() {
                        break;
                    }
                    let mut remaining =
                        u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
                    let result = match header[0] {
                        b'D' => {
                            while remaining > 0 {
                                let n = remaining.min(block.len());
                                if stream.write_all(&block[..n]).is_err() {
                                    return;
                                }
                                remaining -= n;
                            }
                            Ok(())
                        }
                        b'U' => {
                            while remaining > 0 {
                                let n = remaining.min(incoming.len());
                                if stream.read_exact(&mut incoming[..n]).is_err() {
                                    return;
                                }
                                if incoming[..n].iter().any(|&byte| byte != 0x5A) {
                                    return;
                                }
                                remaining -= n;
                            }
                            stream.write_all(&[1])
                        }
                        _ => return,
                    };
                    if result.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(addr)
}

fn reserve_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn proxy(
    binary: &Path,
    backend: SocketAddr,
    mode: &str,
    config: &Path,
) -> Result<(Guard, SocketAddr)> {
    let port = reserve_port()?;
    fs::write(
        config,
        format!(
            "[[forwarding_rules]]\nbind_address = \"127.0.0.1\"\nbind_port = {port}\nconnect_address = \"127.0.0.1\"\nconnect_port = {}\nprotocol = \"tcp\"\n",
            backend.port()
        ),
    )?;
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.env_remove("OI_COPY_BUFFER_MODE");
    command.env_remove("OI_RELAY_BACKEND");
    if mode == "rio" || mode == "splice" {
        command.env("OI_RELAY_BACKEND", mode);
    } else if mode != "adaptive" {
        command.env("OI_COPY_BUFFER_MODE", mode);
    }
    let mut child = Guard(command.spawn()?);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for _ in 0..100 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            return Ok((child, addr));
        }
        if let Some(status) = child.0.try_wait()? {
            return Err(format!("proxy exited: {status}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("proxy did not bind".into())
}

fn transfer(
    stream: &mut TcpStream,
    direction: u8,
    size: usize,
    upload: &[u8],
    download: &mut [u8],
) -> Result<u64> {
    let mut header = [0; 5];
    header[0] = direction;
    header[1..].copy_from_slice(&(size as u32).to_be_bytes());
    let start = Instant::now();
    stream.write_all(&header)?;
    if direction == b'D' {
        stream.read_exact(&mut download[..size])?;
        if download[..size].iter().any(|&byte| byte != 0xA5) {
            return Err("download corrupted".into());
        }
    } else {
        stream.write_all(&upload[..size])?;
        let mut ack = [0];
        stream.read_exact(&mut ack)?;
        if ack != [1] {
            return Err("upload acknowledgement invalid".into());
        }
    }
    Ok(start.elapsed().as_micros() as u64)
}

struct Row {
    mode: &'static str,
    size: usize,
    clients: usize,
    phase: &'static str,
    p50: u64,
    p95: u64,
    mib_per_second: f64,
}

fn case(
    mode: &'static str,
    addr: SocketAddr,
    size: usize,
    clients: usize,
    rounds: usize,
) -> Result<Vec<Row>> {
    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut handles = Vec::new();
    for _ in 0..clients {
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || -> Result<[Vec<u64>; 4]> {
            let mut stream = TcpStream::connect(addr)?;
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(15)))?;
            stream.set_write_timeout(Some(Duration::from_secs(15)))?;
            let upload = vec![0x5A; size];
            let mut download = vec![0; size];
            let mut phases: [Vec<u64>; 4] = std::array::from_fn(|_| Vec::new());
            barrier.wait();
            for direction in [b'D', b'D', b'D', b'D', b'U', b'U', b'U', b'U'] {
                transfer(&mut stream, direction, size, &upload, &mut download)?;
            }
            for _ in 0..rounds {
                for (index, direction) in [b'D', b'D', b'D', b'D', b'U', b'U', b'U', b'U']
                    .into_iter()
                    .enumerate()
                {
                    let elapsed = transfer(&mut stream, direction, size, &upload, &mut download)?;
                    let phase = match index {
                        0 => 0,
                        1..=3 => 1,
                        4 => 2,
                        _ => 3,
                    };
                    phases[phase].push(elapsed);
                }
            }
            Ok(phases)
        }));
    }
    barrier.wait();
    let mut phases: [Vec<u64>; 4] = std::array::from_fn(|_| Vec::new());
    for handle in handles {
        let samples = handle.join().map_err(|_| "client thread panicked")??;
        for (all, mut part) in phases.iter_mut().zip(samples) {
            all.append(&mut part);
        }
    }
    let names = [
        "download after upload",
        "download steady",
        "upload after download",
        "upload steady",
    ];
    Ok(phases
        .into_iter()
        .zip(names)
        .map(|(mut values, phase)| {
            values.sort_unstable();
            let p50 = values[values.len() / 2];
            let p95 = values[values.len() * 95 / 100];
            Row {
                mode,
                size,
                clients,
                phase,
                p50,
                p95,
                mib_per_second: (size as f64 / 1_048_576.0) / (p50 as f64 / 1_000_000.0),
            }
        })
        .collect())
}

fn main() -> Result<()> {
    if !cfg!(any(windows, target_os = "linux")) {
        return Err("this experiment requires Windows or Linux".into());
    }
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/direction-switch-matrix.md".into());
    let release = std::env::current_exe()?
        .parent()
        .ok_or("example parent missing")?
        .parent()
        .ok_or("release directory missing")?
        .to_path_buf();
    let binary = release.join(if cfg!(windows) { "oi.exe" } else { "oi" });
    if !binary.is_file() {
        return Err(format!("build {} first", binary.display()).into());
    }
    let backend = server()?;
    let temporary = tempfile::tempdir()?;
    let mut proxies = Vec::new();
    for mode in MODES {
        let (guard, addr) = proxy(
            &binary,
            backend,
            mode,
            &temporary.path().join(format!("{mode}.toml")),
        )?;
        proxies.push((mode, guard, addr));
    }
    let mut rows = Vec::new();
    for (case_index, (size, clients, rounds)) in [
        (64 * 1024, 1, 20),
        (64 * 1024, 8, 8),
        (1024 * 1024, 1, 8),
        (1024 * 1024, 8, 4),
    ]
    .into_iter()
    .enumerate()
    {
        for offset in 0..proxies.len() {
            let (mode, _, addr) = &proxies[(case_index + offset) % proxies.len()];
            eprintln!("{mode}: {size} bytes, {clients} clients");
            rows.extend(case(mode, *addr, size, clients, rounds)?);
        }
    }
    let mut report = String::from(
        "# Direction-switch TCP relay matrix\n\nEach client keeps one connection and repeats four downloads followed by four uploads, with one unmeasured warmup cycle. The first transfer after each reversal is reported separately from the following three. All payloads are checked for corruption. Numbers are local single-run measurements.\n\n| Payload | Clients | Mode | Phase | p50 (µs) | p95 (µs) | MiB/s at p50 |\n| ---: | ---: | --- | --- | ---: | ---: | ---: |\n",
    );
    for row in rows {
        report.push_str(&format!(
            "| {} KiB | {} | {} | {} | {} | {} | {:.1} |\n",
            row.size / 1024,
            row.clients,
            row.mode,
            row.phase,
            row.p50,
            row.p95,
            row.mib_per_second
        ));
    }
    if let Some(parent) = Path::new(&output).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, report)?;
    println!("Wrote {output}");
    Ok(())
}
