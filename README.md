# Ductile

> 声明式流水线 DSL —— 声明意图，引擎自动处理路由、降级和质量控制。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-90%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.4.1-blue.svg)](https://pypi.org/project/ductile/)

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
| 关键路径静态分析 | ✅ graph 命令 | ❌ 仅运行时 trace | ⚠️ 运行时 | ❌ 无 |
| 零改接入外部工具 | ✅ DSL_RESULT 协议（5 行 echo） | ❌ 需写 Tool 类 | ❌ 需写 Operator | ⚠️ 需适配器 |
| 外部依赖 | 0（纯 Rust std） | Python 生态 | Python + DB | 各异 |

**一句话**：路由层只解决"选哪个模型"，编排框架只解决"怎么连起来"，Ductile 把"怎么选 + 怎么降级 + 怎么越跑越聪明"塞进一个引擎。

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
  fast -> quick_api(query="{topic}")     // 优先尝试
    .tags(#search, #api),
  slow -> deep_search(query="{topic}")   // 失败后自动滑到这里
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

### 5. 版本快照（Haskell 版）

Haskell 版提供版本管理命令，不需要 git 就能追踪 pipeline 变更：

```bash
ductile version save research.pipeline "加了 MCP fallback"   # 保存快照
ductile version log  research.pipeline                        # 查看历史
ductile version diff research.pipeline 1 3                    # 对比两个版本
```

### 6. DSL_RESULT 协议：零改接入

任何脚本只需在 stdout 末尾打几行，就能返回结构化数据：

```bash
#!/bin/bash
# 你的脚本照常输出...
echo "处理完成"

# 末尾加上这5行
echo "##DSL_RESULT"
echo "path=/tmp/output.mp4"
echo "duration=5.2"
echo "##DSL_END"
```

下游通过 `@proc.field` 引用具体字段。**新工具接入零改 Haskell/Rust 代码。**

### 7. 标签系统

自由格式 tag 做结构识别，不需要预定义枚举：

```
.tags(#search, #web)
.tags(#llm, #parse)
```

标签用 BTreeSet 存储，保证确定性。Pipeline 效果 = 所有 impl 标签的并集（自动计算）。

## DSL 语法参考

### Pipeline 头

```
Pipeline("名称", "可选描述")
```

### Proc（处理节点）

```
.proc("search")
  .desc("多路搜索")           // 可选描述
```

处理步骤。依赖关系自动从 `@proc_name` 引用推导，不需要手动声明 DAG。

### Plan（多路径 + fallback）

```
.plan(
  path_a -> body.tags(#tag1),
  path_b -> body.tags(#tag2)
)
```

声明顺序 = 优先级。

### Tags

```
.tags(#search, #web)
```

只在叶子 impl 上声明。

### Description

```
.proc("search")
  .desc("多路并行搜索")    // proc 级

web_search(query="{topic}")
  .desc("网页搜索")        // impl 级
```

### Check

```
.check(result => not_empty, "结果为空")
```

内置谓词：`not_empty`、`has_results`、`has_items`、`has_content`、`no_error`、`has_date`、`has_citations`。

### Retry

```
.retry(n=3)
```

指数退避：2s → 4s → 8s。

### Foreach（扇出迭代）

```
.foreach(source=@decompose, var=subtask)
```

遍历源 proc 结果的每一行，将 `{subtask}` 替换到下游 body。

### When（条件路由）

```
.when(mode == "deep")
```

### Disabled

```
.disabled
```

### Deliver（终端输出）

```
.proc("deliver")
  .deliver(@report)
```

## CLI 命令

| 命令 | 说明 |
|------|------|
| `check <file>` | 解析 + 类型检查（跑之前拦错） |
| `run <file> [topic]` | 解析 + 检查 + 执行 |
| `graph <file>` | E-graph 结构：并行组 + 关键路径 |
| `parse <file>` | 仅解析（展示 AST 结构） |
| `version save <file> "desc"` | 保存版本快照（Haskell 版） |
| `version log <file>` | 查看版本历史（Haskell 版） |
| `version diff <file> v1 v2` | 对比两个版本（Haskell 版） |

## 内置函数

| 函数 | 说明 |
|------|------|
| `web_search(query="...")` | 网页搜索（通过 bridge 脚本） |
| `mcp_search(query="...", engine=zai)` | MCP 搜索 |
| `llm(input=@prev, template="...", count=5)` | LLM 调用 |
| `write(to="path", content=@prev)` | 写文件 |
| `read(from="path")` | 读文件 |
| `run("shell command")` / `sh(...)` | 执行 shell 命令 |
| `merge(@a, @b, dedup)` | 合并多个结果 |

## 设计哲学

- **声明意图，不声明猜测值** — 不需要手写 cost 数字，引擎自己学
- **机制替代意志力** — 连续失败自动 BLOCKED，不需要人盯
- **tag 做索引，description 做判断** — 系统识别结构，决定权在使用者

## 测试

```bash
cargo test --lib
```

90 个测试，全部通过。

## License

MIT
