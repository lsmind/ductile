#!/usr/bin/env python3
"""CrewAI-style role/task crew extract (no CrewAI runtime required).

Mirrors CrewAI metaphors: Agent(role/goal) + Task(expected_output) + sequential Crew.
Offline deterministic so CI can compare shapes with Ductile.

Same task: unstructured blurb -> {title,url,topic,count}
"""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass

FIXTURE = """
搜索结果：
1. Transformer模型详解 - https://example.com/transformer
2. 注意力机制原理解析 - https://example.com/attention
相关主题：大模型基础设施
"""


@dataclass
class Agent:
    role: str
    goal: str


@dataclass
class Task:
    name: str
    description: str
    expected_output: str
    agent: Agent
    context: list[str] | None = None


def tool_parse(text: str) -> list[dict]:
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


def run_task(task: Task, text: str, prior: dict) -> dict:
    # Deterministic stand-in for LLM+tools: role decides which tool path.
    if "extract" in task.name:
        items = tool_parse(text)
        topic = prior.get("topic", "unknown")
        for line in text.splitlines():
            if "主题" in line:
                topic = line.split("主题")[-1].strip("：: \n")
                break
        return {
            "title": items[0]["title"] if items else "",
            "url": items[0]["url"] if items else "",
            "topic": topic,
            "count": len(items),
            "items": items,
            "by": task.agent.role,
        }
    if "review" in task.name:
        draft = prior.get("extract_task", {})
        missing = [k for k in ("title", "url", "topic") if not draft.get(k)]
        return {
            **draft,
            "ok": not missing,
            "missing": missing,
            "approved": not missing,
            "by": task.agent.role,
        }
    return {"ok": False, "error": f"unknown task {task.name}"}


def kickoff(text: str) -> dict:
    extractor = Agent(
        role="Data Extraction Specialist",
        goal="Pull title/url/topic from unstructured search blurbs",
    )
    reviewer = Agent(
        role="Quality Reviewer",
        goal="Reject drafts missing required JSON keys",
    )
    tasks = [
        Task(
            name="extract_task",
            description="Extract structured fields from blurb",
            expected_output="JSON with title,url,topic,count",
            agent=extractor,
        ),
        Task(
            name="review_task",
            description="Validate extract_task output",
            expected_output="Approved JSON or missing[] list",
            agent=reviewer,
            context=["extract_task"],
        ),
    ]
    # sequential process= (CrewAI default)
    state: dict = {}
    for t in tasks:
        out = run_task(t, text, state)
        state[t.name] = out
    final = state["review_task"]
    return {
        "ok": bool(final.get("ok")),
        "framework": "crewai-style-crew",
        "process": "sequential",
        "agents": [extractor.role, reviewer.role],
        "tasks": [t.name for t in tasks],
        "title": final.get("title", ""),
        "url": final.get("url", ""),
        "topic": final.get("topic", ""),
        "count": final.get("count", 0),
        "missing": final.get("missing", []),
    }


def main() -> int:
    if not sys.stdin.isatty():
        text = sys.stdin.read()
    else:
        text = FIXTURE
    out = kickoff(text)
    print(json.dumps(out, ensure_ascii=False, indent=2))
    return 0 if out.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())
