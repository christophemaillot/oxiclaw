#!/usr/bin/env bash
set -euo pipefail

# Smoke test for memory_search ranking + archive toggle.
# Usage:
#   ./scripts/smoke_memory_search.sh [BASEDIR]

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASEDIR="${1:-$ROOT_DIR/basedir}"

cd "$ROOT_DIR"

echo "== OxiClaw memory_search smoke test =="
echo "basedir: $BASEDIR"

echo
echo "[1/4] Reindex Tantivy from transcripts + memory files"
cargo run --quiet --bin memory_probe -- --basedir "$BASEDIR" --query "ping" --limit 1 --reindex >/dev/null || true

echo
echo "[2/4] Query default scope (archive=false)"
Q1="memory_search archive hot transcripts"
cargo run --quiet --bin memory_probe -- --basedir "$BASEDIR" --query "$Q1" --limit 5 || true

echo
echo "[3/4] Same query with archive=true"
cargo run --quiet --bin memory_probe -- --basedir "$BASEDIR" --query "$Q1" --limit 5 --archive || true

echo
echo "[4/4] Repetition signal probe"
Q2="curator nightly transcript memory"
cargo run --quiet --bin memory_probe -- --basedir "$BASEDIR" --query "$Q2" --limit 8 || true

echo
echo "Done. Compare [mentions=...] and result ordering between archive=false/true."
