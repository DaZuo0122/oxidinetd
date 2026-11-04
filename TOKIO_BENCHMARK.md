# Tokio runtime migration: local benchmark comparison

Measured on 2026-11-03 and 2026-11-04 on Windows (AMD Ryzen 9 9955HX,
`x86_64-pc-windows-gnu`, rustc 1.99.1). The smol build is the original `main`
code; the Tokio build is this branch with the updated `Cargo.lock`. Both were
compiled with the repository's release profile and Criterion 0.8.2.

All values are Criterion mean time per iteration in microseconds. Lower is
faster. The change column is `(Tokio / smol - 1) * 100`; negative means Tokio
was faster. These are local single-run results, not a statistically reliable
prediction for other machines or workloads.

| Benchmark | smol (µs) | Tokio (µs) | Change |
| --- | ---: | ---: | ---: |
| concurrent/1 | 1,221.2 | 1,039.0 | -14.9% |
| concurrent/4 | 13,747.1 | 9,986.7 | -27.4% |
| concurrent/10 | 29,050.8 | 16,898.6 | -41.8% |
| concurrent/25 | 43,890.5 | 7,532.5 | -82.8% |
| tcptoudp/128 | 319.1 | 364.0 | +14.1% |
| tcptoudp/1024 | 374.4 | 370.8 | -1.0% |
| tcptoudp/64000 | 406.0 | 392.2 | -3.4% |
| udptotcp/128 | 97.1 | 79.3 | -18.4% |
| udptotcp/1024 | 83.3 | 80.0 | -4.0% |
| tcp_connection_rate/setup | 114.8 | 120.8 | +5.2% |
| tcp_latency/round_trip/1 | 802.5 | 1,235.0 | +53.9% |
| tcp_latency/round_trip/100 | 814.4 | 1,279.2 | +57.1% |
| tcp_latency/round_trip/1024 | 816.8 | 1,197.3 | +46.6% |
| tcp_latency/round_trip/4096 | 977.0 | 1,328.0 | +35.9% |
| tcp_latency/round_trip/65536 | 1,117.1 | 1,988.9 | +78.0% |
| tcp_throughput/per_connection/128 | 997.4 | 1,404.8 | +40.8% |
| tcp_throughput/per_connection/1024 | 797.3 | 1,199.0 | +50.4% |
| tcp_throughput/per_connection/65536 | 1,177.2 | 2,111.6 | +79.4% |
| tcp_throughput/sustained/1048576 | 2,910.7 | 5,307.4 | +82.3% |
| tcp_throughput/sustained/16777216 | 39,540.7 | 65,234.2 | +65.0% |
| udp_throughput/128 | 125.9 | 141.1 | +12.1% |
| udp_throughput/1024 | 132.1 | 146.7 | +11.0% |
| udp_throughput/8192 | 128.8 | 140.6 | +9.2% |
| udp_throughput/64000 | 154.1 | 157.7 | +2.3% |

The TCP result is a regression for this workload. The concurrent result is an
improvement, most visibly at 25 clients. The cross-protocol and UDP results
are closer to the baseline. The benchmark clients and echo servers were also
migrated to Tokio, so these figures measure the whole test stack. Dependencies
were updated at the same time; they do not isolate the runtime's contribution.

The full `cargo test --all-targets` suite passed on the final lockfile. The
smol baseline and final Tokio Criterion JSON results are retained locally under
`target/criterion-smol-2026-11-03` and
`target/criterion-tokio-final-2026-11-04`. `target/` is ignored by Git.

The existing per-connection benchmark occasionally exited early when several
input sizes ran in one process, under both runtimes. The final TCP latency and
per-connection Tokio values were therefore collected by running each filter in
a separate benchmark process. The smol 64 KiB per-connection and sustained
values were also rerun individually. A failed run was not included in the
table. This intermittent benchmark failure still needs investigation before
using the suite as an automated performance gate.

To reproduce an individual result, run, for example:

```powershell
cargo bench --bench tcp_latency -- 'tcp_latency/round_trip/1024' --noplot
cargo bench --bench tcp_throughput -- 'tcp_throughput/sustained' --noplot
```

## Re-run the comparison on another machine

Both entry points accept the same positional arguments:

`BASE_REF CANDIDATE_REF [OUTPUT.md] [NAME_FILTER]`

They default to `main` and `HEAD`. An empty output argument creates a uniquely
named report in the current directory. The optional fourth argument selects
benchmark names containing that text.

```powershell
pwsh -File scripts/compare-benchmarks.ps1 main HEAD comparison.md
pwsh -File scripts/compare-benchmarks.ps1 main HEAD comparison-tcp.md tcp_latency
```

```bash
bash scripts/compare-benchmarks.sh main HEAD comparison.md
bash scripts/compare-benchmarks.sh main HEAD comparison-tcp.md tcp_latency
```

The scripts need Git and Cargo. They resolve each ref to a commit, discover
benchmark binaries and cases from each version, and run every case in its own
process. Each invocation creates temporary worktrees and a separate fresh Cargo
target directory for each version; it never reads a previous Criterion result. The
worktrees and temporary build data are removed after the report is written.
Cases that exist in only one version are listed separately in the report.

## Tokio TCP copy experiment (2026-11-05)

The `copy_bidirectional` change in `c004d75` was compared with the preceding
Tokio implementation in `bcd1e36`. The comparison ran all 11 TCP cases on
Windows x86_64 with Rust 1.99.1. Each commit was compiled in its own fresh
temporary target directory. Criterion mean times were:

| Case | Before (µs) | After (µs) | Change |
| --- | ---: | ---: | ---: |
| Connection setup | 114.7 | 110.9 | -3.3% |
| Latency 1 B | 1221.9 | 1356.0 | +11.0% |
| Latency 100 B | 1227.6 | 1101.2 | -10.3% |
| Latency 1 KiB | 1246.5 | 1266.9 | +1.6% |
| Latency 4 KiB | 1416.3 | 1294.7 | -8.6% |
| Latency 64 KiB | 1926.3 | 1844.9 | -4.2% |
| Per connection 128 B | 1227.7 | 1226.3 | -0.1% |
| Per connection 1 KiB | 1070.9 | 1253.4 | +17.0% |
| Per connection 64 KiB | 1992.3 | 1818.1 | -8.7% |
| Sustained 1 MiB | 4182.7 | 6523.1 | +56.0% |
| Sustained 16 MiB | 63923.4 | 53099.5 | -16.9% |

Lower is faster. Seven cases improved and four regressed. This single local
run is mixed and does not establish an overall speedup. The base 1 KiB latency
case failed once and succeeded on the helper's retry. The change also uses
Tokio's half-close-aware bidirectional copy, which is covered by the new
half-close integration test. The full generated report is available locally at
`target/tokio-copy-ab-valid.md` and can be reproduced with:

```powershell
pwsh -File scripts/compare-benchmarks.ps1 bcd1e36 c004d75 tcp-copy.md tcp_
```
