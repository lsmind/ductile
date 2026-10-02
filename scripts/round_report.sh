#!/usr/bin/env bash
# round_report.sh — Phase 收口战报四要素底稿（docs/DCP_PROTOCOL.md §4）
# 用法: bash scripts/round_report.sh "<边界账：本 Phase 动了什么/没动什么/下口断点>"
# 退出码: 0=四账全绿（ROUND-REPORT-READY） 3=测试红 4=golden 漂移 —— 脚本即门（fail-closed）
set -euo pipefail
TOPIC="${1:-（未填边界账——必须补：动了什么/没动什么/下口断点）}"
ROOT="$(git rev-parse --show-toplevel)"
OUT="$ROOT/target/round-report"
mkdir -p "$OUT"

# ── 账1 测试：全库 release ──
TESTS_LOG="$OUT/tests.log"
(cargo test --release 2>&1 || true) | tee "$TESTS_LOG" >/dev/null
PASS=$(grep -oE '[0-9]+ passed' "$TESTS_LOG" | awk '{s+=$1} END {print s+0}' || true)
FAIL=$(grep -oE '[0-9]+ failed' "$TESTS_LOG" | awk '{s+=$1} END {print s+0}' || true)

# ── 账2 golden：tests/golden 哈希 vs 登记（首次=登记，之后=零破坏比对）──
BASE="$ROOT/docs/charter/golden-baseline.txt"
NOW=$(cd "$ROOT" && sha256sum tests/golden/* 2>/dev/null | sort || true)
if [ -f "$BASE" ]; then
  if [ "$NOW" = "$(cat "$BASE")" ]; then
    GOLDEN="零破坏（与登记一致）"
  else
    GOLDEN="漂移——登记与现态不一致，停 ship 查因"
  fi
else
  printf '%s\n' "$NOW" > "$BASE"
  GOLDEN="首次登记 $(echo "$NOW" | wc -l) 项"
fi

# ── 账3 ship：git 状态 ──
HEAD=$(git -C "$ROOT" rev-parse --short HEAD)
DIRTY=$(git -C "$ROOT" status --short | wc -l)
LOG=$(git -C "$ROOT" log --oneline -5)

# ── 账4 边界：TOPIC ──
cat > "$OUT/report.md" <<EOF
# Phase 收口战报（round-report 自动底稿）
- 生成: $(date -Is)

ACCOUNT-1 测试: ${PASS} passed / ${FAIL} failed（全库 release）
ACCOUNT-2 golden: ${GOLDEN}
ACCOUNT-3 ship: HEAD=${HEAD} dirty=${DIRTY}
${LOG}
ACCOUNT-4 边界: ${TOPIC}
EOF
cat "$OUT/report.md"
if [ "$FAIL" -ne 0 ]; then
  echo "!! 测试有红——战报已生成但不得 ship" >&2
  exit 3
fi
case "$GOLDEN" in
  零破坏*|首次登记*) ;;
  *) exit 4 ;;
esac
echo "ROUND-REPORT-READY" >> "$OUT/report.md"
echo "ROUND-REPORT-READY"
