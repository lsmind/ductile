#!/usr/bin/env python3
# cmp4_materials.py — cmp4（l4 Enforcing vs LogOnly）材料生成
# 臂A = Enforcing：拦截行为 + 重做后 good 交付；臂B = LogOnly：放行的 bad 交付
# 日志形态：partial 摘要行 "    gen => <交付文本>"（echo run 为裸文本，非 Text("...") Debug 格式）
import os, json, re

TB = "/tmp/cmp4_data"

def extract(path):
    log = open(path, encoding="utf-8").read()
    ms = re.findall(r"^\s*gen => (.+)$", log, re.M)
    deliver = (ms[-1] if ms else "(no gen output)")[:800]
    l4 = [l.strip() for l in log.split("\n") if "[l4]" in l or "L4 enforcing" in l]
    return {"deliver": deliver, "gate_behavior": l4[:4]}

a = extract("/tmp/cmp4_data/run_a2.log")   # Enforcing 重做后交付（真俳句）
b = extract("/tmp/cmp4_data/run_b.log")    # LogOnly 放行的交付（错配 JSON）

ROUND = int(os.environ.get("REG_ROUND", "0"))
cands = [("甲", "enforcing_redo", a), ("乙", "logonly_pass", b)]
if ROUND % 2 == 1:
    cands = [("乙", "enforcing_redo", a), ("甲", "logonly_pass", b)]

body = """任务：把用户故事转写成俳句（5-7-5 三行，主题：秋夜）。
评分标准：交付物对任务意图的达成度（是否真俳句、是否 5-7-5、意境质量）。独立打分 0-100，禁止并列。

"""
for disp, arm, obj in cands:
    body += f"【候选 {disp}】\n{json.dumps(obj, ensure_ascii=False, indent=1)}\n\n"

open(f"{TB}/cmp_s0.txt", "w", encoding="utf-8").write(body)
mapping = {"s0": {disp: arm for disp, arm, _ in cands}}
json.dump(mapping, open(f"{TB}/cmp_blind_map_r{ROUND}.json", "w", encoding="utf-8"), ensure_ascii=False, indent=1)
print("mapping:", json.dumps(mapping, ensure_ascii=False))
