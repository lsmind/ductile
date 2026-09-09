#!/usr/bin/env python3
"""Minimal LangGraph-style multi-tool extract (no LangGraph runtime required).

Mirrors the official create_react_agent shape: LLM decides tool calls in a loop,
tools are plain Python callables. Here we simulate the loop with a deterministic
stub LLM so the script runs offline for comparison with ductile pipelines.

Same task as examples/scripts/unstructured-extract.pipeline:
  unstructured blurb -> structured {title,url,topic}
"""

from __future__ import annotations

import json
import re
import sys

FIXTURE = """
搜索结果：
1. Transformer模型详解 - https://example.com/transformer
2. 注意力机制原理解析 - https://example.com/attention
相关主题：大模型基础设施
"""


def tool_search(query: str) -> str:
    """Fake web search tool (multi-script surface)."""
    q = query.lower()
    if "transformer" in q or "attention" in q or "模型" in query:
        return (
            "1. Transformer模型详解 - https://example.com/transformer\n"
            "2. 注意力机制原理解析 - https://example.com/attention"
        )
    return "no hits"


def tool_parse_lines(blob: str) -> list[dict]:
    """Second tool: parse title/url lines from unstructured text."""
    rows = []
    for line in blob.splitlines():
        m = re.search(r"(\S.+?)\s+-\s+(https?://\S+)", line.strip())
        if m:
            rows.append({"title": m.group(1).strip("0123456789. "), "url": m.group(2)})
    return rows


def react_loop(user_text: str) -> dict:
    """Deterministic stand-in for LangGraph ReAct: search -> parse -> synthesize."""
    # Step 1: "agent" chooses search
    search_hit = tool_search("transformer attention")
    # Step 2: "agent" chooses parse
    items = tool_parse_lines(search_hit if search_hit != "no hits" else user_text)
    topic = "unknown"
    if "主题" in user_text:
        topic = user_text.split("主题")[-1].strip("：: \n")
    return {
        "ok": True,
        "framework": "langgraph-style-react",
        "tools_used": ["search", "parse_lines"],
        "count": len(items),
        "title": items[0]["title"] if items else "",
        "url": items[0]["url"] if items else "",
        "topic": topic,
        "items": items,
    }


def main() -> int:
    text = sys.stdin.read() if not sys.stdin.isatty() else FIXTURE
    out = react_loop(text)
    print(json.dumps(out, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
