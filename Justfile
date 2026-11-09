# Common local commands. Run `just --list` to see the available recipes.
set windows-shell := ["cmd.exe", "/c"]

default:
    @just --list

check:
    cargo check --workspace --all-targets --locked

test:
    cargo test --workspace --locked -- --test-threads=1

clippy:
    cargo clippy --workspace --all-targets --locked

build:
    cargo build --release --locked --bin oi

bench:
    cargo bench --benches -- --noplot

# Benchmark comparison creates temporary worktrees; the helper handles cleanup.
compare:
    {{ if os() == "windows" { "powershell.exe -NoProfile -File scripts/compare-benchmarks.ps1" } else { "bash scripts/compare-benchmarks.sh" } }}

# Production mode compares the default relay with the platform's experimental relay.
network-matrix:
    {{ if os() == "windows" { "powershell.exe -NoProfile -File scripts/run-network-matrix.ps1 -Production" } else { "bash scripts/run-network-matrix.sh --production" } }}

network-profile:
    {{ if os() == "windows" { "powershell.exe -NoProfile -File scripts/run-network-matrix.ps1" } else { "bash scripts/run-network-matrix.sh" } }}

direction-matrix:
    {{ if os() == "windows" { "powershell.exe -NoProfile -File scripts/run-direction-switch.ps1" } else { "bash scripts/run-direction-switch.sh" } }}
