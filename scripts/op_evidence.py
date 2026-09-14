#!/usr/bin/env python3
# ductile: v1
# name: op_evidence
# desc: 收集 agent 的失败证据（tier_journal 失败反馈 + runs 失败记录）供垂直优化提议
# lang: python
# params: agent(str, required), limit(int, default=20)
# output: agent(str), failures(int), notes(str), runs_failed(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 10
# mcsm: F(1)-O(3)-P(3)-T(4)
import os
import sqlite3
import json

agent = os.environ["DUCTILE_ARG_AGENT"]
limit = int(os.environ.get("DUCTILE_ARG_LIMIT", "20"))
db = os.path.expanduser("~/.local/share/ductile/ductile.db")
if os.environ.get("DUCTILE_DATA"):
    db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")

conn = sqlite3.connect(db)
# 失败反馈（tier_journal，去重后最近优先）
notes = [r[0] for r in conn.execute(
    "SELECT DISTINCT note FROM tier_journal WHERE agent=? AND ok=0 AND note!='' ORDER BY id DESC LIMIT ?",
    (agent, limit))]
# runs 失败统计（该 agent 名下的 llm proc 失败）
runs_failed = list(conn.execute(
    "SELECT proc_name, status, COUNT(*) FROM runs WHERE proc_name LIKE ? AND status!='Ok' GROUP BY proc_name, status ORDER BY 3 DESC LIMIT 10",
    (f"%{agent}%",)))
conn.close()

print("##DSL_RESULT")
print(f"agent={agent}")
print(f"failures={len(notes)}")
# note 内容转义（含换行/等号）
esc = lambda s: s.replace("\\", "\\\\").replace("\n", "\\n").replace("=", "\\=")
print("notes=" + json.dumps([esc(n) for n in notes], ensure_ascii=False))
print("runs_failed=" + json.dumps(runs_failed, ensure_ascii=False))
print("##DSL_END")
