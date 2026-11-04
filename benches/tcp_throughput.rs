//! TCP forwarding throughput benchmarks.
//!
//! Runs the library's `start_tcp_forwarding` in-process on a Tokio executor
//! and drives it with async Tokio clients — the same async stack the proxy is
//! built on.
//!
//! Two measurement modes per payload size:
//! - per-connection: a fresh connection per iteration (includes accept +
//!   handler-task spawn + backend connect); the backend is an echo-once
//!   server and the client tears the connection down with a RST, so no
//!   TIME_WAIT accumulates
//! - sustained: one persistent connection, `iters` full-duplex write+read
//!   cycles per sample via `iter_custom` (isolates pure copy throughput)
//!
//! The echo server and forwarder are created inside the benchmark closure so
//! they only start when the benchmark actually runs (criterion also invokes
//! the binary in `--list` mode, where nothing may execute).

mod common;

use common::*;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxidinetd::config::Protocol;

const PER_CONN_SIZES: &[usize] = &[128, 1024, 65536];
const SUSTAINED_SIZES: &[usize] = &[1024 * 1024, 16 * 1024 * 1024];

fn bench_per_connection(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_throughput/per_connection");
    tune_group(&mut group);
    for &size in PER_CONN_SIZES {
        let payload = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            let echo = spawn_echo_once_server();
            let forwarder = spawn_forwarder(echo.addr, Protocol::Tcp);
            let addr = forwarder.bind_addr;
            let payload = payload.to_vec();
            b.to_async(TokioExec::new()).iter(move || {
                let payload = payload.clone();
                async move { async_session(addr, &payload).await }
            });
        });
    }
    group.finish();
}

fn bench_sustained(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_throughput/sustained");
    tune_group(&mut group);
    for &size in SUSTAINED_SIZES {
        let payload = vec![0xABu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            let echo = spawn_echo_server();
            let forwarder = spawn_forwarder(echo.addr, Protocol::Tcp);
            let addr = forwarder.bind_addr;
            let payload = payload.to_vec();
            b.to_async(TokioExec::new()).iter_custom(move |iters| {
                let payload = payload.clone();
                let addr = addr;
                async move {
                    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
                    let start = std::time::Instant::now();
                    async_full_duplex(&mut stream, &payload, iters).await;
                    start.elapsed()
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_per_connection, bench_sustained);
criterion_main!(benches);
