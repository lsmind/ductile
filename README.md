# Ductile

> 声明式流水线引擎 —— 声明意图，引擎自动处理路由、降级、质量控制和认知上下文。

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tests](https://img.shields.io/badge/Tests-417%20passed-brightgreen.svg)](#测试)
[![PyPI](https://img.shields.io/badge/PyPI-0.15.0-blue.svg)](https://pypi.org/project/ductile/)

---

## 为什么是 Ductile

跑过 LLM 流水线的人都撞过同一堵墙：**流程跑通了，质量靠玄学**。约束在第二段被
消化成散文、第三段模型开始臆造、第四段工整地输出一堆没人认领的任务单——每一步
都绿灯，整体是废品。

Ductile 的回答是把三类人工判断变成引擎结构：

**1. 不确定性管理 → 数值优化。** 你声明"做什么 + 有哪些备选路径"，引擎选路、
降级、淘汰废路径。web 挂了滑到 mcp，裁判打分 < 80 deliver 被门住（fail-closed），
连续失败 3 次永久 BLOCKED。不写 `if/else/try/catch`。

**2. 上下文管理 → 认知合成。** `llm(agent)` 不写 prompt 时，引擎按节点在图中的
位置自动合成八段认知上下文：身份、主题、上游输入预览、继承约束、下游消费者、
开放动作、错误记忆、输出契约（v0.17）。LLM 不再"瞎接活"。

**3. 质量进化 → 可证伪循环。** 结构探针（零 LLM 判"有没有错"）+ 盲评批语（判
"好不好"）+ 提示词医生（判"为什么+怎么改"）组成自动进化环（v0.18.3），处方
应用后重跑复检，预期不兑现就回滚。

### 实测数据（game 场景，三轮独立盲评，judge=27B）

| 配置 | s2 深拆解盲评 | 说明 |
|---|---|---|
| 裸 27B | 66.7–79.3（波动大） | 单发无结构 |
| 五段链（v0.17 前） | 56.7–59.3 | **约束逐跳衰减，比裸模型还差** |
| + 约束继承（v0.18.1） | 差距 -16.3 → -9.7 | `.constraint` 全链注入 |
| + owner 槽位（v0.18.2） | **+14.7，反超裸模型** | 约束有落点 |
| + 问询链（防臆造） | **82.7（+15）** | 下级提问上级裁决 |

最有说服力的一条曲线是第一行到第四行：**结构化管线从"比裸模型差 16 分"走到
"反超 15 分"**——差距不是靠换更大的模型填的，是靠把约束送到位、留好槽位填的。

### 和现有工具的区别

| 能力 | Ductile | LangGraph / AutoGen | Airflow / Prefect |
|------|---------|---------------------|--------------------|
| 声明式流程定义 | ✅ 纯文本 | ⚠️ 代码+图 | ✅ DAG |
| 多路径自动降级 | ✅ 内核 | ❌ 手写 | ❌ 手写 |
| e-graph 等价类 + CSE | ✅ | ❌ | ❌ |
| 认知上下文自动合成 | ✅ v0.17 | ❌ 全手拼 prompt | ❌ |
| 链级约束继承 | ✅ v0.18 | ❌ | ❌ |
| 提示词自动进化环 | ✅ v0.18.3 | ❌ | ❌ |
| 认知层（契约/canary/incident/L4） | ✅ v0.15 | ❌ | ❌ |
| SQLite 单文件全记录 | ✅ | ❌ | ⚠️ 外部 DB |
| 零改接入外部脚本 | ✅ 5 行 DSL_RESULT | ❌ Tool 类 | ⚠️ Operator |

路由层只解决"选哪个模型"，编排框架只解决"怎么连起来"。Ductile 把选择 + 降级 +
上下文 + 约束传递 + 自学习塞进一个引擎。

## 安装

```bash
pip install ductile          # 或源码：cargo build --release
```

Windows 依赖 Git Bash，见 [docs/WINDOWS.md](docs/WINDOWS.md)。

## 30 秒看懂

```
Pipeline("research")
  .proc("search")
    .plan(
      web -> web_search(query="{topic}").tags(#search, #web).retry(n=3),
      mcp -> mcp_search(query="{topic}").tags(#search, #mcp)
    )
  .proc("gate")
    .plan(g -> run("judge.sh {topic}").tags(#judge))
  .proc("write_report")
    .when(@gate.score < 80)
    .plan(w -> write(to="~/output/report.md", content=@search))
  .proc("deliver")
    .deliver(@write_report)
```

```bash
ductile run research.pipeline "RISC-V 架构"
```

web 失败自动滑到 mcp；裁判 < 80 deliver 被门住。**你只管声明，引擎自己学。**

## 三个真实场景（全部盲评验证）

**防臆造约束**：用户说"预算有限"，需求节点臆造成"不能外包"，毒害全链。问询链
（req 提问 → resolver 裁决 → `.constraint` 全链继承）让约束只来自原话：+15 分。

**约束要有落点**：任务单 schema 没有 owner 字段，注入再强的约束也写不进去。加一个
ticket 内嵌 owner 槽位 + 白名单 guide：-9.7 → **+14.7**。

**提示词自动进化**：9B 输出 7 张粗票，机器医生读探针报告开处方（数量锚/owner 枚举/
priority），42.7 → 64.0（+21.3）；处方里的"自检清单"触发算术反噬导致输出崩溃，
可证伪环捕获并选择性回滚，复检 88 分。**机器开的处方和人工三轮实锤的处方逐条对齐，
还比人工多抓了一处。**

## 核心能力索引

- **超网络（`.hyper`）**：不确定时生成运行图再执行 — [SPEC §2.4b](SPEC.md)
- **e-graph 熔合 + CSE**：`.pick(egraph)` 一行开启等价 proc 只跑一次 — [SPEC §3.0](SPEC.md)
- **auto-prompt 认知合成**：八段上下文，盲评"英文 system 干中文活"的 10+ 分坑自动填平 — [SPEC §14](SPEC.md)
- **问询链 / owner 槽位 / 进化环**：LLM 管线四大配方 — [SPEC §14](SPEC.md)
- **裁判分离**：质量门槛 = 独立 judge + `.when(@judge.score < 80)`，产出者不自证清白 — [SPEC §3.7](SPEC.md)
- **热补丁**：`ductile patch` 不改源文件禁用/调整任意节点
- **认知层**：契约卡/canary/incident/L4 复核/shelve — [docs/cognition_spec.md](docs/cognition_spec.md)
- **Shell 安全门**：`DUCTILE_RESTRICT_SHELL=1` 多租户收紧 — [SPEC §6a.4](SPEC.md)
- **LangChain 一行接入**：`ductile.langchain_tools()` — [SPEC §11](SPEC.md)

## 给 AI 协作者

Ductile 为 AI 协作设计。把 [SPEC.md](SPEC.md) 喂给你的 AI 助手（完整安装/编写/
执行/调优/配方），或：

> 请阅读 https://github.com/lsmind/ductile/blob/main/SPEC.md 后帮我写 pipeline。

写 .pipeline 的铁律（外部实测血泪）：**抄范本，禁止凭记忆**——v0.18.4 起
`.when` 条件 check 期静态校验兜底。

## 代码结构

核心在 `src/`（解析、编排、执行、超网络、认知层）；Python 包与 LLM 桥在
`python/`、`bridge/`。细节以源码为准。

## 测试

```bash
cargo test --lib     # 417 passed / 0 failed
```

## License

MIT
