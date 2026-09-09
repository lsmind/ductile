# Ductile vs agent / workflow competitors

可跑草图：[`benches/competitors/`](../benches/competitors/)  
一键矩阵：`python benches/competitors/run_compare.py`  
同场景 ductile：[`examples/scripts/unstructured-extract.pipeline`](../examples/scripts/unstructured-extract.pipeline)

可视化摘要：Cursor Canvas `ductile-competitors`（侧栏打开即可）。

---

## 1. 定位地图（2026）

| 阵营 | 代表 | 隐喻 | 典型用途 |
|------|------|------|----------|
| **图状态机** | LangGraph | Node + typed state + edges | 生产级分支/重试/checkpoint |
| **角色团队** | CrewAI | Agent role/goal + Task + Crew | 快速原型、层级委派 |
| **会话协作** | AutoGen → Microsoft Agent Framework | 多 agent 对话 | 研究、辩论、涌现协作 |
| **批处理 DAG** | Prefect / Airflow | Task + 依赖边 | ETL、定时批、运维友好 |
| **声明式多路径** | **Ductile** | `.pipeline` + 备选 impl | 降级/裁判分离/脚本契约/单文件 SQLite |

没有单一赢家：选隐喻，再选框架。

---

## 2. 能力矩阵（相对 Ductile）

评分：强 / 中 / 弱 / 无（相对「声明式 agent 流水线」需求）

| 能力 | LangGraph | CrewAI | AutoGen | Prefect | **Ductile** |
|------|-----------|--------|---------|---------|-------------|
| 动态环 / 条件边 | 强 | 中 | 中 | 弱 | 中（`.when` + 备选，非任意环） |
| 多工具 / 多脚本 | 强（ToolNode） | 强（agent tools） | 强 | 中（算子） | 强（`script()`+内置步） |
| 非结构化→字段 | 中（自写） | 中（自写） | 中 | 弱 | **强**（`llm`+`schema`+`##DSL_RESULT`） |
| 失败降级 | 中（手写边） | 弱（自包） | 弱 | 中（重试） | **强**（plan 备选+ranking+errflow） |
| 质量门槛 / 裁判 | 弱 | 弱 | 中（reviewer agent） | 无 | **强**（judge+`.when` fail-closed） |
| 热补丁 / 不改源 | 无 | 无 | 无 | 弱 | **强** |
| 偏好自学习 | 无 | 无 | 无 | 无 | **强**（impl_prefs） |
| 等价消解 / CSE | 无 | 无 | 无 | 无 | 中（e-graph 可选） |
| Day-1 DX | 中 | **强** | 中 | 中 | 中（要学 DSL） |
| 生产可观测 | **强**（LangSmith） | 中 | 中 | **强** | 中（SQLite runs） |
| 依赖体积 | 大 | 中 | 大 | 中-大 | **小**（二进制+可选 bridge） |
| Human-in-the-loop | **强** | 弱/文档缺口 | 中 | 中 | 弱（未一等公民） |

---

## 3. 同场景对照：非结构化搜索摘要 → JSON 字段

**Fixture**：[`examples/scripts/fixtures/search_blurb.txt`](../examples/scripts/fixtures/search_blurb.txt)  
**期望字段**：`title` / `url` / `topic` / `count`

| 实现 | 文件 | 编排形状 | 离线可跑 |
|------|------|----------|----------|
| LangGraph 形 | `langgraph_unstructured_extract.py` | ReAct：search tool → parse tool | 是 |
| AutoGen 形 | `autogen_two_agent_extract.py` | extractor ↔ reviewer | 是 |
| CrewAI 形 | `crewai_role_crew_extract.py` | sequential Crew tasks | 是 |
| Prefect 形 | `prefect_flow_extract.py` | 固定 DAG 任务边 | 是 |
| **Ductile** | `unstructured-extract.pipeline` | `read` → `llm(schema=…)` / stub → gate | 有 stub；live 需 `config.toml` |

```bash
# 竞品离线矩阵（应 agreement.same_title/url = true）
python benches/competitors/run_compare.py

# Ductile（推荐 config.toml [llm]）
ductile run examples/scripts/unstructured-extract.pipeline
```

草图**不 vendor** 完整 upstream，只保留「多脚本/多角色/多任务」形状，避免拖入 langchain/crewai 锁版本。官方 quickstart 链接见各框架仓库。

---

## 4. 代码与控制流差异（直觉）

```text
LangGraph:   state ──► agent node ──► tools node ──► (loop) ──► END
CrewAI:      Crew(agents, tasks, process=sequential|hierarchical) ──► kickoff
AutoGen:     AgentA.chat(AgentB) until terminate
Prefect:     @flow: t2(t1()); t3(t1()); t4(t2,t3)     # 无 LLM 选边
Ductile:     .proc.plan(primary, fallback).when(@judge…)  # 声明备选+门禁
```

Ductile 把「失败换路 / 质量门」做成**引擎语义**；竞品大多要你在 Python 里手写等价逻辑。

---

## 5. 何时选谁

| 你的约束 | 优先 |
|----------|------|
| 要 checkpoint、HITL、复杂分支、LangSmith | LangGraph |
| 两周出角色团队 Demo | CrewAI |
| 对话式多 agent 研究 / 辩论 | AutoGen / MAF |
| 夜间批、SLA、运维 DAG | Prefect / Airflow |
| 声明意图、多路径降级、裁判分离、脚本即 API、单文件库 | **Ductile** |
| 动态图生态 + Ductile 降级 | 并存：竞品做 reasoning，Ductile 做可审计交付管线 |

---

## 6. Ductile 已闭合的缺口

- `llm()` + `bridge/llm_bridge.py` + `config.toml` `[llm]`：非结构化 → `@proc.field`
- 同场景 stub 备选：无 API 时仍可演示降级语义
- 竞品侧四处形状（ReAct / 双 agent / Crew / DAG）+ `run_compare.py` 字段一致性检查
- **超图层** `.hyper`：有序/类型化关联超图（`chain`/`gate`/`bundle`/`xor`）编译期投影与校验；`gate` 显式端口；与运行时动态环分离；复用分 `hypergraph_key` / `dag_key`

## 7. 仍弱于竞品（诚实清单）

- 任意动态环 / HITL 暂停不如 LangGraph
- Python 工具生态与可视化调试不如 LangChain 系
- 无内置分布式调度（不替代 Airflow）
- e-graph 全量 CSE 仍为 opt-in（默认只用调度分层）
