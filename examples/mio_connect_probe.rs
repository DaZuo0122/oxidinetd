//! Compare Mio writable readiness with a blocking TCP connect on loopback.
//! Run with: cargo run --release --example mio_connect_probe -- [PAIRS] [shared|fresh]

use std::error::Error;
use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn summarize(label: &str, values: &mut [u64]) {
    values.sort_unstable();
    let p50 = values[values.len() / 2];
    let p95 = values[values.len() * 95 / 100];
    let p99 = values[values.len() * 99 / 100];
    let over_1ms = values.iter().filter(|&&value| value > 1000).count();
    println!("{label}: p50 {p50} µs, p95 {p95} µs, p99 {p99} µs, >1 ms {over_1ms}");
}

fn mio_connect(
    addr: std::net::SocketAddr,
    poll: &mut mio::Poll,
    events: &mut mio::Events,
    token: mio::Token,
) -> Result<Instant> {
    let mut socket = mio::net::TcpStream::connect(addr)?;
    poll.registry()
        .register(&mut socket, token, mio::Interest::WRITABLE)?;
    loop {
        poll.poll(events, Some(Duration::from_secs(5)))?;
        if events.iter().any(|event| event.token() == token) {
            break;
        }
    }
    if let Some(error) = socket.take_error()? {
        return Err(error.into());
    }
    let ready = Instant::now();
    socket.write_all(&[1])?;
    poll.registry().deregister(&mut socket)?;
    Ok(ready)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let pairs: usize = args.next().unwrap_or_else(|| "400".into()).parse()?;
    let mode = args.next().unwrap_or_else(|| "shared".into());
    if pairs == 0 || !["shared", "fresh"].contains(&mode.as_str()) || args.next().is_some() {
        return Err("usage: mio_connect_probe [PAIRS>0] [shared|fresh]".into());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let (accepted_sender, accepted_receiver) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for _ in 0..pairs * 2 {
            let (mut stream, _) = listener.accept().expect("accept");
            accepted_sender
                .send(Instant::now())
                .expect("send accept time");
            let mut one = [0u8; 1];
            std::io::Read::read_exact(&mut stream, &mut one).expect("read byte");
        }
    });

    let mut blocking = Vec::with_capacity(pairs);
    let mut mio = Vec::with_capacity(pairs);
    let mut mio_accept_before_ready = 0;
    let mut shared_poll = mio::Poll::new()?;
    let mut shared_events = mio::Events::with_capacity(8);
    let mut next_token = 0;
    for i in 0..pairs {
        for is_mio in [i % 2 == 0, i % 2 != 0] {
            let start = Instant::now();
            let ready = if is_mio {
                next_token += 1;
                let token = mio::Token(next_token);
                if mode == "fresh" {
                    let mut poll = mio::Poll::new()?;
                    let mut events = mio::Events::with_capacity(8);
                    mio_connect(addr, &mut poll, &mut events, token)?
                } else {
                    mio_connect(addr, &mut shared_poll, &mut shared_events, token)?
                }
            } else {
                let mut socket = std::net::TcpStream::connect(addr)?;
                let ready = Instant::now();
                socket.write_all(&[1])?;
                ready
            };
            let accepted = accepted_receiver.recv_timeout(Duration::from_secs(5))?;
            if is_mio {
                mio.push(ready.duration_since(start).as_micros() as u64);
                mio_accept_before_ready += usize::from(accepted < ready);
            } else {
                blocking.push(ready.duration_since(start).as_micros() as u64);
            }
        }
    }
    server.join().expect("server thread panicked");
    println!("{pairs} paired loopback connections, {mode} Mio poll, microseconds");
    summarize("blocking connect", &mut blocking);
    summarize("Mio writable readiness", &mut mio);
    println!(
        "Mio: backend accepted before writable event in {mio_accept_before_ready}/{pairs} cases"
    );
    Ok(())
}
