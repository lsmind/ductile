#!/usr/bin/env python3
"""Prefect/Airflow-style deterministic task DAG extract (no Prefect/Airflow runtime).

Mirrors batch orchestrators: named tasks, explicit dependencies, no agent loop.
Useful contrast: ductile/LangGraph are agent-aware; Prefect-class is DAG-first.

Same task: unstructured blurb -> {title,url,topic,count}
"""

from __future__ import annotations

import json
import re
import sys
from typing import Callable

FIXTURE = """
搜索结果：
1. Transformer模型详解 - https://example.com/transformer
2. 注意力机制原理解析 - https://example.com/attention
相关主题：大模型基础设施
"""


def task_load(text: str) -> str:
    return text


def task_parse_lines(text: str) -> list[dict]:
    rows = []
    for line in text.splitlines():
        m = re.search(r"(\S.+?)\s+-\s+(https?://\S+)", line.strip())
        if m:
            rows.append(
                {
                    "title": m.group(1).strip("0123456789. "),
                    "url": m.group(2),
                }
            )
    return rows


def task_topic(text: str) -> str:
    for line in text.splitlines():
        if "主题" in line:
            return line.split("主题")[-1].strip("：: \n")
    return "unknown"


def task_assemble(items: list[dict], topic: str) -> dict:
    return {
        "ok": bool(items),
        "framework": "prefect-style-dag",
        "tasks": ["load", "parse_lines", "topic", "assemble"],
        "deps": {
            "parse_lines": ["load"],
            "topic": ["load"],
            "assemble": ["parse_lines", "topic"],
        },
        "title": items[0]["title"] if items else "",
        "url": items[0]["url"] if items else "",
        "topic": topic,
        "count": len(items),
        "items": items,
    }


def run_flow(text: str) -> dict:
    # Explicit DAG edges (Airflow/Prefect mental model), not LLM-chosen tools.
    loaded = task_load(text)
    items = task_parse_lines(loaded)
    topic = task_topic(loaded)
    return task_assemble(items, topic)


def main() -> int:
    text = sys.stdin.read() if not sys.stdin.isatty() else FIXTURE
    out = run_flow(text)
    print(json.dumps(out, ensure_ascii=False, indent=2))
    return 0 if out.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
