# Ductile

> 声明式流水线引擎 —— 声明意图，引擎自动处理路由、降级和质量控制。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-98%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.5.0-blue.svg)](https://pypi.org/project/ductile/)

---

## Ductile 是什么

你写一个 `.pipeline` 文本文件，声明"做什么"和"有哪些备选路径"。引擎负责剩下的全部：选哪条路、怎么降级、什么时候淘汰废路径。

一句话：**把不确定性管理从人工判断变成引擎的自适应数值优化。**

### 和现有工具的区别

| 能力 | Ductile | LangGraph / AutoGen | Airflow / Prefect | 传统路由层 |
|------|---------|---------------------|--------------------|-----------|
| 声明式流程定义 | ✅ 纯文本 | ⚠️ 代码+图 | ✅ DAG | ❌ |
| 多路径自动降级 | ✅ 内核 | ❌ 手写 | ❌ 手写 | ⚠️ 静态 |
| 运行时自适应惩罚 | ✅ 独家 | ❌ | ❌ | ❌ |
| 同构发现 + 打散重组 | ✅ | ❌ | ❌ | ❌ |
| 热补丁（不改源文件） | ✅ | ❌ | ❌ | ❌ |
| SQLite 统一存储 | ✅ 单文件 | ❌ | ✅ 外部 DB | ❌ |
| 版本快照 | ✅ 内置 | ❌ | ❌ | ❌ |
| 零改接入外部工具 | ✅ 5 行 echo | ❌ Tool 类 | ❌ Operator | ⚠️ 适配器 |
| 零外部服务依赖 | ✅ 内嵌 SQLite，无 DB server | ❌ Python 生态 | ✅ 需外部 DB | 各异 |

**路由层只解决"选哪个模型"，编排框架只解决"怎么连起来"。Ductile 把选择 + 降级 + 自学习 + 热补丁 + 零改接入塞进一个引擎。**

## 安装

```bash
pip install ductile
```

## 30 秒看懂

写一个 `.pipeline` 文件：

```
Pipeline("research")
  .proc("search")
    .plan(
      web -> web_search(query="{topic}")
        .tags(#search, #web)
        .retry(n=3),
      mcp -> mcp_search(query="{topic}")
        .tags(#search, #mcp)
    )
    .check(result => has_results, "搜索无结果")

  .proc("write_report")
    .plan(
      w -> write(to="~/output/report.md", content=@search)
        .tags(#write, #file)
    )

  .proc("deliver")
    .deliver(@write_report)
```

运行：

```bash
ductile run research.pipeline "RISC-V 架构"
```

web 路径失败 → 自动滑到 mcp → check 不过 → 触发降级。连续失败 3 次 → 永久跳过。**你只管声明，引擎自己学。**

## 核心能力

### 声明意图，不写控制流

不写 `if/else/try/catch`。声明路径和优先级，引擎自动排序、降级、短路。

### 越跑越聪明

每个路径保留最近 20 次记录。失败率 > 10% → 指数惩罚。连续失败 3 次 → 自动 BLOCKED。不需要手调参数。

### 率失真感知排序（RD）

快但有损的路径不再无条件赢。每次执行自动测量 token 消耗（rate）与字段保留度（est_loss），排序时按 `weights.rd × (Σest_loss / Σrate_tokens)` 加附加费——同样预算下信息损耗大的路径排后。默认关闭（`weights.rd = 0`），惩罚域（瞬时故障）与失真域（信息质量）分家计费，互不重复。

### Check 硬门槛

结果不达标 = 当前路径失败 = 触发降级。废品不进门。

### 热补丁

不改源文件，一行命令禁用/调整任意节点：

```bash
ductile patch research search web enabled false
ductile patch research summarize s1 retry 5
```

### 同构发现 + 打散重组

导入多个 pipeline 后，自动识别跨 pipeline 的相似节点。从零件库按 tag 组装新流水线。

### SQLite 统一存储

pipeline 库、执行记录、proc 注册、组合方案、热补丁——全部一个 `.db` 文件。

### 零改接入

任何脚本 stdout 末尾打 5 行就能返回结构化数据，引擎自动解析，下游直接 `@proc.field` 引用。

---

## 🤖 让 AI 帮你用 Ductile

Ductile 专为 AI 协作设计。**把 [SPEC.md](SPEC.md) 喂给你的 AI 助手**，它就能：

1. **自动安装** — `pip install ductile`
2. **编写 pipeline** — 按 SPEC 语法生成 `.pipeline` 文件
3. **执行和管理** — 调用全部 CLI 命令
4. **热补丁调优** — 根据执行结果动态 patch
5. **同构发现** — 跨 pipeline 搜索可复用节点

### 使用方法

把下面这段话发给你的 AI 助手（ChatGPT / Claude / Gemini / 任何支持工具调用的 Agent）：

> 请阅读以下 Ductile DSL 规格文档，然后帮我完成流水线任务。
> 
> [粘贴 SPEC.md 全文，或提供链接：https://github.com/lsmind/ductile/blob/main/SPEC.md]

AI 读完 SPEC 后，你就可以直接用自然语言下指令：

- *"帮我写一个搜索+总结+写报告的 pipeline"*
- *"把 search 节点的 web 路径禁掉，看看效果"*
- *"分析一下我这几个 pipeline 有没有可以复用的节点"*
- *"给 research pipeline 加个版本快照"*

AI 会自动生成正确的 `.pipeline` 文件和 CLI 命令。

---

## 设计哲学

- **声明意图，不声明猜测值** — 不需要手写 cost 数字，引擎自己学
- **机制替代意志力** — 连续失败自动 BLOCKED，不需要人盯
- **tag 做索引，description 做判断** — 系统识别结构，决定权在使用者

## 测试

```bash
cargo test --lib
```

102 个测试，全部通过（含 RD 排序、est_loss v1 字段覆盖度、反双重计费回归门禁）。

## License

MIT
