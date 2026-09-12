#!/usr/bin/env python3
# c188_tally.py — 9B加压版：三轮盲评聚合
import json

TB = "/tmp/regchain_data"
ARMS = ("gc2", "g9d4", "gbar")
scores = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c188_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}
    raw = open(f"{TB}/c188r{r}_c181_s2.txt").read()
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
    print(f"--- round {r} notes: {nt[:220]}")

print()
for arm in ARMS:
    v = scores[arm]
    print(f"{arm}: mean={sum(v)/len(v):.1f} rounds={v} 轮间极差={max(v)-min(v)}")

print()
print(f"g9d4 vs gbar: {sum(scores['g9d4'])/3 - sum(scores['gbar'])/3:+.1f}（g9c 未加压 -33.7；目标≥0）")
print(f"g9d4 vs gc2:  {sum(scores['g9d4'])/3 - sum(scores['gc2'])/3:+.1f}")
print("C188-DONE")
