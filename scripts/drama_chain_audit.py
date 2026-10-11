#!/usr/bin/env python3
# ductile: v1
# args: env!
# name: drama_chain_audit
# desc: 漫剧三道门禁机审：门1 剧本⊆逻辑链 + 门2 提示词⊆剧本（台词/VO/画面文字逐句）
# lang: python
# params: ep(str, default=EP001)
# output: gates_ok(int), gate1_fail(int), gate2_fail(int)
# pure: false
# idempotent: true
# concurrency: safe
# effects: none
# timeout: 120
"""drama_chain_audit.py — 漫剧三道门禁机审（drama.pipeline 的 script 契约实现）

门1 剧本⊆链 + 门2 提示词⊆剧本。EP 由 DUCTILE_ARG_EP / DUCTILE_TOPIC / argv 传入，缺省 EP001。
退出码：0=全过；1=FAIL（stdout 列缺失项）。
"""
import re
import sys
import os

def env_arg(name, default):
    v = os.environ.get(f"DUCTILE_ARG_{name.upper()}") or default
    if "--ep=" in " ".join(sys.argv):
        v = [a.split("=", 1)[1] for a in sys.argv if a.startswith("--ep=")][0]
    return v

EP = env_arg("ep", "EP001")
SIGOU = os.environ.get("SIGOU_ROOT", "/home/beef/projects/ai/sigou")
base = os.path.join(SIGOU, "season1", "剧集", EP)

def read(name):
    with open(os.path.join(base, name), encoding="utf-8") as f:
        return f.read()

def read_any(*names):
    for n in names:
        if os.path.exists(os.path.join(base, n)):
            return n, read(n)
    raise FileNotFoundError(names)

strip = lambda s: re.sub(r"[，。！？…—、\s“”\"：:]", "", s)

def gate1(chain, script):
    rows = re.findall(r"\| (L\d+) \| ([^|]+) \|", chain)
    fail = []
    for node, carrier in rows:
        for anchor in re.findall(r"[“\"]([^”\"]{4,})[”\"]", carrier):
            if strip(anchor) not in strip(script):
                fail.append(f"{node}: 载体缺失 [{anchor}]")
    return rows, fail

def gate2(script, vp):
    vpc = strip(vp)
    fail = []
    for l in script.splitlines():
        l = l.strip()
        if "：" not in l:
            continue
        head = l.split("：", 1)[0]
        if not (l.startswith("[VO]") or ("）" in head and head.rstrip("）").endswith(")")) or not head):
            if not (l.startswith("[VO]") or "）" in head):
                continue
        quote = l.split("：", 1)[1].strip()
        if not quote or quote.startswith("["):
            continue
        core = strip(quote)
        anchor = core[:12]
        if anchor and anchor not in vpc:
            fail.append(f"台词缺失: [{quote[:24]}…]")
    def _nums(t):
        t = strip(t)
        for k, v in {"一":"1","二":"2","三":"3","四":"4","五":"5","六":"6","七":"7","八":"8","九":"9","〇":"0","零":"0"}.items():
            t = t.replace(k, v)
        return re.sub(r"[^0-9]", "", t)
    vpnum = _nums(vp)
    for pic in re.findall(r"\[画面文字\][^\n]*", script):
        for frag in re.findall(r"[“\"]([^”\"]{2,})[”\"]", pic):
            if strip(frag) in vpc:
                continue
            fn = _nums(frag)
            if not (fn and fn in vpnum):
                fail.append(f"画面文字缺失: [{frag}]")
    # 数字/点号归一: 「1987年9月」≈「一九八七·九月」不做翻译对账——画面文字账以数字块归一后必须命中
    def _n(t):
        t = strip(t)
        cn = {"０":"0","１":"1","２":"2","３":"3","４":"4","５":"5","６":"6","７":"7","８":"8","９":"9"}
        t = "".join(cn.get(c, c) for c in t)
        for k, v in {"一":"1","二":"2","三":"3","四":"4","五":"5","六":"6","七":"7","八":"8","九":"9","〇":"0","零":"0"}.items():
            t = t.replace(k, v)
        return re.sub(r"[^0-9]", "", t)
    for frag in re.findall(r"「([^」]{2,})」", script):
        nums = _n(frag)
        if nums and nums not in re.sub(r"[^0-9]", "", vp):
            fail.append(f"画面文字数字账缺失: [{frag}]")
    return fail

def main():
    chain = read("逻辑链.md")
    _, script = read_any("秒表剧本.md", "剧本.md")
    _, vp = read_any("生成提示词.md", "视频提示词.md")
    # v13 声列制: (台词|VO) 人（…）：“…” → 归一成 'head：quote' 行供 gate2
    if "秒表剧本" in _ or "镜段" in script:
        norm = []
        for m in re.finditer(r"(?:台词|VO) \S+?（[^）]*）：“(.+?)”", script):
            norm.append(f"甲（语气）：{m.group(1)}")
        norm += [f"[画面文字]“{f}”" for f in re.findall(r"「([^」]{2,})」", script)]
        script = "\n".join(norm)
    rows, f1 = gate1(chain, script)
    f2 = gate2(script, vp)
    ok = True
    if f1:
        ok = False
        print("门1 FAIL (剧本⊆链):")
        for f in f1:
            print("  ", f)
    else:
        print(f"门1 PASS: {len(rows)} 链节点载体核对（动作类留人审）")
    if f2:
        ok = False
        print("门2 FAIL (提示词⊆剧本):")
        for f in f2:
            print("  ", f)
    else:
        print("门2 PASS: 台词/VO/画面文字逐句核对（道具动作/SFX/连续性留人审）")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
