# Ductile 仓库工作规范（对所有 AI 智能体生效）

**进本仓库干活的任何智能体（Claude/Hermes/Codex/…）必须遵守：**

1. **编排逻辑必须写 .pipeline，不写 Python/bash 脚本**。多步骤工作流（测试→构建→提交→验证、扫描→统计→报告）= 管线，不是脚本。写之前先问：这能不能是一个 proc 链？
2. **git 提交一律走 `./ship.pipeline "commit message"`**，不手敲 git add/commit/push。改完先 `./selftest.pipeline`，绿了再 ship。
3. **测试用 `ductile` 命令**（PATH 软链直指 target/release，cargo build 后自动最新，无需拷贝）。
4. **新工作流先想 DSL 写法**：内置动词 run/write/read/ls/stat/cp/exists/mkdir/llm/search/merge/script/spawn。run() 里路径一律绝对路径；多行 @ref 引用必须引号包裹 `echo "@x"`。
5. **执行纪律**：引擎审计不信注释——声称的特性要 grep 调用点 + 真跑验证；预期失败的测试先 git stash 回旧版确认"本来就坏"。

反例（历史教训，都真实发生过）：
- 手敲 git 提交 → 触发审批超时 BLOCKED，而 ship.pipeline 跑 git 不触发
- 写 Python 脚本做"扫描+统计+报告" → 用户驳回："为什么不用 ductile"
- 用 bash 写 19 项回归而不是 .pipeline → 用户驳回："为什么不用 ductile 对 ductile 写测试"

本项目存在的意义就是验证 DSL 能驱动真实工作流——本仓库内的操作是第一现场。
