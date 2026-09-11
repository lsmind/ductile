#!/usr/bin/env python3
# reggame_tally.py — 三臂盲评聚合：轮转位置偏差打印 + 各段均值 + 总排名
import json, re, glob, collections

TB = "/tmp/regchain_data"
# 按轮读 per-round 映射（盲评r 用 map_r 解——映射随轮轮转，单一 map 文件
# 只存最后一轮，用它解全部轮次会把分数归错臂）
def round_map(r):
    return json.load(open(f"{TB}/game_blind_map_r{r}.json"))

# 读 3 轮 verdict（盲评{r}_game_s*.txt）
rounds = sorted(glob.glob(f"{TB}/盲评*_game_s*.txt"))
data = collections.defaultdict(list)   # (seg, arm) -> [score...]
pos = collections.defaultdict(list)    # 展示位 -> [score...]
for f in rounds:
    r = int(re.search(r"盲评(\d+)_game", f).group(1))
    mp = round_map(r)
    seg = re.search(r"_game_(s\d)", f).group(1)
    raw = open(f).read()
    m = re.search(r"scores=([^§]+)", raw)
    if not m:
        continue
    pairs = dict(re.findall(r"(甲|乙|丙)=(\d+)", m.group(1)))
    for disp, sc in pairs.items():
        arm = mp[seg][disp].split(".")[0]
        data[(seg, arm)].append(int(sc))
        pos[f"{seg}:{disp}"].append(int(sc))

arms = ["g27", "g8", "gbar"]
names = {"g27": "ductile-27B", "g8": "ductile-8B", "gbar": "裸27B"}
print("== 三臂分段均值 ==")
for seg in ["s0", "s1", "s2", "s3"]:
    row = []
    for a in arms:
        v = data.get((seg, a), [])
        row.append(f"{names[a]} {sum(v)/len(v):.1f}" if v else f"{names[a]} -")
    base = data.get((seg, "gbar"), [])
    det = ""
    if base:
        bm = sum(base)/len(base)
        for a in ["g27", "g8"]:
            v = data.get((seg, a), [])
            if v:
                det += f"  {names[a]}-裸={sum(v)/len(v)-bm:+.1f}"
    print(f"{seg}: " + " | ".join(row) + det)

print("\n== 各臂总分（4 段平均）==")
for a in arms:
    seg_means = [sum(v)/len(v) for (seg, aa), v in data.items() if aa == a and seg in ["s0","s1","s2","s3"]]
    if seg_means:
        print(f"{names[a]}: {sum(seg_means)/len(seg_means):.1f}")

print("\n== 位置偏差（同臂不同展示位均值）==")
for k in sorted(pos):
    v = pos[k]
    if len(v) >= 2:
        print(f"{k}: 均值 {sum(v)/len(v):.1f} (n={len(v)})")
print("\nREGGAME-DONE")
