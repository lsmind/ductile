#!/usr/bin/env python3
# ductile: v1
# name: op_replay_evidence
# desc: 收集重放树失败证据（per-proc 失败聚合 + 重放分），供 LLM 提议 replay_effect 声明
# lang: python
# params: pipeline(str, default=)
# output: evidence_txt(str), n_sessions(int), n_fail_procs(int)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(2)-O(2)-P(3)-T(4)
# mcsm_note_f: 只读 SQLite 账本
# mcsm_note_o: runs 聚合行组
# mcsm_note_p: stdout 证据文本 + staged= 行
# mcsm_note_t: 证据分期（v0 = per-proc 聚合）
import os
import sqlite3

pl_filter = os.environ.get("DUCTILE_ARG_PIPELINE", "")
db = os.path.expanduser("~/.local/share/ductile/ductile.db")
if os.environ.get("DUCTILE_DATA"):
    db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")

conn = sqlite3.connect(db)
where = "WHERE session != ''"
args = []
if pl_filter:
    where += " AND pipeline = ?"
    args.append(pl_filter)

# per-proc 聚合：总尝试/失败数/最近 session/是否从无成功
rows = conn.execute(
    f"""
    SELECT proc_name,
           COUNT(*) AS attempts,
           SUM(CASE WHEN status='Fail' THEN 1 ELSE 0 END) AS fails,
           COUNT(DISTINCT session) AS sessions
    FROM runs {where}
    GROUP BY proc_name
    ORDER BY fails DESC, attempts DESC
    """,
    args,
).fetchall()

n_sessions = conn.execute(
    f"SELECT COUNT(DISTINCT session) FROM runs {where}", args
).fetchone()[0]

lines = []
fail_procs = 0
for proc, attempts, fails, sessions in rows:
    if fails and fails == attempts:
        # 从未成功过的 proc——skip_procs 候选（若非关键链）
        lines.append(f"proc={proc} attempts={attempts} fails={fails} sessions={sessions} verdict=never_ok")
        fail_procs += 1
    elif fails:
        lines.append(f"proc={proc} attempts={attempts} fails={fails} sessions={sessions} verdict=flaky")
    else:
        lines.append(f"proc={proc} attempts={attempts} fails=0 sessions={sessions} verdict=stable")

evidence = "\n".join(lines) if lines else "(no runs with session yet)"
print("##DSL_RESULT")
print(f"n_sessions={n_sessions}")
print(f"n_fail_procs={fail_procs}")
print(f"evidence_txt={evidence}")
print("##DSL_END")
