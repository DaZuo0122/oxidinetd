//! Concurrent TCP load benchmarks.
//!
//! N pre-spawned client threads each hold a persistent async connection
//! through the in-process forwarder and stream `PER_CLIENT_BYTES` per round
//! (full-duplex echo). A double barrier synchronises each round: the
//! benchmarked closure releases the start barrier and blocks on the end
//! barrier until every client finished its transfer, so the measured time
//! covers the full concurrent round with no per-iteration thread-spawn cost.
//!
//! Teardown uses a third (final) barrier: the quit flag is only consulted
//! right after the start-barrier rendezvous, so every client always
//! participates in the same barrier sequence as the main thread.

mod common;

use common::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};

const CLIENT_COUNTS: &[usize] = &[1, 4, 10, 25];
const PER_CLIENT_BYTES: usize = 256 * 1024;

fn bench_concurrent(c: &mut Criterion) {
    maybe_silence_output();
    let mut group = c.benchmark_group("concurrent");
    tune_group(&mut group);
    for &clients in CLIENT_COUNTS {
        group.throughput(Throughput::Bytes((clients * PER_CLIENT_BYTES) as u64));
        group.bench_function(BenchmarkId::from_parameter(clients), |b| {
            let echo = spawn_echo_server();
            let forwarder = spawn_forwarder(echo.addr, oxidinetd::config::Protocol::Tcp);
            let addr = forwarder.bind_addr;
            let suite = ConcurrentSuite::start(addr, clients);
            b.iter(|| suite.round());
            suite.stop();
        });
    }
    group.finish();
}

struct ConcurrentSuite {
    start_bar: Arc<Barrier>,
    end_bar: Arc<Barrier>,
    final_bar: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl ConcurrentSuite {
    fn start(addr: std::net::SocketAddr, clients: usize) -> Self {
        let start_bar = Arc::new(Barrier::new(clients + 1));
        let end_bar = Arc::new(Barrier::new(clients + 1));
        let final_bar = Arc::new(Barrier::new(clients + 1));
        let stop = Arc::new(AtomicBool::new(false));
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let mut handles = Vec::with_capacity(clients);
        for _ in 0..clients {
            let (start_bar, end_bar, final_bar, stop) = (
                start_bar.clone(),
                end_bar.clone(),
                final_bar.clone(),
                stop.clone(),
            );
            let error = error.clone();
            handles.push(std::thread::spawn(move || {
                let payload = Arc::new(vec![0x33u8; PER_CLIENT_BYTES]);
                let mut stream = match smol::block_on(smol::net::TcpStream::connect(addr)) {
                    Ok(stream) => stream,
                    Err(e) => {
                        *error.lock().unwrap() = Some(format!("connect failed: {}", e));
                        return;
                    }
                };
                // The quit flag is only consulted right after the start
                // barrier, so every client always participates in the same
                // barrier sequence as the main thread — no thread can skip a
                // barrier and strand the others.
                loop {
                    start_bar.wait();
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if error.lock().unwrap().is_none() {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            smol::block_on(async_full_duplex(&mut stream, &payload, 1));
                        }));
                        if let Err(payload) = result {
                            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                                (*s).to_string()
                            } else if let Some(s) = payload.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            *error.lock().unwrap() = Some(format!("client panic: {}", msg));
                        }
                    }
                    // Always reach the end barrier so the main thread cannot
                    // deadlock when a client fails mid-round.
                    end_bar.wait();
                }
                // Rendezvous so `stop()` can join every client deterministically.
                final_bar.wait();
            }));
        }

        ConcurrentSuite {
            start_bar,
            end_bar,
            final_bar,
            stop,
            error,
            handles,
        }
    }

    /// Runs one concurrent round (all clients transfer PER_CLIENT_BYTES).
    fn round(&self) {
        self.check_error();
        self.start_bar.wait();
        self.end_bar.wait();
        self.check_error();
    }

    fn stop(self) {
        self.check_error();
        self.stop.store(true, Ordering::Relaxed);
        // After the last measured round every client is parked at (or on its
        // way to) the start barrier — `round()` only returns once the end
        // barrier has drained, so no client can be mid-transfer here. Release
        // them; they observe the quit flag and move to the final barrier.
        self.start_bar.wait();
        // Collect everyone before joining.
        self.final_bar.wait();
        self.check_error();
        for handle in self.handles {
            let _ = handle.join();
        }
    }

    fn check_error(&self) {
        let err = self.error.lock().unwrap().clone();
        if let Some(err) = err {
            panic!("concurrent benchmark client failed: {}", err);
        }
    }
}

criterion_group!(benches, bench_concurrent);
criterion_main!(benches);
