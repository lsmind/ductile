#!/usr/bin/env python3
# c187_tally.py — 9B问询链 vs 裸27B：三轮盲评聚合
import json

TB = "/tmp/regchain_data"
ARMS = ("gc2", "g9c", "gbar")
scores = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c187_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}
    raw = open(f"{TB}/c187r{r}_c181_s2.txt").read()
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
print(f"g9c vs gbar: {sum(scores['g9c'])/3 - sum(scores['gbar'])/3:+.1f}（lfm2.5-8b 问询链为 -39.0；目标≥0）")
print(f"g9c vs gc2:  {sum(scores['g9c'])/3 - sum(scores['gc2'])/3:+.1f}")
print("C187-DONE")
