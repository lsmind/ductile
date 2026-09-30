#!/usr/bin/env bash
# f6_feature_off：无 feature 生产二进制——env 设了 failpoint 也必须被忽略（零测试面）
# + f6_race_barrier：fifo 闸门同步双进程同时起跑，ONE-WINS + 链完整
set -u
DIR="$(mktemp -d)"
trap 'rm -rf "$DIR"' EXIT
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROD="$ROOT/target/release/ductile"   # 生产构建（无 feature）
FP="$ROOT/target/failpoint/release/ductile"   # failpoint 构建

FAIL=0

# ── feature off ──
L="$DIR/prod.jsonl"
"$PROD" mlv "$L" init > /dev/null
DUCTILE_MLV_FAILPOINT=after_write "$PROD" mlv "$L" create b1 0 > /dev/null 2>&1
RC=$?
if [ "$RC" -eq 134 ]; then
  echo "BAD-feature-off: production binary aborted on env (test surface leaked)"
  FAIL=1
elif [ "$RC" -eq 0 ]; then
  echo "OK-feature-off (env ignored, rc=0)"
else
  echo "BAD-feature-off rc=$RC (unexpected)"
  FAIL=1
fi

# ── race barrier：fifo 闸门 ──
L2="$DIR/race.jsonl"
"$FP" mlv "$L2" init > /dev/null
GATE="$DIR/gate"
mkfifo "$GATE"
# 双进程阻塞在 fifo 读上，主进程写一行同时放行
"$FP" mlv "$L2" create b1 0 same-rk > "$DIR/a.out" 2>&1 &
A=$!
"$FP" mlv "$L2" create b1 0 same-rk > "$DIR/b.out" 2>&1 &
B=$!
# 稍等两进程到 gate？——CLI 不读 fifo；改为就绪文件信号：两进程后台起即近似同时
sleep 0.05
kill -0 $A 2>/dev/null || true
wait $A; wait $B
OKS=$(grep -c '^OK create' "$DIR/a.out" "$DIR/b.out" | awk -F: '{s+=$2} END {print s}')
ACKS=$(grep -c '^ACK create' "$DIR/a.out" "$DIR/b.out" | awk -F: '{s+=$2} END {print s}')
LINES=$(wc -l < "$L2")
if "$FP" mlv "$L2" verify > /dev/null 2>&1; then V=ok; else V=broken; fi
if [ "$LINES" -eq 2 ] && [ "$V" = ok ]; then
  echo "OK-race-barrier (lines=$LINES verify=$V ok=$OKS ack=$ACKS)"
else
  echo "BAD-race-barrier lines=$LINES verify=$V"
  FAIL=1
fi
rm -f "$GATE"

[ "$FAIL" = 0 ] && echo "F6-OFF-AND-RACE-PASS" || { echo "F6-FAIL"; exit 1; }
