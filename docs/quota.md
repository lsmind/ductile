# 资源配额与沙箱说明（quota）

> 适用：`src/kernel/quota.rs`（v0.24 D6）与 script/LLM 运行面。
> 承接 docs/mlv-security.md 同一信任模型——本文件补齐其 §八 quota 侧说明。

## 一、信任模型与降级诚实原则

配额层与 MLV 同属**内部可信工具边界**。沙箱探测**如实分级、绝不静默假装**：
`detect_sandbox()` 探测 nsjail——可用即 `Nsjail`（namespace+seccomp+cgroup+rlimit
全隔离，生产目标），不可用降级 `Rlimit`（地址空间/nofile+maxrss 事后判定），
`None` 仅显式 opt-out 且**审计红牌**。每次运行在审计记录落 `sandbox_mode`
字段，降级是事实不是秘密。

## 二、E406 ResourceQuota 三维

- **内存**：maxrss（KiB，ru_maxrss 语义；rlimit 模式事后判定）
- **PID/文件**：nofile+nproc（个数，rlimit 先置）
- **输出体积**：bytes（累积计数，超限硬拒）

测量时点：maxrss=进程退出后；nofile/nproc=执行前置；输出=写入时累积。
继承范围：rlimit 经 fork 继承至全部子进程（递归覆盖）；maxrss 仅主进程（子进程核算待 PO0）。

## 三、script 传参校验（§6 wrapper 契约）

wrapper 运行前校验缺失/额外/类型错误参数 → `E312 Args`；不无条件信任任意
`DUCTILE_ARG_*` env——与 DSL fail-closed 原则一致（未声明参数=硬错）。

## 四、与 MLV 的关系

配额审计事件走引擎账本（非 MLV 治理账本）；两账本边界=「引擎执行层资源事实」
vs「治理层状态转移」。MLV 侧安全边界见 docs/mlv-security.md。
