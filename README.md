# Ductile DSL

> 声明式流水线 DSL —— 用文本描述工作流，运行时自动学习最优路径。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-90%20passed-brightgreen.svg)](#测试)

## 这是什么

Ductile 让你把数据处理流水线写成一个简单的 `.pipeline` 文本文件：

- **零配置** — 不需要声明 cost、weight、level。声明意图，不声明猜测值
- **多路径容错** — 声明顺序即优先级，A 路径失败自动尝试 B
- **运行时学习** — 执行记录自动跟踪成功率 + 延迟，好路径排名上升，连续 3 次失败的路径自动 BLOCKED
- **标签系统** — 自由格式 tag 做同构识别和检索，不需要预定义枚举
- **零外部依赖** — 纯 Rust std，二进制约 400KB，启动 1ms

## 安装

**Cargo（推荐）：**

```bash
cargo install --path .
```

**pip（Python 绑定）：**

```bash
pip install ductile
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
    .check(not_empty, "搜索无结果")

  .proc("summarize")
    .desc("LLM 总结")
    .plan(
      s1 -> llm(input=@search, template="summary")
        .tags(#llm, #parse)
        .desc("LLM综合")
    )
    .check(not_empty, "总结为空")

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

其他命令：

```bash
ductile check research.pipeline    # 解析 + 类型检查
ductile parse research.pipeline    # 查看解析结构
ductile graph research.pipeline    # 查看 DAG 依赖图
ductile stats research.pipeline    # 查看执行统计
ductile tags research.pipeline     # 查看计算出的标签
```

## DSL 语法

### Pipeline 头

```
Pipeline("名称", "描述")
```

- `名称` — 流水线唯一标识
- `描述`（可选）— 自然语言描述，用于检索索引

### Proc（处理节点）

```
.proc("search")
  .desc("多路搜索")      // 可选描述
```

处理步骤。依赖关系自动从 `@proc_name` 引用推导。

### Plan（多路径 + fallback）

```
.plan(
  path_a -> body.tags(#tag1),
  path_b -> body.tags(#tag2)
)
```

声明顺序 = 优先级。A 失败 → 自动尝试 B。

### Tags（标签）

```
.tags(#search, #web)
```

只在叶子 impl 上声明。Pipeline 效果 = 所有 impl 标签的并集（自动计算）。

标签用 BTreeSet 存储，保证确定性匹配。

### Description（描述）

```
.proc("search")
  .desc("多路并行搜索")    // proc 级描述

web_search(query="{topic}")
  .desc("网页搜索")        // impl 级描述
```

proc 和 impl 都可以加描述。用途：
- 跨流水线同构识别（tag 匹配 + description 语义判断）
- 搜索引擎索引
- AI 决定是否复用已有节点

### Check（结果检查）

```
.check(not_empty, "结果为空")
```

核心谓词：`not_empty`、`no_error`、`min_length(N)`、`has_items`、`has_content`。

### Retry（重试）

```
.retry(n=3)
```

指数退避：2s → 4s → 8s。

### Foreach（扇出迭代）

```
.foreach(source=@decompose, var=subtask)
```

遍历源 proc 结果的每一行，将 `{subtask}` 替换到下游 body 中。

### When（条件路由）

```
.when(mode == "deep")
```

### Disabled（临时禁用）

```
.disabled
```

### Deliver（终端输出）

```
.proc("deliver")
  .deliver(@report)
```

## 路径选择（零参数）

完全运行时驱动，不需要调参：

| 阶段 | 策略 |
|------|------|
| 冷启动（< 3 次执行） | 声明顺序优先，探索新路径 |
| 有数据后 | 按最近 20 次成功率排序（高→低），相同则按延迟（低→高） |
| 连续 3 次失败 | 自动 BLOCKED，跳过该路径 |

## 内置函数

| 函数 | 说明 |
|------|------|
| `web_search(query="...")` | 网页搜索（通过 bridge 脚本） |
| `mcp_search(query="...", engine=zai)` | MCP 搜索 |
| `llm(input=@prev, template="...")` | LLM 调用（通过 bridge 脚本） |
| `write(to="path", content=@prev)` | 写文件 |
| `read(from="path")` | 读文件 |
| `run("shell command")` | 执行 shell 命令 |
| `sh("shell command")` | `run()` 的别名 |
| `merge(@a, @b)` | 合并多个结果 |

## CLI 命令

| 命令 | 说明 |
|------|------|
| `check <file>` | 解析 + 类型检查 |
| `run <file> [topic]` | 解析 + 检查 + 执行 |
| `graph <file>` | 查看 DAG 结构（并行组、关键路径） |
| `parse <file>` | 仅解析（展示结构） |
| `stats <file>` | 查看执行统计 |
| `tags <file>` | 查看标签 |

## DSL_RESULT 协议

外部脚本通过 `run()`/`sh()` 可以返回结构化数据：

```
##DSL_RESULT
path=/tmp/output.mp4
duration=5.2
frames=243
##DSL_END
```

下游通过 `@proc.field` 引用具体字段。

## GCF 记录格式

每次执行追加到 `~/.local/share/ductile/records/<proc>/runs.gcf`：

```
GCF profile=generic
## runs
2026-08-13T00:04:51|web|Ok|3124|A|-|-
2026-08-13T00:04:52|mcp|Fail|5100|B|a1b2c3d4|search.mcp.step
```

字段：`时间戳 | 路径 | 状态 | 延迟ms | 选择 | 错误哈希 | 错误位置`

## 测试

```bash
cargo test --lib
```

90 个测试，全部通过。

## 性能

| 指标 | 值 |
|------|-----|
| 启动时间 | ~1ms |
| 外部依赖 | 0（纯 std） |
| 二进制大小 | ~400KB |
| 测试数量 | 90 |

## 设计哲学

- **声明意图，不声明猜测值** — 不需要手写 cost 数字，引擎自己学
- **tag 做索引，description 做判断** — 系统识别同构，AI 决定是否复用
- **机制替代意志力** — 连续失败自动 BLOCKED，不需要人盯

## License

MIT
