# 外部反馈单 #5（2026-09-12，germline S0BQ 实测后续）

**来源**: krc/semantic S0BQ 先验审计管线（九臂并行，germline git 832c573）
**状态**: 3 项待修；#1-#4 四修已活体验收 ✅（见文末对账）

---

## 5.1 `.args()` 链式调用炸名解析（高优——唯一未修的 fail-late）

**复现**（v0.18.4，今天 target/release 二进制）：

```
.proc("chained_args")
  .plan(
    b -> script(probe_env_raw)
          .args(val="lex")
  )
```

**现象**: `ductile check` 过（Type check passed）；`ductile run` 炸
`contract error: invalid script name 'probe_env_raw)  .args(val=lex' in body`，
errflow 判 unrecoverable，整条管线 exit flow。

**同族证据**: S0BP（09-12 05:00）、S0BQ（09-12 08:4x）两次生产事故同款；
`.env()`/`.timeout()` 同炸（feedback 单历史）；`.tags()`/`.desc()` 链式不炸。

**建议**（二选一，优先 a）：
- a) **check 期拦截**：与 v0.18.4 对 `.when` 裸引用的处理同款——
  script() 名后跟 `.args(`/`.env(`/`.timeout(` 链的，`ductile check` 直接
  BadChainedScriptCall，把 fail-late 变 fail-fast
- b) 支持链式语法（等价内联参数）

**验收**: 上述 DSL `check` 期报错（a）或 run 绿（b）；加 2 测试。

## 5.2 `ductile --version` 不存在（低优，5 分钟活）

`ductile --version` / `-V` 均无此命令。生产事故排查时无法回答
"我现在跑的是哪个构建"——S0BQ 今早的版本疑云（08:47 旧二进制 vs 11:26
新二进制）就是靠 `ls -la target/release/ductile` + git log 对钟的。

**建议**: clap 标准 `version` flag，输出版本 + 构建时间 + git hash。

## 5.3 Cargo.toml 版本漂移（低优但记账性质）

`Cargo.toml version = "0.15.0"`，SPEC/commit 已到 v0.18.4。
版本账不平：修了 5.1 之后正好一起 bump 到 0.18.x，SPEC §14.7 归账。

---

## 附：#1-#4 活体回归对账（v0.18.4，今天二进制，非读 changelog）

| 反馈项 | 探针 | 判决 |
|---|---|---|
| env 序列化引号（`int('"16"')` 炸 import） | pipeline run 默认填充路径 raw 回显 | **CLEAN** ✅ |
| err_msg 截断 | （未注入新错误，changelog+测试为准） | 信任 ✅ |
| `.when` 裸引用 check 期拦截 | （无新用例） | 信任 ✅ |
| SPEC §9.3 DUCTILE_ARG_* 文档化 | 已读，与 S0BM 模式一致 | ✅ |

九臂并行的 `__pycache__` 竞态属调用方卫生（wrapper 已设
PYTHONDONTWRITEBYTECODE=1），不入引擎单；可选：并行 spawn 子进程时
引擎默认带此 env（一行，防御性）。
