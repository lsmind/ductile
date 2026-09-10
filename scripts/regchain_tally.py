#!/usr/bin/env python3
# regchain_tally.py — 解盲汇总 + 回归门禁：auto 均值 < baseline 均值 - 5 → exit 1
import json, re, sys

TB = "/tmp/regchain_data"
m = json.load(open(f"{TB}/reg_blind_map.json"))

def scores(stage):
    t = open(f"{TB}/verdict_s{stage}.txt", encoding="utf-8").read()
    return dict(re.findall(r"(甲|乙)=(\d+)", t))

arms = {}
for s in ["0", "1", "2", "3"]:
    sc = scores(s)
    if not sc:
        print(f"REGFAIL verdict_s{s} 无分数"); sys.exit(1)
    for anon, real in m[f"s{s}"].items():
        arms.setdefault(real.split(".")[0], []).append(int(sc[anon]))

auto = arms.get("auto", [])
base = arms.get("baseline", [])
ma, mb = sum(auto)/len(auto), sum(base)/len(base)
print(f"auto(当前auto-prompt) 均值 {ma:.1f}  各段 {auto}")
print(f"baseline(手写prompt) 均值 {mb:.1f}  各段 {base}")
if ma >= mb - 5:
    print(f"REGCHAIN-PASS (差值 {mb-ma:+.1f}，容差 5)")
else:
    print(f"REGCHAIN-FAIL auto 退化 {mb-ma:.1f} 分 (>容差5)"); sys.exit(1)
