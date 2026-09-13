#!/usr/bin/env python3
# cmp_materials.py — 通用两臂对比材料生成器（v0.18.6 对比项目四件套）
# 从臂产物提取 JSON、匿名化（甲/乙）、REG_ROUND 奇偶轮换防位置偏差。
# 与 regchain_materials.py 同模式；臂名/段名/评分标准由调用方经 CMP_ARMS 覆盖。
import json, re, os, sys

TB = os.environ.get("CMP_TB", "/tmp/cmp1_data")
ROUND = int(os.environ.get("REG_ROUND", "0"))

# 默认：cmp1 τ_off 对比（阶梯 vs 固定高档），两段
SEGS = json.loads(os.environ.get("CMP_SEGS", """{
  "s0": [["甲","ladder","req"], ["乙","fixed","req"]],
  "s1": [["甲","ladder","arch"], ["乙","fixed","arch"]]
}"""))

# 产物文件名模式：{TB}/cmp_{arm}_{seg_suffix}.txt
FILE_FMT = os.environ.get("CMP_FILE_FMT", "{tb}/cmp_{arm}_{seg}.txt")

RUBRIC = json.loads(os.environ.get("CMP_RUBRIC", """{
  "s0": "需求提炼：隐含约束捕获、结构清晰、无幻觉、范围边界明确",
  "s1": "架构设计：与约束一致性、选型成熟度、模块划分合理性、风险识别"
}"""))

STORY = os.environ.get("CMP_STORY", f"{TB}/story.txt")


def ductile_json(path):
    raw = open(path, encoding="utf-8").read()
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
    m = re.search(r'§§RAW§§\n(\{.*\})\n?\s*##DSL_RESULT', raw, re.S)
    if not m:
        m = re.search(r'§§RAW§§\n(\{.*\})', raw, re.S)
    if not m:
        m = re.search(r'\{.*\}', raw, re.S)  # 兜底：全文找 JSON
    if m is None:
        raise ValueError(f"{path}: 未找到 JSON 产物（格式不符）")
    return json.loads(m.group(1))


# 轮换：奇数轮甲乙互换（位置偏差控制）
if ROUND % 2 == 1:
    _lbl = {"甲": "乙", "乙": "甲"}
    SEGS = {k: [(_lbl[c[0]], c[1], c[2]) for c in v] for k, v in SEGS.items()}

story = open(STORY, encoding="utf-8").read()

mapping = {}
sizes = {}
for seg, cands in SEGS.items():
    body = f"原始用户描述：\n{story}\n\n评分标准：{RUBRIC[seg]}\n\n以下是对同一任务的{len(cands)}份候选产出（互为竞争方案，独立打分0-100，禁止并列）。\n\n"
    for disp, arm, s in cands:
        obj = ductile_json(FILE_FMT.format(tb=TB, arm=arm, seg=s))
        body += f"【候选 {disp}】\n{json.dumps(obj, ensure_ascii=False, indent=1)}\n\n"
        mapping.setdefault(seg, {})[disp] = f"{arm}.{s}"
        sizes[f"{arm}.{s}"] = len(json.dumps(obj, ensure_ascii=False))
    open(f"{TB}/cmp_{seg}.txt", "w", encoding="utf-8").write(body)
    # 裁判文件名与 regchain 对齐（cmp_judge.pipeline 读 reg_ 前缀不合适，用 cmp_ 前缀）

json.dump(mapping, open(f"{TB}/cmp_blind_map_r{ROUND}.json", "w", encoding="utf-8"), ensure_ascii=False, indent=1)
print("映射（裁判不可见）:", json.dumps(mapping, ensure_ascii=False))
print("各臂产物大小:", json.dumps(sizes, ensure_ascii=False))
