#!/usr/bin/env python3
# c185_tally.py — 27B 问询链 vs 裸27B：三轮盲评聚合
import json

TB = "/tmp/regchain_data"
ARMS = ("gc2", "g27c", "gbar")
scores = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c185_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}
    raw = open(f"{TB}/c185r{r}_c181_s2.txt").read()
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

g27c = scores["g27c"]
gbar = scores["gbar"]
print()
print(f"g27c vs gbar: {sum(g27c)/3 - sum(gbar)/3:+.1f}")
print(f"g27c vs gc2 (问询链 vs v0.18.2管线): {sum(g27c)/3 - sum(scores['gc2'])/3:+.1f}")
print("C185-DONE")
