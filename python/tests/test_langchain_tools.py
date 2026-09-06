"""Tests for the ductile LangChain adapter (python/ductile/__init__.py).

Run: .venv/bin/python -m pytest python/tests/test_langchain_tools.py -v
Requires the wheel built with maturin (maturin develop --release).
"""

import json
import os
import sys

import pytest

ductile = pytest.importorskip("ductile")

try:
    import langchain_core  # noqa: F401
    HAS_LANGCHAIN = True
except ImportError:
    HAS_LANGCHAIN = False

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def test_native_surface_present():
    for fn in ("scripts_json", "procs_json", "runs_json", "db_stats_json",
               "pipeline_json", "run_json", "script_call_json"):
        assert hasattr(ductile, fn), fn


def test_all_json_valid():
    for raw in (ductile.scripts_json(), ductile.procs_json(""), ductile.db_stats_json()):
        json.loads(raw)  # must not raise


@pytest.mark.skipif(not HAS_LANGCHAIN, reason="langchain-core not installed")
class TestLangchainTools:
    def test_tool_list(self):
        tools = ductile.langchain_tools()
        names = {t.name for t in tools}
        assert "ductile_run" in names
        assert "ductile_stats" in names
        assert "ductile_call_script" in names

    def test_stats_tool(self):
        tools = ductile.langchain_tools()
        stats = next(t for t in tools if t.name == "ductile_stats").invoke({})
        d = json.loads(stats)
        assert set(d) >= {"pipelines", "procs", "runs", "compositions"}

    def test_run_tool_happy_path(self):
        tools = ductile.langchain_tools()
        run_tool = next(t for t in tools if t.name == "ductile_run")
        out = json.loads(run_tool.invoke({
            "pipeline_path": os.path.join(REPO, "pipelines/script_demo.pipeline"),
            "topic": "pytest integration",
        }))
        assert out["ok"] is True
        assert "analyze" in out["results"]

    def test_run_tool_authoring_error_raises(self):
        """文件不存在 = 创作错误 → 异常（不静默）。执行失败才是 ok=false 数据。"""
        tools = ductile.langchain_tools()
        run_tool = next(t for t in tools if t.name == "ductile_run")
        with pytest.raises(RuntimeError):
            run_tool.invoke({
                "pipeline_path": os.path.join(REPO, "pipelines/no_such_file.pipeline"),
                "topic": "",
            })

    def test_run_tool_execution_failure_is_data(self):
        """真实管线 + 全路径必败条件 → {"ok":false} 数据返回，不抛异常。"""
        import tempfile, textwrap
        with tempfile.TemporaryDirectory() as td:
            pipe = os.path.join(td, "fail.pipeline")
            open(pipe, "w").write(textwrap.dedent("""\
                Pipeline("t")
                  .proc("boom")
                    .plan(
                      x -> run("exit 7")
                    )
            """))
            tools = ductile.langchain_tools()
            run_tool = next(t for t in tools if t.name == "ductile_run")
            out = json.loads(run_tool.invoke({"pipeline_path": pipe, "topic": "x"}))
            assert out["ok"] is False
            assert "error" in out

    def test_call_script_failure_is_data(self):
        tools = ductile.langchain_tools()
        call = next(t for t in tools if t.name == "ductile_call_script")
        out = json.loads(call.invoke({"name": "no_such_script", "kv": ""}))
        assert out["ok"] is False
        assert "no_such_script" in out["error"]

    def test_call_script_happy(self):
        tools = ductile.langchain_tools()
        call = next(t for t in tools if t.name == "ductile_call_script")
        out = json.loads(call.invoke({"name": "word_stats", "kv": "text=hello pytest world"}))
        assert out["ok"] is True
        assert out["fields"]["words"] == "3"

    def test_per_script_tool_schema_and_call(self):
        tools = ductile.langchain_tools()
        ws = next((t for t in tools if t.name == "ductile_script_word_stats"), None)
        if ws is None:
            pytest.skip("word_stats script not attached in this environment")
        props = ws.args_schema.model_json_schema()["properties"]
        assert "text" in props  # 契约参数进入 schema（不再是 **kwargs 黑洞）
        out = json.loads(ws.invoke({"text": "four score and seven"}))
        assert out["ok"] is True
        assert out["fields"]["words"] == "4"

    def test_script_tools_refresh_on_attach(self):
        """每次调用 langchain_tools() 重新读契约卡 → 新 attach 的脚本即时出现。"""
        tools = ductile.langchain_tools()
        names = {t.name for t in tools}
        for card in json.loads(ductile.scripts_json()):
            assert f"ductile_script_{card['name']}" in names


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
