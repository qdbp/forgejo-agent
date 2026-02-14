#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_BIN="${1:-$HOME/.local/bin/forgejoctl}"
OUT_ORCHD="${2:-$HOME/.local/bin/orchd}"

cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
mkdir -p "$(dirname "$OUT_BIN")"
mkdir -p "$(dirname "$OUT_ORCHD")"
install -m 0755 "$ROOT_DIR/target/release/forgejo-agent" "$OUT_BIN"
install -m 0755 "$ROOT_DIR/target/release/orchd" "$OUT_ORCHD"

echo "installed: $OUT_BIN"
echo "installed: $OUT_ORCHD"
