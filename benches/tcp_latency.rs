//! TCP forwarding latency and connection setup rate benchmarks.
//!
//! The latency-vs-payload sweep (1B..64KiB) yields a curve whose intercept
//! reflects per-connection overhead (accept + task spawn + backend connect)
//! and whose slope reflects per-byte copy cost.

mod common;

use common::*;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use oxidinetd::config::Protocol;

const LATENCY_SIZES: &[usize] = &[1, 100, 1024, 4096, 65536];

fn bench_latency(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("tcp_latency/round_trip");
    tune_group(&mut group);
    for &size in LATENCY_SIZES {
        let payload = vec![0x42u8; size];
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

fn bench_connection_rate(c: &mut Criterion) {
    maybe_silence_output();
    // Bound the iteration count: every connect+drop leaves the forwarder's
    // backend socket in TIME_WAIT, so keep the measurement window short to
    // stay well within the ephemeral port range.
    let mut group = c.benchmark_group("tcp_connection_rate");
    tune_group(&mut group);
    group.sample_size(50);
    group.measurement_time(std::time::Duration::from_millis(200));
    group.bench_function("setup", |b| {
        let echo = spawn_echo_once_server();
        let forwarder = spawn_forwarder(echo.addr, Protocol::Tcp);
        let addr = forwarder.bind_addr;
        b.to_async(TokioExec::new()).iter_custom(move |iters| {
            let addr = addr;
            async move {
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let stream = connect_rst(addr).expect("connect");
                    drop(stream); // RST: client port freed immediately
                }
                start.elapsed()
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_latency, bench_connection_rate);
criterion_main!(benches);
