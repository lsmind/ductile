#!/usr/bin/env python3
"""Ductile LLM bridge — OpenAI-compatible chat → stdout (+ optional ##DSL_RESULT).

Config precedence (highest first):
  1. CLI flags (--model / --prompt / ...)
  2. Process env OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL / OPENAI_TIMEOUT_SECS
  3. config.toml [llm]  (DUCTILE_CONFIG, ./config.toml, ./ductile.toml, ~/.config/ductile/...)
  4. Built-in defaults

CLI:
  python llm_bridge.py --prompt TEXT [--model M] [--system S] [--schema keys] [--template T]
  python llm_bridge.py --self-test
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import os
import re
import sys
import urllib.error
import urllib.request


def env(name: str, default: str = "") -> str:
    return os.environ.get(name, default).strip()


def _strip_comment(line: str) -> str:
    out = []
    in_s = in_d = False
    for c in line:
        if c == '"' and not in_s:
            in_d = not in_d
            out.append(c)
            continue
        if c == "'" and not in_d:
            in_s = not in_s
            out.append(c)
            continue
        if c == "#" and not in_s and not in_d:
            break
        out.append(c)
    return "".join(out)


def _parse_value(raw: str) -> str:
    raw = raw.strip()
    if not raw:
        return ""
    if (raw[0] == raw[-1] == '"') or (raw[0] == raw[-1] == "'"):
        return raw[1:-1].replace("\\\"", '"').replace("\\\\", "\\")
    return raw


def parse_toml_sections(text: str) -> dict[str, dict[str, str]]:
    sections: dict[str, dict[str, str]] = {}
    current = ""
    for raw in text.splitlines():
        line = _strip_comment(raw).strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1].strip()
            sections.setdefault(current, {})
            continue
        if "=" not in line:
            continue
        k, _, v = line.partition("=")
        key = k.strip()
        if not key:
            continue
        sections.setdefault(current, {})[key] = _parse_value(v)
    return sections


def config_search_paths() -> list[str]:
    paths = []
    override = env("DUCTILE_CONFIG")
    if override:
        paths.append(override)
    paths.extend(["config.toml", "ductile.toml"])
    home = env("HOME") or env("USERPROFILE")
    if home:
        paths.append(os.path.join(home, ".config", "ductile", "config.toml"))
        paths.append(os.path.join(home, ".local", "share", "ductile", "config.toml"))
    return paths


def load_llm_config() -> dict[str, str]:
    """Return flat llm settings with defaults applied."""
    cfg = {
        "base_url": "https://api.openai.com/v1",
        "api_key": "",
        "model": "gpt-4o-mini",
        "timeout_secs": "120",
    }
    path = next((p for p in config_search_paths() if os.path.isfile(p)), None)
    if path:
        try:
            with open(path, encoding="utf-8") as f:
                sections = parse_toml_sections(f.read())
        except OSError:
            sections = {}
        llm = sections.get("llm", {})
        if llm.get("base_url") or llm.get("api_base") or llm.get("url"):
            cfg["base_url"] = llm.get("base_url") or llm.get("api_base") or llm.get("url")
        if llm.get("api_key") or llm.get("key"):
            cfg["api_key"] = llm.get("api_key") or llm.get("key")
        if llm.get("model") or llm.get("default_model"):
            cfg["model"] = llm.get("model") or llm.get("default_model")
        if llm.get("timeout_secs") or llm.get("timeout"):
            cfg["timeout_secs"] = llm.get("timeout_secs") or llm.get("timeout")
        cfg["_path"] = path
    return cfg


def resolve_settings(args: argparse.Namespace) -> tuple[str, str, str, int]:
    file_cfg = load_llm_config()
    base = env("OPENAI_BASE_URL") or file_cfg["base_url"]
    key = env("OPENAI_API_KEY") or file_cfg["api_key"]
    model = (args.model or "").strip() or env("OPENAI_MODEL") or file_cfg["model"]
    timeout = int(env("OPENAI_TIMEOUT_SECS") or file_cfg["timeout_secs"] or "120")
    return base, key, model, timeout


def chat_completions(base: str, key: str, model: str, messages: list, timeout: int = 120) -> tuple[str, dict]:
    url = base.rstrip("/") + "/chat/completions"
    payload: dict = {
        "model": model,
        "messages": messages,
        "temperature": 0.2,
    }
    max_tokens = env("OPENAI_MAX_TOKENS")
    if max_tokens.isdigit():
        payload["max_tokens"] = int(max_tokens)
    # 思考型模型压制：ollama OpenAI 端点支持 reasoning_effort（实测 ornith-1.5:35b
    # 在长材料上思考吞掉全部 max_tokens 预算 → content 空 → "no JSON" 确定性失败，
    # 三连重试全败；reasoning_effort=none 后 0s 出活）。OPENAI_REASONING_EFFORT
    # 可设 "keep" 显式保留思考（思考增益场景）。
    effort = env("OPENAI_REASONING_EFFORT")
    if effort and effort != "keep":
        payload["reasoning_effort"] = effort
    elif not effort:
        payload["reasoning_effort"] = "none"
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {key}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        err = e.read().decode("utf-8", errors="replace")[:500]
        raise SystemExit(f"llm http {e.code}: {err}") from e
    except urllib.error.URLError as e:
        raise SystemExit(f"llm network error: {e}") from e
    try:
        choice = data["choices"][0]
        content = choice["message"]["content"]
    except (KeyError, IndexError, TypeError) as e:
        raise SystemExit(f"llm bad response shape: {data!r}") from e
    # L0.5 溯源 META（cognition spec A1/A2）：桥丢掉的恰恰是归因需要的证据。
    # finish_reason=length ⇒ 尾字段静默截断（score=10 实为 100 事故）；
    # bridge_hash ⇒ 旧桥劫持检测（--prompt 被当字面量事故）。
    meta = {
        "finish_reason": choice.get("finish_reason") or "",
        "model_id": data.get("model") or model,
        "prompt_tokens": data.get("usage", {}).get("prompt_tokens", 0),
        "completion_tokens": data.get("usage", {}).get("completion_tokens", 0),
    }
    return content, meta


def build_messages(prompt: str, system: str, template: str, schema: str | None) -> list:
    sys_parts = []
    if system:
        sys_parts.append(system)
    if template:
        sys_parts.append(f"Style/template hint: {template}")
    if schema:
        # T17（规格 §五.3/五.4 TOON-CLOSED-1）：契约卡只教 TOON——
        # 无 JSON 对照、无 pretty 样例泄漏。closed 裁判=ductile toon（Rust 本尊）。
        keys = [k.strip() for k in schema.split(",") if k.strip()]
        example = "\n".join(f'{k}: "..."' for k in keys)
        sys_parts.append(
            "Extract structured data. Output ONLY a TOON document (no markdown, no code fences, no explanations):\n"
            + example
            + "\nRules: two-space indent for nested objects; one entry per line as `key: value`; "
            "quote strings that contain spaces or punctuation; numbers bare; booleans true/false; null. "
            "No extra keys."
        )
    messages = []
    if sys_parts:
        messages.append({"role": "system", "content": "\n".join(sys_parts)})
    messages.append({"role": "user", "content": prompt})
    return messages


def bridge_hash() -> str:
    """本文件 sha256 前 12 位——L0.5 溯源：实际执行的是哪个桥。"""
    with open(__file__, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()[:12]


def emit_dsl_result(fields: dict, raw: str, meta: dict | None = None) -> None:
    print(raw.rstrip())
    print()
    print("##DSL_RESULT")
    print("ok=1")
    for k, v in fields.items():
        if k == "ok":
            continue
        val = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
        print(f"{k}={val}")
    if meta is not None:
        # META 块（cognition spec §7 P0）：finish_reason/usage/bridge_hash/model_id
        print(f"meta_finish_reason={meta.get('finish_reason', '')}")
        print(f"meta_model_id={meta.get('model_id', '')}")
        print(f"meta_prompt_tokens={meta.get('prompt_tokens', 0)}")
        print(f"meta_completion_tokens={meta.get('completion_tokens', 0)}")
        print(f"meta_bridge_hash={bridge_hash()}")
    print("##DSL_END")


def _ductile_toon_bin() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    cand = os.path.join(here, "..", "target", "release", "ductile")
    if os.path.exists(cand):
        return os.path.abspath(cand)
    return shutil.which("ductile") or ""


def _toon_table_row(line: str, cols: list[str]) -> list[str]:
    """表行 `  v1,v2` → 值列表（TOON 表行值逗号后禁空格，见 toon 裁判）。"""
    return line.strip().split(",")


def _toon_judge(text: str, schema: str = ""):
    """ductile toon = closed parser 单一裁判源。schema 非空 → 传 --schema 白名单
    （未知字段 E461 拒，规格 §五.4「只输出 schema 允许字段」）。

    字段提取（§四.5 内部传输统一）：
    - 标量键 `k: v` → fields[k]=v
    - 表键 `k[N]{c1,c2}:` → fields[k] = JSON 行数组（读侧 parse_missing_json
      本就找 [..] JSON 兼容；单值列也成 dict——列名即键）。"""
    bin_ = _ductile_toon_bin()
    if not bin_:
        return None, "ductile binary not found (closed judge unavailable)"
    cmd = [bin_, "toon"]
    if schema:
        cmd += ["--schema", schema]
    try:
        p = subprocess.run(
            cmd, input=text.encode("utf-8"),
            capture_output=True, timeout=30,
        )
    except Exception as e:  # noqa: BLE001
        return None, f"judge subprocess failed: {e}"
    if p.returncode != 0:
        return None, p.stderr.decode("utf-8", "replace").strip()
    canonical = p.stdout.decode("utf-8")
    fields: dict[str, str] = {}
    pending_cols: dict[str, list[str]] = {}
    rows: dict[str, list[dict]] = {}
    for line in canonical.split("\n"):
        if not line:
            continue
        t = re.match(r'^(\"?)([^\"\[\]:]+)\1\[(\d+)\]\{([^}]*)\}:\s*$', line)
        if t:
            tkey = t.group(2)
            cols = [c.strip() for c in t.group(4).split(",") if c.strip()]
            pending_cols.setdefault(tkey, cols)
            rows.setdefault(tkey, [])
            continue
        if line.startswith("  "):
            for tkey in list(rows):
                if tkey in pending_cols:
                    vals = _toon_table_row(line, pending_cols[tkey])
                    rows[tkey].append(dict(zip(pending_cols[tkey], vals)))
            continue
        m = re.match(r'^("?)([^":]+)\1: (.*)$', line)
        if m:
            fields[m.group(2)] = m.group(3)
    for tkey, rlist in rows.items():
        fields[tkey] = json.dumps(rlist, ensure_ascii=False)
    return fields, canonical


def closed_toon_fields(content: str, schema: str = ""):
    """T17 闭环：首判 →（拒则）一次修复 → 终判。fail-closed。"""
    text = content.strip() + "\n"
    fields, canonical = _toon_judge(text, schema)
    if fields is not None:
        return fields, canonical, False
    err = canonical or "rejected"
    repaired_text = re.sub(r"^```[a-zA-Z]*\s*", "", content)
    repaired_text = re.sub(r"\s*```$", "", repaired_text)
    repaired_text = repaired_text.strip() + "\n"
    if repaired_text == text:
        return None, err, True
    fields2, canonical2 = _toon_judge(repaired_text, schema)
    if fields2 is not None:
        return fields2, canonical2, True
    return None, err, True


def parse_args(argv: list[str]) -> argparse.Namespace:
    if argv and not argv[0].startswith("-"):
        return argparse.Namespace(
            prompt=argv[0],
            template=argv[1] if len(argv) > 1 else "",
            count=argv[2] if len(argv) > 2 else "5",
            model="",
            system="",
            schema="",
        )
    ap = argparse.ArgumentParser(description="Ductile OpenAI-compatible LLM bridge")
    ap.add_argument("--prompt", required=True)
    ap.add_argument("--model", default="")
    ap.add_argument("--system", default="")
    ap.add_argument("--template", default="")
    ap.add_argument("--schema", default="", help="comma-separated key list describing output fields (TOON closed output)")
    ap.add_argument("--count", default="5", help="legacy unused hint")
    return ap.parse_args(argv)


def _self_test() -> None:
    f, canon, rep = closed_toon_fields('title: "x"\ncount: 2\n')
    assert f is not None and f["title"] == "x" and f["count"] == "2", f
    f2, canon2, rep2 = closed_toon_fields('```\ntitle: "y"\n```')
    assert f2 is not None and rep2 is True, (f2, rep2)
    f3, _, _ = closed_toon_fields("total garbage no colons")
    assert f3 is None
    # 未知字段：无 schema 过；带 schema E461 拒（fail-closed）
    f4, _, _ = closed_toon_fields('title: x\nextra: 1\n', schema="title,count")
    assert f4 is None, f4
    f5, _, _ = closed_toon_fields('title: x\ncount: 2\n', schema="title,count")
    assert f5 is not None and f5["count"] == "2", f5
    msgs = build_messages("hi", "sys", "tmpl", "title,url")
    assert msgs[0]["role"] == "system" and 'title: "..."' in msgs[0]["content"] and "JSON" not in msgs[0]["content"]
    assert msgs[1] == {"role": "user", "content": "hi"}
    secs = parse_toml_sections(
        '[llm]\nbase_url = "http://x/v1"\napi_key = "k"\nmodel = \'m\'\n# c\n'
    )
    assert secs["llm"]["base_url"] == "http://x/v1"
    assert secs["llm"]["api_key"] == "k"
    assert secs["llm"]["model"] == "m"
    print("llm_bridge self-test ok")


def main(argv: list[str] | None = None) -> int:
    args = parse_args(list(argv if argv is not None else sys.argv[1:]))
    prompt = (args.prompt or "").strip()
    if not prompt:
        print("llm bridge: empty prompt", file=sys.stderr)
        return 2
    base, key, model, timeout = resolve_settings(args)
    if not key:
        print(
            "llm bridge: no api_key (set OPENAI_API_KEY or config.toml [llm] api_key=...)",
            file=sys.stderr,
        )
        return 2
    schema = (args.schema or "").strip() or None
    messages = build_messages(prompt, args.system or "", args.template or "", schema)
    content, meta = chat_completions(base, key, model, messages, timeout=timeout)
    if schema:
        # T17（规格 §五.4）：closed parser 裁判=Rust 本尊（ductile toon）。
        # 无 constrained decoding 供应商 → 最多修复一次；修复后仍失败 fail-closed。
        fields, raw_canonical, repaired = closed_toon_fields(content, schema=args.schema or "")
        if fields is None:
            print(content)
            print(
                "llm bridge: schema requested but model output failed closed TOON parse (one repair attempted)",
                file=sys.stderr,
            )
            return 1
        emit_dsl_result(fields, raw_canonical, meta=meta)
        if repaired:
            print("meta_repaired=1", flush=True)
    else:
        print(content.rstrip())
        print()
        print("##DSL_RESULT")
        print("ok=1")
        print(f"meta_finish_reason={meta.get('finish_reason', '')}")
        print(f"meta_model_id={meta.get('model_id', '')}")
        print(f"meta_prompt_tokens={meta.get('prompt_tokens', 0)}")
        print(f"meta_completion_tokens={meta.get('completion_tokens', 0)}")
        print(f"meta_bridge_hash={bridge_hash()}")
        print("##DSL_END")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        _self_test()
        raise SystemExit(0)
    raise SystemExit(main())
