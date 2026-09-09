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
import json
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


def chat_completions(base: str, key: str, model: str, messages: list, timeout: int = 120) -> str:
    url = base.rstrip("/") + "/chat/completions"
    body = json.dumps(
        {
            "model": model,
            "messages": messages,
            "temperature": 0.2,
        }
    ).encode("utf-8")
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
        return data["choices"][0]["message"]["content"]
    except (KeyError, IndexError, TypeError) as e:
        raise SystemExit(f"llm bad response shape: {data!r}") from e


def extract_json_object(text: str):
    text = text.strip()
    if text.startswith("```"):
        text = re.sub(r"^```(?:json)?\s*", "", text)
        text = re.sub(r"\s*```$", "", text)
    try:
        obj = json.loads(text)
        return obj if isinstance(obj, dict) else None
    except json.JSONDecodeError:
        pass
    m = re.search(r"\{[\s\S]*\}", text)
    if not m:
        return None
    try:
        obj = json.loads(m.group(0))
        return obj if isinstance(obj, dict) else None
    except json.JSONDecodeError:
        return None


def build_messages(prompt: str, system: str, template: str, schema: str | None) -> list:
    sys_parts = []
    if system:
        sys_parts.append(system)
    if template:
        sys_parts.append(f"Style/template hint: {template}")
    if schema:
        sys_parts.append(
            "Extract structured data from the user text. "
            "Reply with a single JSON object only (no markdown). "
            "Required keys: " + schema
        )
    messages = []
    if sys_parts:
        messages.append({"role": "system", "content": "\n".join(sys_parts)})
    messages.append({"role": "user", "content": prompt})
    return messages


def emit_dsl_result(fields: dict, raw: str) -> None:
    print(raw.rstrip())
    print()
    print("##DSL_RESULT")
    print("ok=1")
    for k, v in fields.items():
        if k == "ok":
            continue
        val = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)
        print(f"{k}={val}")
    print("##DSL_END")


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
    ap.add_argument("--schema", default="", help="JSON object or key list describing output fields")
    ap.add_argument("--count", default="5", help="legacy unused hint")
    return ap.parse_args(argv)


def _self_test() -> None:
    assert extract_json_object('{"a":1}') == {"a": 1}
    assert extract_json_object('```json\n{"b":2}\n```') == {"b": 2}
    assert extract_json_object('noise {"c": 3} trail') == {"c": 3}
    assert extract_json_object("nope") is None
    msgs = build_messages("hi", "sys", "tmpl", "title,url")
    assert msgs[0]["role"] == "system" and "title,url" in msgs[0]["content"]
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
    content = chat_completions(base, key, model, messages, timeout=timeout)
    if schema:
        obj = extract_json_object(content)
        if not obj:
            print(content)
            print(
                "llm bridge: schema requested but no JSON object in model output",
                file=sys.stderr,
            )
            return 1
        emit_dsl_result(obj, content)
    else:
        print(content.rstrip())
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        _self_test()
        raise SystemExit(0)
    raise SystemExit(main())
