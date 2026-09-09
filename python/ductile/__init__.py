"""Ductile — declarative pipeline DSL engine (Rust core, Python surface).

Hybrid package: the compiled `ductile._ductile` extension module carries the
fast paths; this Python layer adds the LangChain adapter and helpers.
"""

from . import _ductile  # Rust extension (maturin-built)
from ._ductile import (  # re-exported native functions
    check,
    parse,
    graph,
    run,
    scripts_json,
    procs_json,
    runs_json,
    db_stats_json,
    pipeline_json,
    run_json,
    script_call_json,
    hyper_similar_json,
    hyper_nodes_json,
    cli_main,
)

__version__ = _ductile.__version__
__all__ = [
    "check", "parse", "graph", "run",
    "scripts_json", "procs_json", "runs_json", "db_stats_json",
    "pipeline_json", "run_json", "script_call_json",
    "hyper_similar_json", "hyper_nodes_json", "cli_main",
    "langchain_tools", "get_tools",
    "__version__",
]


def _optional_import_langchain():
    try:
        from langchain_core.tools import tool as _lc_tool
        return _lc_tool
    except ImportError:
        try:
            from langchain.tools import tool as _lc_tool  # legacy path
            return _lc_tool
        except ImportError:
            return None


def langchain_tools(pipeline_dir: str = "pipelines"):
    """Build ductile LangChain tools (requires langchain-core).

    Returns a list of @tool-decorated functions covering the ductile
    surface an agent needs: introspection (scripts/procs/pipeline/stats),
    execution, and one tool per registered script contract.

    Raises ImportError with a helpful message if langchain-core is absent.
    """
    tool = _optional_import_langchain()
    if tool is None:
        raise ImportError(
            "langchain-core is required for ductile.langchain_tools(): "
            "pip install langchain-core  (or the full langchain)"
        )

    import json as _json

    # ── Introspection tools ──────────────────────────────────

    @tool
    def ductile_list_scripts() -> str:
        """List all registered ductile script contracts (name, params, output, purity, concurrency). Read this before calling ductile_call_script."""
        return _ductile.scripts_json()

    @tool
    def ductile_list_procs(query: str = "") -> str:
        """Search the ductile proc library. Empty query lists everything. Returns procs with their pipelines, tags, impl counts."""
        return _ductile.procs_json(query)

    @tool
    def ductile_pipeline_info(pipeline_path: str) -> str:
        """Parse a .pipeline file and return its structure as JSON: procs, impls, when-conditions, refs, parallel groups, critical path."""
        return _ductile.pipeline_json(pipeline_path)

    @tool
    def ductile_stats() -> str:
        """Ductile library stats: pipeline count, proc count, run history count, compositions."""
        return _ductile.db_stats_json()

    @tool
    def ductile_recent_runs(proc_name: str, limit: int = 10) -> str:
        """Recent runs of one ductile proc (newest first): impl chosen, status, latency_ms. Use to check a proc's health before relying on it."""
        return _ductile.runs_json(proc_name, limit)

    # ── Execution tools ──────────────────────────────────────

    @tool
    def ductile_run(pipeline_path: str, topic: str = "", params: str = "", policy: str = "") -> str:
        """Run a ductile .pipeline file. topic fills {topic}; params is comma-separated k=v pairs; policy is an optional .eval file path.
        Returns JSON: {"ok":true,"results":{...}} on success, {"ok":false,"error":"..."} when all paths failed — treat failure as data, not exception."""
        kv = {}
        params = (params or "").strip()
        if params:
            for part in params.split(","):
                if "=" in part:
                    k, _, v = part.partition("=")
                    kv[k.strip()] = v.strip()
        return _ductile.run_json(pipeline_path, topic, kv, policy or None)

    @tool
    def ductile_call_script(name: str, kv: str = "") -> str:
        """One-off invoke of a registered ductile script (debug/agent entry). kv is comma-separated k=v pairs (e.g. "text=hello,n=2"). Failures come back as JSON {"ok":false,...}; authoring errors raise."""
        argv = {}
        kv = (kv or "").strip()
        if kv:
            for part in kv.split(","):
                if "=" in part:
                    k, _, v = part.partition("=")
                    argv[k.strip()] = v.strip()
        return _ductile.script_call_json(name, argv)

    @tool
    def ductile_hyper_similar(query_path: str, roots: str = "") -> str:
        """BEFORE inventing a new .hyper/.pipeline topology: WHOLE-GRAPH structural reuse.
        query_path is a .hyper or .pipeline. roots = optional comma-separated dirs.
        isomorphic=true → reuse_pipeline. Tags soft only."""
        root_list = None
        roots = (roots or "").strip()
        if roots:
            root_list = [r.strip() for r in roots.split(",") if r.strip()]
        return _ductile.hyper_similar_json(query_path, root_list)

    @tool
    def ductile_node_similar(query: str = "", roots: str = "", role: str = "", op: str = "") -> str:
        """BEFORE writing a new proc/stage: NODE-level reuse lookup (not whole workflow).
        query = 'file.pipeline:proc' or 'file.hyper:stage' or '' when filtering by role/op.
        roots = comma-separated dirs. role e.g. judge|source|sink|default. op e.g. llm|read|write|run.
        Returns JSON hits with body_preview. isomorphic=true → reuse_action=reuse_node (copy that proc plan).
        same role+op → adapt_ports. Prefer pipeline-proc hits. Never reuse on tags alone.
        Use ductile_hyper_similar for whole-graph reuse; use this for single-node reuse."""
        root_list = None
        roots = (roots or "").strip()
        if roots:
            root_list = [r.strip() for r in roots.split(",") if r.strip()]
        return _ductile.hyper_nodes_json(
            query or "",
            root_list,
            role or None,
            op or None,
        )

    tools = [
        ductile_list_scripts,
        ductile_list_procs,
        ductile_pipeline_info,
        ductile_stats,
        ductile_recent_runs,
        ductile_run,
        ductile_call_script,
        ductile_hyper_similar,
        ductile_node_similar,
    ]

    # ── One tool per registered script contract ──────────────
    try:
        cards = _json.loads(_ductile.scripts_json())
    except Exception:
        cards = []
    for c in cards:
        def _make(card):
            doc_lines = [card["desc"] or f"Run the registered ductile script '{card['name']}'."]
            if card.get("params"):
                ps = [f'- {p["name"]} ({p["type"]}' + (", required" if p["required"] else "") +
                      (f', default={p["default"]}' if p.get("default") is not None else "") + ")"
                      for p in card["params"]]
                doc_lines.append("Args: " + "; ".join(ps))
            if card.get("output"):
                outs = [f'{o["name"]}:{o["type"]}' for o in card["output"]]
                doc_lines.append("Returns: " + ", ".join(outs))
            doc_lines.append(f'Purity: {"pure" if card["pure"] else "side-effects"}; '
                             f'concurrency: {card["concurrency"]}; effects: {card["effects"]}.')

            # 契约参数 → 显式 Python 签名（**kwargs 无法被 Pydantic 内省成 schema）
            import inspect as _inspect

            def script_tool(*args, **kwargs) -> str:
                if args and not kwargs and isinstance(args[0], dict):
                    kwargs = args[0]  # 兼容按位置传 dict 的调用方
                return _ductile.script_call_json(card["name"], kwargs)

            params = []
            for p in card.get("params") or []:
                if p.get("required"):
                    params.append(_inspect.Parameter(p["name"], _inspect.Parameter.POSITIONAL_OR_KEYWORD))
                else:
                    default = p.get("default")
                    dv = default if default is not None else ""
                    try:
                        dv = int(default) if p["type"] == "int" and default is not None else dv
                    except (TypeError, ValueError):
                        pass
                    params.append(_inspect.Parameter(p["name"], _inspect.Parameter.POSITIONAL_OR_KEYWORD, default=dv))
            script_tool.__signature__ = _inspect.Signature(params)  # type: ignore[attr-defined]
            script_tool.__doc__ = "\n".join(doc_lines)
            return tool(f"ductile_script_{card['name']}")(script_tool)
        tools.append(_make(c))

    return tools


def get_tools(pipeline_dir: str = "pipelines"):
    """Alias of langchain_tools()."""
    return langchain_tools(pipeline_dir)
