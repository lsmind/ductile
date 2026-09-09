# Ductile 设计概要

> 产品说明见 [README.md](../README.md)；命令与语法见 [SPEC.md](../SPEC.md)。

## 定位

声明式 Agent 流水线引擎：声明「做什么」和「有哪些备选」，由引擎完成选路、降级、质量门槛与经验沉淀。

## 原则

1. **声明意图，不声明控制流**
2. **裁判与流程分离**（`.pipeline` vs `.eval` / judge）
3. **失败是数据**（执行失败可路由；创作错误才中断）
4. **默认 fail-closed**

## 分层

```
.hyper（超网络）→ .pipeline（运行图）→ e-graph / ranking / errflow（执行）
                                                    ↓
                              认知层（v0.15）：contract / canary / incident / l4 / shelve
```

| 层 | 职责 |
|----|------|
| 超网络 `.hyper` | 不确定时生成可运行拓扑；`check` 校验形状 |
| 运行图 `.pipeline` | 工序、备选 impl、数据依赖 `@ref`、`.when`、`.contract` |
| 执行 | 选路、降级、等价消解、学习 |
| 认知层 | 契约校验产生误差信号 → 事故聚合与分层信号 → 归因判别（canary/搁置）→ 端到端复核（log-only→enforcing） |

认知层原则：确定性证据短路 LLM；无 canary 通过记录禁止本地 patch；判别模糊必搁置。
设计全文见 [cognition_spec.md](cognition_spec.md)。

LLM 步骤应只消费入边数据（`@proc` / `@proc.field`），按数据流取最小必要上下文。

## 超网络（简述）

- `chain`：数据路径  
- `gate`：显式 `judge=` / `producers=` / `consumers=`  
- `bundle`：共现  
- `xor`：互斥方案（落入多 impl / 备选）

```bash
ductile hyper build file.hyper -o out.pipeline
ductile hyper check file.hyper file.pipeline
```

语法细节见 SPEC §2.4b。

## 运行图（简述）

- `.plan` 内多路径互为备选  
- `@ref` 推导依赖；结构化结果走 `##DSL_RESULT`  
- 质量门槛：独立 judge + `.when(@judge…)`  

## 仓库工作流

本仓库用 ductile 自测与落盘：见 [DEVCYCLE.md](DEVCYCLE.md)、根目录 `AGENTS.md`。

```bash
./devcycle.pipeline "start: …"   # 开工
./devcycle.pipeline "feat: …"  # 验证并落盘
```
