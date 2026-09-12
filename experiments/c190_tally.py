#!/usr/bin/env python3
# c190_tally.py — 27B加压版：三轮盲评聚合
import json

TB = "/tmp/regchain_data"
ARMS = ("gc2", "g27d", "gbar")
scores = {a: [] for a in ARMS}

for r in (0, 1, 2):
    m = json.load(open(f"{TB}/c190_map_r{r}.json"))
    inv = {v: k for k, v in m.items()}
    raw = open(f"{TB}/c190r{r}_c181_s2.txt").read()
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
print(f"g27d vs gbar:  {sum(scores['g27d'])/3 - sum(scores['gbar'])/3:+.1f}")
print(f"g27d vs gc2:   {sum(scores['g27d'])/3 - sum(scores['gc2'])/3:+.1f}")
print(f"对照 g27c2(旧guide)=82.7 vs gbar=+20.0 → 加压guide的27B增量: {sum(scores['g27d'])/3 - 82.7:+.1f}")
print("C190-DONE")
