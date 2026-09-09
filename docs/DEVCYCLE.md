# 开发周期（devcycle）

用 ductile 编排本仓库的「指令 → 验证 → 落盘」。

```bash
./devcycle.pipeline "start: <需求>"     # → target/devcycle/handoff.md
./devcycle.pipeline "feat: <说明>"    # selftest → ship
```

| 入口 | 作用 |
|------|------|
| `workflows/devcycle-start.pipeline` | 计划与 handoff |
| `workflows/devcycle-land.pipeline` | 自测与提交 |
| `selftest.pipeline` / `ship.pipeline` | 亦可单独使用 |

规范见根目录 `AGENTS.md`。
