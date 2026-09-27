#!/usr/bin/env python3
"""Ductile MCP bridge — mcp(server, tool=...) 动词的协议层。

分层：Rust `mcp()` 动词（exec_mcp_call）只做参数解析与结果解析，
协议（Streamable HTTP JSON-RPC）与 token 刷新都在本桥。

调用约定（与 llm_bridge.py 同风格）：
  python mcp_bridge.py --server LibTV --tool doctor [--args '{"k": "v"}']
  python mcp_bridge.py --list-servers
  python mcp_bridge.py --self-test

配置（config.toml 连接层）：
  [mcp.servers.LibTV]
  url = "https://mcp.liblib.tv/mcp"
  token_dir = "~/.hermes/profiles/zaomeng/mcp-tokens"   # OAuth tokens: LibTV{,.client,.meta}.json
  style = "bearer"          # bearer=Authorization Bearer <access_token>（Hermes OAuth 型）
  # command/args = stdio 型服务器（未实现，fail-closed 拒绝）

Config precedence: CLI flags > env MCP_URL/MCP_TOKEN_FILE > config.toml > fail-closed。

stdout: 单 JSON（工具 structuredContent 或解析后的 content）。
失败: exit 2 + stderr 错误说明（Rust 侧转为 Err → errflow）。
OAuth 刷新: token 文件 fetched_at 超龄 → client/meta 自动 refresh_token 换新。
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# ── config.toml 装载（与 llm_bridge 同源的精简版）──


def _find_config() -> str | None:
    cands = []
    if os.environ.get("DUCTILE_CONFIG"):
        cands.append(os.environ["DUCTILE_CONFIG"])
    cands += ["config.toml", "ductile.toml"]
    cands.append(os.path.expanduser("~/.config/ductile/config.toml"))
    for c in cands:
        p = os.path.abspath(c)
        if os.path.exists(p):
            return p
    return None


def _parse_toml_min(text: str) -> dict:
    """只解析 [mcp.servers.<name>] 扁平段的最小 TOML 读取器。"""
    out: dict[str, dict[str, str]] = {}
    cur: dict[str, str] | None = None
    cur_name = ""
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            sec = line[1:-1].strip()
            cur = out.setdefault(sec, {})
            cur_name = sec
            continue
        if cur is not None and "=" in line:
            k, v = line.split("=", 1)
            k = k.strip()
            v = v.strip()
            if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
                v = v[1:-1]
            cur[k] = v
    return out


def load_mcp_servers() -> dict[str, dict[str, str]]:
    cfg_path = _find_config()
    if not cfg_path:
        return {}
    try:
        with open(cfg_path, encoding="utf-8") as f:
            secs = _parse_toml_min(f.read())
    except OSError:
        return {}
    servers: dict[str, dict[str, str]] = {}
    for name, kv in secs.items():
        if name.startswith("mcp.servers."):
            servers[name[len("mcp.servers."):]] = kv
    return servers


# ── OAuth token 装载/刷新（照搬 libtv_mcp.py 已验证逻辑）──


def _token_paths(token_dir: str, server: str):
    base = os.path.expanduser(token_dir)
    return (
        os.path.join(base, f"{server}.json"),
        os.path.join(base, f"{server}.client.json"),
        os.path.join(base, f"{server}.meta.json"),
    )


def load_access_token(token_dir: str, server: str, max_age: int = 3000) -> str:
    tp, cp, mp = _token_paths(token_dir, server)
    if not os.path.exists(tp):
        raise SystemExit(f"no token file at {tp} — complete OAuth for server '{server}' first")
    with open(tp, encoding="utf-8") as f:
        t = json.load(f)
    if "fetched_at" not in t:
        t["fetched_at"] = time.time()
        with open(tp, "w", encoding="utf-8") as f:
            json.dump(t, f, indent=2)
    if time.time() - t["fetched_at"] < max_age:
        return t["access_token"]
    # 刷新（client_secret_post）
    if not (os.path.exists(cp) and os.path.exists(mp)):
        raise SystemExit(f"token expired and no client/meta for refresh: {cp} / {mp}")
    with open(cp, encoding="utf-8") as f:
        cli = json.load(f)
    with open(mp, encoding="utf-8") as f:
        meta = json.load(f)
    data = urllib.parse.urlencode({
        "grant_type": "refresh_token",
        "refresh_token": t["refresh_token"],
        "client_id": cli["client_id"],
        "client_secret": cli["client_secret"],
    }).encode()
    req = urllib.request.Request(
        meta["token_endpoint"], data=data,
        headers={"Content-Type": "application/x-www-form-urlencoded"})
    r = json.loads(urllib.request.urlopen(req, timeout=30).read())
    r["fetched_at"] = time.time()
    r.setdefault("scope", t.get("scope"))
    with open(tp, "w", encoding="utf-8") as f:
        json.dump(r, f, indent=2)
    return r["access_token"]


# ── Streamable HTTP MCP 客户端（纯 urllib 实现，无第三方依赖）──

_MCP_PROTOCOL = "2025-06-18"


def _http_post(url: str, payload: dict, headers: dict, timeout: int) -> dict:
    body = json.dumps(payload).encode()
    h = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        **headers,
    }
    req = urllib.request.Request(url, data=body, headers=h, method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8", "replace")
        ctype = resp.headers.get("Content-Type", "")
    if "application/json" in ctype:
        return json.loads(raw)
    # text/event-stream：逐行取 data: 的 JSON，取首个带 id 的响应
    for line in raw.splitlines():
        line = line.strip()
        if line.startswith("data:"):
            chunk = line[len("data:"):].strip()
            try:
                obj = json.loads(chunk)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict) and ("id" in obj or "method" in obj):
                return obj
    raise SystemExit(f"unparseable MCP response (content-type={ctype}): {raw[:200]}")


def _notify(url: str, payload: dict, headers: dict, timeout: int) -> None:
    """JSON-RPC notification：期望 202/空体，任何 2xx 都算送达。"""
    body = json.dumps(payload).encode()
    h = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
        **headers,
    }
    req = urllib.request.Request(url, data=body, headers=h, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout):
            pass  # 202 空体即成功
    except urllib.error.HTTPError as e:
        if e.code >= 400:
            raise


def mcp_call(url: str, tool: str, args: dict, token: str, timeout: int = 120) -> object:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    # initialize → notifications/initialized → tools/call
    init_id = 1
    init = _http_post(url, {
        "jsonrpc": "2.0", "id": init_id, "method": "initialize",
        "params": {
            "protocolVersion": _MCP_PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "ductile-mcp-bridge", "version": "0.1"},
        }}, headers, timeout)
    if init.get("id") != init_id or "result" not in init:
        raise SystemExit(f"MCP initialize failed: {json.dumps(init)[:300]}")
    # notification（无 id，服务器可回 202 空体——不可当 JSON 解析）
    _notify(url, {"jsonrpc": "2.0", "method": "notifications/initialized"}, headers, timeout)
    call_id = 2
    resp = _http_post(url, {
        "jsonrpc": "2.0", "id": call_id, "method": "tools/call",
        "params": {"name": tool, "arguments": args or {}},
    }, headers, timeout)
    if "error" in resp:
        raise SystemExit(f"MCP tool error: {resp['error'].get('message', resp['error'])}")
    result = resp.get("result", {})
    if "toolExecutionId" in result and "structuredContent" not in result:
        # 异步执行型（如 generation_submit）：返回任务句柄原样交给 DSL
        return result
    sc = result.get("structuredContent")
    if isinstance(sc, dict) and sc:
        return sc
    # text content 列表 → 尝试 JSON 解析
    for c in result.get("content", []) or []:
        s = c.get("text") if isinstance(c, dict) else None
        if isinstance(s, str) and s.strip().startswith(("{", "[")):
            try:
                return json.loads(s)
            except json.JSONDecodeError:
                continue
    texts = [c.get("text", "") for c in result.get("content", []) or [] if isinstance(c, dict)]
    return " ".join(texts) if texts else result


def flatten_result(obj: object) -> dict[str, str]:
    """MCP 结果 → ##DSL_RESULT 字段拍平（Rust 零 JSON 依赖的关键）。

    规则：顶层标量（bool→0/1、数字、短字符串）直接成字段（key 小写下划线化）；
    完整 JSON 恒放 raw=（Value 通道超 5000 自截）；list 长度放 n=。
    """
    out: dict[str, str] = {}
    if isinstance(obj, dict):
        for k, v in obj.items():
            fk = "".join(c if c.isalnum() or c == "_" else "_" for c in k).lower().strip("_")
            if isinstance(v, bool):
                out[fk] = "1" if v else "0"
            elif isinstance(v, (int, float)):
                out[fk] = str(v)
            elif isinstance(v, str) and len(v) <= 200 and "\n" not in v:
                out[fk] = v
        # 常用聚合字段
        for k in ("matches", "projects", "nodes", "models", "tools", "tasks", "items", "rows"):
            if isinstance(obj.get(k), list):
                out.setdefault("n", str(len(obj[k])))
                break
    elif isinstance(obj, list):
        out["n"] = str(len(obj))
    try:
        out["raw"] = json.dumps(obj, ensure_ascii=False)
    except (TypeError, ValueError):
        out["raw"] = str(obj)
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--server", help="config.toml [mcp.servers.<name>] 里的服务器名")
    ap.add_argument("--tool", help="MCP 工具名")
    ap.add_argument("--args", default="{}", help="工具参数 JSON 字符串")
    ap.add_argument("--timeout", type=int, default=120)
    ap.add_argument("--list-servers", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()

    servers = load_mcp_servers()

    if a.self_test:
        print(json.dumps({"ok": True, "servers": sorted(servers)}, ensure_ascii=False))
        return
    if a.list_servers:
        print(json.dumps(sorted(servers)))
        return
    if not a.server or not a.tool:
        raise SystemExit("need --server and --tool (see --self-test)")
    cfg = servers.get(a.server)
    if cfg is None:
        raise SystemExit(
            f"unknown MCP server '{a.server}' — declare it in config.toml [mcp.servers.{a.server}]; "
            f"known: {sorted(servers)}")
    style = cfg.get("style", "bearer")
    if style != "bearer":
        raise SystemExit(f"unsupported style '{style}' — only OAuth bearer servers supported (fail-closed)")
    url = cfg.get("url") or os.environ.get("MCP_URL", "")
    if not url:
        raise SystemExit(f"[mcp.servers.{a.server}] missing url")
    token_dir = cfg.get("token_dir", "")
    token = load_access_token(token_dir, a.server) if token_dir else ""
    try:
        out = mcp_call(url, a.tool, json.loads(a.args or "{}"), token, a.timeout)
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001
        print(f"mcp_bridge: {type(e).__name__}: {e}", file=sys.stderr)
        sys.exit(2)
    # ##DSL_RESULT 协议：拍平字段 + raw= 完整 JSON（Rust 侧零 JSON 解析复用现有通道）
    fields = flatten_result(out)
    lines = [f"{k}={v}" for k, v in fields.items()]
    print("##DSL_RESULT\n" + "\n".join(lines) + "\n##DSL_END")
    # JSON 也走 stderr 供人看
    print(json.dumps(out, ensure_ascii=False)[:2000], file=sys.stderr)


if __name__ == "__main__":
    main()
