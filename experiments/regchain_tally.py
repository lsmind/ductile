#!/usr/bin/env python3
# regchain_tally.py — N 轮均值门禁：聚合 3 轮盲评，auto < baseline - 5 → exit 1
# 轮间噪声（同臂跨轮极差）与位置偏差（甲/乙均值差）一并打印——测量方法学透明
import json, re, sys, statistics

TB = "/tmp/regchain_data"
ROUNDS = 3

def load_map(r):
    return json.load(open(f"{TB}/reg_blind_map_r{r}.json"))

def scores(stage, r):
    t = open(f"{TB}/盲评{r}_s{stage}.txt", encoding="utf-8").read()
    return dict(re.findall(r"(甲|乙)=(\d+)", t))

# 轮 x 臂 x 段
per_round = []  # [{arm: {seg: score}}]
pos_scores = {"甲": [], "乙": []}
for r in range(ROUNDS):
    m = load_map(r)
    arms = {}
    for s in ["0", "1", "2", "3"]:
        sc = scores(s, r)
        if not sc:
            print(f"REGFAIL round{r} verdict_s{s} 无分数"); sys.exit(1)
        for anon, real in m[f"s{s}"].items():
            arms.setdefault(real.split(".")[0], {})[s] = int(sc[anon])
            pos_scores[anon].append(int(sc[anon]))
    per_round.append(arms)

# 每臂：各段跨轮均值 + 轮均值
def agg(arm):
    segs = ["0", "1", "2", "3"]
    seg_mean = [statistics.mean(a[arm][s] for a in per_round) for s in segs]
    round_means = [statistics.mean([a[arm][s] for s in segs]) for a in per_round]
    return seg_mean, round_means

auto_seg, auto_r = agg("auto")
base_seg, base_r = agg("baseline")
ma, mb = statistics.mean(auto_seg), statistics.mean(base_seg)

print("== 分段均值（3 轮）==")
for i, s in enumerate(["s0", "s1", "s2", "s3"]):
    d = auto_seg[i] - base_seg[i]
    print(f"{s}: auto {auto_seg[i]:.1f} vs baseline {base_seg[i]:.1f}  ({d:+.1f})")
print(f"auto 轮均值: {[f'{x:.1f}' for x in auto_r]}")
print(f"baseline 轮均值: {[f'{x:.1f}' for x in base_r]}")
noise_a = max(auto_r) - min(auto_r)
noise_b = max(base_r) - min(base_r)
print(f"轮间噪声: auto 极差 {noise_a:.1f} / baseline 极差 {noise_b:.1f}")
print(f"位置偏差: 甲均值 {statistics.mean(pos_scores['甲']):.1f} vs 乙均值 {statistics.mean(pos_scores['乙']):.1f}")

diff = mb - ma
if diff > 5:
    print(f"REGCHAIN-FAIL auto 退化 {diff:.1f} 分 (>容差5)"); sys.exit(1)
print(f"REGCHAIN-PASS (auto 落后 {diff:.1f}，容差 5；3 轮均值)")
