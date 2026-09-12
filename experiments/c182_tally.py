#!/usr/bin/env python3
# c182_tally.py — v0.18.2 owner 槽位验收：三轮盲评聚合
# 读 c181_map_r{r}.json（显示位→臂）+ c182r{r}_c181_s2.txt（verdict），输出臂均值与轮间波动
import json

TB = "/tmp/regchain_data"
ARMS = ("g27", "gc2", "gbar")
scores = {a: [] for a in ARMS}
crits = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c181_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}  # 显示位 -> 臂
    raw = open(f"{TB}/c182r{r}_c181_s2.txt").read()
    body = raw.split("##DSL_RESULT")[0].split("§§RAW§§")[0]
    sc, nt = {}, ""
    for part in body.split("§§"):
        if part.startswith("scores="):
            for kv in part[len("scores="):].split(","):
                c, v = kv.split("=")
                sc[c.strip()] = int(v.strip())
        elif part.startswith("notes="):
            nt = part[len("notes="):]
    for arm in ARMS:
        scores[arm].append(sc[inv[arm]])
    crits_note = ""
    # 批语里截取各臂相关片段太碎，整段存 r{r} 供人工回看
    print(f"--- round {r} notes: {nt[:200]}")

print()
for arm in ARMS:
    v = scores[arm]
    spread = max(v) - min(v)
    print(f"{arm}: mean={sum(v)/len(v):.1f} rounds={v} 轮间极差={spread}")

gc2 = scores["gc2"]
gbar = scores["gbar"]
print()
print(f"gc2 vs gbar 差距: {sum(gbar)/3 - sum(gc2)/3:+.1f}（v0.18.1 时 gc 落后 9.7）")
print("C182-DONE")
