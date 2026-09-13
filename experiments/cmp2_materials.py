#!/usr/bin/env python3
# cmp2_materials.py — cmp2（triage 归因对比）材料生成：两份处置报告匿名化
import os, json

TB = "/tmp/cmp2_data"

triaged = open(f"{TB}/cmp_triaged.txt", encoding="utf-8").read()
raw = open(f"{TB}/cmp_raw_inc.txt", encoding="utf-8").read()

ROUND = int(os.environ.get("REG_ROUND", "0"))
cands = [("甲", "triaged", triaged), ("乙", "raw", raw)]
if ROUND % 2 == 1:
    cands = [("乙", "triaged", triaged), ("甲", "raw", raw)]

body = """你收到一份 agent 管线系统的 incident 处置报告（两份候选，同一事故的两种处置路径产物）。
评分标准：归因准确性线索、信息密度、可操作性（操作者拿到能否直接行动）、是否定位到责任节点。
独立打分 0-100，禁止并列。

"""
for disp, arm, text in cands:
    body += f"【候选 {disp}】\n{text}\n\n"

open(f"{TB}/cmp_s0.txt", "w", encoding="utf-8").write(body)
mapping = {"s0": {disp: arm for disp, arm, _ in cands}}
json.dump(mapping, open(f"{TB}/cmp_blind_map_r{ROUND}.json", "w", encoding="utf-8"), ensure_ascii=False, indent=1)
print("映射:", json.dumps(mapping, ensure_ascii=False))
print("triaged {} chars | raw {} chars".format(len(triaged), len(raw)))
