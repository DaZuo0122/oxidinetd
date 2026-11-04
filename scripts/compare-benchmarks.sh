#!/usr/bin/env bash
set -euo pipefail

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo run --quiet --manifest-path "$repo/scripts/bench-compare/Cargo.toml" -- "$repo" "$@"
