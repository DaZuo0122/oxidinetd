//! Stage timings for direct and proxied TCP echo sessions.
//! Run with: cargo run --release --example tcp_stages -- [SAMPLES] [BYTES] [async|sync]

#[cfg(feature = "tcp-profile")]
use oxidinetd::tcp_profile::{Event, Stage};
#[cfg(feature = "tcp-profile")]
use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

struct BackendEvent {
    id: u64,
    accepted: Instant,
    request_read: Instant,
    response_written: Instant,
}

#[cfg(feature = "tcp-profile")]
struct TraceSample {
    client_addr: SocketAddr,
    start: Instant,
    connected: Instant,
    finished: Instant,
    backend_accepted: Instant,
    backend_read: Instant,
}

#[derive(Default)]
struct Timings {
    client_connect: Vec<u64>,
    backend_accept: Vec<u64>,
    request_delivery: Vec<u64>,
    response_delivery: Vec<u64>,
    total: Vec<u64>,
}

impl Timings {
    fn add(&mut self, start: Instant, connected: Instant, finished: Instant, event: BackendEvent) {
        self.client_connect.push(us(start, connected));
        self.backend_accept.push(us(start, event.accepted));
        self.request_delivery
            .push(us(connected, event.request_read));
        self.response_delivery
            .push(us(event.response_written, finished));
        self.total.push(us(start, finished));
    }

    fn print(&mut self, label: &str) {
        eprintln!("{label}:");
        for (name, data) in [
            ("client connect", &mut self.client_connect),
            ("backend accept from start", &mut self.backend_accept),
            ("client connect to backend read", &mut self.request_delivery),
            ("backend write to client read", &mut self.response_delivery),
            ("complete round trip", &mut self.total),
        ] {
            data.sort_unstable();
            let p50 = data[data.len() / 2];
            let p95 = data[(data.len() * 95 / 100).min(data.len() - 1)];
            let p99 = data[(data.len() * 99 / 100).min(data.len() - 1)];
            let above_1ms = data.iter().filter(|&&v| v > 1000).count();
            eprintln!("  {name}: p50 {p50} µs, p95 {p95} µs, p99 {p99} µs, >1 ms {above_1ms}");
        }
    }
}

fn us(start: Instant, end: Instant) -> u64 {
    end.saturating_duration_since(start).as_micros() as u64
}

async fn backend(listener: TcpListener, bytes: usize, events: mpsc::UnboundedSender<BackendEvent>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let accepted = Instant::now();
        let events = events.clone();
        tokio::spawn(async move {
            let mut payload = vec![0; bytes];
            if stream.read_exact(&mut payload).await.is_err() {
                return; // readiness probe or disconnected client
            }
            let request_read = Instant::now();
            let id = u64::from_le_bytes(payload[..8].try_into().expect("sequence number"));
            if stream.write_all(&payload).await.is_err() {
                return;
            }
            let response_written = Instant::now();
            let _ = events.send(BackendEvent {
                id,
                accepted,
                request_read,
                response_written,
            });
        });
    }
}

async fn session(
    addr: SocketAddr,
    id: u64,
    bytes: usize,
    sync_connect: bool,
    events: &mut mpsc::UnboundedReceiver<BackendEvent>,
) -> Result<(SocketAddr, Instant, Instant, Instant, BackendEvent)> {
    let mut payload = vec![0x42; bytes];
    payload[..8].copy_from_slice(&id.to_le_bytes());
    let start = Instant::now();
    let mut client = if sync_connect {
        connect_sync(addr)?
    } else {
        TcpStream::connect(addr).await?
    };
    let client_addr = client.local_addr()?;
    let connected = Instant::now();
    client.write_all(&payload).await?;
    let mut response = vec![0; bytes];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut response)).await??;
    let finished = Instant::now();
    if response != payload {
        return Err("echo mismatch".into());
    }
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await?
        .ok_or("backend event channel closed")?;
    if event.id != id {
        return Err(format!("backend event mismatch: expected {id}, got {}", event.id).into());
    }
    Ok((client_addr, start, connected, finished, event))
}

#[cfg(feature = "tcp-profile")]
fn print_profile(samples: &[TraceSample], events: &mut mpsc::UnboundedReceiver<Event>) {
    let mut by_client: HashMap<SocketAddr, HashMap<Stage, Instant>> = HashMap::new();
    while let Ok(event) = events.try_recv() {
        by_client
            .entry(event.client_addr)
            .or_default()
            .insert(event.stage, event.at);
    }
    eprintln!("proxy internal stages (microseconds):");
    for (name, from, to) in [
        ("accept to handler", Stage::Accepted, Stage::HandlerStarted),
        (
            "backend connect",
            Stage::BackendConnectStarted,
            Stage::BackendConnected,
        ),
        (
            "connect to copy",
            Stage::BackendConnected,
            Stage::CopyStarted,
        ),
        (
            "copy to first client read",
            Stage::CopyStarted,
            Stage::ClientRead,
        ),
        (
            "client read to backend write",
            Stage::ClientRead,
            Stage::BackendWritten,
        ),
        (
            "backend read to client write",
            Stage::BackendRead,
            Stage::ClientWritten,
        ),
    ] {
        let mut values: Vec<u64> = samples
            .iter()
            .filter_map(|sample| {
                let stages = by_client.get(&sample.client_addr)?;
                Some(us(*stages.get(&from)?, *stages.get(&to)?))
            })
            .collect();
        values.sort_unstable();
        if !values.is_empty() {
            let p50 = values[values.len() / 2];
            let p99 = values[(values.len() * 99 / 100).min(values.len() - 1)];
            let over_1ms = values.iter().filter(|&&value| value > 1000).count();
            eprintln!(
                "  {name}: n {}, p50 {p50}, p99 {p99}, >1 ms {over_1ms}",
                values.len()
            );
        }
    }
    let mut outliers: Vec<_> = samples
        .iter()
        .filter(|sample| us(sample.start, sample.finished) > 5000)
        .collect();
    outliers.sort_unstable_by_key(|sample| std::cmp::Reverse(us(sample.start, sample.finished)));
    eprintln!("proxy sessions >5 ms: {}", outliers.len());
    for sample in outliers.into_iter().take(8) {
        eprintln!(
            "  total {} µs, client connect {} µs, backend accept {} µs, backend read {} µs",
            us(sample.start, sample.finished),
            us(sample.start, sample.connected),
            us(sample.start, sample.backend_accepted),
            us(sample.start, sample.backend_read)
        );
        if let Some(stages) = by_client.get(&sample.client_addr) {
            for stage in [
                Stage::Accepted,
                Stage::HandlerStarted,
                Stage::BackendConnectStarted,
                Stage::BackendConnected,
                Stage::CopyStarted,
                Stage::ClientRead,
                Stage::BackendWritten,
                Stage::BackendRead,
                Stage::ClientWritten,
            ] {
                if let Some(at) = stages.get(&stage) {
                    eprint!(" {:?}={}", stage, us(sample.start, *at));
                }
            }
            eprintln!();
        }
    }
}

fn connect_sync(addr: SocketAddr) -> std::io::Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.connect(&addr.into())?;
    let stream: std::net::TcpStream = socket.into();
    stream.set_nonblocking(true)?;
    TcpStream::from_std(stream)
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let samples: usize = args.next().unwrap_or_else(|| "200".into()).parse()?;
    let bytes: usize = args.next().unwrap_or_else(|| "8".into()).parse()?;
    let connect_mode = args.next().unwrap_or_else(|| "async".into());
    if samples == 0
        || bytes < 8
        || !["async", "sync"].contains(&connect_mode.as_str())
        || args.next().is_some()
    {
        return Err("usage: tcp_stages [SAMPLES>0] [BYTES>=8] [async|sync]".into());
    }
    let sync_connect = connect_mode == "sync";
    #[cfg(feature = "tcp-profile")]
    let (profile_sender, mut profile_events) = mpsc::unbounded_channel();
    #[cfg(feature = "tcp-profile")]
    assert!(
        oxidinetd::tcp_profile::install(profile_sender),
        "profile sink already installed"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let backend_addr = listener.local_addr()?;
    let (sender, mut events) = mpsc::unbounded_channel();
    let backend_task = tokio::spawn(backend(listener, bytes, sender));

    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let proxy_addr = reservation.local_addr()?;
    drop(reservation);
    let proxy_task = tokio::spawn(async move {
        let _ = oxidinetd::tcp_handler::start_tcp_forwarding(
            proxy_addr,
            backend_addr.to_string(),
            oxidinetd::config::Protocol::Tcp,
        )
        .await;
    });
    for _ in 0..100 {
        if TcpStream::connect(proxy_addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut direct = Timings::default();
    let mut proxied = Timings::default();
    #[cfg(feature = "tcp-profile")]
    let mut traces = Vec::with_capacity(samples);
    for i in 0..samples {
        let paths = if i % 2 == 0 {
            [
                (backend_addr, &mut direct, (i as u64) * 2),
                (proxy_addr, &mut proxied, (i as u64) * 2 + 1),
            ]
        } else {
            [
                (proxy_addr, &mut proxied, (i as u64) * 2 + 1),
                (backend_addr, &mut direct, (i as u64) * 2),
            ]
        };
        for (addr, timings, id) in paths {
            let (client_addr, start, connected, finished, event) =
                session(addr, id, bytes, sync_connect, &mut events).await?;
            #[cfg(not(feature = "tcp-profile"))]
            let _ = client_addr;
            #[cfg(feature = "tcp-profile")]
            if addr == proxy_addr {
                traces.push(TraceSample {
                    client_addr,
                    start,
                    connected,
                    finished,
                    backend_accepted: event.accepted,
                    backend_read: event.request_read,
                });
            }
            timings.add(start, connected, finished, event);
        }
    }
    proxy_task.abort();
    backend_task.abort();
    eprintln!(
        "{samples} paired samples, {bytes} bytes, {connect_mode} client connect, microseconds"
    );
    direct.print("direct");
    proxied.print("proxy");
    #[cfg(feature = "tcp-profile")]
    print_profile(&traces, &mut profile_events);
    Ok(())
}

fn main() -> Result<()> {
    let workers: usize = std::env::var("OI_PROFILE_WORKERS")
        .ok()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(std::thread::available_parallelism()?.get());
    if workers == 0 {
        return Err("OI_PROFILE_WORKERS must be positive".into());
    }
    eprintln!("Tokio worker threads: {workers}");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?
        .block_on(run())
}
