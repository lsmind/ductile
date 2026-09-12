#!/usr/bin/env python3
# c181_materials.py — v0.18.2 owner 槽位验收：s2 拆解段三候选盲评
# 臂：g27=旧管线(无.constraint无owner) / gc2=v0.18.2(.constraint+brk owner槽位) / gbar=裸27B
# 历史对照：gc(v0.18.1 无owner槽位) 三轮均值 64.0；gbar 73.7；g27 56.7
# 只生成 s2 材料 → c181_s2.txt，映射 c181_map_r{REG_ROUND}.json
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
    "g27":  ductile_json(f"{TB}/g27_brk.txt"),
    "gc2":  ductile_json(f"{TB}/gc2_brk.txt"),
    "gbar": ductile_json(f"{TB}/gbar_brk.txt"),
}

story = open(f"{TB}/story_game.txt").read()

cands = [("甲", "g27"), ("乙", "gc2"), ("丙", "gbar")]
_r = int(os.environ.get("REG_ROUND", "0"))
_l = ["甲", "乙", "丙"]
if _r % 3 != 0:
    cands = [(_l[(_l.index(c[0]) + _r) % 3], c[1]) for c in cands]

rubric = "任务拆解：任务粒度与依赖合理性、预估可信、是否体现MVP优先（无程序员团队要能落地）、任务分配是否回应团队构成（6人美术策划、无程序员——谁做什么必须明确）、可执行性"

body = f"原始用户描述：\n{story}\n\n评分标准：{rubric}\n\n以下是对同一任务的{len(cands)}份候选产出（互为竞争方案，独立打分0-100，禁止并列）。\n\n"
mapping = {}
for disp, arm in cands:
    body += f"【候选 {disp}】\n{json.dumps(arms[arm], ensure_ascii=False, indent=1)}\n\n"
    mapping[disp] = arm

open(f"{TB}/c181_s2.txt", "w").write(body)
json.dump(mapping, open(f"{TB}/c181_map_r{_r}.json", "w"), ensure_ascii=False, indent=1)
print(f"round {_r} 映射:", json.dumps(mapping, ensure_ascii=False))
