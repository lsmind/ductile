# Ductile 仓库工作规范（对所有 AI 智能体生效）

1. **编排写 `.pipeline`，拓扑写 `.hyper`**，不要用 Python/bash 顶替多步工作流。
2. **默认走 devcycle**：`./devcycle.pipeline "start: …"` → 改代码 → `./devcycle.pipeline "feat: …"`（见 `docs/DEVCYCLE.md`）。提交也可用 `./ship.pipeline "…"`。
3. **先自测再落盘**：`./selftest.pipeline` 绿了再 ship。
4. **测试与运行用 `ductile` 命令**（指向当前构建产物；引擎会注入 PATH，见 `docs/WINDOWS.md`）。
5. **DSL**：内置动词见 SPEC；`run()` 用绝对路径；多行 `@ref` 须引号；`llm` + `schema` 做结构化抽取；`gate` 用显式 `judge=` / `producers=` / `consumers=`。写新图前 `hyper similar`；写新节点前 `hyper nodes`。
6. **声称的行为要真跑验证**，不信注释。
7. **LLM 输出一律不进 bash**：`echo '@ref'` / `>/dev/null` 都会被单引号炸——只允许 `.when` 结构化字段或 `write` 动词落盘后 `cat`（SPEC §13.5）。
8. **隔离用 `DUCTILE_DATA`**（真的会被 `db_path()` 读）；E2E 探针跑完确认真库未污染。
9. **测试禁 `set_var` 全局 env**（并行竞态）——改参数注入（如 `db_path_with`）。

竞品对照草图：`benches/competitors/`、`docs/COMPETITORS.md`。
