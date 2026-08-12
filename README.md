# Ductile

A declarative pipeline DSL with tag-based effects and runtime-learned path selection. Written in Rust, zero external dependencies.

## What is Ductile?

Ductile lets you describe data-processing pipelines as a simple text file (`.pipeline`):

- **Procs** — discrete steps (search, write, run, etc.)
- **Tags** — free-form labels on leaf nodes; pipeline effects auto-computed as union
- **Fallback** — declaration order is priority; path A fails → try path B
- **Runtime learning** — GCF records track success rate + latency per path; good paths rank higher, bad paths get blocked
- **Checks** — validate results before passing them downstream
- **Foreach** — fan out over a list of items
- **Retry** — exponential backoff on transient failures

No cost declarations. No level/scope enums. No weights to tune.

## Install

```bash
cargo install --path .
```

Via pip (Python binding):

```bash
pip install ductile
```

## Quick start

Write a `.pipeline` file:

```
Pipeline("research", "Research a topic and write a report")

.proc("search")
  .plan(
    web -> web_search(query="{topic}").tags(#search, #network).retry(n=3),
    mcp -> mcp_search(query="{topic}").tags(#search)
  )
  .check(not_empty, "No search results")

.proc("write")
  .plan(
    out -> write(to="/tmp/report.md", content=@search).tags(#file)
  )

.proc("deliver")
  .deliver(media=[@write])
```

Run it:

```bash
ductile run research.pipeline "RISC-V architecture"
```

Check, parse, graph:

```bash
ductile check research.pipeline
ductile parse research.pipeline
ductile graph research.pipeline
```

## DSL Syntax

### Pipeline header

```
Pipeline("name", "description")
```

- `name` — pipeline identifier
- `description` (optional) — natural language description for future search/retrieval

### Proc

```
.proc("name")
```

A processing step. Dependencies auto-derived from `@proc_name` references.

### Plan (multi-path + fallback)

```
.plan(
  path_a -> body.tags(#tag1),
  path_b -> body.tags(#tag2)
)
```

Declaration order = priority. A fails → try B automatically.

### Tags

```
.tags(#search, #network)
```

Only on leaf impls. Pipeline effects auto-computed as union of all impl tags. Tags stored sorted (BTreeSet) for deterministic matching.

### Check

```
.check(not_empty, "Result is empty")
```

Core predicates: `not_empty`, `no_error`, `min_length(N)`, `has_items`.

### Retry

```
.retry(n=3)
```

Exponential backoff: 2s → 4s → 8s.

### Foreach

```
.foreach(source=@decompose, var=subtask)
```

Fan out: iterate over each line of the source proc's result, substituting `{subtask}` in downstream bodies.

### Deliver

```
.proc("deliver")
  .deliver(media=[@write])
```

Terminal node marking pipeline output.

### When (conditional routing)

```
.when(mode == "deep")
```

### Disabled

```
.disabled
```

Temporarily disable a path.

## Path selection (no cost, no weights)

Path ranking is entirely runtime-driven:

| Stage | Strategy |
|-------|----------|
| Cold start (< 3 runs) | Declaration order; new paths explored first |
| Has data | Sort by recent-20 success rate (high→low), tie-break by avg latency (low→high) |
| 3 consecutive failures | BLOCKED — skipped |

## GCF record format

Each execution appends to `~/.local/share/ductile/records/<proc>/runs.gcf`:

```
GCF profile=generic
## runs
2026-08-13T00:04:51|web|Ok|3124|A|-|-
2026-08-13T00:04:52|mcp|Fail|5100|B|a1b2c3d4|search.mcp.step
```

Fields: `timestamp | path | status | latency_ms | pick | err_hash | err_at`

## ##DSL_RESULT protocol

External programs called via `run()`/`sh()` can return structured data:

```
##DSL_RESULT
path=/tmp/output.mp4
duration=5.2
frames=243
##DSL_END
```

Access downstream via `@proc.field` references.

## Built-in functions

| Function | Description |
|---|---|
| `web_search(query="...")` | Web search via bridge |
| `mcp_search(query="...", engine=zai)` | MCP-based search |
| `llm("desc", input=@prev, template="...")` | LLM call via bridge |
| `write(to="path", content=@prev)` | Write file |
| `read(from="path")` | Read file |
| `run("shell command")` | Execute shell command |
| `sh("shell command")` | Alias for `run()` |
| `merge(@a, @b)` | Concatenate results |

## CLI commands

| Command | Description |
|---|---|
| `check <file>` | Parse + type check |
| `run <file> [topic]` | Parse + check + execute |
| `graph <file>` | Show DAG structure (parallel groups, critical path) |
| `parse <file>` | Parse only (show structure) |

## Performance

| Metric | Value |
|---|---|
| Startup (check) | 1ms |
| Dependencies | 0 (std only) |
| Binary size | ~400KB |
| Tests | 90, all passing |

## License

MIT
