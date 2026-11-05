//! Shared infrastructure for the in-process, fully-async proxy benchmarks.
//!
//! Unlike the earlier subprocess design, every benchmark runs the actual
//! library forwarding functions (`start_tcp_forwarding`,
//! `start_udp_forwarding`) in-process on a Tokio executor, and all clients
//! and echo servers use async Tokio I/O — the same async stack the proxy
//! itself is built on. This makes the benchmarks sensitive to changes in
//! the library's network code paths (accept loops, handler tasks, `io::copy`
//! in both directions, UDP connection tracking), which is exactly what a
//! regression check needs.
//!
//! # TIME_WAIT handling
//!
//! Per-connection benchmarks close a socket every iteration; at thousands of
//! connections per second the ephemeral port range would be exhausted by
//! TIME_WAIT sockets within seconds. The per-connection protocol avoids this:
//! the client half-closes (`shutdown(Write)`), reads to EOF, then drops the
//! socket with `SO_LINGER = 0` (forcing a RST). The backend is an
//! echo-once-close server, so the forwarder is always the *passive* closer
//! of its backend connection. The result is no TIME_WAIT accumulation on
//! either the client or the forwarder, keeping per-connection benchmarks
//! runnable indefinitely.

#![allow(dead_code)]

use oxidinetd::config::Protocol;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

fn bench_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
    })
}

/// Criterion's `AsyncExecutor` adapter, backed by the same Tokio runtime the
/// library uses.
pub struct TokioExec(&'static tokio::runtime::Runtime);
impl TokioExec {
    pub fn new() -> Self {
        Self(bench_runtime())
    }
}
impl criterion::async_executor::AsyncExecutor for TokioExec {
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.block_on(future)
    }
}

/// Applies sane default timings to a benchmark group. Network benchmarks are
/// inherently noisier than micro-benchmarks, so a 1s warm-up and a 2s
/// measurement window keep the whole suite quick while retaining enough
/// samples for tight confidence intervals.
pub fn tune_group<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
) {
    group.warm_up_time(Duration::from_millis(1000));
    group.measurement_time(Duration::from_secs(2));
}

/// Silences the process's stdout/stderr by default.
///
/// The library under test logs "New connection from ..." on stdout and
/// "Connection error: ..." on stderr for every forwarded connection. These
/// lines would flood the benchmark process's output — and when the output is
/// consumed through pipes (cargo-nextest, CI) thousands of lines per
/// benchmark can fill the pipe, causing the forwarder to block inside
/// `println!`/`eprintln!`, which stalls the accept loop and the Tokio
/// executor — benchmarks hang instead of measuring. Output is therefore
/// redirected to a null sink unless the `OI_BENCH_OUTPUT` flag is set.
/// Benchmark results are always saved to `target/criterion` regardless.
pub fn maybe_silence_output() {
    // The `--list` query must keep stdout: nextest parses the
    // "name: benchmark" lines from it.
    if std::env::args().any(|a| a == "--list") {
        return;
    }
    // Printing is opt-in: set OI_BENCH_OUTPUT=1 to see the forwarder's logs
    // and criterion's console output during a benchmark run.
    if std::env::var_os("OI_BENCH_OUTPUT").is_some() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let null = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open /dev/null");
        unsafe {
            libc::dup2(null.as_raw_fd(), 1);
            libc::dup2(null.as_raw_fd(), 2);
        }
    }
    #[cfg(windows)]
    unsafe {
        use winapi::um::fileapi::{CreateFileW, OPEN_EXISTING};
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::processenv::SetStdHandle;
        use winapi::um::winbase::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_WRITE};

        let wide: Vec<u16> = "NUL".encode_utf16().collect();
        let mut wide = wide;
        wide.push(0);
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
}

static NEXT_PROXY_PORT: AtomicU16 = AtomicU16::new(0);

pub fn random_port() -> u16 {
    let n = NEXT_PROXY_PORT.fetch_add(1, Ordering::Relaxed);
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as u64
        ^ ((std::process::id() as u64) << 32)
        ^ (n as u64) << 16;
    seed ^= seed >> 12;
    seed ^= seed << 25;
    seed ^= seed >> 27;
    let value = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32;
    20_000 + (value as u16) % 45_536
}

// --- in-process forwarder ----------------------------------------------------

struct Background {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Background {
    fn spawn<F, Fut>(run: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let handle = std::thread::spawn(move || {
            bench_runtime().block_on(async move {
                tokio::select! {
                    _ = run() => {},
                    _ = stopped => {},
                }
            });
        });
        Self {
            stop: Some(stop),
            handle: Some(handle),
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.join().expect("background benchmark thread panicked");
        }
    }
}

/// Runs one of the library's forwarding functions in-process on a Tokio
/// executor thread, mirroring how the real binary starts them. The handler
/// tasks spawned internally by the library run on Tokio's runtime worker
/// thread, exactly as in production.
pub struct Forwarder {
    pub bind_addr: SocketAddr,
    _background: Background,
}

impl Forwarder {
    pub fn start(bind_addr: SocketAddr, connect_addr: SocketAddr, protocol: Protocol) -> Forwarder {
        let background = Background::spawn(move || async move {
            let connect = connect_addr.to_string();
            let _ = match protocol {
                Protocol::Tcp | Protocol::TcpToUdp => {
                    oxidinetd::tcp_handler::start_tcp_forwarding(bind_addr, connect, protocol).await
                }
                Protocol::Udp | Protocol::UdpToTcp => {
                    oxidinetd::udp_handler::start_udp_forwarding(bind_addr, connect, None, protocol)
                        .await
                }
            };
        });
        Forwarder {
            bind_addr,
            _background: background,
        }
    }
}

/// Starts a forwarder on a free port, retrying if the chosen port was taken.
pub fn spawn_forwarder(connect_addr: SocketAddr, protocol: Protocol) -> Forwarder {
    let binds_tcp = matches!(&protocol, Protocol::Tcp | Protocol::TcpToUdp);
    for _ in 0..8 {
        let bind_addr = SocketAddr::from(([127, 0, 0, 1], random_port()));
        let forwarder = Forwarder::start(bind_addr, connect_addr, protocol.clone());
        let ready = if binds_tcp {
            wait_for_port(bind_addr, Duration::from_millis(500))
        } else {
            std::thread::sleep(Duration::from_millis(100));
            true
        };
        if ready {
            return forwarder;
        }
        drop(forwarder); // stop the listener before retrying with a new port
    }
    panic!("could not start forwarder: bind port taken on every attempt");
}

pub fn wait_for_port(addr: SocketAddr, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// --- async echo servers -------------------------------------------------------

pub struct EchoOnceServer {
    pub addr: SocketAddr,
    _background: Background,
}

/// Async TCP echo server that closes the connection after echoing. Used by
/// per-connection benchmarks so the forwarder is always the passive closer
/// of its backend connection (no TIME_WAIT accumulation).
pub fn spawn_echo_once_server() -> EchoOnceServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let background = Background::spawn(move || async move {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let listener = TcpListener::from_std(listener).expect("async listener");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(echo_until_closed(stream));
        }
    });
    EchoOnceServer {
        addr,
        _background: background,
    }
}

pub struct EchoServer {
    pub addr: SocketAddr,
    _background: Background,
}

/// Async TCP echo server that keeps connections open until EOF. Used by
/// sustained and concurrent benchmarks where connections must persist.
pub fn spawn_echo_server() -> EchoServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let addr = listener.local_addr().unwrap();
    let background = Background::spawn(move || async move {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let listener = TcpListener::from_std(listener).expect("async listener");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(echo_until_closed(stream));
        }
    });
    EchoServer {
        addr,
        _background: background,
    }
}

async fn echo_until_closed(mut stream: TcpStream) {
    let mut buf = vec![0u8; 65536];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
}

pub struct UdpEchoServer {
    pub addr: SocketAddr,
    _background: Background,
}

pub fn spawn_udp_echo_server() -> UdpEchoServer {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp echo server");
    let addr = socket.local_addr().unwrap();
    let background = Background::spawn(move || async move {
        socket
            .set_nonblocking(true)
            .expect("nonblocking udp socket");
        let socket = UdpSocket::from_std(socket).expect("async udp socket");
        let mut buf = vec![0u8; 65536];
        while let Ok((len, src)) = socket.recv_from(&mut buf).await {
            let _ = socket.send_to(&buf[..len], src).await;
        }
    });
    UdpEchoServer {
        addr,
        _background: background,
    }
}

// --- async clients ------------------------------------------------------------

/// Connects an async stream that resets (RST) on close instead of closing
/// gracefully, so per-connection benchmarks never accumulate TIME_WAIT
/// sockets. `SO_LINGER = 0` is applied before wrapping the socket.
pub fn connect_rst(addr: SocketAddr) -> std::io::Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_linger(Some(Duration::ZERO))?;
    socket.connect(&addr.into())?;
    let stream: std::net::TcpStream = socket.into();
    stream.set_nonblocking(true)?;
    TcpStream::from_std(stream)
}

/// One complete per-connection session through the forwarder: connect, write
/// the payload, read the exact echo, then tear down with a RST so the client
/// port is freed immediately.
///
/// The session reads exactly `payload.len()` bytes instead of waiting for
/// EOF, keeping the measurement independent of connection teardown.
pub async fn async_session(addr: SocketAddr, payload: &[u8]) {
    let mut stream = connect_rst(addr).expect("connect");
    stream.write_all(payload).await.expect("write");
    let mut buf = vec![0u8; 65536];
    let mut received = 0usize;
    while received < payload.len() {
        // Guard against stalls: if the echo never arrives (e.g. the
        // forwarder's executor is wedged), fail loudly instead of hanging
        // the benchmark process forever.
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
            .await
            .expect("session stalled waiting for the echo")
            .expect("read");
        assert!(n > 0, "connection closed before the full echo arrived");
        received += n;
    }
    assert_eq!(
        received,
        payload.len(),
        "echo size mismatch: expected {}, got {}",
        payload.len(),
        received
    );
    // Drop with SO_LINGER(0) sends a RST.
}

/// Runs `iters` write+read echo cycles on a persistent connection in full
/// duplex: a writer future streams the payload while the reader drains the
/// echo. No threads involved — both futures share the connection via a
/// split read and write halves of the stream.
pub async fn async_full_duplex(stream: &mut TcpStream, payload: &[u8], iters: u64) {
    let total_bytes = iters * payload.len() as u64;
    let (mut reader_stream, mut writer_stream) = stream.split();
    let write = async move {
        for _ in 0..iters {
            writer_stream.write_all(payload).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let read = async {
        let mut buf = vec![0u8; 65536];
        let mut received = 0u64;
        while received < total_bytes {
            let n = reader_stream.read(&mut buf).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before the full echo arrived",
                ));
            }
            received += n as u64;
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::try_join!(write, read).expect("full-duplex echo");
}

/// One datagram round trip on a persistent async UDP socket. A stall guard
/// converts a missed response (e.g. the forwarder's 100ms response window
/// elapsing on a slow machine) into a visible failure instead of a hang.
pub async fn async_udp_round_trip(socket: &UdpSocket, addr: SocketAddr, payload: &[u8]) {
    socket.send_to(payload, addr).await.expect("send datagram");
    let mut buf = vec![0u8; 65536];
    let n = tokio::time::timeout(Duration::from_secs(10), socket.recv_from(&mut buf))
        .await
        .expect("datagram round trip stalled waiting for a response")
        .expect("receive response");
    let (n, _) = n;
    assert_eq!(n, payload.len(), "echo size mismatch");
}
