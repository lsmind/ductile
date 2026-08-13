# Ductile DSL — 技术规格书

> 版本：0.5.0 | 更新：2026-08-13

## 目录

1. [概述](#1-概述)
2. [安装](#2-安装)
3. [DSL 语法](#3-dsl-语法)
4. [CLI 命令](#4-cli-命令)
5. [执行引擎](#5-执行引擎)
6. [SQLite 存储](#6-sqlite-存储)
7. [同构发现](#7-同构发现)
8. [库学习](#8-库学习)
9. [热补丁](#9-热补丁)
10. [版本快照](#10-版本快照)
11. [DSL_RESULT 协议](#11-dsl_result-协议)
12. [内置函数](#12-内置函数)

---

## 1. 概述

Ductile 是声明式流水线引擎。核心思想：

- **声明意图**：用户写"有哪些路径"，不写控制流
- **运行时学习**：引擎根据执行数据自动排序、降级、淘汰路径
- **机制替代意志力**：连续失败自动 BLOCKED，不需要人盯

### 架构

```
.pipeline 文件
    ↓
  Parser (parser.rs)     → AST (ast.rs)
    ↓
  TypeCheck (typecheck.rs) → 编译时拦错
    ↓
  EGraph (egraph.rs)     → DAG 拓扑 + 并行组 + 关键路径
    ↓
  Executor (executor.rs)  → 多路径选择 + 降级 + check + retry
    ↓
  SQLite (db.rs)         → 执行记录 + proc 库 + 补丁 + 组合
```

### 源文件结构

| 文件 | 职责 |
|------|------|
| `ast.rs` | 类型定义：Pipeline / Proc / Impl / Cost / Check / Status / Value |
| `parser.rs` | 递归下降解析器，处理 `.pipeline` 语法 |
| `typecheck.rs` | 编译时检查：cost 非负、plan 非空、有 enabled impl |
| `egraph.rs` | DAG 构建、并行组分层、关键路径分析 |
| `executor.rs` | 执行引擎：排名、降级、retry、check、foreach、热补丁加载 |
| `db.rs` | SQLite 统一存储：pipelines/procs/runs/compositions/patches |
| `registry.rs` | 同构发现：tag 匹配 + 跨 pipeline 提示 |
| `learn.rs` | 库学习：频率压缩，静态扫描 tag 序列 |
| `version.rs` | 版本快照：save/log/diff |
| `cli.rs` | CLI 命令分发 |
| `lib.rs` | 库入口 + Python 绑定（pyo3） |

---

## 2. 安装

```bash
# pip（推荐，已发布到 PyPI）
pip install ductile

# 从源码编译
git clone https://github.com/lsmind/ductile.git
cd ductile && cargo build --release
# 二进制：target/release/ductile
```

---

## 3. DSL 语法

### 3.1 Pipeline 头

```
Pipeline("名称", "可选描述")
```

- `名称`：流水线唯一标识
- `描述`（可选）：自然语言描述，用于检索索引

### 3.2 Proc（处理节点）

```
.proc("search")
  .desc("多路搜索")
```

处理步骤。依赖关系自动从 `@proc_name` 引用推导，不需要手动声明 DAG。

**修饰符：**

| 修饰符 | 说明 |
|--------|------|
| `.desc("text")` | proc 级描述 |
| `.plan(...)` | 多路径声明 |
| `.check(cond, "msg")` | 结果检查 |
| `.foreach(source=@proc, var=item)` | 扇出迭代 |
| `.deliver(@proc)` | 终端输出 |

### 3.3 Plan（多路径 + fallback）

```
.plan(
  web -> web_search(query="{topic}")
    .tags(#search, #web)
    .desc("网页搜索")
    .retry(n=3),
  mcp -> mcp_search(query="{topic}")
    .tags(#search, #mcp)
    .desc("MCP搜索")
)
```

**声明顺序 = 优先级。** A 失败 → 自动尝试 B → 直到成功或全挂。

**Impl 修饰符：**

| 修饰符 | 说明 |
|--------|------|
| `.tags(#tag1, #tag2)` | 标签（仅叶子 impl） |
| `.desc("text")` | impl 级描述 |
| `.retry(n=3)` | 指数退避重试（2s→4s→8s） |
| `.ensure(cond, "msg")` | inline check |
| `.when(mode == "deep")` | 条件路由 |
| `.disabled` | 临时禁用 |
| `.stub` | 桩（不执行，返回 `[STUB:name]`） |
| `.cost(latency=100, risk=0.1, tokens=500, money=0.01)` | 声明式成本 |

### 3.4 Tags（标签）

```
.tags(#search, #web)
```

- 自由格式字符串，不需要预定义枚举
- 用 BTreeSet 存储，保证确定性
- Pipeline 效果 = 所有 impl 标签的并集（自动计算，不需声明）

### 3.5 Check（结果检查）

```
.check(result => not_empty, "结果为空")
.check(result => has_items, "结果少于2条")
```

**内置谓词：**

| 谓词 | 判断逻辑 |
|------|---------|
| `not_empty` | 结果非空 |
| `has_results` / `has_content` / `valid_output` | 结果长度 > 20 |
| `has_items` | 结果有 ≥ 2 个非空行 |
| `no_error` | 结果不含 "ERROR"/"Error"/"error" |
| `has_date` | 结果含 2020-2030 的年份 |
| `has_citations` | 结果含 `[1]` 或 `[链接` 或 `来源` |

Check 失败 = 当前路径失败 = 触发降级。

### 3.6 Foreach（扇出迭代）

```
.proc("process_each")
  .foreach(source=@decompose, var=subtask)
  .plan(
    p1 -> llm(input="{subtask}", template="process")
      .tags(#llm, #parse)
  )
```

遍历源 proc 结果的每一行，将 `{subtask}` 替换到下游 body 中。

### 3.7 变量引用

| 语法 | 说明 |
|------|------|
| `{topic}` | 当前主题 |
| `{hash(topic)}` | 主题的 hash |
| `{var_name}` | foreach 变量 |
| `@proc_name` | 上游 proc 的完整输出 |
| `@proc_name.field` | 上游 proc 的结构化字段（DSL_RESULT 协议） |

### 3.8 完整示例

```
Pipeline("research", "搜索主题并撰写报告")
  .proc("search")
    .desc("多路搜索")
    .plan(
      web -> web_search(query="{topic}")
        .tags(#search, #web)
        .desc("网页搜索")
        .retry(n=3),
      mcp -> mcp_search(query="{topic}")
        .tags(#search, #mcp)
        .desc("MCP搜索")
    )
    .check(result => has_results, "搜索无结果")

  .proc("summarize")
    .desc("LLM 总结")
    .plan(
      s1 -> llm(input=@search, template="summary")
        .tags(#llm, #parse)
        .desc("LLM综合")
    )

  .proc("report")
    .desc("写入报告文件")
    .plan(
      w1 -> write(to="~/output/report.md", content=@summarize)
        .tags(#write, #file)
        .desc("写文件")
    )

  .proc("deliver")
    .deliver(@report)
```

---

## 4. CLI 命令

### 4.1 核心命令

```bash
ductile check <file>              # 解析 + 类型检查
ductile run <file> [topic]        # 解析 + 检查 + 执行
ductile graph <file>              # E-graph 结构：并行组 + 关键路径
ductile parse <file>              # 仅解析（展示结构 + 同构提示）
```

**run 的参数格式：**

```bash
ductile run research.pipeline "RISC-V 架构" --mode=deep --lang=zh
```

`--` 之前是 topic，之后是 key=value 参数。

### 4.2 数据库命令

```bash
ductile import <dir|file>         # 导入 pipeline 到 SQLite
ductile search "query"            # 搜索 proc 库（tag + name + description）
ductile compose <name> <desc> <#tag1> <#tag2>...  # 从零件库组装 pipeline
ductile db-stats                  # 数据库统计
```

**compose 示例：**

```bash
ductile compose "my_pipeline" "搜索+翻译+写入" "#search,#web" "#llm,#parse" "#write,#file"
```

每一步展示 top-3 候选 proc + description，自动选出最匹配的。

### 4.3 发现命令

```bash
ductile discover [file]           # 同构发现（跨 pipeline tag 匹配）
ductile learn [dir]               # 频率压缩 / 库学习
```

**discover** 会先导入（如果给了 file），然后展示所有跨 pipeline 的同构 proc 组。

**learn** 默认扫描 `~/projects/ductile/pipelines/` + `experiments/`。可指定目录。

### 4.4 版本命令

```bash
ductile version save <file> "描述"    # 保存版本快照
ductile version log <file>            # 版本历史
ductile version diff <file> v1 v2     # 逐行对比两个版本
```

### 4.5 热补丁命令

```bash
ductile patch <pipeline> <proc> <impl> <field> <value>   # 设置补丁
ductile patch list                                        # 查看所有补丁
ductile patch clear <pipeline>                            # 清除补丁
```

详见 [第 9 节：热补丁](#9-热补丁)。

---

## 5. 执行引擎

### 5.1 执行流程

```
exec_pipeline(topic, params, pipeline)
  1. apply_patches(pl)         ← 从 SQLite 加载热补丁
  2. build_egraph(pl)          ← 构建 DAG
  3. parallel_groups(eg)       ← 拓扑分层
  4. 逐层执行：
     for proc in layer:
       a. filter eligible       ← .when() 条件过滤
       b. rank_impls            ← cost × (1 + penalty)
       c. for impl in ranked:
          - run_impl_with_retry ← 指数退避
          - run_checks          ← check 谓词
          - 成功 → record_run(Ok) → 下一个 proc
          - 失败 → record_run(Fail) → 尝试下一个 impl
       d. 全部失败 → pipeline 失败
```

### 5.2 路径排名

```
score(impl) = base_cost × (1 + penalty)
```

- `base_cost` = `weights.cost_total(impl.cost)`（默认权重：latency×0.001 + risk×10 + tokens×0.0001 + money×1）
- `penalty` 由运行时数据计算（见下）

### 5.3 滑动窗口自适应惩罚

每个 impl 保留最近 20 次执行记录（`WINDOW_SIZE = 20`）：

| 条件 | penalty |
|------|---------|
| 无执行记录 | 0.0 |
| 窗口 < 3 次 | 0.0（冷启动探索） |
| 失败率 ≤ 10% | 0.0 |
| 失败率 > 10% | `e^(7×failure_rate) - 1`（指数惩罚） |
| 连续失败 ≥ 3 次 | `∞`（永久 BLOCKED） |

### 5.4 Retry（指数退避）

```
.retry(n=3)
```

失败后等待 `2^(attempt+1)` 秒再重试：2s → 4s → 8s。

### 5.5 Check 硬门槛

```
.check(result => has_results, "搜索无结果")
```

Check 在 impl 执行成功后运行：
- 通过 → proc 成功
- 不通过 → 当前 impl 标记为 Fail → 触发降级 → 尝试下一个 impl

**废品不进门。** Check 失败会拉低 impl 排名，最终被淘汰。

---

## 6. SQLite 存储

### 6.1 数据库位置

```
~/.local/share/ductile/ductile.db
```

单文件，WAL 模式。所有数据统一存储，不再有 .gcf 文件。

### 6.2 表结构

**pipelines**

| 列 | 类型 | 说明 |
|----|------|------|
| id | INTEGER PK | 自增 |
| name | TEXT UNIQUE | 流水线名 |
| description | TEXT | 描述 |
| source_file | TEXT | 源文件路径 |
| imported_at | TEXT | 导入时间 |

**procs**

| 列 | 类型 | 说明 |
|----|------|------|
| id | INTEGER PK | 自增 |
| name | TEXT | proc 名 |
| pipeline_id | INTEGER FK | 所属 pipeline |
| description | TEXT | 描述 |
| tags | TEXT | 逗号分隔的 tag |
| impl_count | INTEGER | impl 数量 |
| is_deliver | INTEGER | 是否为 deliver |

**runs**

| 列 | 类型 | 说明 |
|----|------|------|
| id | INTEGER PK | 自增 |
| proc_name | TEXT | proc 名 |
| impl_name | TEXT | impl 名 |
| pipeline | TEXT | pipeline 名 |
| status | TEXT | Ok / Fail |
| latency_ms | INTEGER | 延迟 |
| err_hash | TEXT | 错误 hash |
| err_at | TEXT | 错误位置 |
| recorded_at | TEXT | 记录时间 |

**compositions**

| 列 | 类型 | 说明 |
|----|------|------|
| id | INTEGER PK | 自增 |
| name | TEXT | 组合名 |
| description | TEXT | 描述 |
| proc_names | TEXT | 逗号分隔的 proc 名 |
| created_at | TEXT | 创建时间 |

**patches**

| 列 | 类型 | 说明 |
|----|------|------|
| id | INTEGER PK | 自增 |
| pipeline | TEXT | pipeline 名 |
| proc_name | TEXT | proc 名 |
| impl_name | TEXT | impl 名 |
| field | TEXT | 字段名 |
| value | TEXT | 覆盖值 |
| created_at | TEXT | 创建时间 |
| | | UNIQUE(pipeline, proc_name, impl_name, field) |

### 6.3 自动注册

`ductile run` 执行成功后会自动将 pipeline 导入 SQLite（`db::import_pipeline`），不需要手动 import。

---

## 7. 同构发现

### 7.1 原理

两个 proc 如果有**完全相同的 tag 集合**，则视为同构。系统识别但不自动合并——由使用者通过 description 判断是否复用。

### 7.2 三个提示时机

| 命令 | 行为 |
|------|------|
| `parse` | 解析完后展示同构提示 |
| `run` | 执行前展示同构提示 |
| `discover` | 全局扫描所有已注册 proc 的同构组 |

### 7.3 输出格式

```
Isomorphism hints:
  search ≅ find [translate] — 网页检索
  summarize ≅ digest [report] — LLM综合
```

用户看到后可以决定：是否复用已有 proc 而不是重新写一个。

### 7.4 打散重组（compose）

```bash
ductile compose "pipeline_name" "描述" "#search,#web" "#llm,#parse" "#write,#file"
```

按 tag 链逐步搜索候选 proc：
1. 每一步展示 top-3 候选 + description
2. 自动选第一个（best match）
3. 保存到 compositions 表

---

## 8. 库学习

### 8.1 原理

静态扫描 `.pipeline` 文件，提取每个 proc 的 tag 序列，找出出现 ≥ 2 次的重复模式。不依赖执行记录。

### 8.2 用法

```bash
# 默认扫描标准目录
ductile learn

# 指定目录
ductile learn ./my_pipelines
```

### 8.3 输出

```
Library Learning (Frequency Compression)
=========================================

Pipelines scanned: 18
Tag sequences: 42
Total tokens: 87

Discovered patterns:
  [×6] llm → parse  → found in: research, translate, summarize, ...
  [×5] search → web  → found in: research, news, factcheck, ...

Compression summary:
  Original:   87 tokens
  Compressed: 46 tokens
  Ratio:      52.9%
```

---

## 9. 热补丁

### 9.1 设计目的

不改 `.pipeline` 源文件，在运行时覆盖节点属性。适用于：
- 紧急禁用某个不稳定的 impl
- 临时调整 cost 影响排名
- 测试时用 stub 替换真实调用

### 9.2 用法

```bash
# 禁用 research pipeline 中 search 下的 web 路径
ductile patch research search web enabled false

# 调整 cost.latency
ductile patch research search web cost.latency 5000

# 改 retry 次数
ductile patch research summarize s1 retry 5

# 设为 stub（不执行，直接返回占位符）
ductile patch research search mcp stub true

# 查看所有补丁
ductile patch list

# 清除某 pipeline 的所有补丁
ductile patch clear research
```

### 9.3 可补丁字段

| field | value 格式 | 说明 |
|-------|-----------|------|
| `enabled` | `true` / `false` | 启用/禁用 |
| `cost.latency` | 整数 | 延迟成本 |
| `cost.risk` | 浮点数 | 风险成本 |
| `cost.tokens` | 整数 | token 成本 |
| `cost.money` | 浮点数 | 金钱成本 |
| `retry` | 整数 | 重试次数 |
| `stub` | `true` / `false` | 是否为桩 |

### 9.4 工作原理

1. `ductile patch` → 写入 SQLite `patches` 表（UPSERT）
2. `ductile run` → `exec_pipeline` 开头调用 `apply_patches(pl)`
3. `apply_patches` 从 SQLite 加载补丁 → 克隆 pipeline → 按 (pipeline, proc, impl) 匹配 → 覆盖字段
4. stderr 打印 `[patch] proc.impl: field=value`
5. 执行修改后的副本，源文件不动
6. `ductile patch clear` 清除后恢复原始行为

### 9.5 冲突处理

同一 (pipeline, proc, impl, field) 只保留最后一个值（UPSERT 语义）。多次 patch 同一字段以最后一次为准。

---

## 10. 版本快照

### 10.1 用法

```bash
ductile version save research.pipeline "加了 MCP fallback"
# Saved version v1

ductile version save research.pipeline "调整了 check"
# Saved version v2

ductile version log research.pipeline
# v1|加了 MCP fallback
# v2|调整了 check

ductile version diff research.pipeline 1 2
# v1 → v2
#   - .check(result => has_results, "搜索无结果")
#   + .check(result => has_items, "结果太少")
```

### 10.2 存储位置

```
~/.local/share/ductile/versions/<pipeline_name>/
  ├── meta.log          # 版本历史
  ├── v1.snapshot       # 快照内容
  └── v2.snapshot
```

---

## 11. DSL_RESULT 协议

### 11.1 设计目的

让任何外部脚本返回结构化数据，**不需要修改引擎代码**。只需在 stdout 末尾打几行。

### 11.2 格式

```bash
#!/bin/bash
# 正常输出...
echo "处理完成"

# 末尾加上结构化数据块
echo "##DSL_RESULT"
echo "path=/tmp/output.mp4"
echo "duration=5.2"
echo "frames=243"
echo "##DSL_END"
```

### 11.3 下游引用

```
.proc("encode")
  .plan(
    e1 -> run("ffmpeg -i {topic} output.mp4")
      .tags(#encode, #gpu)
  )

.proc("verify")
  .plan(
    v1 -> run("ffprobe @encode.path")
      .tags(#parse, #file)
  )
```

`@encode.path` 引用 `##DSL_RESULT` 块中的 `path` 字段。

引擎内部用 `§§FIELDS§§` 前缀编码结构化结果，`@proc.field` 解析时提取对应字段。如果没找到字段则输出 `<no field:proc.field>`。

---

## 12. 内置函数

| 函数 | 签名 | 说明 |
|------|------|------|
| `web_search` | `web_search(query="...")` | 网页搜索（通过 bridge 脚本） |
| `mcp_search` | `mcp_search(query="...", engine=zai)` | MCP 搜索 |
| `llm` | `llm(input=@prev, template="...", count=5)` | LLM 调用 |
| `write` | `write(to="path", content=@prev)` | 写文件 |
| `read` | `read(from="path")` | 读文件 |
| `run` | `run("shell command")` | 执行 shell 命令 |
| `sh` | `sh("shell command")` | `run()` 的别名 |
| `merge` | `merge(@a, @b, dedup)` | 合并多个结果（可选去重） |

### Bridge 脚本

`web_search` / `mcp_search` / `llm` 通过 Python bridge 脚本执行，查找路径：

1. `/home/beef/projects/pipeline-dsl/bridge/<script>.py`
2. `~/.local/share/ductile/bridge/<script>.py`

---

## 附录：测试

```bash
cargo test --lib
```

98 个测试，覆盖：AST 构造、parser 解析、typecheck、egraph 拓扑、executor 排名/降级/penalty/check/retry/foreach、DSL_RESULT 编解码、registry 同构匹配、learn 频率压缩、version 快照、db 初始化。
