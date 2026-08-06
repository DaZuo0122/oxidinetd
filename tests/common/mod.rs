//! Shared helpers for the integration tests. Each test binary only uses a
//! subset of these, so dead-code analysis is disabled for the whole module.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_oi");

static NEXT_PROXY_PORT: AtomicU16 = AtomicU16::new(0);

/// Returns a port for the proxy to bind. The port is random and free of any
/// pid-derived scheme: under cargo-nextest every test runs in its own
/// process, and pid-based allocation has been observed to collide (Windows
/// allocates pids with ASLR-like randomization). Actual exclusivity is
/// guaranteed by the proxy's own bind; collisions are detected via the
/// proxy's stderr and retried by `spawn_proxy` with a fresh random port.
pub fn reserve_proxy_port() -> u16 {
    random_proxy_port()
}

fn random_proxy_port() -> u16 {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
        ^ ((std::process::id() as u64) << 32);
    // xorshift64*
    seed ^= seed >> 12;
    seed ^= seed << 25;
    seed ^= seed >> 27;
    let value = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32;
    20_000 + (value as u16) % 45_536
}

pub struct TestProxy {
    pub child: Child,
    pub bind_addr: SocketAddr,
    pub _dir: tempfile::TempDir,
}

impl TestProxy {
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn spawn_proxy(toml_config: &str) -> TestProxy {
    spawn_proxy_with_file(toml_config, "proxy.toml")
}

pub fn spawn_proxy_from_legacy_conf(conf: &str) -> TestProxy {
    spawn_proxy_with_file(conf, "proxy.conf")
}

/// The proxy prints "<proto> forwarding error: ..." when its bind fails.
const BIND_ERROR_MARKER: &str = "forwarding error";

/// Spawns the proxy and makes sure it actually owns its bind port. If
/// another concurrently running test grabbed the port first, the proxy
/// reports the bind failure on stderr; the helper then kills it and retries
/// with a fresh random port.
fn spawn_proxy_with_file(content: &str, file_name: &str) -> TestProxy {
    let binds_tcp_listener = binds_tcp_listener(content);
    let mut port = extract_bind_port(content).expect("bind_port in config");
    let mut config = content.to_string();

    for _attempt in 0..8 {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(file_name);
        std::fs::write(&path, &config).expect("write config");

        let mut child = Command::new(BIN)
            .arg("-c")
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn oi binary");

        // Drain the proxy's stderr on a background thread so a bind failure
        // is observable (and the pipe cannot fill up).
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        {
            let buf = stderr_buf.clone();
            let mut stderr = child.stderr.take().expect("piped stderr");
            std::thread::spawn(move || {
                let mut data = Vec::new();
                let _ = stderr.read_to_end(&mut data);
                *buf.lock().unwrap() = data;
            });
        }

        let bind_addr = SocketAddr::from(([127, 0, 0, 1], port));

        let mut bound_at: Option<Instant> = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        let udp_ready_deadline = Instant::now() + Duration::from_millis(400);
        let mut collided = false;

        while Instant::now() < deadline {
            if stderr_contains(&stderr_buf, BIND_ERROR_MARKER) {
                collided = true;
                break;
            }
            if binds_tcp_listener {
                if bound_at.is_none()
                    && TcpStream::connect_timeout(&bind_addr, Duration::from_millis(30)).is_ok()
                {
                    bound_at = Some(Instant::now());
                }
                if let Some(t) = bound_at {
                    // Grace period: if another test's proxy holds the port,
                    // our proxy's bind error surfaces in this window.
                    if t.elapsed() >= Duration::from_millis(50) {
                        break;
                    }
                }
            } else if Instant::now() >= udp_ready_deadline {
                // UDP-only: there is no TCP listener to probe. Give the
                // proxy time to bind and error out, then let the caller's
                // retrying UDP round trips take over.
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if !collided {
            collided = stderr_contains(&stderr_buf, BIND_ERROR_MARKER);
        }

        if !collided && (!binds_tcp_listener || bound_at.is_some()) {
            return TestProxy {
                child,
                bind_addr,
                _dir: dir,
            };
        }

        // The port was taken by another concurrent test: kill and retry.
        let _ = child.kill();
        let _ = child.wait();
        let new_port = random_proxy_port();
        config = rewrite_bind_port(&config, port, new_port);
        port = new_port;
    }
    panic!("could not start the proxy: the bind port was taken on every attempt");
}

fn stderr_contains(buf: &Arc<Mutex<Vec<u8>>>, marker: &str) -> bool {
    let data = buf.lock().unwrap();
    let text = String::from_utf8_lossy(&data);
    text.contains(marker)
}

/// True when the rule binds a TCP listener. `tcp` and `tcptoudp` rules do;
/// `udp` and `udptotcp` rules bind a UDP socket only, so the port cannot be
/// probed with a TCP connect.
fn binds_tcp_listener(content: &str) -> bool {
    !(content.contains("protocol = \"udp\"") || content.contains("protocol = \"udptotcp\""))
}

/// Rewrites the bind port in a TOML or legacy conf config string.
fn rewrite_bind_port(content: &str, old: u16, new: u16) -> String {
    if let Some(idx) = content.find("bind_port") {
        let eq = content[idx..].find('=').map(|i| idx + i).expect("bind_port =");
        let rest = &content[eq + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let num = rest[..end].trim();
        if num.parse::<u16>().ok() == Some(old) {
            return format!("{}= {}{}", &content[..eq + 1], new, &rest[end..]);
        }
        return content.to_string();
    }
    // Legacy conf: the second token of the first forwarding rule line.
    let mut out = String::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 4 && parts[1].parse::<u16>().ok() == Some(old) {
            let start = line.find(parts[1]).expect("token");
            out.push_str(&line[..start]);
            out.push_str(&new.to_string());
            out.push_str(&line[start + parts[1].len()..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn extract_bind_port(content: &str) -> Option<u16> {
    // TOML style: bind_port = N
    if let Some(port) = content.lines().find_map(|line| {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("bind_port") {
            rest.split('=').nth(1).map(|p| p.trim().parse::<u16>().ok()).flatten()
        } else {
            None
        }
    }) {
        return Some(port);
    }
    // Legacy conf style: bind_addr bind_port connect_addr connect_port
    content.lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 4 {
            parts[1].parse::<u16>().ok()
        } else {
            None
        }
    })
}

pub fn wait_for_port(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

pub fn tcp_round_trip(proxy_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy_addr).expect("connect to proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    // The proxy does not propagate half-close to the backend, so the echo
    // server never sees EOF. Write in chunks and read the echoed bytes
    // interleaved, so neither direction stalls on saturated socket buffers.
    let mut response = Vec::with_capacity(payload.len());
    let mut buf = [0u8; 65536];
    let mut expected = 0;
    for chunk in payload.chunks(262_144) {
        stream.write_all(chunk).expect("write payload");
        expected += chunk.len();
        while response.len() < expected {
            let n = stream.read(&mut buf).expect("read response");
            assert!(n > 0, "connection closed before the full response arrived");
            response.extend_from_slice(&buf[..n]);
        }
    }
    response
}

pub struct TcpEchoServer {
    pub addr: SocketAddr,
    pub connections: Arc<std::sync::atomic::AtomicUsize>,
    handle: std::thread::JoinHandle<()>,
}

pub fn spawn_tcp_echo_server() -> TcpEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connections_clone = connections.clone();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            connections_clone.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    TcpEchoServer {
        addr,
        connections,
        handle,
    }
}

pub struct TcpSlowWriteEchoServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Echo server that writes responses back in small chunks with delays,
/// simulating a slow server (backpressure on the server-to-client path).
pub fn spawn_tcp_slow_write_echo_server(chunk_size: usize, delay: Duration) -> TcpSlowWriteEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut written = 0;
                            while written < n {
                                let end = (written + chunk_size).min(n);
                                if stream.write_all(&buf[written..end]).is_err() {
                                    return;
                                }
                                written = end;
                                std::thread::sleep(delay);
                            }
                        }
                    }
                }
            });
        }
    });
    TcpSlowWriteEchoServer { addr, _handle: handle }
}

pub struct TcpSlowReadEchoServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Echo server that reads at most `chunk_size` bytes per cycle and sleeps
/// in between, throttling the client-to-server path.
pub fn spawn_tcp_slow_read_echo_server(chunk_size: usize, delay: Duration) -> TcpSlowReadEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                let target = chunk_size.min(65536);
                loop {
                    let mut read = 0;
                    while read < target {
                        match stream.read(&mut buf[read..target]) {
                            Ok(0) => return,
                            Ok(n) => read += n,
                            Err(_) => return,
                        }
                    }
                    let _ = stream.write_all(&buf[..read]);
                    std::thread::sleep(delay);
                }
            });
        }
    });
    TcpSlowReadEchoServer { addr, _handle: handle }
}

pub struct TcpDelayedEchoServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Echo server that sleeps before responding, simulating a slow backend.
pub fn spawn_tcp_delayed_echo_server(delay: Duration) -> TcpDelayedEchoServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            std::thread::sleep(delay);
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    TcpDelayedEchoServer { addr, _handle: handle }
}

pub struct TcpCloseAfterReadServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Accepts a connection, reads one chunk, then drops the socket without
/// draining pending data so the peer observes a connection reset (RST).
pub fn spawn_tcp_close_after_read_server() -> TcpCloseAfterReadServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                drop(stream);
            });
        }
    });
    TcpCloseAfterReadServer { addr, _handle: handle }
}

pub struct TcpCloseImmediatelyServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Accepts a connection and immediately closes it.
pub fn spawn_tcp_close_immediately_server() -> TcpCloseImmediatelyServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    TcpCloseImmediatelyServer { addr, _handle: handle }
}

pub struct TcpEchoOnceServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

/// Echoes the first message then closes the connection.
pub fn spawn_tcp_echo_once_server() -> TcpEchoOnceServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 16384];
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let _ = stream.write_all(&buf[..n]);
                    }
                }
            });
        }
    });
    TcpEchoOnceServer { addr, _handle: handle }
}

pub struct UdpEchoServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

pub fn spawn_udp_echo_server() -> UdpEchoServer {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind udp echo server");
    let addr = socket.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((len, src)) => {
                    let _ = socket.send_to(&buf[..len], src);
                }
                Err(_) => break,
            }
        }
    });
    UdpEchoServer { addr, _handle: handle }
}

pub struct UdpDelayedEchoServer {
    pub addr: SocketAddr,
    _handle: std::thread::JoinHandle<()>,
}

pub fn spawn_udp_delayed_echo_server(delay: Duration) -> UdpDelayedEchoServer {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind udp echo server");
    let addr = socket.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((len, src)) => {
                    std::thread::sleep(delay);
                    let _ = socket.send_to(&buf[..len], src);
                }
                Err(_) => break,
            }
        }
    });
    UdpDelayedEchoServer { addr, _handle: handle }
}

pub struct UdpSinkServer {
    pub addr: SocketAddr,
    pub received: Arc<std::sync::atomic::AtomicUsize>,
    _handle: std::thread::JoinHandle<()>,
}

/// Receives datagrams and never responds.
pub fn spawn_udp_sink_server() -> UdpSinkServer {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind udp sink server");
    let addr = socket.local_addr().unwrap();
    let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let received_clone = received.clone();
    let handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match socket.recv_from(&mut buf) {
                Ok(_) => {
                    received_clone.fetch_add(1, Ordering::SeqCst);
                }
                Err(_) => break,
            }
        }
    });
    UdpSinkServer {
        addr,
        received,
        _handle: handle,
    }
}

pub fn udp_round_trip(proxy_addr: SocketAddr, payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    socket.send_to(payload, proxy_addr)?;
    let mut buf = vec![0u8; 65536];
    let (len, _) = socket.recv_from(&mut buf)?;
    Ok(buf[..len].to_vec())
}

/// Retries a UDP round trip until it succeeds or the deadline passes.
/// Needed because the proxy task may not have bound its socket yet.
pub fn udp_round_trip_with_retries(proxy_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_err: Option<std::io::Error> = None;
    while Instant::now() < deadline {
        match udp_round_trip(proxy_addr, payload) {
            Ok(resp) => return resp,
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    panic!("udp round trip failed: {:?}", last_err);
}
