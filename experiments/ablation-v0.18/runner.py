#!/usr/bin/env python3
"""Ablation runner — 每个消融: old臂(该修复的父提交) vs new臂(112ccf3)。
判据全部确定性(grep/文件/exit code), 无 LLM 参与。
用法: python3 runner.py [--arm A1,A3,...] [--out result.json]
"""
import json
import os
import subprocess
import sys
import time

AB = "/tmp/abl"
BIN = f"{AB}/bin"
NEW = f"{BIN}/ductile-new"
DATA = f"{AB}/data"


def sh(cmd, timeout=120, env=None, cwd=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    p = subprocess.run(
        cmd, shell=True, capture_output=True, text=True,
        timeout=timeout, env=e, cwd=cwd)
    return p.returncode, p.stdout, p.stderr


def run_arm(binary, pipeline, extra_env=None, timeout=180):
    """隔离数据目录跑一臂; 返回 (exit, out, err)。"""
    run_data = f"{DATA}/{os.path.basename(pipeline)}-{os.path.basename(binary)}"
    sh(f"rm -rf {run_data} && mkdir -p {run_data}")
    env = {"DUCTILE_DATA": run_data}
    if extra_env:
        env.update(extra_env)
    t0 = time.time()
    code, out, err = sh(
        f"{binary} run {pipeline} abl",
        timeout=timeout, env=env)
    return code, out, err, time.time() - t0


# ── 各消融定义 ────────────────────────────────────────────

def probe_A1(binary):
    """foreach 依赖边: 消费proc名(acon)字典序<源(zsrc)。
    new=两项都迭代, 文件末项beta; old=炸 foreach source not found 无文件"""
    sh(f"rm -f {AB}/a1_out.txt")
    code, out, err, _t = run_arm(binary, f"{AB}/ab1.pipeline")
    content = ""
    if os.path.exists(f"{AB}/a1_out.txt"):
        content = open(f"{AB}/a1_out.txt").read().strip()
    ok = code == 0 and content == "beta"
    return dict(verdict="PASS" if ok else "FAIL",
                metric=content, unit="a1_out.txt内容(期望beta=两项都迭代)",
                detail=(err or out).strip().splitlines()[-1:], exit=code)


def probe_A2(binary):
    """foreach var: item 含双引号。
    new=末项has\"quote完整存活; old=引号项炸参数语法只剩plain"""
    sh(f"rm -f {AB}/a2_out.txt")
    code, out, err, _t = run_arm(binary, f"{AB}/ab2.pipeline")
    content = ""
    if os.path.exists(f"{AB}/a2_out.txt"):
        content = open(f"{AB}/a2_out.txt").read().strip()
    ok = code == 0 and content == 'has"quote'
    return dict(verdict="PASS" if ok else "FAIL",
                metric=content, unit='a2_out.txt内容(期望has"quote)',
                detail=(err or out).strip().splitlines()[-1:], exit=code)


def probe_A3(binary):
    """when 合取: @src.value != "NO-OP" && != "" 旧版静默永真。
    new=gated 不跑(无VERDICT输出); old=VERDICT:A3-RAN(放行=缺陷)"""
    code, out, err, _t = run_arm(binary, f"{AB}/ab3.pipeline")
    ran = "A3-RAN" in (out + err)
    return dict(verdict="PASS" if not ran else "FAIL",
                metric=0 if ran else 1, unit="0=缺陷放行 1=正确拦截",
                detail=(err or out).strip().splitlines()[-1:], exit=code)


def probe_A4(binary):
    """跨行 llm(): graph 判据——@src 依赖边。
    new=Edges含src→ask; old=continuation蒸发prompt→边消失(平行组两个孤立)"""
    code, out, err = sh(f"{binary} graph {AB}/ab4.pipeline")
    g = out + err
    import re
    m = re.search(r"Edges:\s*(\d+)", g)
    edges = int(m.group(1)) if m else -1
    # src 与 ask 出现在同一个平行组括号内 = 无依赖边; 分层 = 有边。
    # (只看 Parallel groups 区, 不误匹配 Critical path; 组内顺序/成员数无关)
    pg = g.split("Critical path")[0]
    same_group = any(
        "src" in grp and "ask" in grp
        for grp in re.findall(r"\[([^\]]*)\]", pg))
    ok = edges >= 1 and not same_group
    return dict(verdict="PASS" if ok else "FAIL",
                metric=edges, unit="graph边数(期望>=1且src/ask分层不同组)",
                detail=[l.strip() for l in g.splitlines() if "ask" in l][:2], exit=code)


def probe_A5(binary):
    """mcsm/cse_safe 禁熔合: 副作用脚本双 impl 同签名。
    new=graph 无 isomorphic_union; old=熔合(CSE aliases second→first,
    静态提取计划丢 second 节点)"""
    run_data = f"{DATA}/ab5-{os.path.basename(binary)}"
    sh(f"rm -rf {run_data} && mkdir -p {run_data} && rm -f {AB}/a5_count {AB}/a5.lock")
    # 脚本注册进隔离库
    sh(f"{binary} script attach {AB}/abl_echo5.sh", env={"DUCTILE_DATA": run_data})
    code, out, err = sh(
        f"{binary} graph {AB}/ab5.pipeline",
        timeout=60, env={"DUCTILE_DATA": run_data})
    g = out + err
    fused = "isomorphic_union" in g
    aliased = "second→first" in g or "second->first" in g
    ok = code == 0 and not fused and not aliased
    return dict(verdict="PASS" if ok else "FAIL",
                metric=1 if fused else 0, unit="0=无熔合 1=副作用impl被熔合",
                detail=[l.strip() for l in g.splitlines()
                        if "Fusion" in l or "CSE" in l or "class" in l][:3], exit=code)


PROBES = {"A1": probe_A1, "A2": probe_A2, "A3": probe_A3,
          "A4": probe_A4, "A5": probe_A5}
ARMS = {
    "A1": ("6329e1d", "v0.18.8 foreach依赖边"),
    "A2": ("1b292e4", "v0.18.9 foreach var运行时化"),
    "A3": ("babff00", "v0.18.13 when合取"),
    "A4": ("6c276dd", "v0.18.14 跨行llm"),
    "A5": ("babff00", "v0.18.13 mcsm/cse_safe禁熔合"),
}


def main():
    only = None
    if "--arm" in sys.argv:
        only = set(sys.argv[sys.argv.index("--arm") + 1].split(","))
    sh(f"mkdir -p {BIN} {DATA}")
    results = {}
    for name, probe in PROBES.items():
        if only and name not in only:
            continue
        old_sha, title = ARMS[name]
        old_b = f"{BIN}/ductile-{old_sha}"
        r = {}
        for label, b in (("old", old_b), ("new", NEW)):
            if not os.path.exists(b):
                r[label] = dict(verdict="MISSING-BINARY", binary=b)
                continue
            try:
                r[label] = probe(b)
            except Exception as ex:
                r[label] = dict(verdict="ERROR", error=str(ex))
        # 消融结论: old FAIL + new PASS = 修复有效(可观察行为差异)
        old_ok = r.get("old", {}).get("verdict") == "PASS"
        new_ok = r.get("new", {}).get("verdict") == "PASS"
        r["ablation"] = ("DIFF-CONFIRMED" if (not old_ok and new_ok)
                         else "NO-DIFF" if (old_ok and new_ok)
                         else "REGRESSION" if (old_ok and not new_ok)
                         else "BOTH-FAIL")
        r["title"] = title
        results[name] = r
        print(f"[{name}] {title}: old={r['old'].get('verdict')} "
              f"new={r['new'].get('verdict')} => {r['ablation']}")
    out_path = f"{AB}/result.json"
    if "--out" in sys.argv:
        out_path = sys.argv[sys.argv.index("--out") + 1]
    with open(out_path, "w") as f:
        json.dump(results, f, ensure_ascii=False, indent=2)
    print(f"saved -> {out_path}")


if __name__ == "__main__":
    main()
