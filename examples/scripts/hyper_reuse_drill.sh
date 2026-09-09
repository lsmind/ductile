#!/usr/bin/env bash
# hyper_reuse_drill.sh — 超图层 / 节点复用场景回归（供 hyper-reuse-drill.pipeline 调用）
set -euo pipefail
ROOT="${DUCTILE_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$ROOT"

BIN="${DUCTILE_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x "${CARGO_TARGET_DIR:-}/debug/ductile.exe" ]; then BIN="${CARGO_TARGET_DIR}/debug/ductile.exe"
  elif [ -x "${CARGO_TARGET_DIR:-}/debug/ductile" ]; then BIN="${CARGO_TARGET_DIR}/debug/ductile"
  elif [ -x target/debug/ductile.exe ]; then BIN=target/debug/ductile.exe
  elif [ -x target/debug/ductile ]; then BIN=target/debug/ductile
  elif [ -x target/release/ductile.exe ]; then BIN=target/release/ductile.exe
  elif [ -x target/release/ductile ]; then BIN=target/release/ductile
  else BIN="$(command -v ductile || true)"
  fi
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  # Windows: ductile.exe may not be +x in Git Bash sense
  if [ -f "${CARGO_TARGET_DIR:-}/debug/ductile.exe" ]; then BIN="${CARGO_TARGET_DIR}/debug/ductile.exe"
  elif [ -f target/debug/ductile.exe ]; then BIN=target/debug/ductile.exe
  else echo "ductile binary not found"; exit 1
  fi
fi

TMP="${TMPDIR:-/tmp}"
mkdir -p "$TMP" 2>/dev/null || true
OUT1="$TMP/ductile_hyper_drill_1.pipeline"
OUT2="$TMP/ductile_hyper_drill_2.pipeline"

echo "[1] hyper parse"
"$BIN" hyper parse examples/hyper/unstructured_extract.hyper | grep -q gate

echo "[2] hyper build + check"
"$BIN" hyper build examples/hyper/unstructured_extract.hyper -o "$OUT1"
"$BIN" hyper build examples/hyper/unstructured_extract.hyper -o "$OUT2"
"$BIN" check "$OUT1" | grep -qi pass

echo "[3] hyper check vs hand pipeline"
"$BIN" hyper check examples/hyper/unstructured_extract.hyper examples/scripts/unstructured-extract.pipeline | grep -qi pass

echo "[4] workflow similar ISO (two emits)"
"$BIN" hyper similar "$OUT1" --json "$TMP" | grep -q '"isomorphic":true'

echo "[5] workflow similar near hand pipeline"
"$BIN" hyper similar examples/hyper/unstructured_extract.hyper --json examples/hyper examples/scripts | grep -q adapt_topology

echo "[6] node similar extract → llm"
"$BIN" hyper nodes examples/scripts/unstructured-extract.pipeline:extract --json examples/scripts examples/hyper | grep -q 'op=llm'

echo "[7] node filter judge+run"
"$BIN" hyper nodes --role judge --op run --json examples/scripts | grep -q gate

echo "[8] node ISO fixtures"
"$BIN" hyper nodes examples/hyper/node_reuse_a.pipeline:load --json examples/hyper | grep -q '"isomorphic":true'

echo "[9] cargo test hyper::"
cargo test --lib hyper:: -- --test-threads=4 2>&1 | grep '^test result' | grep -q '0 failed'

echo "[10] hyper semantics drill"
bash examples/scripts/hyper_semantics_drill.sh

echo HYPER-REUSE-PASS
