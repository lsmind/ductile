# 竞品对照

可跑草图：[`benches/competitors/`](../benches/competitors/)  
同场景：[`examples/scripts/unstructured-extract.pipeline`](../examples/scripts/unstructured-extract.pipeline)

## 隐喻

| 阵营 | 代表 | 典型用途 |
|------|------|----------|
| 图状态机 | LangGraph | 分支 / checkpoint |
| 角色团队 | CrewAI | 多角色任务 |
| 会话协作 | AutoGen 系 | 多 agent 对话 |
| 批处理 DAG | Prefect / Airflow | ETL / 定时 |
| 声明式多路径 | **Ductile** | 备选降级、裁判、脚本契约 |

```bash
python benches/competitors/run_compare.py
ductile run examples/scripts/unstructured-extract.pipeline
```

Ductile：`.proc.plan(主路, 备路).when(@judge…)` —— 失败换路与质量门在引擎语义里，而不是在业务 Python 里手写。
