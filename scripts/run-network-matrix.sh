#!/usr/bin/env bash
set -euo pipefail

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n "${IPERF3:-}" && ! -f "$IPERF3" ]]; then
    printf 'IPERF3 does not exist: %s\n' "$IPERF3" >&2
    exit 1
fi
cd -- "$repo"
cargo build --release --features tcp-profile --bin oi --example network_workload_matrix
cargo run --release --features tcp-profile --example network_workload_matrix -- "${1:-target/network-workload-matrix.md}"
