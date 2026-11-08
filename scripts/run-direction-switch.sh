#!/usr/bin/env bash
set -euo pipefail

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repo"
cargo build --release --bin oi --example direction_switch_matrix
cargo run --release --example direction_switch_matrix -- "${1:-target/direction-switch-matrix.md}"
