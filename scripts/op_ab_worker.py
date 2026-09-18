#!/usr/bin/env python3
# ductile: v1
# name: op_ab_worker
# desc: A/B 实验底层——armA 固定策略（全展开）vs armB replay 进化（跳过重放门确认过的 never_ok）
# lang: python
# params: arm(str), rounds(int, default=6), base(int, default=0), tag(str, default=)
# output: rounds_done(int), v_mean(str), skips(int), summary(str)
# pure: false
# idempotent: false
# concurrency: exclusive
# effects: writes runs table in DUCTILE_DATA db
# timeout: 120
# mcsm: F(2)-O(2)-P(3)-T(3)
# mcsm_note_f: 隔离库 SQLite 写
# mcsm_note_o: runs 行组
# mcsm_note_p: stdout 摘要
# mcsm_note_t: A/B 对照底层
import os
import random
import sqlite3
import time

ARM = os.environ.get("DUCTILE_ARG_ARM", "A")
R = int(os.environ.get("DUCTILE_ARG_ROUNDS", "6"))
BASE = int(os.environ.get("DUCTILE_ARG_BASE", "0"))
TAG = os.environ.get("DUCTILE_ARG_TAG", "")
db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")

# 场景：医美机构私域转化 SOP（新域，防单场景过拟合）
# sane 链（真实业务流）：咨询分流→分诊规则→方案报价→客户档案→回访
SANE = ["consult_intake", "triage_rule", "plan_quote", "customer_profile", "followup_call"]
# 病灶：bad1/bad2 never_ok（合规敏感词过滤、渠道归因——一直坏）；bad3 flaky（企业微信推送——时好时坏）
BAD = {
    "bad_sensword_filter": "never_ok",
    "bad_channel_attribution": "never_ok",
    "bad_wecom_push": "flaky",
}
PIPELINE = "sop_ab"


def outcome(proc, seed_key):
    """确定性的场景结果：sane 恒 Ok；never_ok 恒 Fail；flaky 按种子掷硬币。"""
    if proc in BAD:
        if BAD[proc] == "never_ok":
            return "Fail", 0.0
        ok = random.Random(seed_key).random() < 0.5
        return ("Ok", 1.0) if ok else ("Fail", 0.0)
    return "Ok", 1.0


conn = sqlite3.connect(db)
now = time.strftime("%Y-%m-%dT%H:%M:%S")

# armB 的 active 策略 = 重放门 confirm 过的 skip_procs 声明集合
skips = set()
if ARM == "B":
    rows = conn.execute(
        "SELECT value FROM patches WHERE field='when' "
        "AND value LIKE 'replay_effect: skip_procs=%' AND status='confirmed'"
    ).fetchall()
    for (v,) in rows:
        skips |= {p.strip() for p in v.split("=", 1)[1].split(",") if p.strip()}

v_list = []
skipped_rows = 0
for r in range(R):
    session = f"{ARM.lower()}_{TAG}_{BASE + r:02d}" if TAG else f"{ARM.lower()}_{BASE + r:02d}"
    executed = []
    for proc in SANE + list(BAD):
        if proc in skips:
            skipped_rows += 1
            continue
        executed.append(proc)
        status, score = outcome(proc, f"{session}-{proc}")
        conn.execute(
            "INSERT INTO runs (proc_name, impl_name, pipeline, status, latency_ms, recorded_at, session, score) "
            "VALUES (?,?,?,?,?,?,?,?)",
            (proc, "main", PIPELINE, status, 100, now, session, score),
        )
    conn.commit()
    # Eq.1（β1=0.02 β2=0.01，与引擎 v_proxy 同参）：best − β1·N + β2·N/k
    scores = [outcome(p, f"{session}-{p}")[1] for p in executed]
    n = len(scores)
    k = len(set(executed))
    best = max(scores) if scores else 0.0
    v_list.append(best - 0.02 * n + 0.01 * n / max(k, 1))
conn.close()

v_mean = sum(v_list) / len(v_list) if v_list else 0.0
print("##DSL_RESULT")
print(f"rounds_done={R}")
print(f"v_mean={v_mean:.4f}")
print(f"skips={skipped_rows}")
print(f"summary=arm{ARM} base={BASE} rounds={R} v_mean={v_mean:.4f} active_skips={sorted(skips) if skips else '[]'}")
print("##DSL_END")
