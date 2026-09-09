# Competitor multi-script sketches (offline)

Dependency-free sketches mirroring **LangGraph / AutoGen / CrewAI / Prefect** shapes
for the same unstructured → structured task as Ductile.

| Script | Paradigm | Upstream metaphor |
|--------|----------|-------------------|
| `langgraph_unstructured_extract.py` | ReAct + tools | LangGraph ToolNode loop |
| `autogen_two_agent_extract.py` | extractor + reviewer | AutoGen chat pair |
| `crewai_role_crew_extract.py` | role/task sequential crew | CrewAI Crew |
| `prefect_flow_extract.py` | fixed task DAG | Prefect/Airflow |
| `run_compare.py` | matrix runner | — |

## Run

```bash
# from repo root
python benches/competitors/run_compare.py
python benches/competitors/langgraph_unstructured_extract.py
python benches/competitors/crewai_role_crew_extract.py < examples/scripts/fixtures/search_blurb.txt
```

## Ductile counterpart

```bash
cp config.toml.example config.toml   # fill [llm] or use OPENAI_*
ductile run examples/scripts/unstructured-extract.pipeline
```

Full write-up: [`docs/COMPETITORS.md`](../../docs/COMPETITORS.md).

## Upstream (not vendored)

- https://github.com/langchain-ai/langgraph
- https://github.com/microsoft/autogen
- https://github.com/crewaiinc/crewai
- https://github.com/PrefectHQ/prefect
