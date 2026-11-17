#!/usr/bin/env bash
# Локальный бенчмарк движка на хосте: prefill с KV-кэшем vs без него.
# Usage: scripts/bench-local.sh model.gguf [tokens] [turns]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${1:?usage: bench-local.sh model.gguf [tokens] [turns]}"
TOKENS="${2:-96}"
TURNS="${3:-6}"
cargo build --release --no-default-features --bin turbollm-bench
exec ./target/release/turbollm-bench --model "$MODEL" --tokens "$TOKENS" --turns "$TURNS" --compare
