#!/usr/bin/env python3
"""Minimal AutoGen-style two-agent extract (no AutoGen runtime required).

Shape mirrors AutoGen: extractor agent drafts JSON; reviewer agent checks fields.
Offline deterministic agents so the script is reproducible without API keys.

Same task as examples/scripts/unstructured-extract.pipeline.
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


def extractor_agent(text: str) -> dict:
    items = []
    for line in text.splitlines():
        m = re.search(r"(\S.+?)\s+-\s+(https?://\S+)", line.strip())
        if m:
            items.append(
                {
                    "title": m.group(1).strip("0123456789. "),
                    "url": m.group(2),
                }
            )
    topic = "unknown"
    for line in text.splitlines():
        if "主题" in line:
            topic = line.split("主题")[-1].strip("：: \n")
            break
    draft = {
        "title": items[0]["title"] if items else "",
        "url": items[0]["url"] if items else "",
        "topic": topic,
        "count": len(items),
    }
    return draft


def reviewer_agent(draft: dict) -> dict:
    missing = [k for k in ("title", "url", "topic") if not draft.get(k)]
    return {
        "ok": not missing,
        "framework": "autogen-style-two-agent",
        "agents": ["extractor", "reviewer"],
        "approved": not missing,
        "missing": missing,
        **draft,
    }


def main() -> int:
    text = sys.stdin.read() if not sys.stdin.isatty() else FIXTURE
    draft = extractor_agent(text)
    final = reviewer_agent(draft)
    print(json.dumps(final, ensure_ascii=False, indent=2))
    return 0 if final["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
