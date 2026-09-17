#!/usr/bin/env python3
"""replay_tree.py — Dream-RSI 式发现树重建 + 重放评分 PoC（ductile v0.20 前置）。

数据源：ductile.db runs 表（pipeline 字段自本 PoC 起落库）。
树重建：按 (pipeline, recorded_at) 分组一次 run-session，节点=proc，
        边=DSL 的 needs/when 拓扑（从 pipeline 文件静态解析）+ runs 时序印证。
重放评分（论文 Eq.1）：V = max_score − β1·N + β2·N/max(1,k)
        max_score: 树内 best 质量代理（v0: Ok=1.0/Fail=0.0；终版接 judge 分）
        N:         展开节点数（≈ agent calls 代理）
        k:         决策轮数（≈ 拓扑层级深度，并行波计 1 轮）
用法：python3 replay_tree.py <ductile.db> [pipeline] [beta1] [beta2]
"""
import json
import sqlite3
import sys
from collections import defaultdict

SCORE_OK, SCORE_FAIL = 1.0, 0.0


def load_sessions(db_path: str, pipeline: str | None):
    """runs → run-sessions（同管线、时间相邻的行聚成一次执行）。"""
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    where = "WHERE pipeline != ''" + (f" AND pipeline = ?" if pipeline else "")
    args = (pipeline,) if pipeline else ()
    rows = conn.execute(
        f"SELECT id, pipeline, proc_name, impl_name, status, latency_ms, recorded_at "
        f"FROM runs {where} ORDER BY id", args
    ).fetchall()
    # 粗聚：同 pipeline 连续行 + 时间间隔 < 10min 算同一 session
    sessions, cur, last_ts = [], [], None
    import datetime as dt

    def ts(s):
        return dt.datetime.fromisoformat(s).timestamp() if s else 0.0

    for r in rows:
        if last_ts is not None and (
            r["pipeline"] != cur[-1]["pipeline"] or ts(r["recorded_at"]) - last_ts > 600
        ):
            sessions.append(cur)
            cur = []
        cur.append(dict(r))
        last_ts = ts(r["recorded_at"])
    if cur:
        sessions.append(cur)
    conn.close()
    return sessions


def build_tree(session):
    """一次 session → 发现树。v0: 按 proc 名去重取末次尝试（含失败），
    层级 = 拓扑深度（由记录顺序推断：needs 引用的 proc 排前面）。"""
    nodes, order = {}, []
    for r in session:
        n = r["proc_name"]
        if n not in nodes:
            order.append(n)
        nodes[n] = {
            "proc": n,
            "impl": r["impl_name"],
            "status": r["status"],
            "latency_ms": r["latency_ms"],
            "score": SCORE_OK if r["status"] == "Ok" else SCORE_FAIL,
            "attempts": 1 + sum(
                1 for x in session if x["proc_name"] == n and x["id"] < r["id"]
            ),
        }
    # 层级推断：首节点为根；其余节点深度 = 已出现节点引用关系未知，v0 用顺序索引
    for i, n in enumerate(order):
        nodes[n]["depth"] = i
    return {"nodes": [nodes[n] for n in order], "edges": "v0:见 SPEC replay 边格式"}


def replay_score(tree, beta1=0.02, beta2=0.01):
    """论文 Eq.1 的 ductile v0 代理。
    N=展开节点数；k=最大深度+1（每层=一个决策轮；foreach 波算 1 轮）。
    off-policy 语义 v0 简化：对已记录树，'另一策略' = 停在不同深度的前缀。"""
    nodes = tree["nodes"]
    if not nodes:
        return {"V": 0.0, "max_score": 0.0, "N": 0, "k": 0, "note": "empty"}
    max_score = max(n["score"] for n in nodes)
    N = len(nodes)
    k = max(n["depth"] for n in nodes) + 1
    V = max_score - beta1 * N + beta2 * N / max(1, k)
    return {
        "V": round(V, 4),
        "max_score": max_score,
        "N": N,
        "k": k,
        "note": "v0 prefix-stop 代理；终版接入 DSL 边重放与 foreach 波深",
    }


if __name__ == "__main__":
    db = sys.argv[1] if len(sys.argv) > 1 else "ductile.db"
    pl = sys.argv[2] if len(sys.argv) > 2 else None
    b1 = float(sys.argv[3]) if len(sys.argv) > 3 else 0.02
    b2 = float(sys.argv[4]) if len(sys.argv) > 4 else 0.01
    sessions = load_sessions(db, pl)
    print(f"sessions={len(sessions)} (beta1={b1}, beta2={b2})")
    for i, s in enumerate(sessions[-8:]):
        tree = build_tree(s)
        sc = replay_score(tree, b1, b2)
        procs = " → ".join(
            f"{n['proc']}{'✓' if n['status']=='Ok' else '✗'}" for n in tree["nodes"]
        )
        print(f"  [{i}] V={sc['V']:+.3f} N={sc['N']} k={sc['k']} | {procs}")
    if not sessions:
        print("（无带管线名的 runs——需用 v0.20 前置补丁后重跑管线）")
