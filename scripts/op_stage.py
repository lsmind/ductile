#!/usr/bin/env python3
# ductile: v1
# name: op_stage
# desc: 把垂直优化提议写进 patches 表（tentative，origin=llm），等下轮实测裁决
# lang: python
# params: agent(str, required), field(str, default=guide), value(str, required), rationale(str, default=)
# output: staged(int), patch_id(int)
# pure: false
# idempotent: true
# concurrency: exclusive
# effects: fs
# timeout: 10
# mcsm: F(1)-O(3)-P(3)-T(4)
import os
import sqlite3
from datetime import datetime, timezone

agent = os.environ["DUCTILE_ARG_AGENT"]
field = os.environ.get("DUCTILE_ARG_FIELD", "guide")
value = os.environ["DUCTILE_ARG_VALUE"]
rationale = os.environ.get("DUCTILE_ARG_RATIONALE", "")
model = os.environ.get("OPENAI_MODEL", "unknown")
db = os.path.expanduser("~/.local/share/ductile/ductile.db")
if os.environ.get("DUCTILE_DATA"):
    db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")

conn = sqlite3.connect(db)
now = datetime.now(timezone.utc).isoformat(timespec="seconds")
conn.execute(
    "INSERT INTO patches (pipeline, proc_name, impl_name, field, value, created_at, origin, status, confirmed_at, reverted_at) "
    "VALUES ('op-revise', ?, 'llm', ?, ?, ?, ?, 'tentative', '', '')",
    (agent, field, value, now, f"llm:{model}"))
conn.commit()
pid = conn.execute("SELECT last_insert_rowid()").fetchone()[0]
conn.close()

print("##DSL_RESULT")
print("staged=1")
print(f"patch_id={pid}")
print("##DSL_END")
