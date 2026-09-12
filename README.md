# Ductile

> 声明式 LLM 流水线引擎 —— 把质量从玄学变成结构。Rust 单二进制，SQLite 单文件全记录。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/lsmind/ductile/actions/workflows/ci.yml/badge.svg)](https://github.com/lsmind/ductile/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-418%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.13.1-blue.svg)](https://pypi.org/project/ductile/)

---

## 先说一个行业真相

把 LLM 串成流水线，质量不升反降。我们实测（三轮独立盲评，judge=27B）：

| 配置 | 盲评分 | 结论 |
|---|---|---|
| 裸 27B 单发 | 66.7–79.3 | 有波动但没有结构病 |
| 五段结构化链 | **56.7–59.3** | **比裸模型差 16 分** |
| + `.constraint` 链级约束继承 | 差距 -16.3 → -9.7 | 约束不再逐跳蒸发 |
| + owner 槽位（约束有了落点） | **+14.7，反超裸模型** | 结构第一次赢 |
| + 问询链（防臆造） | **82.7** | 裸模型永远到不了 |

**约束逐跳衰减：链上每加一段，约束被稀释一分，上限 = 最弱一段。** 这是结构化管线自身的病，不是模型的错。大多数框架假装这病不存在——Ductile 是第一个把它当引擎结构问题对待、并给出完整实测治疗曲线的。

## 三个别人没有的优势

### 1. 质量靠可证伪循环，不靠玄学

```text
探针（零 LLM，判"有没有错"）→ 盲评（判"好不好"）→ 医生 LLM（判"为什么+怎么改"）
→ 处方 apply → 重跑 → 探针复检 → 预期不兑现 → 自动回滚
```

实测：9B 模型输出烂任务单（42.7 分），机器医生读探针报告开出 4 张处方，42.7 → **64.0**（+21.3）。其中一张处方触发"算术反噬"导致输出崩溃——进化环当场捕获并选择性回滚，复检 88 分。**机器处方与人工三轮实锤的处方逐条对齐，还比人工多抓一处。** LLM 的每次建议都自带死刑复核程序。

### 2. LLM 是系统里的节点，不是系统的中心

- **LLM 输出一律不进 shell**（引擎物理禁止，不是提示词劝告）
- est 算术、票数对账、枚举校验永远归机器——LLM 只产票面
- 每条热补丁带 `origin` 出处（human / llm:model），谁的改动谁负责，可按物种统计存活率
- "有没有错"永远由确定性机器回答；LLM 只回答"为什么"和"怎么改"

### 3. 接入成本趋近于零

- 任何语言的外部脚本，打印 `##DSL_RESULT` 结构化协议即可被编排——不写 Tool 类，不包 wrapper
- 多路径降级是内核语义：`web -> ...` 失败自动滑到 `mcp -> ...`，连续失败永久 BLOCKED，不写 if/else/try/catch
- `.pick(egraph)` 一行开启 e-graph 等价类熔合，等价 proc 只跑一次
- 引擎按节点在图中的位置自动合成八段认知上下文（身份/上游预览/继承约束/下游消费者/错误记忆/输出契约），prompt 不再手拼
- LangChain 一行接入：`ductile.langchain_tools()`

## 和现有工具的区别

| 能力 | Ductile | LangGraph / AutoGen | Airflow / Prefect |
|------|---------|---------------------|-------------------|
| 链级约束继承（治衰减） | ✅ v0.18 | ❌ | ❌ |
| owner 槽位 / 问询链（防臆造） | ✅ | ❌ | ❌ |
| 提示词自动进化环（可证伪） | ✅ | ❌ | ❌ |
| 多路径自动降级 | ✅ 内核 | ⚠️ 手写 | ❌ 手写 |
| 认知上下文自动合成 | ✅ | ❌ 手拼 prompt | ❌ |
| 认知回传（契约/canary/incident/L4 复核） | ✅ v0.15 | ❌ | ❌ |
| 零改接入外部脚本 | ✅ 5 行协议 | ❌ Tool 类 | ⚠️ Operator |
| Rust 单二进制 + SQLite 单文件 | ✅ | ❌ | ⚠️ 外部 DB |

路由层只解决"选哪个模型"，编排框架只解决"怎么连起来"。**Ductile 解决"连起来之后质量为什么崩、怎么治"。**

## 30 秒示例

```
Pipeline("research")
  .proc("search")
    .plan(
      web -> web_search(query="{topic}").tags(#search, #web).retry(n=3),
      mcp  -> mcp_search(query="{topic}").tags(#search, #mcp)
    )
  .proc("gate")
    .plan(g -> run("judge.sh {topic}").tags(#judge))
  .proc("write_report")
    .when(@gate.score >= 80)
    .plan(w -> write(to="~/output/report.md", content=@search))
  .proc("deliver")
    .deliver(@write_report)
```

```bash
ductile run research.pipeline "RISC-V 架构"
```

web 失败自动滑到 mcp；裁判 < 80 deliver 被门住（fail-closed）。**你只管声明，引擎管质量。**

## 安装

```bash
pip install ductile          # 或源码：cargo build --release
```

Windows 依赖 Git Bash，见 [docs/WINDOWS.md](docs/WINDOWS.md)。

## 给 AI 协作者

Ductile 为 AI 协作设计。把 [SPEC.md](SPEC.md) 喂给你的 AI 助手（完整安装/编写/执行/调优/配方，§14 是 LLM 管线四大配方），或直接说：

> 请阅读 https://github.com/lsmind/ductile/blob/main/SPEC.md 后帮我写 pipeline。

写 .pipeline 的铁律：**抄范本，禁止凭记忆**——v0.18.4 起引擎在 check 期静态校验兜底。

## 更多

- [SPEC.md](SPEC.md) —— 完整引擎规格（给 LLM 读，教全用法）
- [docs/cognition_spec.md](docs/cognition_spec.md) —— 认知回传系统：七层误差信号栈 + 归因五分类
- 核心在 `src/`（30 个 Rust 模块按七层认知栈物理分层，见 `docs/LAYERS.md`，层边界由 selftest 探针守护）；Python 包与 LLM 桥在 `python/`、`bridge/`；一次性实验脚本归档在 `experiments/`

## 测试

```bash
cargo test --lib     # 418 passed / 0 failed
./selftest.pipeline  # 九重探针门禁 SELFTEST-PASS
```

## License

MIT
