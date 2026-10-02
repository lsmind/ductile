#!/usr/bin/env python3
"""T18 LLM 端到端发布门运行器（规格 §五.6 / 硬门14）。

240 卡 ×3 次 = 720 跑。门限：
  - 首判 closed-parse 成功 ≥ 706/720 (98%)
  - 终判（一次修复后）schema+语义联合成功 ≥ 713/720 (99%)
  - 任一类语义成功 ≥ 86/90 (95%)
  - 相对 JSON 基线语义成功率降幅 ≤ 1 个百分点（原始计数）
  - 中位输入 token 降幅 ≥ 15%

判定链（与生产 bridge 完全同构）：model 输出 → closed parse（ductile toon）
→ schema 白名单（--schema）→ 语义 oracle（expect_* 值相等）。
输出：结果 JSONL（legacy 例外区）+ 终局摘要 stdout。
"""
import json
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BIN = REPO / "target" / "release" / "ductile"
CORPUS = REPO / "tests" / "fixtures" / "t18_corpus"
OUTDIR = REPO / "tests" / "fixtures" / "legacy_v1" / "t18_baseline"
RUNS = 3

# LLM 供应商：ollama qwen3.8:27b（OpenAI 兼容 /v1）
import urllib.request

BASE = "http://localhost:11434/v1"
MODEL = "qwen3.8:27b"

SYSTEM_TOON = "每行k: v，仅限:{schema}"
def system_json(schema_str: str) -> str:
    """P0 冻结基线卡：迁移前生产 JSON 契约卡（git 34d5d29^ bridge 原文，字面冻结）。"""
    return (
        "Extract structured data from the user text. Reply with a single JSON "
        "object only (no markdown). Required keys: " + schema_str
    )
def chat(messages: list, temperature: float = 0.0) -> tuple[str, dict]:
    body = json.dumps({
        "model": MODEL, "messages": messages,
        "temperature": temperature, "max_tokens": 512,
        "reasoning_effort": "none",
    }).encode()
    req = urllib.request.Request(
        BASE + "/chat/completions", data=body,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=180) as r:
        d = json.loads(r.read().decode())
    content = d["choices"][0]["message"]["content"] or ""
    usage = d.get("usage", {})
    return content, usage


def judge(text: str, schema: str) -> tuple[dict | None, str]:
    """ductile toon --schema：closed parse + 白名单。返回 (fields|None, err)。"""
    p = subprocess.run(
        [str(BIN), "toon", "--schema", schema],
        input=text.encode(), capture_output=True, timeout=30,
    )
    if p.returncode != 0:
        return None, p.stderr.decode("utf-8", "replace").strip()
    fields = {}
    for line in p.stdout.decode().split("\n"):
        if not line or line.startswith("  "):
            continue
        if ": " in line:
            k, v = line.split(": ", 1)
            fields[k] = v
        print("DBG arg:", repr(schema[:200]), file=_sys.stderr)
    return fields, ""


def unquote(v: str) -> str:
    if len(v) >= 2 and v[0] == '"' and v[-1] == '"':
        return v[1:-1]
    return v


def semantic_eq(model_val: str, expect: str) -> bool:
    return unquote(model_val.strip()) == unquote(expect.strip())


def repair_once(content: str) -> str:
    t = content.strip()
    if t.startswith("```"):
        t = t.strip("`")
        # 剥语言标注
        if t.startswith("toon"):
            t = t[4:]
        t = t.strip()
    return t


def run_card(card: dict, mode: str) -> dict:
    """mode: toon | json。返回单跑结果记录。"""
    schema_keys = card["schema_keys"]
    schema_str = ",".join(schema_keys)
    if mode == "toon":
        system = SYSTEM_TOON.format(schema=schema_str)
    else:
        system = system_json(schema_str)
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": card["input"]},
    ]
    t0 = time.time()
    try:
        content, usage = chat(messages)
    except Exception as e:  # noqa: BLE001
        return {"ok": False, "stage": "llm", "err": str(e), "elapsed": time.time() - t0}
    elapsed = time.time() - t0
    prompt_tokens = usage.get("prompt_tokens", 0)

    if mode == "json":
        # JSON 基线：宽松解析（json.loads + 围栏剥离）——这正是被替换的旧路径
        txt = content.strip()
        if txt.startswith("```"):
            txt = txt.strip("`").strip()
        try:
            obj = json.loads(txt)
            if isinstance(obj, dict):
                fields = {k: (v if isinstance(v, str) else json.dumps(v, ensure_ascii=False))
                          for k, v in obj.items()}
                ok, bad = True, []
                for k in schema_keys:
                    if not semantic_eq(fields.get(k, ""), card["expects"][k]):
                        ok = False
                        bad.append(k)
                return {"ok": ok, "stage": "semantic", "bad": bad,
                        "elapsed": elapsed, "prompt_tokens": prompt_tokens}
        except json.JSONDecodeError:
            pass
        return {"ok": False, "stage": "parse", "elapsed": elapsed, "prompt_tokens": prompt_tokens}

    # TOON 首判
    text = content.strip() + "\n"
    fields, err = judge(text, schema_str)
    first_pass = fields is not None
    repaired = False
    if not first_pass:
        rt = repair_once(content)
        if rt + "\n" != text:
            fields, err = judge(rt + "\n", schema_str)
            repaired = True
    if fields is None:
        return {"ok": False, "stage": "parse", "err": err, "first_pass": first_pass,
                "repaired": repaired, "elapsed": elapsed, "prompt_tokens": prompt_tokens}
    # 语义 oracle
    ok, bad = True, []
    for k in schema_keys:
        if not semantic_eq(fields.get(k, ""), card["expects"][k]):
            ok = False
            bad.append(k)
    return {"ok": ok, "stage": "semantic", "bad": bad, "first_pass": first_pass,
            "repaired": repaired, "elapsed": elapsed, "prompt_tokens": prompt_tokens}


def load_cards() -> list[dict]:
    cards = []
    for f in sorted(CORPUS.glob("*.toon")):
        # 卡自带 expect_* oracle 键——白名单需容其入（首扫取键名，二扫判值）
        raw = f.read_text()
        expect_keys = "," + ",".join(
            l.split(":", 1)[0].strip() for l in raw.splitlines() if l.startswith("expect_")
        )
        fields, err = judge(raw, "class,card_id,schema,input" + expect_keys)
        if fields is None:
            raise SystemExit(f"corpus card broken: {f.name}: {err}")
        schema_keys = unquote(fields["schema"]).split(",")
        expects = {k: unquote(v) for k, v in fields.items() if k.startswith("expect_")}
        cards.append({
            "class": unquote(fields["class"]), "card_id": unquote(fields["card_id"]),
            "schema_keys": schema_keys, "input": unquote(fields["input"]),
            "expects": {k[len("expect_"):]: v for k, v in expects.items()},
        })
    return cards


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "toon"
    limit = int(sys.argv[2]) if len(sys.argv) > 2 else 0  # 0=all（烟测传 8）
    cards = load_cards()
    if limit:
        by_class: dict[str, list[dict]] = {}
        for c in cards:
            by_class.setdefault(c["class"], []).append(c)
        take = max(1, limit // len(by_class))
        cards = [c for cls in sorted(by_class) for c in by_class[cls][:take]]
    OUTDIR.mkdir(parents=True, exist_ok=True)
    out_path = OUTDIR / f"runs_{mode}{'_' + str(limit) if limit else ''}.jsonl"
    n_first, n_final, n_sem = 0, 0, 0
    per_class: dict[str, list[int]] = {}
    tokens: list[int] = []
    with out_path.open("w") as fh:
        for card in cards:
            for r_i in range(RUNS):
                rec = run_card(card, mode)
                rec.update({"card_id": card["card_id"], "class": card["class"],
                            "run": r_i + 1, "mode": mode})
                fh.write(json.dumps(rec, ensure_ascii=False) + "\n")
                fh.flush()
                if rec["ok"]:
                    n_final += 1
                    n_sem += 1
                elif rec.get("first_pass"):
                    # 首判过但语义败——终判败
                    pass
                if rec.get("first_pass"):
                    n_first += 1
                pc = per_class.setdefault(card["class"], [0, 0, 0])
                pc[2] += 1
                if rec.get("first_pass"):
                    pc[0] += 1
                if rec["ok"]:
                    pc[1] += 1
                if rec.get("prompt_tokens"):
                    tokens.append(rec["prompt_tokens"])
    total = len(cards) * RUNS
    print(f"MODE={mode} cards={len(cards)} runs={total}")
    print(f"first_pass={n_first} ({n_first / total:.1%})  final_ok={n_final} ({n_final / total:.1%})")
    for cls in sorted(per_class):
        f_, o_, t_ = per_class[cls]
        print(f"  {cls}: first {f_}/{t_}  ok {o_}/{t_}")
    if tokens:
        print(f"median_prompt_tokens={statistics.median(tokens)}")
    print(f"results: {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
