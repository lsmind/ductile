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

# ── race barrier：父进程持锁，观察子进程真阻塞后释放 ──
# 终验应改③：sleep 0.05 非同步栅栏（假绿风险）。改为可观察的阻塞-释放过程，
# 全程单 Python 编排（无 fifo 字节分配不确定性）：
# 持锁 → 起两子进程抢 create → 轮询 /proc/<pid>/wchan 直到都阻塞在 flock →
# 释放 → 收割。观察不到阻塞（超时 15s）= 测试失败，不是跳过。
L2="$DIR/race.jsonl"
"$FP" mlv "$L2" init > /dev/null
python3 - "$FP" "$L2" "$DIR" <<'PYEOF'
import fcntl, subprocess, sys, time

fp, ledger, dirp = sys.argv[1], sys.argv[2], sys.argv[3]
lock = ledger + ".lock"
f = open(lock, "a+")
fcntl.flock(f.fileno(), fcntl.LOCK_EX)      # 父进程先持锁 → 子进程必然阻塞
a = subprocess.Popen([fp, "mlv", ledger, "create", "b1", "0", "same-rk"],
                     stdout=open(f"{dirp}/a.out", "w"), stderr=subprocess.STDOUT)
b = subprocess.Popen([fp, "mlv", ledger, "create", "b1", "0", "same-rk"],
                     stdout=open(f"{dirp}/b.out", "w"), stderr=subprocess.STDOUT)

def wchan(pid):
    try:
        with open(f"/proc/{pid}/wchan") as fh:
            return fh.read().strip()
    except FileNotFoundError:
        return "gone"

def on_flock(w):
    # flock 系统调用的内核等待点：locks_lock_inode_wait（不同内核版本亦见 flock_ 等）
    return "lock_inode" in w or "flock" in w

deadline = time.time() + 15
observed = False
while time.time() < deadline:
    wa, wb = wchan(a.pid), wchan(b.pid)
    if on_flock(wa) and on_flock(wb):
        observed = True        # 可观察的并发争用证据：两进程同时等同一把锁
        break
    if a.poll() is not None and b.poll() is not None:
        break                  # 有进程已退出（异常路径，交由后续断言报红）
    time.sleep(0.02)

if not observed:
    print(f"BAD-race-barrier: never observed both children blocked on flock "
          f"(wchan a={wchan(a.pid)} b={wchan(b.pid)} rc_a={a.poll()} rc_b={b.poll()})")
    sys.exit(1)

fcntl.flock(f.fileno(), fcntl.LOCK_UN)       # 释放 → 子进程竞争获锁
a.wait(); b.wait()
PYEOF
PYRC=$?
if [ "$PYRC" -ne 0 ]; then
  echo "F6-FAIL (race barrier not observed)"
  exit 1
fi
OKS=$(grep -c '^OK create' "$DIR/a.out" "$DIR/b.out" | awk -F: '{s+=$2} END {print s}')
ACKS=$(grep -c '^ACK create' "$DIR/a.out" "$DIR/b.out" | awk -F: '{s+=$2} END {print s}')
LINES=$(wc -l < "$L2")
if "$FP" mlv "$L2" verify > /dev/null 2>&1; then V=ok; else V=broken; fi
if [ "$LINES" -eq 2 ] && [ "$V" = ok ]; then
  echo "OK-race-barrier (lines=$LINES verify=$V ok=$OKS ack=$ACKS barrier=observed-flock-block)"
else
  echo "BAD-race-barrier lines=$LINES verify=$V"
  FAIL=1
fi

[ "$FAIL" = 0 ] && echo "F6-OFF-AND-RACE-PASS" || { echo "F6-FAIL"; exit 1; }
