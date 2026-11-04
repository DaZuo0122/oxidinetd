//! UDP forwarding throughput benchmarks.
//!
//! Runs the library's `start_udp_forwarding` in-process on a Tokio executor
//! with an async Tokio UDP client. UDP is connectionless: the client reuses
//! one socket across all iterations, while the forwarder creates a fresh
//! ephemeral socket per datagram — that per-request socket allocation is the
//! dominant forwarder cost being measured.

mod common;

use common::*;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxidinetd::config::Protocol;

const DATAGRAM_SIZES: &[usize] = &[128, 1024, 8192, 64000];

fn bench_udp_throughput(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("udp_throughput");
    tune_group(&mut group);
    for &size in DATAGRAM_SIZES {
        let payload = vec![0xCDu8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            let echo = spawn_udp_echo_server();
            let forwarder = spawn_forwarder(echo.addr, Protocol::Udp);
            let addr = forwarder.bind_addr;
            let payload = payload.to_vec();
            b.to_async(TokioExec::new()).iter_custom(move |iters| {
                let payload = payload.clone();
                let addr = addr;
                async move {
                    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
                        .await
                        .expect("bind client socket");
                    let start = std::time::Instant::now();
                    for _ in 0..iters {
                        async_udp_round_trip(&socket, addr, &payload).await;
                    }
                    start.elapsed()
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_udp_throughput);
criterion_main!(benches);
