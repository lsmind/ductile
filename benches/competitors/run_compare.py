#!/usr/bin/env python3
"""Run all offline competitor sketches on the shared fixture; print comparison matrix.

Usage (repo root or this dir):
  python benches/competitors/run_compare.py
"""

from __future__ import annotations

import importlib.util
import json
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
FIXTURE = ROOT / "examples" / "scripts" / "fixtures" / "search_blurb.txt"

SCRIPTS = [
    ("langgraph-react", "langgraph_unstructured_extract.py"),
    ("autogen-two-agent", "autogen_two_agent_extract.py"),
    ("crewai-crew", "crewai_role_crew_extract.py"),
    ("prefect-dag", "prefect_flow_extract.py"),
]


def load_module(path: Path):
    name = f"competitors_{path.stem}"
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    # dataclasses need the module registered before @dataclass runs
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


def run_one(name: str, filename: str, text: str) -> dict:
    path = HERE / filename
    t0 = time.perf_counter()
    mod = load_module(path)
    # Prefer explicit entry if present
    if hasattr(mod, "react_loop"):
        out = mod.react_loop(text)
    elif hasattr(mod, "kickoff"):
        out = mod.kickoff(text)
    elif hasattr(mod, "run_flow"):
        out = mod.run_flow(text)
    else:
        # Autogen module: extractor + reviewer
        draft = mod.extractor_agent(text)
        out = mod.reviewer_agent(draft)
        out.setdefault("framework", "autogen-style-two-agent")
    ms = (time.perf_counter() - t0) * 1000
    return {
        "name": name,
        "script": filename,
        "ok": bool(out.get("ok", True)),
        "framework": out.get("framework", name),
        "title": out.get("title", ""),
        "url": out.get("url", ""),
        "topic": out.get("topic", ""),
        "count": out.get("count", 0),
        "latency_ms": round(ms, 2),
        "keys": sorted(out.keys()),
    }


def main() -> int:
    text = FIXTURE.read_text(encoding="utf-8") if FIXTURE.is_file() else ""
    if not text.strip():
        print("fixture missing:", FIXTURE, file=sys.stderr)
        return 2
    rows = [run_one(n, f, text) for n, f in SCRIPTS]
    # agreement on core fields
    titles = {r["title"] for r in rows}
    urls = {r["url"] for r in rows}
    summary = {
        "fixture": str(FIXTURE.relative_to(ROOT)),
        "ductile_counterpart": "examples/scripts/unstructured-extract.pipeline",
        "agreement": {
            "same_title": len(titles) == 1,
            "same_url": len(urls) == 1,
            "title": next(iter(titles)) if titles else "",
            "url": next(iter(urls)) if urls else "",
        },
        "rows": rows,
        "notes": [
            "Offline sketches only — no LangGraph/CrewAI/Prefect packages installed.",
            "Ductile live path needs config.toml [llm] or OPENAI_*; offline stub in .pipeline plan.",
        ],
    }
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0 if all(r["ok"] for r in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
