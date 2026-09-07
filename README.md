# Ductile

> 声明式流水线引擎 —— 声明意图，引擎自动处理路由、降级和质量控制。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-271%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.6.1-blue.svg)](https://pypi.org/project/ductile/)

---

## Ductile 是什么

你写一个 `.pipeline` 文本文件，声明"做什么"和"有哪些备选路径"。引擎负责剩下的全部：选哪条路、怎么降级、什么时候淘汰废路径。

一句话：**把不确定性管理从人工判断变成引擎的自适应数值优化。**

### 和现有工具的区别

| 能力 | Ductile | LangGraph / AutoGen | Airflow / Prefect | 传统路由层 |
|------|---------|---------------------|--------------------|-----------|
| 声明式流程定义 | ✅ 纯文本 | ⚠️ 代码+图 | ✅ DAG | ❌ |
| 多路径自动降级 | ✅ 内核 | ❌ 手写 | ❌ 手写 | ⚠️ 静态 |
| e-graph 等价类 + equality saturation | ✅ v0.10 | ❌ | ❌ | ❌ |
| CSE（等价 proc 只跑一次） | ✅ v0.10 | ❌ | ❌ | ❌ |
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

  .proc("gate")
    .plan(
      g -> run("judge.sh {topic}")   # 裁判独立：stdout 末尾输出 ##DSL_RESULT score=85
        .tags(#judge)
    )

  .proc("write_report")
    .when(@gate.score < 80)          # 分数不达标 → 走返工路径，废品不进门
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

web 路径失败 → 自动滑到 mcp；裁判打分 < 80 → deliver 被门住（fail-closed）。连续失败 3 次 → 永久跳过。**你只管声明，引擎自己学。**

## 核心能力

### e-graph 等价类 + equality saturation（v0.10 新增）

`.pick(egraph)` 一行开启。等价 proc 自动并入同一 e-class（同构合并、merge 交换/结合律、write→read 对消），执行时每 class 只跑一个代表，其余 CSE 共享结果。5 procs 的管线压成 3 classes、少跑 2 次重复计算——多路径选择的数学本体从"贪心排序"升级为"等价类提取"。**when-载体守卫（v0.11.1）**：挂 `.when()` 裁判路由的 impl 不参与熔合——否则 judge→consumer 依赖边会被抹掉，deliver 抢跑、判决落空。

```bash
ductile graph your.pipeline   # 看 e-class 明细 / 融合规则命中 / CSE 别名 / 静态提取计划
```

### 声明意图，不写控制流

不写 `if/else/try/catch`。声明路径和优先级，引擎自动排序、降级、短路。

### 越跑越聪明

每个路径保留最近 20 次记录。失败率 > 10% → 指数惩罚。连续失败 3 次 → 自动 BLOCKED。不需要手调参数。

### 评价与流程分离（裁判分离）

**评价与流程分离（v0.11「裁判分离」）**：`.pipeline` 只描述流程；权重与 cost 来源写在 `.eval` 策略文件，`ductile run x.pipeline "topic" --policy p.eval` 挂载。cost 支持两形态：直接数值，或 `latency=measure("bench.sh {topic}")` 链接测试脚本实测取值（缓存 24h）。`.cost()/.check()/.ensure()` 已退役——质量门槛改用独立 judge proc（输出 `##DSL_RESULT score=N`）+ `.when(@judge.score < 80)` 路由；未知函数 fail-closed（`<noop>` 假成功已删除）。执行实测 latency_ms 落库。

`.when(cond)` 支持两种写法（v0.11.1）：内联（`x -> body.when(cond)`，impl 级）与块级（`.when(cond)` 独立成行，下推到该 proc 全部 impls，内联优先）。块级条件里的 `@judge.field` 引用自动建 DAG 边保证裁判先执行；求值 fail-closed（坏条件/缺席裁判不放行）。实测：`score=72` → deliver 放行；`score=85` → `All paths failed for proc: deliver` + degraded 置位（exit 1）。

### 裁判分离的质量门槛（v0.11）

`.cost()/.check()/.ensure()` 已退役。质量门槛 = 独立 judge proc（输出 `##DSL_RESULT score=N`）+ 下游 `.when(@judge.score < 80)` 路由——产出者不自证清白，裁判缺席/坏条件一律不放行（fail-closed）。

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

## LangChain 插件接口（v0.13）

Python 混合包（Rust 核心 + Python 适配层），`ductile.langchain_tools()` 一行接入：

```python
import ductile

tools = ductile.langchain_tools()   # 6 个内省/执行工具 + 每个脚本契约一个类型化工具
agent = create_react_agent(llm, tools, prompt)   # 或任何 LangChain Agent
```

- **6 个基础工具**：`ductile_list_scripts` / `ductile_list_procs` / `ductile_pipeline_info` / `ductile_stats` / `ductile_recent_runs` / `ductile_run` / `ductile_call_script`
- **每脚本契约一个类型化工具**：从契约卡自动生成 docstring + 参数 schema（`ductile_script_word_stats(text: str)`），LLM 不读脚本体
- **失败是数据不是异常**：执行失败返回 `{"ok":false,"error":...}`，agent 可按 JSON 路由；创作错误（解析/类型检查）才抛异常
- 脚本 attach 后重新调用 `langchain_tools()` 即时刷新工具集

## 代码结构

```
src/
├── parser.rs     # DSL 解析（.pipeline / .eval 策略）
├── ast.rs        # 类型（Impl/Proc/Pipeline/Policy/CostValue）
├── typecheck.rs  # 类型检查
├── executor.rs   # 编排主干（pipeline/proc/foreach/retry）
├── steps.rs      # step_registry + 21 个内置执行器（fs/进程/读写/run/llm/search/script）
├── textargs.rs   # 纯文本解析原语（detect_func/resolve_vars/extract_*）
├── dslresult.rs  # ##DSL_RESULT 协议编解码
├── ranking.rs    # 偏好学习/排序/失败惩罚
├── eval.rs       # Evaluator/CostSource/CostCache（裁判分离运行时）
├── egraph.rs     # e-graph 等价类 + CSE + when-载体守卫
├── db.rs         # SQLite（*_conn 注入内核，测试用内存库）
├── script.rs     # v0.12 脚本契约（脚本即 API）
├── api.rs        # v0.13 富 API 层（core+pyo3 薄壳双形态，LangChain 共用）
└── cli.rs        # 命令分发 + 纯参数解析
```

17 模块各带单元测试；总 271 个测试（纯函数直测 + 内存库回路 + 真子进程集成），`cargo test --lib` 一条命令全跑。

## 设计哲学

- **声明意图，不声明猜测值** — 不需要手写 cost 数字，引擎自己学
- **机制替代意志力** — 连续失败自动 BLOCKED，不需要人盯
- **tag 做索引，description 做判断** — 系统识别结构，决定权在使用者

## 测试

```bash
cargo test --lib
```

271 个测试，全部通过。覆盖：解析原语、##DSL_RESULT 协议、内存库 CRUD/TTL 回路、偏好学习收敛、e-graph 熔合守卫、fs/进程算子（真子进程）、JSON 解析器（含 UTF-16 代理对）。

## License

MIT
