#!/usr/bin/env bash
# hyper_semantics_drill.sh — 有序/类型化超图语义对照（gate/xor/bundle/iso/legacy）
set -euo pipefail
ROOT="${DUCTILE_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$ROOT"
SEM=examples/hyper/semantics

BIN="${DUCTILE_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -f "${CARGO_TARGET_DIR:-}/release/ductile.exe" ]; then BIN="${CARGO_TARGET_DIR}/release/ductile.exe"
  elif [ -f "${CARGO_TARGET_DIR:-}/debug/ductile.exe" ]; then BIN="${CARGO_TARGET_DIR}/debug/ductile.exe"
  elif [ -f "${CARGO_TARGET_DIR:-}/release/ductile" ]; then BIN="${CARGO_TARGET_DIR}/release/ductile"
  elif [ -f "${CARGO_TARGET_DIR:-}/debug/ductile" ]; then BIN="${CARGO_TARGET_DIR}/debug/ductile"
  elif [ -f target/release/ductile.exe ]; then BIN=target/release/ductile.exe
  elif [ -f target/debug/ductile.exe ]; then BIN=target/debug/ductile.exe
  elif [ -f target/release/ductile ]; then BIN=target/release/ductile
  elif [ -f target/debug/ductile ]; then BIN=target/debug/ductile
  else BIN="$(command -v ductile || true)"
  fi
fi
[ -n "$BIN" ] && [ -f "$BIN" ] || { echo "ductile binary not found"; exit 1; }

TMP="${TMPDIR:-/tmp}/ductile_hyper_sem_$$"
mkdir -p "$TMP"

echo "[S1] gate projection: producers→judge; consumer when-only"
P=$("$BIN" hyper parse "$SEM/gate_clean.hyper")
echo "$P" | grep -q 'out after=\["mid"\]'
echo "$P" | grep -q 'out .*gated_by=Some("j")'
echo "$P" | grep -q 'j after=\["mid"\]'
! echo "$P" | grep -E 'out after=\[[^]]*j' >/dev/null

echo "[S2] gate rejects positional"
set +e
ERR=$("$BIN" hyper parse "$SEM/gate_bad_positional.hyper" 2>&1)
RC=$?
set -e
[ "$RC" -ne 0 ]
echo "$ERR" | grep -Eq 'judge=|ports|consumers='

echo "[S3] xor suppresses alts; emit ≥2 impls on slot"
P=$("$BIN" hyper parse "$SEM/xor_alts.hyper")
PROJ=$(echo "$P" | sed -n '/Projected DAG stages:/,/Deliver:/p')
echo "$PROJ" | grep -qE '^[[:space:]]+live '
echo "$PROJ" | grep -qE '^[[:space:]]+out '
! echo "$PROJ" | grep -qE '^[[:space:]]+stub '
"$BIN" hyper build "$SEM/xor_alts.hyper" -o "$TMP/xor.pipeline" >/dev/null
grep -q '.proc("live")' "$TMP/xor.pipeline"
! grep -q '.proc("stub")' "$TMP/xor.pipeline"
N=$(awk '
  /\.proc\("live"\)/ { p=1; next }
  p && /\.proc\("/ { exit }
  p && /->/ { c++ }
  END { print c+0 }
' "$TMP/xor.pipeline")
[ "$N" -ge 2 ]

echo "[S4] bundle co-occurrence"
"$BIN" hyper build "$SEM/bundle_ok.hyper" -o "$TMP/bundle.pipeline" >/dev/null
"$BIN" hyper check "$SEM/bundle_ok.hyper" "$TMP/bundle.pipeline" | grep -qi pass
set +e
BOUT=$("$BIN" hyper check "$SEM/bundle_ok.hyper" "$SEM/bundle_missing.pipeline" 2>&1)
BRC=$?
set -e
[ "$BRC" -ne 0 ]
echo "$BOUT" | grep -q bundle
echo "$BOUT" | grep -q a

echo "[S5] hyper↔hyper rename iso + skip bad fixtures"
KA=$("$BIN" hyper parse "$SEM/iso_a.hyper" | sed -n 's/^Hypergraph key: //p')
KB=$("$BIN" hyper parse "$SEM/iso_b.hyper" | sed -n 's/^Hypergraph key: //p')
[ -n "$KA" ] && [ "$KA" = "$KB" ]
SIM=$("$BIN" hyper similar "$SEM/iso_a.hyper" --json "$SEM")
echo "$SIM" | grep -q '"isomorphic":true'
echo "$SIM" | grep -Eq 'ordered typed hypergraph isomorphic|hypergraph isomorphic'

echo "[S6] emit DAG iso with hyper; pipeline note is DAG not hypergraph"
"$BIN" hyper build "$SEM/iso_a.hyper" -o "$TMP/iso_a.pipeline" >/dev/null
SIM2=$("$BIN" hyper similar "$SEM/iso_a.hyper" --json "$TMP" "$SEM")
echo "$SIM2" | grep -q '"dag_key"'
echo "$SIM2" | grep -q '"hypergraph_key"'
echo "$SIM2" | grep -q 'dag_key only'
echo "$SIM2" | grep -q '"kind":"pipeline"'
echo "$SIM2" | grep -Eq 'DAG projection|not hypergraph'

echo "[S7] legacy desugar key == explicit iso_a"
KL=$("$BIN" hyper parse "$SEM/legacy_gate.hyper" | sed -n 's/^Hypergraph key: //p')
[ "$KL" = "$KA" ]

echo "[S8] example check + projection"
"$BIN" hyper check examples/hyper/unstructured_extract.hyper examples/scripts/unstructured-extract.pipeline | grep -qi pass
EP=$("$BIN" hyper parse examples/hyper/unstructured_extract.hyper)
echo "$EP" | grep -q 'report after=\["extract"\]'
echo "$EP" | grep -q 'report .*gated_by=Some("gate")'

rm -rf "$TMP"
echo HYPER-SEMANTICS-PASS
