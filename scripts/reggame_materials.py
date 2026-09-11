#!/usr/bin/env python3
# reggame_materials.py — 三臂盲评材料：ductile-27B(g27) / ductile-8B(g8) / 裸27B(gbar)
# 每段三候选匿名甲乙丙，轮换用 REG_ROUND 轮转（防位置偏差），裁判 blind_judge 支持丙。
import json, os

TB = "/tmp/regchain_data"

def ductile_json(path):
    raw = open(path).read()
    if raw.startswith("§§FIELDS§§"):
        body = raw[len("§§FIELDS§§"):]
        if "##DSL_RESULT" in body:
            body = body.split("##DSL_RESULT")[0]
        out = {}
        for part in body.split("§§"):
            if not part or "=" not in part:
                continue
            k, v = part.split("=", 1)
            v = v.strip()
            if v.startswith(("[", "{")):
                try:
                    v = json.loads(v)
                except Exception:
                    pass
            out[k] = v
        return {k: v for k, v in out.items() if k != "ok" and not k.startswith("meta_")}
    import re
    m = re.search(r'§§RAW§§\n(\{.*\})', raw, re.S)
    return json.loads(m.group(1))

arms = {
    "g27":  {s: ductile_json(f"{TB}/g27_{s}.txt")  for s in ["req","arch","brk","audit"]},
    "g8":   {s: ductile_json(f"{TB}/g8_{s}.txt")   for s in ["req","arch","brk","audit"]},
    "gbar": {s: ductile_json(f"{TB}/gbar_{s}.txt") for s in ["req","arch","brk","audit"]},
}

story = open(f"{TB}/story_game.txt").read()

# 基准展示位（REG_ROUND 轮转偏移防位置偏差）
segs = {
  "s0": [("甲","g27","req"), ("乙","g8","req"), ("丙","gbar","req")],
  "s1": [("甲","g27","arch"), ("乙","g8","arch"), ("丙","gbar","arch")],
  "s2": [("甲","g27","brk"), ("乙","g8","brk"), ("丙","gbar","brk")],
  "s3": [("甲","g27","audit"), ("乙","g8","audit"), ("丙","gbar","audit")],
}
_r = int(os.environ.get("REG_ROUND", "0"))
_l = ["甲", "乙", "丙"]
if _r % 3 != 0:
    segs = {k: [(_l[(_l.index(c[0]) + _r) % 3], c[1], c[2]) for c in v] for k, v in segs.items()}

rubric = {
 "s0": "需求提炼：隐含约束捕获（6人美术策划团队/无程序员/预算有限/按键级操作/50条规则/300用例/嚣张型单独建模）、结构清晰、无幻觉、范围边界明确",
 "s1": "架构设计：与约束一致性（低成本/无程序员团队维护/按键级驱动Unity/每周新关卡节奏）、技术选型成熟度、模块划分合理性、风险识别",
 "s2": "任务拆解：任务粒度与依赖合理性、预估可信、是否体现MVP优先（无程序员团队要能落地）、可执行性",
 "s3": "审计质量：发现是否真实具体（引用任务号）、是否抓住用户原话里的隐含约束（按键级/50条/300用例/嚣张型/无程序员）、可操作性、无泛泛而谈",
}

mapping = {}
for seg, cands in segs.items():
    body = f"原始用户描述：\n{story}\n\n评分标准：{rubric[seg]}\n\n以下是对同一任务的{len(cands)}份候选产出（互为竞争方案，独立打分0-100，禁止并列）。\n\n"
    for disp, arm, s in cands:
        body += f"【候选 {disp}】\n{json.dumps(arms[arm][s], ensure_ascii=False, indent=1)}\n\n"
        mapping.setdefault(seg, {})[disp] = f"{arm}.{s}"
    open(f"{TB}/game_{seg}.txt", "w").write(body)

json.dump(mapping, open(f"{TB}/game_blind_map.json", "w"), ensure_ascii=False, indent=1)
# per-round 映射落盘（tally 按轮解映射——盲评映射随 REG_ROUND 轮转，
# 单一 map 文件会被下一轮覆盖，tally 读到的是最后一轮 → 归臂错乱）
json.dump(mapping, open(f"{TB}/game_blind_map_r{_r}.json", "w"), ensure_ascii=False, indent=1)
print("映射（裁判不可见）:", json.dumps(mapping, ensure_ascii=False))
print("\n各臂产物大小:")
segmap = {"s0": "req", "s1": "arch", "s2": "brk", "s3": "audit"}
for seg in ["s0","s1","s2","s3"]:
    s = segmap[seg]
    for arm in ["g27","g8","gbar"]:
        print(f"  {seg} {arm}: {len(json.dumps(arms[arm][s], ensure_ascii=False))} chars")
