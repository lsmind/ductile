#!/usr/bin/env python3
# regchain_materials.py — 回归探针材料生成：从 regchain 产物提取 JSON、匿名化、轮换顺序
import json, re, os

TB = "/tmp/regchain_data"

def ductile_json(path):
    raw = open(path).read()
    if raw.startswith("§§FIELDS§§"):
        body = raw[len("§§FIELDS§§"):]
        # 截到 ##DSL_RESULT 之前（若有）
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
        # 剔除元数据字段（ok/meta_*）——盲评匿名性：不能让裁判看到模型身份
        return {k: v for k, v in out.items() if k != "ok" and not k.startswith("meta_")}
    m = re.search(r'§§RAW§§\n(\{.*\})\n?\s*##DSL_RESULT', raw, re.S)
    if not m:
        m = re.search(r'§§RAW§§\n(\{.*\})', raw, re.S)
    return json.loads(m.group(1))

def fence_json(raw):
    m = re.search(r'\{.*\}', raw, re.S)
    return json.loads(m.group(0))

arms = {
    # auto 臂 = 当前引擎 auto-prompt 产物（回归对象）
    "auto":  {s: ductile_json(f"{TB}/reg_{s}.txt") for s in ["req","arch","brk","audit"]},
    # baseline 臂 = 手写 prompt 的 all27 链产物（固化基线）
    "baseline": {s: ductile_json(f"{TB}/out27_{s}.txt") for s in ["req","arch","brk","audit"]},
}
json.dump(arms, open(f"{TB}/reg_products.json","w"), ensure_ascii=False, indent=1)

# 匿名化候选材料（每段轮换顺序防位置偏差；映射记录在本地，不给裁判看）
story = open(f"{TB}/story.txt").read()
def cand_block(name, obj):
    return f"【候选 {name}】\n{json.dumps(obj, ensure_ascii=False, indent=1)}\n"

segs = {
  # 段: [(展示名, 臂, 段名), ...] —— 两方，每段轮换顺序防位置偏差
  "s0": [("甲","auto","req"), ("乙","baseline","req")],
  "s1": [("甲","baseline","arch"), ("乙","auto","arch")],
  "s2": [("甲","auto","brk"), ("乙","baseline","brk")],
  "s3": [("甲","baseline","audit"), ("乙","auto","audit")],
}
# N 轮顺序轮换（v0.17.2）：奇数轮甲乙互换——位置偏差控制
_r = int(__import__("os").environ.get("REG_ROUND", "0"))
if _r % 2 == 1:
    _lbl = {"甲": "乙", "乙": "甲"}
    segs = {k: [(_lbl[c[0]], c[1], c[2]) for c in v] for k, v in segs.items()}

rubric = {
 "s0": "需求提炼：隐含约束捕获（5人团队/每天几十GB/低预算/避免复杂/先跑起来）、结构清晰、无幻觉、范围边界明确",
 "s1": "架构设计：与约束一致性（低成本/简单/5人非IT团队/几十GB日增）、技术选型成熟度、模块划分合理性、风险识别",
 "s2": "任务拆解：任务粒度与依赖合理性、预估可信、是否体现MVP优先、可执行性",
 "s3": "审计质量：发现是否真实具体（引用任务号）、是否抓住用户原话里的隐含约束、可操作性、无泛泛而谈",
}
mapping = {}
for seg, cands in segs.items():
    body = f"原始用户描述：\n{story}\n\n评分标准：{rubric[seg]}\n\n以下是对同一任务的{len(cands)}份候选产出（互为竞争方案，独立打分0-100，禁止并列）。\n\n"
    for disp, arm, s in cands:
        body += cand_block(disp, arms[arm][s])
        mapping.setdefault(seg, {})[disp] = f"{arm}.{s}"
    open(f"{TB}/reg_{seg}.txt","w").write(body)

json.dump(mapping, open(f"{TB}/reg_blind_map.json","w"), ensure_ascii=False, indent=1)
json.dump(mapping, open(f"{TB}/reg_blind_map_r{_r}.json","w"), ensure_ascii=False, indent=1)
print("映射（裁判不可见）:", json.dumps(mapping, ensure_ascii=False))
print("\n各臂产物大小:")
for seg, cands in segs.items():
    for disp, arm, s in cands:
        print(f"  {seg} {disp}={arm}.{s}: {len(json.dumps(arms[arm][s], ensure_ascii=False))} chars")
