#!/usr/bin/env bash
# 双进程并发仅一胜：两进程同时 create 同 (binding,rev) 同 request_key 到同一账本。
# 期望：一个 OK，另一个要么 E425（后到者重放时同键同摘要=ACK 不加行）要么 E422——
# 关键断言=账本里该 (binding,rev) 的 CREATE_PROPOSAL 记录只有 1 行 + 链验证通过。
set -u
DIR="${TMPDIR:-/tmp}/mlv-conc-$$"
rm -rf "$DIR" && mkdir -p "$DIR"
D="$(cd "$(dirname "$0")/.." && pwd)/target/release/ductile"
[ -x "$D" ] || D=ductile
L="$DIR/ledger.jsonl"
"$D" mlv "$L" init > /dev/null

# 两进程背靠背同时提交同键同摘要 create
"$D" mlv "$L" create b1 0 same-rk > "$DIR/a.out" 2>&1 &
"$D" mlv "$L" create b1 0 same-rk > "$DIR/b.out" 2>&1 &
wait

OKS=$(grep -l '^OK create' "$DIR/a.out" "$DIR/b.out" 2>/dev/null | wc -l)
ACKS=$(grep -l '^ACK create' "$DIR/a.out" "$DIR/b.out" 2>/dev/null | wc -l)
# 行数=init(1)+create(1)=2；链验证过
LINES=$(wc -l < "$L")
if "$D" mlv "$L" verify > /dev/null 2>&1; then V=ok; else V=broken; fi

if [ "$LINES" -eq 2 ] && [ "$V" = ok ] && { [ "$OKS" -eq 1 ] || [ "$ACKS" -eq 1 ]; }; then
  echo "ONE-WINS lines=$LINES verify=$V ok=$OKS ack=$ACKS"
else
  echo "BAD lines=$LINES verify=$V ok=$OKS ack=$ACKS"
  cat "$DIR/a.out" "$DIR/b.out"
  exit 1
fi
rm -rf "$DIR"
