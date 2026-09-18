#!/usr/bin/env python3
# ductile: v1
# name: op_ab_neg
# desc: A/B 实验负例——直接 stage 一个「跳过 stable 好节点」的坏声明，验证重放门 REJECT 物理拦截
# lang: python
# params: none
# output: staged(int), patch_id(int)
# pure: false
# idempotent: true
# concurrency: exclusive
# effects: writes patches table (tentative)
# timeout: 10
# mcsm: F(2)-O(2)-P(3)-T(4)
# mcsm_note_f: 隔离库 patches 写
# mcsm_note_o: 1 行 patch
# mcsm_note_p: stdout staged=/patch_id=
# mcsm_note_t: 负例验证
import os
import sqlite3
from datetime import datetime, timezone

db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")
conn = sqlite3.connect(db)
now = datetime.now(timezone.utc).isoformat(timespec="seconds")
# 坏声明：跳过两个恒 Ok 的核心业务节点——重放分必跌，门必须 REJECT
value = "replay_effect: skip_procs=consult_intake,triage_rule"
conn.execute(
    "INSERT INTO patches (pipeline, proc_name, impl_name, field, value, created_at, origin, status, confirmed_at, reverted_at) "
    "VALUES ('op-revise', 'neg_probe', 'llm', 'when', ?, ?, 'llm:neg', 'tentative', '', '') "
    "ON CONFLICT(pipeline, proc_name, impl_name, field) DO UPDATE SET value=excluded.value, status='tentative'",
    (value, now),
)
conn.commit()
pid = conn.execute("SELECT last_insert_rowid()").fetchone()[0]
conn.close()
print("##DSL_RESULT")
print("staged=1")
print(f"patch_id={pid}")
print("##DSL_END")
