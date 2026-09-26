#!/usr/bin/env python3
"""learn_probe — v0.22 PR-3 学习环黑箱探针（selftest 第 11 重门禁）。

真引擎黑箱验证 PR-1 接线后的两个此前「不可观测」行为：
  F1 学习环生效：base≡1 路径（未声明 cost）下，毒臂 a 连败后 b 必须至少一次首发。
     ——旧态：Cost::default()=全0 → effective=0×(1+penalty)/w → 恒 Equal → 永远声明序。
  E1 键隔离：(pipeline, proc) 键维度下，P1 的毒臂惩罚不得污染 P2 同名 proc 的排名。
     ——旧态：键只有 proc_name → 跨管线污染。

方法学承接 lab fp_key_e2e（水位线 + runs 表行序重构首发），但压缩到 9 次 CLI
调用（selftest 时间预算内）。隔离纪律：独立 DUCTILE_DATA，不碰真库。

输出：LEARN-PROBE-PASS / LEARN-PROBE-FAIL(<原因>)，exit 0/1。
"""
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile


def resolve_bin() -> str:
    """selftest 引擎注入 PATH 含 target/{release,debug}；兜底按脚本相对定位。"""
    hit = shutil.which("ductile")
    if hit:
        return hit
    rel = os.path.join(os.path.dirname(__file__), "..", "target", "release", "ductile")
    if os.path.exists(rel):
        return os.path.abspath(rel)
    raise SystemExit("LEARN-PROBE-FAIL(ductile binary not found)")


BIN = resolve_bin()

POISON_PIPELINE = """Pipeline("LP1", "learn probe: poison arm")
  .proc("mix")
    .plan(
      a -> run("exit 1").desc("poison: always fails"),
      b -> run("echo B-OK").desc("healthy fallback")
    )
"""

CLEAN_PIPELINE = """Pipeline("LP2", "learn probe: clean twin, same proc name")
  .proc("mix")
    .plan(
      a -> run("echo A2-OK").desc("healthy primary"),
      b -> run("echo B2-OK").desc("healthy fallback")
    )
"""


def sh(args: list, data: str) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    env["DUCTILE_DATA"] = data
    return subprocess.run([BIN] + args, capture_output=True, text=True, env=env, cwd=data)


def impl_counts(db: str, pipeline: str, wm: int) -> tuple:
    """水位线后该管线 mix proc 的 impl 尝试行数（黑箱不变量，无需调用重构）。

    executor 首胜即停（PR-1 lab E2 实证：首发随学习变化）。因此：
      毒臂 a 全败时，惰性引擎每 run 必先试 a → N_a == N_runs；
      学习引擎降级 a 后，后续 run 只记 b 行 → N_a < N_runs。
    """
    if not os.path.exists(db) or os.path.getsize(db) == 0:
        return (0, 0)
    conn = sqlite3.connect(db)
    try:
        rows = conn.execute(
            "SELECT impl_name, COUNT(*) FROM runs "
            "WHERE pipeline=? AND proc_name='mix' AND id>? GROUP BY impl_name",
            (pipeline, wm),
        ).fetchall()
    except sqlite3.OperationalError:
        rows = []
    finally:
        conn.close()
    m = dict(rows)
    return (m.get("a", 0), m.get("b", 0))


def watermark(db: str, pipeline: str) -> int:
    if not os.path.exists(db) or os.path.getsize(db) == 0:
        return 0  # 冷库（首次 run 前 db 未初始化）
    conn = sqlite3.connect(db)
    try:
        r = conn.execute(
            "SELECT COALESCE(MAX(id),0) FROM runs WHERE pipeline=?", (pipeline,)
        ).fetchone()[0]
    except sqlite3.OperationalError:
        r = 0  # runs 表未建（版本差异兜底）
    finally:
        conn.close()
    return r


def main() -> int:
    d = tempfile.mkdtemp(prefix="dt-learn-")
    p1 = os.path.join(d, "lp1.pipeline")
    p2 = os.path.join(d, "lp2.pipeline")
    open(p1, "w").write(POISON_PIPELINE)
    open(p2, "w").write(CLEAN_PIPELINE)
    for f in (p1, p2):
        r = sh(["check", f], d)
        if "passed" not in r.stdout + r.stderr:
            print(f"LEARN-PROBE-FAIL(check {os.path.basename(f)}): {r.stdout}{r.stderr}")
            return 1

    db = os.path.join(d, "ductile.db")

    # ── F1：学习环生效。毒臂 a 全败 → 降级后 b 直接首发 → a 行数 < run 数 ──
    wm = watermark(db, "LP1")
    N1 = 5
    for _ in range(N1):
        sh(["run", p1], d)
    n_a, n_b = impl_counts(db, "LP1", wm)
    if n_a >= N1:
        print(
            f"LEARN-PROBE-FAIL(F1 learning loop inert): "
            f"a tried in all {n_a}/{N1} runs — never demoted, base≡1 wiring regression?"
        )
        return 1
    if n_b < N1:
        print(
            f"LEARN-PROBE-FAIL(F1 engine broken): b rows {n_b} < runs {N1} — "
            f"fallback not executed every run"
        )
        return 1

    # ── E1：键隔离。LP1 毒臂已污染 mix.a（在 LP1 键下）；LP2 同名 proc 的
    #    首发必须仍是自己的声明序 a —— 旧 proc-only 键会把惩罚泄漏过来 ──
    wm2 = watermark(db, "LP2")
    r = sh(["run", p2], d)
    if "successfully" not in r.stdout:
        print(f"LEARN-PROBE-FAIL(LP2 run): {r.stdout}{r.stderr}")
        return 1
    n_a2, _ = impl_counts(db, "LP2", wm2)
    if n_a2 < 1:
        print(
            f"LEARN-PROBE-FAIL(E1 cross-pipeline contamination): "
            f"LP2 mix has 0 healthy-a rows — LP1 poison leaked into LP2 ranking"
        )
        return 1

    print(f"LEARN-PROBE-PASS f1_demoted_after={n_a}/{N1}runs e1_lp2_a_first(n_a2={n_a2})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
