# Ductile

A declarative pipeline DSL with e-graph multi-path selection. Written in Rust, zero external dependencies.

## What is Ductile?

Ductile lets you describe data-processing pipelines as a simple text file (`.pipeline`). Each pipeline declares:

- **Procs** — discrete steps (search, write, run, etc.)
- **Plans** — multiple implementation paths per proc, each with a cost
- **E-graph selection** — automatically picks the cheapest path, falls back on failure
- **Checks** — validate results before passing them downstream
- **Foreach** — fan out over a list of items
- **Retry** — exponential backoff on transient failures

## Install

### From source (Rust)

```bash
cargo install --path .
```

### Via pip (Python binding)

```bash
pip install ductile
```

## Quick start

Write a `.pipeline` file:

```
Pipeline("research", effects=[Search, File], min_level=L3)
  .proc("search", level=L1, scope=Search)
    .plan(
      fast -> web_search(query="{topic}").cost(latency=3000, risk=0.1, tokens=0, money=0),
      safe -> mcp_search(query="{topic}", engine=zai).cost(latency=2000, risk=0.05, tokens=0, money=0)
    )
    .pick
    .check(result => has_results, "搜索无结果")
  .proc("write", level=L2, scope=File)
    .plan(
      out -> write(to="/tmp/report.md", content=@search).cost(latency=50, risk=0, tokens=0, money=0)
    )
    .pick
  .proc("deliver", level=L0)
    .deliver(media=[@write])
```

Run it:

```bash
ductile run research.pipeline "RISC-V架构最新进展"
```

Check it without executing:

```bash
ductile check research.pipeline
```

Show the e-graph structure and parallel groups:

```bash
ductile graph research.pipeline
```

## DSL Syntax

### Pipeline header

```
Pipeline("name", effects=[Search, File], min_level=L3)
```

- `effects` — declared scopes that procs in this pipeline may use
- `min_level` — maximum execution level (L0=local, L1-L5=escalating privilege)

### Proc

```
.proc("name", level=L1, scope=Search)
```

A discrete processing step. Each proc has one or more implementation paths.

### Plan

```
.plan(
  path_a -> web_search(query="AI").cost(latency=3000, risk=0.1, tokens=0, money=0),
  path_b -> mcp_search(query="AI", engine=zai).cost(latency=2000, risk=0.05, tokens=0, money=0)
)
```

Multiple paths compete. The engine picks the lowest-cost one first; if it fails, falls back to the next.

### Pick

```
.pick
.pick(by=cost + history)
.pick(by=latency)
```

Selection strategy. Default is `cost + history` (cost-weighted with failure penalty from recent runs). Bare `.pick` uses the default.

### Check

```
.check(result => has_results, "搜索无结果")
```

Built-in predicates: `has_results`, `has_items`, `has_summary`, `has_content`, `not_empty`, `no_error`, `has_date`, `min_length(N)`.

### Foreach

```
.foreach(source=@decompose, var=subtask)
```

Fan out: iterate over each line of the source proc's result, substituting `{subtask}` in downstream bodies.

### Retry

```
sh("ffmpeg -i input.mp4 output.mp4").cost(...).retry(n=3)
```

Exponential backoff: 2s → 4s → 8s.

### Ensure (inline check)

```
sh("make build").cost(...).ensure(not has_error, "编译失败")
```

Impl-level check that runs after execution.

### Deliver

```
.proc("deliver", level=L0)
  .deliver(media=[@write])
```

Terminal proc that marks pipeline outputs for delivery.

## Built-in functions

| Function | Description |
|---|---|
| `web_search(query="...")` | Web search via bridge script |
| `mcp_search(query="...", engine=zai)` | MCP-based search |
| `llm("desc", input=@prev, template="...")` | LLM call via bridge |
| `write(to="path", content=@prev)` | Write file |
| `read(from="path")` | Read file |
| `run("shell command")` | Execute shell command |
| `sh("shell command")` | Alias for `run()` |
| `merge(@a, @b)` | Concatenate results |

## ##DSL_RESULT protocol

External programs called via `run()`/`sh()` can return structured data by appending a block to stdout:

```
##DSL_RESULT
path=/tmp/output.mp4
duration=5.2
frames=243
##DSL_END
```

Fields are accessible downstream via `@proc.field` references (e.g. `@convert.duration`).

## Record (GCF)

Each proc execution is logged to `~/.local/share/ductile/records/<proc>/runs.gcf` in append-only GCF format. The engine uses a sliding window (last 20 runs) to penalize frequently failing paths.

## Performance

| Metric | Haskell | Rust |
|---|---|---|
| Startup (check) | 11ms | 1ms |
| Dependencies | 12+ packages | 0 (std only) |
| Binary size | ~30MB | ~400KB |

## License

MIT
