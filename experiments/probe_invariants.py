#!/usr/bin/env python3
# probe_invariants.py — brk 产物结构不变量检查器（评估函数的机器层）
# 「有没有错」机器判：全部确定性检查，零 LLM。输出 ##DSL_RESULT 供 .when 门禁用。
# 判据全部来自 c183-c190 盲评实锤的死因：票数锚/字段完整/desc 负重/est 对账/
# 依赖单向/owner 白名单。LLM 算不对的（est 连加），机器必须算对。
import json, re, sys

TB = "/tmp/regchain_data"

def parse_brk(path):
    raw = open(path).read()
    if "##DSL_RESULT" in raw:
        raw = raw.split("##DSL_RESULT")[0]
    if raw.startswith("§§FIELDS§§"):
        raw = raw[len("§§FIELDS§§"):]
    out = {}
    for part in raw.split("§§"):
        if part and "=" in part:
            k, v = part.split("=", 1)
            v = v.strip()
            if v.startswith(("[", "{")):
                cand = v
                if k == "deps" and not v.startswith("{"):
                    cand = "{" + v          # deps 形态 "T002": [...] 首花括号被 §§ 切走
                try:
                    v = json.loads(cand)
                except Exception:
                    pass
            out[k] = v
    return out

def check(path):
    d = parse_brk(path)
    tickets = d.get("tickets")
    issues = []
    if not isinstance(tickets, list) or not tickets or not isinstance(tickets[0], dict):
        issues.append("FATAL:tickets 非对象数组")
        emit(0, issues, 0, 0, 0, 0)
        return
    n = len(tickets)
    # I1 票数锚（c188 实锤：无锚 9B 出 7 票，锚 15-25 后 20+）
    if n < 12:
        issues.append(f"tickets={n} <12 拆解过粗")
    # I2 六字段完整
    miss_p = [x.get("id","?") for x in tickets if not all(x.get(f) not in (None,"","<无>") for f in ("id","title","desc","priority","est","owner"))]
    if miss_p:
        issues.append(f"字段缺失:{','.join(miss_p[:6])}")
    # I3 desc 负重（太短=没内容，太长=挤占）
    descs = [len(str(x.get("desc","") or "")) for x in tickets]
    short = [tickets[i].get("id") for i, L in enumerate(descs) if L < 15]
    if short:
        issues.append(f"desc过短(<15字):{','.join(short[:6])}")
    # I4 est 对账（9B/27B 都算不对连加——机器算）
    bad_est = []
    s = 0.0
    for x in tickets:
        try:
            s += float(x.get("est", 0) or 0)
        except (TypeError, ValueError):
            bad_est.append(x.get("id","?"))
    if bad_est:
        issues.append(f"est非数值:{','.join(bad_est[:6])}")
    tot = d.get("total_est")
    try:
        tot_v = float(tot)
    except (TypeError, ValueError):
        tot_v = None
    if tot_v is None or abs(tot_v - s) > 0.05:
        issues.append(f"est对账失败:total_est={tot}!=票面{s}")
    # I5 依赖单向（c189 实锤：T015<-T016 倒置被裁判点名）
    deps = d.get("deps")
    inv = []
    if isinstance(deps, dict):
        for tid, ds in deps.items():
            m = re.search(r"(\d+)", str(tid))
            if not m:
                continue
            n_ = int(m.group(1))
            for x in (ds or []):
                m2 = re.search(r"(\d+)", str(x))
                if m2 and int(m2.group(1)) >= n_:
                    inv.append(f"{tid}<-{x}")
        if inv:
            issues.append(f"依赖倒置:{','.join(inv[:6])}")
    # I6 owner 白名单（英文角色/Engineer 类=c183 死因）
    WL = ("策划","美术","外部合约")
    bad_o = [x.get("id","?") for x in tickets
             if not any(w in str(x.get("owner","")) for w in WL)]
    if bad_o:
        issues.append(f"owner白名单外:{','.join(bad_o[:8])}")
    # I7 owner 到人（c189 裁判点名『策划A-E』区间指代模糊）
    range_o = [x.get("id","?") for x in tickets
               if re.search(r"策划\s*[A-Z]\s*-\s*[A-Z]", str(x.get("owner","")))]
    if range_o:
        issues.append(f"owner区间指代:{','.join(range_o[:6])}")
    score = 100 - 12 * len(issues)
    emit(max(0, score), issues, n, s, int(bool(descs and sum(descs)/len(descs) >= 25)),
         sum(1 for x in tickets if any(w in str(x.get("owner","")) for w in ("策划","美术"))))

def emit(score, issues, n, est_sum, desc_ok, internal):
    fields = [
        f"probe_score={score}",
        f"issues_count={len(issues)}",
        f"tickets_n={n}",
        f"est_sum={est_sum}",
        f"desc_ok={desc_ok}",
        f"internal_owner_n={internal}",
        "issues=" + json.dumps(issues, ensure_ascii=False),
    ]
    print("##DSL_RESULT")
    print("\n".join(fields))
    print("##DSL_END")

if __name__ == "__main__":
    check(sys.argv[1] if len(sys.argv) > 1 else f"{TB}/g9d5_brk.txt")
