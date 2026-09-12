#!/usr/bin/env python3
# rx_apply.py — 处方执行器：doctor 输出 → toml 配置写回（v0.18.3 闭环最后一块）
# 语义：target+action+old/new。replace=find(old)→swap；append=段尾追加；remove=find(old)→删。
# 兼容 old/new 与 old_fragment/new_fragment 两种键名。TOML 单行串安全转义（\\n → \n）。
# LLM 输出不进 bash（SPEC §13.5）——本脚本被 run() 调用，参数全是文件路径，合规。
import json, sys, re

def rx_load(path):
    raw = open(path).read()
    body = raw[raw.find("§§RAW§§")+8:] if "§§RAW§§" in raw else raw
    dec = json.JSONDecoder()
    pos, objs = 0, []
    while pos < len(body):
        while pos < len(body) and body[pos] not in "{[":
            pos += 1
        if pos >= len(body):
            break
        try:
            o, n = dec.raw_decode(body[pos:])
            objs.append(o); pos += n
        except Exception:
            break
    for o in objs:
        if isinstance(o, list) and o and isinstance(o[0], dict) and "action" in o[0]:
            return o
    return []

def toml_escape(s):
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")

def seg(conf, target):
    """定位 [agents.X] 段（target=guide → breaker 的 guide/system 键）"""
    m = re.search(r'(\[agents\.[\w.]+\])', conf)
    return conf  # 简化：单 agent 文件；多 agent 时按 target 路由

def apply(conf, rx):
    log = []
    for i, p in enumerate(rx):
        act = p.get("action", "append")
        tgt = p.get("target", "guide")
        old = p.get("old") or p.get("old_fragment") or ""
        new = p.get("new") or p.get("new_fragment") or ""
        # 只动 [agents.breaker] 段内文本（处方 target=guide/system 都落在该段）
        key = "guide" if tgt == "guide" else "system"
        mk = re.search(r'(\[agents\.breaker\][^\n]*\n(?:[^\[]|\[\[)*?%s\s*=)' % key, conf)
        if not mk:
            log.append(f"R{i+1} SKIP: breaker {key} 键未找到")
            continue
        start = mk.start(1) + len(mk.group(1))
        vend = conf.find("\n[", start)
        vend = len(conf) if vend < 0 else vend
        val = conf[start:vend]
        if act == "replace" and old:
            if old not in val:
                log.append(f"R{i+1} SKIP: old 片段不在当前 {key}（可能已应用/漂移）")
                continue
            val = val.replace(old, toml_escape(new), 1)
            log.append(f"R{i+1} replace ok @breaker.{key}")
        elif act == "remove" and old:
            if old not in val:
                log.append(f"R{i+1} SKIP: old 片段不在")
                continue
            val = val.replace(old, "", 1)
            log.append(f"R{i+1} remove ok")
        else:  # append
            val = val.rstrip()
            if val.endswith('"'):
                val = val[:-1] + toml_escape("\n\n" + new) + '"'
            else:
                val = val + toml_escape("\n\n" + new)
            log.append(f"R{i+1} append ok @breaker.{key}")
        conf = conf[:start] + val + conf[vend:]
    return conf, log

if __name__ == "__main__":
    src, dst, rxp = sys.argv[1], sys.argv[2], sys.argv[3]
    rx = rx_load(rxp)
    if not rx:
        print("##DSL_RESULT")
        print("applied=0")
        print("issues=" + json.dumps(["no prescriptions parsed"], ensure_ascii=False))
        print("##DSL_END")
        sys.exit(0)
    conf = open(src).read()
    conf2, log = apply(conf, rx)
    open(dst, "w").write(conf2)
    print("##DSL_RESULT")
    print(f"applied={len(rx)}")
    print("log=" + json.dumps(log, ensure_ascii=False))
    print("##DSL_END")
