#!/usr/bin/env python3
"""layers_probe.py — 七层认知栈边界探针（零 LLM，确定性）。

制度保证：分层不靠自觉靠物理结构。本探针扫描 src/ 生产代码的 use crate::
依赖边，断言合法层向（只能向下依赖，禁止上行/跳层走私），输出
##DSL_RESULT 结构化结果给 selftest 门禁。

合法层序（数值大 = 高层，只许向下依赖）：
  interface(横切) > L5→L4→L3→L2→L1→L0；core/ 是层间共享词汇表
  （ast + dslresult 协议），任何层可依赖，core 不依赖任何层。

已知宽免（白名单，逐条有理由，新增必须在这里记账并说明）：
  - L0/db → L4/harvest.civil_from_days：纯时间函数待下沉（记账宽免，勿效仿）
"""
import re, glob, os, sys, json

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "src")
ORDER = {"L4_structure": 4, "L3_dsl": 3, "L2_orchestration": 2,
         "L1_feedback": 1, "L0_physical": 0}
AMNESTY = {}  # 白名单清零（v0.18.5：civil_from_days 已下沉 L0_physical/time.rs）

def probe():
    issues, cross = [], set()
    n_edges = 0
    for f in sorted(glob.glob(f"{SRC}/L*/*.rs")):
        if f.endswith("mod.rs"):
            continue
        lay = os.path.basename(os.path.dirname(f))
        mod = f"{lay}/{os.path.basename(f)[:-3]}"
        src = open(f, encoding="utf-8").read()
        # 剥 #[cfg(test)] mod tests {...}：测试代码不是生产依赖边
        src = re.sub(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{.*?\n\}\s*(?=\S|\Z)", "", src, flags=re.S)
        for target in sorted(set(re.findall(r"use crate::([A-Za-z0-9_]+)", src))):
            n_edges += 1
            t = None
            for L in ORDER:
                if os.path.exists(f"{SRC}/{L}/{target}.rs"):
                    t = L
                    break
            if t is None:
                continue  # core/、根函数、interface：共享词汇或横切，不参与层序
            if t != lay:
                cross.add(f"{lay.split('_')[1]}→{'_'.join(t.split('_')[1:]) if False else t.split('_')[1]}:{target}")
                if ORDER[lay] < ORDER[t] and (mod, f"{t}/{target}") not in AMNESTY:
                    issues.append(f"层序违规: {mod} → {t}/{target} "
                                  f"(L{ORDER[lay]} 上行依赖 L{ORDER[t]})")
    return n_edges, sorted(cross), issues

if __name__ == "__main__":
    n, cross, issues = probe()
    print("##DSL_RESULT")
    print(json.dumps({
        "probe": "layers",
        "edges_checked": n,
        "cross_layer_edges": cross,
        "amnesty": {f"{k[0]}→{k[1]}": v for k, v in AMNESTY.items()},
        "violations": issues,
        "probe_score": 100 if not issues else max(0, 100 - 25 * len(issues)),
    }, ensure_ascii=False, indent=1))
    sys.exit(1 if issues else 0)
