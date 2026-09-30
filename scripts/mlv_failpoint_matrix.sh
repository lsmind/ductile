#!/usr/bin/env bash
# f6_crash_matrix：五点位确定性 failpoint——abort(134) 后账本必为老本或新本，verify 全过
# 需要 --features mlv-failpoint 构建的 ductile（脚本自检）。
set -u
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
D="$ROOT/target/failpoint/release/ductile"
if [ ! -x "$D" ]; then
  echo "FAILPOINT-BINARY-MISSING (run: cargo build --release --features mlv-failpoint --target-dir target/failpoint)"
  exit 1
fi

one_case() {
  local fp="$1" expect="$2"  # expect=old|new
  local L="$DIR/$fp.jsonl"
  "$D" mlv "$L" init > /dev/null
  local OLD_SHA; OLD_SHA=$(sha256sum "$L" | cut -d' ' -f1)
  local OLD_LINES; OLD_LINES=$(wc -l < "$L")
  DUCTILE_MLV_FAILPOINT="$fp" "$D" mlv "$L" create b1 0 > /dev/null 2>&1
  local RC=$?
  if [ "$RC" -ne 134 ]; then
    echo "BAD-$fp rc=$RC (expect 134 SIGABRT)"
    return 1
  fi
  local NEW_SHA; NEW_SHA=$(sha256sum "$L" | cut -d' ' -f1)
  local NEW_LINES; NEW_LINES=$(wc -l < "$L")
  # 老本判定：字节与 init 后全等
  if [ "$expect" = old ]; then
    if [ "$NEW_SHA" != "$OLD_SHA" ]; then
      echo "BAD-$fp ledger changed (expect old): $OLD_SHA -> $NEW_SHA"
      return 1
    fi
  else
    # 新本判定：行数 +1 且链验证过
    if [ "$NEW_LINES" -ne $((OLD_LINES + 1)) ]; then
      echo "BAD-$fp lines=$NEW_LINES expect $((OLD_LINES+1))"
      return 1
    fi
  fi
  if ! "$D" mlv "$L" verify > /dev/null 2>&1; then
    echo "BAD-$fp verify failed after crash"
    return 1
  fi
  # 恢复力：无 failpoint 下同一 create 重放——必 OK 或 ACK（幂等）
  local OUT; OUT=$("$D" mlv "$L" create b1 0 2>&1)
  case "$OUT" in
    OK\ create*|ACK\ create*) ;;
    *) echo "BAD-$fp post-crash retry: $OUT"; return 1 ;;
  esac
  echo "OK-$fp ($expect, lines=$(wc -l < "$L"))"
}

FAIL=0
for pair in "after_write old" "after_sync old" "before_rename old" "after_rename new" "before_dirsync new"; do
  set -- $pair
  one_case "$1" "$2" || FAIL=1
done
if [ "$FAIL" = 0 ]; then
  echo "F6-CRASH-MATRIX-PASS (5 failpoints deterministic)"
else
  echo "F6-CRASH-MATRIX-FAIL"
  exit 1
fi
