#!/usr/bin/env python3
# c183_tally.py — 8B+管线 vs 裸27B：三轮盲评聚合
import json

TB = "/tmp/regchain_data"
ARMS = ("gc2", "g8b", "gbar")
scores = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c183_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}
    raw = open(f"{TB}/c183r{r}_c181_s2.txt").read()
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
    print(f"--- round {r} notes: {nt[:200]}")

print()
for arm in ARMS:
    v = scores[arm]
    print(f"{arm}: mean={sum(v)/len(v):.1f} rounds={v} 轮间极差={max(v)-min(v)}")

g8b = scores["g8b"]
gbar = scores["gbar"]
print()
print(f"g8b vs gbar: {sum(g8b)/3 - sum(gbar)/3:+.1f}（旧 8B 管线为 -32.4）")
print(f"g8b vs gc2 (8B vs 27B 同管线): {sum(g8b)/3 - sum(scores['gc2'])/3:+.1f}")
print("C183-DONE")
