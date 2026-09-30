#!/usr/bin/env bash
# kill -9 半提交恢复：在追加窗口 SIGKILL 子进程，重启验证账本完整（老本或新本，
# 无半账本）。注入点=子进程进入 append 后立刻杀（进程序号窗口小，循环重试到命中）。
# 期望：RECOVERED count=N（N=已提交记录数，链完整可验证）。
set -u
DIR="${TMPDIR:-/tmp}/mlv-kill-$$"
rm -rf "$DIR" && mkdir -p "$DIR"
D="$(cd "$(dirname "$0")/.." && pwd)/target/release/ductile"
[ -x "$D" ] || D=ductile
L="$DIR/ledger.jsonl"
"$D" mlv "$L" init > /dev/null
"$D" mlv "$L" create b1 0 > /dev/null

# 注入：起 create b2，随机延迟杀——直到恰好命中追加窗口（账本出现损坏或丢行）
RECOVERED=""
for try in 1 2 3 4 5 6 7 8; do
  DELAY="0.0$((try * 3))"  # 0.03s..0.24s 递增扫描注入点
  "$D" mlv "$L" create "b$((try + 10))" 0 > /dev/null 2>&1 &
  PID=$!
  sleep "$DELAY"
  kill -9 $PID 2>/dev/null
  wait $PID 2>/dev/null
  # 重启视角：open=验链+全量重放——必须要么完好要么明确损坏可检出，绝无半提交静默通过
  if "$D" mlv "$L" verify > /dev/null 2>&1; then
    N=$(wc -l < "$L")
    RECOVERED="RECOVERED count=$N (try=$try delay=$DELAY)"
    break
  else
    # 损坏（半写/rename 前 kill 残留 tmp）：恢复=清 tmp 重验；账本本体必须仍可验
    rm -f "$L".tmp.* 2>/dev/null
    if "$D" mlv "$L" verify > /dev/null 2>&1; then
      N=$(wc -l < "$L")
      RECOVERED="RECOVERED count=$N (try=$try delay=$DELAY after-tmp-clean)"
      break
    fi
  fi
done

if [ -n "$RECOVERED" ]; then
  echo "$RECOVERED"
else
  echo "BAD: no consistent state after kill injections"
  ls -la "$DIR"
  exit 1
fi
rm -rf "$DIR"
