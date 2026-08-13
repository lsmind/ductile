# Ductile

> 声明式流水线引擎 —— 声明意图，引擎自动处理路由、降级和质量控制。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-98%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.5.0-blue.svg)](https://pypi.org/project/ductile/)

## 这是什么

Ductile 是一个**声明式流水线引擎**。你写一个 `.pipeline` 文本文件，声明"做什么"和"有哪些备选路径"，引擎负责"怎么选、怎么降级、怎么淘汰"。

它解决的核心问题是：**把不确定性管理从人工判断变成引擎的自适应数值优化。**

### 与同类工具的区别

| 能力 | Ductile | LangGraph / AutoGen | Airflow / Prefect | 传统路由层 |
|------|---------|---------------------|--------------------|-----------|
| 声明式流程定义 | ✅ 纯文本 DSL | ⚠️ 代码+图 | ✅ DAG | ❌ 仅模型级 |
| 编译时类型检查 | ✅ check 命令 | ❌ 运行时 | ⚠️ 部分 | ❌ 无 |
| 多路径自动降级 | ✅ 内核能力 | ❌ 需手写条件边 | ❌ 需重试逻辑 | ⚠️ 静态路由 |
| 运行时自适应惩罚 | ✅ 滑动窗口闭环 | ❌ 无 | ❌ 无 | ❌ 无 |
| 同构发现 + 打散重组 | ✅ tag 匹配 + compose | ❌ 无 | ❌ 无 | ❌ 无 |
| 关键路径静态分析 | ✅ graph 命令 | ❌ 仅运行时 trace | ⚠️ 运行时 | ❌ 无 |
| 零改接入外部工具 | ✅ DSL_RESULT 协议（5 行 echo） | ❌ 需写 Tool 类 | ❌ 需写 Operator | ⚠️ 需适配器 |
| SQLite 统一存储 | ✅ 单文件 ductile.db | ❌ | ✅ 需外部 DB | ❌ |
| 版本快照 | ✅ 内置 version 命令 | ❌ | ❌ | ❌ |
| 外部依赖 | 0（纯 Rust std + SQLite） | Python 生态 | Python + DB | 各异 |

**一句话**：路由层只解决"选哪个模型"，编排框架只解决"怎么连起来"，Ductile 把"怎么选 + 怎么降级 + 怎么越跑越聪明 + 怎么从零件库组装新流水线"塞进一个引擎。

## 安装

```bash
# pip（推荐，已发布到 PyPI）
pip install ductile

# 从源码编译
git clone https://github.com/lsmind/ductile.git
cd ductile && cargo build --release
# 二进制在 target/release/ductile
```

## 30 秒上手

写一个 `.pipeline` 文件：

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

运行：

```bash
ductile run research.pipeline "RISC-V 架构"
```

## 核心特性

### 1. 声明意图，不写控制流

不写 `if/else/try/catch`。声明"有哪些路径"，引擎自动处理排序、降级、短路：

```
.plan(
  fast -> quick_api(query="{topic}")
    .tags(#search, #api),
  slow -> deep_search(query="{topic}")
    .tags(#search, #web)
)
```

A 失败 → 立即尝试 B → 直到成功或全挂。**零人工干预。**

### 2. 运行时自适应：越跑越聪明

每个路径保留最近 20 次执行记录。引擎根据实际数据自动调整：

| 情况 | 引擎行为 |
|------|---------|
| 失败率 ≤ 10% | 无惩罚，维持声明优先级 |
| 失败率 > 10% | 指数权重惩罚，排名下降 |
| 连续失败 3 次 | 自动 BLOCKED，永久跳过 |
| 冷启动（< 3 次） | 按声明顺序探索 |

**不需要手调 cost 参数。声明意图，执行数据自动排序。**

### 3. E-graph 静态分析

跑之前就知道瓶颈在哪：

```bash
$ ductile graph research.pipeline

E-Graph:
  Nodes: 4
  Edges: 3

Parallel groups:
  ["search"]
  ["summarize"]
  ["report"]

Critical path: ["search", "summarize", "report"]
```

并行组（可同时执行的节点）和关键路径（最长依赖链）一目了然。

### 4. Check 硬门槛

结果不达标 = 当前路径失败 = 触发降级：

```
.check(result => has_results, "搜索无结果")
.check(result => has_items, "结果少于2条")
```

**废品不进门。** check 失败的路径会被记录为失败，拉低排名，最终被淘汰。

### 5. SQLite 统一存储

所有数据——pipeline 库、proc 注册、执行记录、组合方案——全部存在单文件 `ductile.db` 中：

```bash
# 导入 pipeline 到库
ductile import ./pipelines

# 搜索 proc 库
ductile search "llm"
ductile search "search"

# 查看数据库统计
ductile db-stats
```

输出：
```
Ductile SQLite Database
=======================
  Pipelines:     3
  Procs:         12
  Run records:   47
  Compositions:  2
  Location:      "~/.local/share/ductile/ductile.db"
```

### 6. 同构发现 + 打散重组

导入多个 pipeline 后，自动识别跨 pipeline 的同构 proc（tag 集合相同的节点）：

```bash
ductile discover
```

输出：
```
Proc Registry — Isomorphism Discovery
=====================================

  {#search, #web} — 3 procs
    search — 多路搜索 [research]
    find — 网页检索 [translate]
    lookup — 在线查找 [summarize]
```

然后从零件库组装新流水线：

```bash
ductile compose "my_pipeline" "搜索+翻译+写入" "#search,#web" "#llm,#parse" "#write,#file"
```

每一步展示 top-3 候选 proc + description，系统自动选出最匹配的。

### 7. 库学习（频率压缩）

扫描所有 pipeline 文件，发现重复的 tag 序列模式：

```bash
ductile learn ./pipelines
```

输出：
```
Library Learning (Frequency Compression)
=========================================

Pipelines scanned: 18
Tag sequences: 42
Total tokens: 87

Discovered patterns:
  [×6] llm → parse  → found in: research, translate, summarize, ...
  [×5] search → web  → found in: research, news, factcheck, ...
  [×3] file → write  → found in: research, report, export, ...

Compression summary:
  Original:   87 tokens
  Compressed: 46 tokens
  Ratio:      52.9%
```

### 8. 版本快照

不需要 git 就能追踪 pipeline 变更：

```bash
ductile version save research.pipeline "加了 MCP fallback"
ductile version log  research.pipeline
ductile version diff research.pipeline 1 3
```

### 9. DSL_RESULT 协议：零改接入

任何脚本只需在 stdout 末尾打几行，就能返回结构化数据：

```bash
#!/bin/bash
echo "处理完成"

echo "##DSL_RESULT"
echo "path=/tmp/output.mp4"
echo "duration=5.2"
echo "##DSL_END"
```

下游通过 `@proc.field` 引用具体字段。**新工具接入零改代码。**

### 10. 标签系统

自由格式 tag 做结构识别，不需要预定义枚举：

```
.tags(#search, #web)
```

标签用 BTreeSet 存储，保证确定性。Pipeline 效果 = 所有 impl 标签的并集（自动计算）。

## DSL 语法参考

| 语法 | 说明 |
|------|------|
| `Pipeline("name", "desc")` | 流水线头 |
| `.proc("name")` | 处理节点，依赖自动从 `@ref` 推导 |
| `.desc("text")` | proc/impl 级描述 |
| `.plan(a -> body, b -> body)` | 多路径，声明顺序=优先级 |
| `.tags(#tag1, #tag2)` | 标签（仅叶子 impl） |
| `.check(result => pred, "msg")` | 结果检查 |
| `.retry(n=3)` | 指数退避重试 |
| `.foreach(source=@proc, var=item)` | 扇出迭代 |
| `.when(mode == "deep")` | 条件路由 |
| `.disabled` | 临时禁用 |
| `.deliver(@proc)` | 终端输出 |

**内置谓词**：`not_empty`、`has_results`、`has_items`、`has_content`、`no_error`、`has_date`、`has_citations`

**内置函数**：`web_search`、`mcp_search`、`llm`、`write`、`read`、`run`/`sh`、`merge`

## CLI 命令

| 命令 | 说明 |
|------|------|
| `check <file>` | 解析 + 类型检查 |
| `run <file> [topic]` | 解析 + 检查 + 执行 |
| `graph <file>` | E-graph 结构：并行组 + 关键路径 |
| `parse <file>` | 仅解析（展示结构 + 同构提示） |
| `import <dir\|file>` | 导入 pipeline 到 SQLite |
| `search "query"` | 搜索 proc 库 |
| `compose <name> <desc> <#tags...>` | 从零件库组装 pipeline |
| `db-stats` | 数据库统计 |
| `discover [file]` | 同构发现 |
| `learn [dir]` | 频率压缩 / 库学习 |
| `version save <file> "desc"` | 保存版本快照 |
| `version log <file>` | 版本历史 |
| `version diff <file> v1 v2` | 版本对比 |

## 测试

```bash
cargo test --lib
```

98 个测试，全部通过。

## 设计哲学

- **声明意图，不声明猜测值** — 不需要手写 cost 数字，引擎自己学
- **机制替代意志力** — 连续失败自动 BLOCKED，不需要人盯
- **tag 做索引，description 做判断** — 系统识别结构，决定权在使用者

## License

MIT
