//! Direct loopback echo versus the same echo server reached through the proxy.
//! This control helps separate client, backend and loopback costs from proxy cost.

mod common;

use common::*;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use oxidinetd::config::Protocol;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ROUND_TRIP_SIZES: &[usize] = &[1, 1024, 65536];
const SUSTAINED_SIZES: &[usize] = &[1024 * 1024, 16 * 1024 * 1024];

fn round_trip(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_diagnostics/round_trip");
    tune_group(&mut group);
    for &size in ROUND_TRIP_SIZES {
        let payload = vec![0x42; size];
        for path in ["direct", "proxy"] {
            group.bench_with_input(BenchmarkId::new(path, size), &payload, |b, payload| {
                let echo = spawn_echo_once_server();
                let forwarder =
                    (path == "proxy").then(|| spawn_forwarder(echo.addr, Protocol::Tcp));
                let addr = forwarder.as_ref().map_or(echo.addr, |f| f.bind_addr);
                let payload = payload.to_vec();
                b.to_async(TokioExec::new()).iter(move || {
                    let payload = payload.clone();
                    async move { async_session(addr, &payload).await }
                });
            });
        }
    }
    group.finish();
}

fn sustained(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_diagnostics/sustained");
    tune_group(&mut group);
    for &size in SUSTAINED_SIZES {
        let payload = vec![0x42; size];
        for path in ["direct", "proxy"] {
            group.bench_with_input(BenchmarkId::new(path, size), &payload, |b, payload| {
                let echo = spawn_echo_server();
                let forwarder =
                    (path == "proxy").then(|| spawn_forwarder(echo.addr, Protocol::Tcp));
                let addr = forwarder.as_ref().map_or(echo.addr, |f| f.bind_addr);
                let payload = payload.to_vec();
                b.to_async(TokioExec::new()).iter_custom(move |iters| {
                    let payload = payload.clone();
                    async move {
                        let mut stream =
                            tokio::net::TcpStream::connect(addr).await.expect("connect");
                        let start = std::time::Instant::now();
                        async_full_duplex(&mut stream, &payload, iters).await;
                        start.elapsed()
                    }
                });
            });
        }
    }
    group.finish();
}

fn persistent_round_trip(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_diagnostics/persistent_round_trip");
    tune_group(&mut group);
    for &size in ROUND_TRIP_SIZES {
        let payload = vec![0x42; size];
        for path in ["direct", "proxy"] {
            group.bench_with_input(BenchmarkId::new(path, size), &payload, |b, payload| {
                let echo = spawn_echo_server();
                let forwarder =
                    (path == "proxy").then(|| spawn_forwarder(echo.addr, Protocol::Tcp));
                let addr = forwarder.as_ref().map_or(echo.addr, |f| f.bind_addr);
                let payload = payload.to_vec();
                b.to_async(TokioExec::new()).iter_custom(move |iters| {
                    let payload = payload.clone();
                    async move {
                        let mut stream = connect_rst(addr).expect("connect");
                        let mut response = vec![0; payload.len()];
                        let start = std::time::Instant::now();
                        for _ in 0..iters {
                            stream.write_all(&payload).await.expect("write");
                            stream.read_exact(&mut response).await.expect("read echo");
                        }
                        start.elapsed()
                    }
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, round_trip, sustained, persistent_round_trip);
criterion_main!(benches);
