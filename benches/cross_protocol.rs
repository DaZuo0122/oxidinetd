//! Cross-protocol forwarding throughput benchmarks.
//!
//! - tcptoudp: `start_tcp_forwarding` with `Protocol::TcpToUdp`; TCP client
//!   sessions per iteration, UDP backend.
//! - udptotcp: `start_udp_forwarding` with `Protocol::UdpToTcp`; one async
//!   UDP client socket, the forwarder keeps a persistent TCP stream to the
//!   backend after the first datagram (sustained mode).

mod common;

use common::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use oxidinetd::config::Protocol;

const SMALL_SIZES: &[usize] = &[128, 1024];
const LARGE_TCP_TO_UDP: &[usize] = &[64000];

fn bench_tcptoudp(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcptoudp");
    tune_group(&mut group);
    for &size in SMALL_SIZES.iter().chain(LARGE_TCP_TO_UDP) {
        let payload = vec![0x11u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            let echo = spawn_udp_echo_server();
            let forwarder = spawn_forwarder(echo.addr, Protocol::TcpToUdp);
            let addr = forwarder.bind_addr;
            let payload = payload.to_vec();
            b.to_async(SmolExec).iter(move || {
                let payload = payload.clone();
                async move { async_session(addr, &payload).await }
            });
        });
    }
    group.finish();
}

fn bench_udptotcp(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("udptotcp");
    tune_group(&mut group);
    for &size in SMALL_SIZES {
        let payload = vec![0x22u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            let echo = spawn_echo_server();
            let forwarder = spawn_forwarder(echo.addr, Protocol::UdpToTcp);
            let addr = forwarder.bind_addr;
            let payload = payload.to_vec();
            b.to_async(SmolExec).iter_custom(move |iters| {
                let payload = payload.clone();
                let addr = addr;
                async move {
                    let socket = smol::net::UdpSocket::bind("127.0.0.1:0")
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

criterion_group!(benches, bench_tcptoudp, bench_udptotcp);
criterion_main!(benches);
