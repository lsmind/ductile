#!/usr/bin/env python3
# ductile: v1
# name: op_ab_report
# desc: A/B 实验终报——按 session 前缀分臂聚合 runs，算每轮 Eq.1 分与臂间差、probes 节省、门裁决统计
# lang: python
# params: rounds(int, default=6), k_every(int, default=2)
# output: report(str), delta(float), a_mean(float), b_mean(float), saved_probes(str), gate_stats(str)
# pure: true
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 30
# mcsm: F(2)-O(2)-P(3)-T(4)
# mcsm_note_f: 只读隔离库
# mcsm_note_o: runs/patches 聚合
# mcsm_note_p: stdout 报告
# mcsm_note_t: A/B 终报
import os
import sqlite3

R = int(os.environ.get("DUCTILE_ARG_ROUNDS", "6"))
K = int(os.environ.get("DUCTILE_ARG_K_EVERY", "2"))
db = os.path.join(os.environ["DUCTILE_DATA"], "ductile.db")

conn = sqlite3.connect(db)
rows = conn.execute(
    "SELECT session, proc_name, score, status FROM runs WHERE session != '' ORDER BY session, id"
).fetchall()
patches = conn.execute("SELECT id, value, status, field FROM patches").fetchall()
conn.close()

by_session = {}
for sess, proc, sc, st in rows:
    score = sc if sc is not None else (1.0 if st == "Ok" else 0.0)
    by_session.setdefault(sess, []).append((proc, score))


def eq1(nodes):
    scores = [s for _, s in nodes]
    n = len(scores)
    k = len(set(p for p, _ in nodes))
    best = max(scores) if scores else 0.0
    return best - 0.02 * n + 0.01 * n / max(k, 1)


def parse_sess(sess):
    """{arm}_{tag}_{round} → (arm, tag)；无 tag 的旧格式 (arm, '')。b0 基线 tag='b0' 排除。"""
    parts = sess.split("_")
    arm = parts[0]
    tag = parts[1] if len(parts) >= 3 else ""
    return arm, tag


def arm_stats(prefix):
    """按臂聚合（排除 b0 基线 session）。返回 (总均, per-session V 列表, probes 列表, per-seed 均值)。"""
    vs, probes, by_seed = [], [], {}
    for sess in sorted(by_session):
        arm, tag = parse_sess(sess)
        if arm == prefix and tag != "b0":
            nodes = by_session[sess]
            v = eq1(nodes)
            vs.append(v)
            probes.append(len(nodes))
            if tag:
                by_seed.setdefault(tag, []).append(v)
    mean = sum(vs) / len(vs) if vs else 0.0
    seed_means = {t: sum(v) / len(v) for t, v in sorted(by_seed.items())}
    return mean, vs, probes, seed_means


a_mean, a_vs, a_probes, a_seeds = arm_stats("a")
b_mean, b_vs, b_probes, b_seeds = arm_stats("b")
delta = b_mean - a_mean
a_p = sum(a_probes) / len(a_probes) if a_probes else 0
b_p = sum(b_probes) / len(b_probes) if b_probes else 0
saved_pct = (a_p - b_p) / a_p * 100 if a_p else 0.0

gate_rows = []
for pid, value, status, field in patches:
    if field == "when" and "skip_procs" in value:
        gate_rows.append(f"#{pid} [{status}] {value[:60]}")
gate_stats = "; ".join(gate_rows) if gate_rows else "no skip patches staged"

lines = []
lines.append("== Dream-RSI A/B Report（场景：医美私域 SOP，LLM=qwen3.8:27b）==")
lines.append(f"armA 固定策略: v_mean={a_mean:.4f}  probes/round={a_p:.1f}  ({len(a_vs)} rounds)")
lines.append(f"armB replay进化: v_mean={b_mean:.4f}  probes/round={b_p:.1f}  ({len(b_vs)} rounds)")
lines.append(f"Δ(B−A) = {delta:+.4f}   probes 节省 = {saved_pct:.1f}%")
lines.append(f"armA per-round: {' '.join(f'{v:.3f}' for v in a_vs)}")
lines.append(f"armB per-round: {' '.join(f'{v:.3f}' for v in b_vs)}")
if a_seeds or b_seeds:
    lines.append(f"armA per-seed: {' '.join(f'{t}={m:.3f}' for t, m in a_seeds.items())}")
    lines.append(f"armB per-seed: {' '.join(f'{t}={m:.3f}' for t, m in b_seeds.items())}")
lines.append(f"gate: {gate_stats}")
report = "\n".join(lines)

print("##DSL_RESULT")
print(f"delta={delta:.4f}")
print(f"a_mean={a_mean:.4f}")
print(f"b_mean={b_mean:.4f}")
print(f"saved_probes={saved_pct:.1f}%")
print(f"gate_stats={gate_stats}")
print(f"report={report}")
print("##DSL_END")
