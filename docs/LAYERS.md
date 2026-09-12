# Ductile 七层认知栈 — 源码地图

> v0.18.5 物理重组落地。你的分层理论从论述变成目录结构：每个 `.rs` 文件
> 归层入 `src/L*/`，层边界由 `scripts/layers_probe.py` 物理守护（selftest
> 第 10 道探针，违规即红）。

## 栈（高 → 低）

| 层目录 | 类比位 | 模块 | 职责一句话 |
|---|---|---|---|
| `interface/` | 横切栈顶 | `cli`, `api` | 人与 Python 的入口，不含业务判断 |
| `core/` | 共享词汇表 | `ast`, `dslresult`, `script_card` | 层间共享的纯数据契约；**不依赖任何层** |
| `L4_structure/` | 编译器/框架 | `hyper`, `learn`, `grow`, `promote`, `harvest` | MDL 压缩、同构复用、结构经验沉淀 |
| `L3_dsl/` | 程序语言位 | `parser`, `typecheck`, `when`, `config`, `version` | `.pipeline` 文本 → 校验过的 AST |
| `L2_orchestration/` | 运行时/VM | `executor`, `steps`, `egraph`, `eval`, `script`, `registry`, `textargs`, `ranking` | 拓扑执行、成本选路、脚本沙箱 |
| `L1_feedback/` | 调试器/观测 | `errflow`, `canary`, `incident`, `l4`, `shelve` | 错误分类回传、金丝雀、事故簿、搁置 |
| `L0_physical/` | 机器码位 | `db`（+3 张 schema.sql） | SQLite、exit code、文件系统 |

## 依赖铁律（probe 强制）

**只许向下依赖，禁止上行/跳层走私。** 合法层向：

```
interface > L4 > L3 > L2 > L1 > L0     （core 任意可依赖）
```

- 测试代码（`#[cfg(test)]`）不算生产边——probe 自动剥离
- 例外必须记在 `layers_probe.py` 的 `AMNESTY` 白名单并写理由。当前 1 条：
  `L0/db → L4/harvest.civil_from_days`（纯时间函数，待下沉）
- 新增走私边 → selftest 红 → 修掉或记账，没有第三条路

## 组件层 vs 信号层（正交）

- **组件分层（本表）**答"这东西放哪"——静态结构
- **信号分层（SPEC cognition_spec）L0-L5** 答"错误从哪层冒出来"——动态行为
- 同名不同物：A1 盲区需两层联手印证，单层验证必有盲区

## "新代码落在哪"决策树

```
是纯数据契约（struct/enum/parse）？          → core/
是格式解析/类型校验/配置读取？               → L3_dsl
是执行、调度、选路、进程管理？               → L2_orchestration
是错误分类/观测/事故记录？                   → L1_feedback
是 SQLite/文件系统/进程原语？                → L0_physical
是从历史运行中沉淀结构（学习/晋升/收割）？    → L4_structure
是人或外部程序的入口？                       → interface/
```

## 归档说明

一次性实验脚本已移 `experiments/`（c18x_*、regchain_*、reggame_*、
probe_invariants、rx_apply，共 24 文件）——它们是评测实验产物，不是引擎。
活管线路径已同步（regcheck*.pipeline、prompt_evolve.pipeline）。
`scripts/` 只留常驻工具：`layers_probe.py`（门禁探针）。

## 迁移要点（给改老代码的人）

- `crate::ast` / `crate::dslresult` → `crate::core::ast` / `crate::core::dslresult`
- `crate::cli` → `crate::interface::cli`
- `ductile::cli::run` → `ductile::interface::cli::run`
- ranking 移到 L2（它是 executor 的选路运行时，不是结构经验）
