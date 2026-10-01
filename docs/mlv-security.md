# MLV 安全边界说明（f2 冻结规格）

> 适用：`ductile mlv` 治理账本 CLI 与 `src/kernel/{mlv,gov}.rs` 记录/治理层。
> 依据：外援终验冻结规格（docs/sonet_mlv_fixround_spec.md §f2）。

## 一、信任模型与密码学边界（同段明示，不得拆分阅读）

本工具属于**内部可信工具边界**：账本由本机受控进程读写，不假设外部攻击者可
直接调用 CLI。`GENESIS_ROOT` 是**公开常量**（mlv.rs），不构成任何秘密。记录
信封中的 `mac` 是**无密钥完整性摘要**（域分隔哈希），**不提供发送方真实性**——
任何持有 `GENESIS_ROOT` 的一方均可构造合法信封。命令行工具当前采用自签自验，
对发送方真实性**不提供任何保证**；实际防线是**绑定校验层**（t16 两层防线：
MAC mismatch 先拦声明篡改，信封声明与记录域绑定校验拦重签挪用）。发送方
真实性必须在 **PO0 之后通过 Ed25519** 提供（方向已定，未实施）。在 Ed25519
落地前，不得将 `mac` 描述为发送方认证机制。

## 二、LEDGER_INIT 豁免范围（显式限定）

`LEDGER_INIT` 仅当账本文件**不存在**时方可写入首行；账本已存在且非空时，
`init` 必须拒绝执行（t 系测试与 `mlv.pipeline` 步 1 覆盖）。

## 三、持久化与崩溃一致性

追加路径：write→sync→rename→dirsync，五点位 failpoint 矩阵
（`scripts/mlv_failpoint_matrix.sh`，feature `mlv-failpoint` 门控，生产二进制
零测试面）。崩溃后账本必为老本或新本（字节级 sha256 断言），`verify` 全过，
同请求重放必 OK/ACK（幂等）。

## 四、并发

跨进程 flock（LOCK_NB 单次让步后阻塞等待）；同进程重入=显式错误
（`flock-reentrant`）。契约只承诺互斥，不承诺公平。
