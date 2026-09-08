//! v0.14 错误分类学 — Either 错误流的分类层。
//!
//! 设计（docs/v0.14_error_flow_spec.md）：
//! - 十五类错误码（v0.14d）：timeout/ratelimit/auth/network/resource/permission/
//!   memory/dependency/truncation/format/schema/data/contract/cancelled/crash
//! - 分类是引擎事实：纯函数启发式，`crash` 兜底永不过时
//! - ErrorRecord 编码为 `§§FIELDS§§` → when.rs 可直接路由 `@proc.err_code`

use std::collections::BTreeMap;

use crate::dslresult;

/// 错误码（十二类，v0.14c 扩充）。`code()` 返回静态串供编码。
/// 划界原则：同一类的错误共享同一处置策略（strategy）与下游响应（respond）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    Timeout,
    /// 限流/配额（429/too many requests/quota）——等待退避后重试通常有效
    RateLimit,
    /// 认证/授权失效（401/403/invalid key/token expired）——重试无意义，换供应商有效
    Auth,
    /// 网络不可达（conn refused/DNS/5xx）——与本地资源分离：网络抖动 vs 确定性缺失
    Network,
    Resource,
    Permission,
    /// OOM/内存耗尽（CUDA OOM/exit 137/killed）——共享机器上常为瞬时
    Memory,
    /// 依赖/环境缺失（ImportError/command not found/shared library）——环境确定性坏
    Dependency,
    /// 输出截断（finish_reason=length/output truncated）——重采样可能更短，重试可救
    Truncation,
    /// 输入格式错（JSONDecodeError/UnicodeDecodeError/parse error）——输入确定性坏
    Format,
    /// 输出结构不合预期（KeyError/TypeError/validation）——模型输出幻觉/缺字段，换路径
    Schema,
    Data,
    Contract,
    /// 用户中断（SIGINT/cancelled）——意图明确，禁止任何自动重试
    Cancelled,
    Crash,
}

impl ErrCode {
    /// 从编码字段还原（传播时继承上游分类）。
    pub fn from_code(s: &str) -> Option<ErrCode> {
        match s.trim() {
            "timeout" => Some(ErrCode::Timeout),
            "ratelimit" => Some(ErrCode::RateLimit),
            "auth" => Some(ErrCode::Auth),
            "network" => Some(ErrCode::Network),
            "resource" => Some(ErrCode::Resource),
            "permission" => Some(ErrCode::Permission),
            "memory" => Some(ErrCode::Memory),
            "dependency" => Some(ErrCode::Dependency),
            "truncation" => Some(ErrCode::Truncation),
            "format" => Some(ErrCode::Format),
            "schema" => Some(ErrCode::Schema),
            "data" => Some(ErrCode::Data),
            "contract" => Some(ErrCode::Contract),
            "cancelled" => Some(ErrCode::Cancelled),
            "crash" => Some(ErrCode::Crash),
            _ => None,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ErrCode::Timeout => "timeout",
            ErrCode::RateLimit => "ratelimit",
            ErrCode::Auth => "auth",
            ErrCode::Network => "network",
            ErrCode::Resource => "resource",
            ErrCode::Permission => "permission",
            ErrCode::Memory => "memory",
            ErrCode::Dependency => "dependency",
            ErrCode::Truncation => "truncation",
            ErrCode::Format => "format",
            ErrCode::Schema => "schema",
            ErrCode::Data => "data",
            ErrCode::Contract => "contract",
            ErrCode::Cancelled => "cancelled",
            ErrCode::Crash => "crash",
        }
    }
}

/// 一个 proc 的失败记录（全路径失败后落入 results 的 Left 值载荷）。
#[derive(Debug, Clone)]
pub struct ErrorRecord {
    pub code: ErrCode,
    pub proc: String,
    pub impl_: String,
    pub message: String,
    pub raw: String,
    pub attempts: u32,
}

const MSG_MAX: usize = 500;
const RAW_MAX: usize = 4096;

/// 引擎原生错误锚点（全小写匹配，模式串本身小写）。
const PAT_CONTRACT: &[&str] = &[
    "not attached",
    "not in contract",
    "missing required param", // 引擎形态（script.rs）
    "missing parameter",      // 外部脚本常见形态
    "invalid script name",
    "protected root",
    "param '",
    // "param '" 较宽，但 contract 在判定链首位且与 not in contract/missing required
    // 同现——引擎真实输出是 "param 'x' not in contract of 'y'"，
    // 外部脚本错误几乎不会以 "param '" 开头形态出现。见 contract_param_quote_pattern 测试。
];

const PAT_TIMEOUT: &[&str] = &["timed out", "timeouterror", "timeout expired"];

const PAT_RATELIMIT: &[&str] = &[
    "too many requests",
    "429",
    "rate limit",
    "ratelimit",
    "rate exceeded",
    "quota exceeded",
    "quota exceeded",
    "quota limit",
    "enhance your calm",
    "slow down",
];

const PAT_AUTH: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "forbidden",
    "invalid api key",
    "invalid_api_key",
    "invalid token",
    "token expired",
    "expired token",
    "authentication",
    "authenticationfailed",
    "permission denied by api",
    "incorrect api key",
];

const PAT_CANCELLED: &[&str] = &[
    "interrupted by user",
    "cancelled",
    "canceled",
    "ctrl+c",
    "sigint",
    "keyboardinterrupt",
];

const PAT_PERMISSION: &[&str] = &[
    "permission denied",
    "permissionerror",
    "eacces",
    "eperm",
    "operation not permitted",
    "read-only file system",
];

/// 网络不可达（v0.14c 拆出）：连接层故障——抖动常自愈，与"文件确实不存在"分离。
const PAT_NETWORK: &[&str] = &[
    "connection refused",
    "connection reset",
    "connectionerror",
    "connection error",
    "connect failed",
    "getaddrinfo failed", // DNS 解析失败真实形态（裸 "enotfound" 会误伤 FileNotFoundError）
    "errno -2",           // getaddrinfo ENOTFOUND 的数字形态
    "econnrefused",
    "ehostunreach",
    "enetunreach",
    "econnreset",
    "network is unreachable",
    "network error",
    "temporary failure in name resolution",
    "name or service not known",
    "service unavailable",
    "503 service",
    "502 bad gateway",
    "504 gateway",
    "gateway timeout",
    "bad gateway",
];

const PAT_MEMORY: &[&str] = &[
    "out of memory",
    "cuda out of memory",
    "cuda error: out of memory",
    "oom",
    "cannot allocate memory",
    "memoryerror",
    "killed",
    "exit some(137)",
    "exit 137",
    "tried to allocate",
];

/// 本地资源不可得（v0.14c 收窄）：确定性缺失——文件不存在/磁盘满/端口占用。
/// 网络连接类已拆去 PAT_NETWORK。
const PAT_RESOURCE: &[&str] = &[
    "not found",
    "no such file",
    "nosuchfile",
    "filenotfounderror",
    "launch failed",
    "spawn failed",
    "disk full",
    "enospc",
    "address in use",
    "eaddrinuse",
];

const PAT_DEPENDENCY: &[&str] = &[
    "modulenotfounderror",
    "importerror",
    "cannot import",
    "no module named",
    "command not found",
    "not found command",
    "shared library",
    "shared libraries", // bash 真实形态是复数："error while loading shared libraries"
    "cannot open shared object",
    "library not found",
    "librarymissingerror",
    "version mismatch",
    "version conflict",
    "incompatible version",
];

/// 输出截断（v0.14d 从 data 拆出）：finish_reason=length / max tokens / truncated——
/// 生成物被切断，重采样（更短输出）可能成功，重试可救。
const PAT_TRUNCATION: &[&str] = &[
    "finish_reason=length",
    "finish reason: length",
    "finish_reason: length",
    "output truncated",
    "truncated output",
    "was truncated",
    "response truncated",
    "max_tokens",
    "max tokens exceeded",
    "context length exceeded",
    "context window exceeded",
    "maximum context length",
    "too long",
    "exceeds the maximum",
];

/// 输入格式错（v0.14d 从 data 拆出）：输入解析/解码失败——确定性坏，重试同错。
const PAT_FORMAT: &[&str] = &[
    "jsondecodeerror",
    "unicodedecodeerror",
    "unicodeencodeerror",
    "invalid utf",
    "invalidutf",
    "parse error",
    "parseerror",
    "parsing error",
    "unexpected df output",
    "unexpected end of input",
    "unexpected token",
    "invalid format",
    "malformed",
    "not well-formed",
];

/// 输出结构不合预期（v0.14d 从 data 拆出）：模型输出幻觉/缺字段/类型不合——
/// 换路径（换模型/换 prompt 策略）可能好，同路径重试无意义。
/// 注意：不用裸 "expected"/"got"（超宽，会误伤一切错误串）。
const PAT_SCHEMA: &[&str] = &[
    "keyerror",
    "typeerror",
    "attributeerror",
    "validation",
    "validationerror",
    "missing field",
    "missing key",
    "expected one of",
    "invalid response format",
    "unrecognized response",
    "field is required",
    "null value",
    "none value",
];

/// 值域错（data 收窄）：值本身不合法（除零/溢出/索引越界/断言）——输入×实现共同决定。
const PAT_DATA: &[&str] = &[
    "valueerror",
    "indexerror",
    "overflow",
    "zerodivisionerror",
    "assertion",
    "assertionerror",
    "out of range",
    "out of bounds",
    "arithmetic",
];

fn matches_any(lower: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| lower.contains(p))
}

/// 分类原始错误串。判定链（先具体后兜底，v0.14d 十五类）：
/// cancelled → contract → ratelimit → auth → timeout → memory → dependency →
/// permission → network → resource → truncation → format → schema → data → crash
///
/// 顺序理由：
/// - cancelled 最先：用户意图是最高优先级信号，任何自动处置都是违背意图
/// - contract 次之：契约错误是引擎层概念，串形态固定，无歧义
/// - ratelimit/auth 在 timeout 前：429 响应体常含 "too many requests" 与
///   "retry after" 字样，401/403 比"慢"更需要先被识别为"凭证坏"
/// - memory 在 network/resource 前：OOM-kill 的 "killed" 是强信号，
///   且 CUDA OOM 常带 "cuda" 弱词，避免被 "not found" 吃掉
/// - dependency 在 resource 前：`ModuleNotFoundError: No module named 'x'`
///   同时含 "no module"（依赖）与字面 "not found" 子串风险——依赖更强
/// - permission 在 network/resource 前：io::Error 的权限是更强语义信号
/// - network 在 resource 前：`connect refused` 若先撞 "not found" 弱词会误判
/// - truncation/format/schema 在 data 前（v0.14d data 四分）：截断/格式/结构
///   是更具体的语义信号，收窄后的 data 只兜值域错
pub fn classify(raw: &str) -> ErrCode {
    let lower = raw.to_lowercase();
    if matches_any(&lower, PAT_CANCELLED) {
        ErrCode::Cancelled
    } else if matches_any(&lower, PAT_CONTRACT) {
        ErrCode::Contract
    } else if matches_any(&lower, PAT_RATELIMIT) {
        ErrCode::RateLimit
    } else if matches_any(&lower, PAT_AUTH) {
        ErrCode::Auth
    } else if matches_any(&lower, PAT_TIMEOUT) {
        ErrCode::Timeout
    } else if matches_any(&lower, PAT_MEMORY) {
        ErrCode::Memory
    } else if matches_any(&lower, PAT_DEPENDENCY) {
        ErrCode::Dependency
    } else if matches_any(&lower, PAT_PERMISSION) {
        ErrCode::Permission
    } else if matches_any(&lower, PAT_NETWORK) {
        ErrCode::Network
    } else if matches_any(&lower, PAT_RESOURCE) {
        ErrCode::Resource
    } else if matches_any(&lower, PAT_TRUNCATION) {
        ErrCode::Truncation
    } else if matches_any(&lower, PAT_FORMAT) {
        ErrCode::Format
    } else if matches_any(&lower, PAT_SCHEMA) {
        ErrCode::Schema
    } else if matches_any(&lower, PAT_DATA) {
        ErrCode::Data
    } else {
        ErrCode::Crash
    }
}

impl ErrorRecord {
    pub fn new(proc: &str, impl_: &str, raw: &str, attempts: u32) -> Self {
        let raw_trunc = trunc_chars(raw, RAW_MAX);
        ErrorRecord {
            code: classify(raw),
            proc: proc.to_string(),
            impl_: impl_.to_string(),
            message: trunc_chars(raw.trim(), MSG_MAX),
            raw: raw_trunc,
            attempts,
        }
    }

    /// 编码为 `§§FIELDS§§` 协议文本——与脚本结构化输出同构，
    /// when.rs 解释器 / api.rs decode_internal_fields 无需改动即可消费。
    pub fn encode(&self) -> String {
        dslresult::encode_structured_result(
            &[
                ("err".to_string(), "1".to_string()),
                ("err_code".to_string(), self.code.code().to_string()),
                ("err_proc".to_string(), self.proc.clone()),
                ("err_impl".to_string(), self.impl_.clone()),
                ("err_msg".to_string(), self.message.clone()),
                ("attempts".to_string(), self.attempts.to_string()),
            ],
            &self.raw,
        )
    }
}

impl ErrorRecord {
    /// 传播构造器：上游 Left → 下游隐式短路（v0.14 隐式传播，无 DSL 面）。
    /// err_code 继承上游根因分类，err_msg 标记传播来源。
    pub fn propagated(proc: &str, from_proc: &str, upstream_encoded: &str) -> Self {
        let code = dslresult::extract_field("err_code", upstream_encoded)
            .and_then(|c| ErrCode::from_code(&c))
            .unwrap_or(ErrCode::Crash);
        ErrorRecord {
            code,
            proc: proc.to_string(),
            impl_: "-".to_string(),
            message: format!("propagated from {}", from_proc),
            raw: String::new(),
            attempts: 0,
        }
    }
}

/// 值是否为错误值（Left）。true = 上游失败。
pub fn is_error_value(val: &crate::ast::Value) -> bool {
    match val {
        crate::ast::Value::Text(t) => dslresult::extract_field("err", t)
            .map(|v| v == "1")
            .unwrap_or(false),
        _ => false,
    }
}

/// 内置策略动作（v0.14 §3）：错误分类 → 自动处置。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// proc 级重试预算（跨 impl：全 plan 失败后线性退避再来整轮）
    Retry { budget: u32, backoff: Backoff },
    /// 数据错→立即换 impl（impl 传输层重试时同输入必再错，不撞墙）
    Reroute,
    /// 不自动处理：Left 落库 + 隐式传播
    Escalate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backoff {
    Linear,
}

impl Backoff {
    pub fn delay_secs(&self, attempt: u32) -> u64 {
        match self {
            // linear: 1s, 2s, 3s…（区别于 impl.retry 的指数 2s/4s/8s）
            Backoff::Linear => (attempt + 1) as u64,
        }
    }
}

/// 内置策略表（硬编码，不可配置——用户裁定 v0.14：错误处理统一模块管理，
/// 不单独配置不走 .eval）。十二类（v0.14c 扩充）。
///
/// | code | action | 理由 |
/// |------|--------|------|
/// | cancelled | Escalate | 用户意图：禁止任何自动重试 |
/// | timeout | Retry{2, linear} | 瞬时资源紧张常见，两轮内常自愈 |
/// | ratelimit | Retry{2, linear} | 限流窗口：等待退避后重试有效（预算比 timeout 更克制，退避即等待窗） |
/// | auth | Escalate | 凭证坏重试无意义（换供应商=Switch 响应层的事） |
/// | memory | Retry{1, linear} | 共享机器显存被占常为瞬时，一轮等待释放 |
/// | network | Retry{2, linear} | 网络抖动自愈概率高 |
/// | resource | Retry{1, linear} | 文件迟到/竞态一轮缓冲 |
/// | data | Reroute | 同输入同 impl 必再错，立即换路径 |
/// | truncation | Retry{2, linear} | 重采样可能更短成功——截断是概率性失败 |
/// | format | Reroute | 输入确定性坏，同 impl 重试必再错（换路径=换解析器） |
/// | schema | Reroute | 输出幻觉/缺字段，换路径=换模型/prompt 策略 |
/// | dependency | Escalate | 环境确定性坏（缺包/缺库），重试无意义 |
/// | permission | Escalate | 权限重试无意义，直接上报 |
/// | contract | Escalate | 创作错误（DSL/契约写错），fail-fast |
/// | crash | Escalate | 未知根因，不瞎猜 |
pub fn strategy(code: ErrCode) -> Action {
    match code {
        ErrCode::Cancelled => Action::Escalate,
        ErrCode::Timeout => Action::Retry {
            budget: 2,
            backoff: Backoff::Linear,
        },
        ErrCode::RateLimit => Action::Retry {
            budget: 2,
            backoff: Backoff::Linear,
        },
        ErrCode::Auth => Action::Escalate,
        ErrCode::Memory => Action::Retry {
            budget: 1,
            backoff: Backoff::Linear,
        },
        ErrCode::Network => Action::Retry {
            budget: 2,
            backoff: Backoff::Linear,
        },
        ErrCode::Resource => Action::Retry {
            budget: 1,
            backoff: Backoff::Linear,
        },
        ErrCode::Data => Action::Reroute,
        ErrCode::Truncation => Action::Retry {
            budget: 2,
            backoff: Backoff::Linear,
        },
        ErrCode::Format => Action::Reroute,
        ErrCode::Schema => Action::Reroute,
        ErrCode::Dependency => Action::Escalate,
        ErrCode::Permission => Action::Escalate,
        ErrCode::Contract => Action::Escalate,
        ErrCode::Crash => Action::Escalate,
    }
}

/// 下游响应策略（v0.14a3 用户裁定：正常节点的策略根据错误进程的判断进行——
/// 无视/等待/切换方法/退出整个流程）。引擎按死亡上游的错误分类自动决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// 无视：与死源无关的 proc/impl 照常运行（全类默认，Exit 除外）
    Ignore,
    /// 等待：瞬态错误先吃满上游 Retry 预算再定生死（分层顺序天然提供）
    Wait,
    /// 切换方法：只封锁真正引用死源的 impl，未引用的备选照跑
    Switch,
    /// 退出整个流程：创作错误 fail-fast，立即中止管线（保留 partial）
    Exit,
}

/// 错误分类 → 下游响应（十二类，v0.14c）。
///
/// | code | 响应 | 理由 |
/// |------|------|------|
/// | cancelled | Exit | 用户已表态，整流立即停，尊重意图 |
/// | timeout / ratelimit / memory | Wait→Switch | 瞬态：上游吃满预算再定生死，之后仅封引用者 |
/// | network / resource | Wait→Switch | 同上（抖动类） |
/// | auth/data/format/schema | Switch | 凭证坏/数据错：封锁该 impl，未引用的备选接管（换供应商/换路径） |
/// | truncation | Wait | 重采样期间下游等预算耗尽 |
/// | dependency / permission / crash | Ignore+Switch | 局部死亡：无关者无视，引用者传播 Left |
/// | contract | Exit | DSL/契约写错，运行期不可恢复，整流退出 |
pub fn respond(code: ErrCode) -> Response {
    match code {
        ErrCode::Cancelled => Response::Exit,
        ErrCode::Timeout
        | ErrCode::RateLimit
        | ErrCode::Memory
        | ErrCode::Network
        | ErrCode::Resource => Response::Wait,
        ErrCode::Auth | ErrCode::Data | ErrCode::Format | ErrCode::Schema => Response::Switch,
        ErrCode::Truncation => Response::Wait,
        ErrCode::Dependency | ErrCode::Permission | ErrCode::Crash => Response::Ignore,
        ErrCode::Contract => Response::Exit,
    }
}

/// refs 中已死亡（Left）的上游列表。
pub fn dead_refs(results: &BTreeMap<String, crate::ast::Value>, refs: &[String]) -> Vec<String> {
    refs.iter()
        .filter(|r| results.get(*r).map(is_error_value).unwrap_or(false))
        .cloned()
        .collect()
}

/// 从编码文本还原错误分类（无 err_code 字段 → crash）。
pub fn code_of(encoded: &str) -> ErrCode {
    dslresult::extract_field("err_code", encoded)
        .and_then(|c| ErrCode::from_code(&c))
        .unwrap_or(ErrCode::Crash)
}

/// 关键节点集（v0.14b：判断关键过程/关键节点并调整行为）。
/// 定义：deliver proc 的引用闭包——`.deliver(@report)` → report → 其 impl refs
/// 逐层 BFS 回溯，凡是主产出链上的 proc 都是关键节点；不在链上的 = 旁路
/// （日志/通知/监控类）。无 deliver proc 的管线 = 全部关键（保守默认，v0.9 兼容）。
pub fn critical_set(pl: &crate::ast::Pipeline) -> std::collections::BTreeSet<String> {
    use std::collections::{BTreeSet, VecDeque};
    let mut critical: BTreeSet<String> = BTreeSet::new();
    let deliverers: Vec<&crate::ast::Proc> = pl.procs.iter().filter(|p| p.deliver).collect();
    if deliverers.is_empty() {
        for p in &pl.procs {
            critical.insert(p.name.clone());
        }
        return critical;
    }
    let mut queue: VecDeque<String> = VecDeque::new();
    for d in &deliverers {
        critical.insert(d.name.clone());
        // v0.14b：deliver 的主产出引用在 deliver_refs（deliver proc 无 impl），
        // 此字段由 parser 从 .deliver(@x) 参数解析（旧版被丢弃的实测 bug）
        for r in &d.deliver_refs {
            queue.push_back(r.clone());
        }
    }
    while let Some(name) = queue.pop_front() {
        if critical.contains(&name) {
            continue;
        }
        if let Some(p) = pl.procs.iter().find(|p| p.name == name) {
            critical.insert(name.clone());
            for imp in &p.plan {
                for r in &imp.refs {
                    queue.push_back(r.clone());
                }
            }
        }
    }
    critical
}

/// 终局裁决：关键 proc 上存在 Left → 致命（返回该 proc 名）。
/// 旁路 Left 不致命——主产出不受旁路失败牵连（调整行为：容忍+标记）。
pub fn fatal_left(
    pl: &crate::ast::Pipeline,
    partial: &BTreeMap<String, crate::ast::Value>,
) -> Option<String> {
    let critical = critical_set(pl);
    partial
        .iter()
        .find(|(name, v)| critical.contains(*name) && is_error_value(v))
        .map(|(name, _)| name.clone())
}

/// 关键性放大的失败策略（v0.14b）：关键节点重试预算 ×2——主链值得更努力；
/// 旁路节点用基础预算。其余 action 不变。
pub fn strategy_for(code: ErrCode, critical: bool) -> Action {
    match strategy(code) {
        Action::Retry { budget, backoff } => Action::Retry {
            budget: if critical { budget * 2 } else { budget },
            backoff,
        },
        other => other,
    }
}

/// 隐式传播（v0.14，无 DSL 面）：下游执行前检查上游引用，任一 Left → 本 proc
/// 落 Left（err_code 继承上游根因，err_msg 标记传播来源）。返回 true = 应短路。
pub fn upstream_left(
    results: &BTreeMap<String, crate::ast::Value>,
    refs: &[String],
) -> Option<String> {
    for r in refs {
        if let Some(v) = results.get(r) {
            if is_error_value(v) {
                return Some(r.clone());
            }
        }
    }
    None
}

/// 截断到约 n 字符（按 char 计，防 panic 于多字节边界）。
fn trunc_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{}…", t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 分类：引擎原生错误串（源码逐条摘录） ──

    #[test]
    fn engine_run_timeout() {
        assert_eq!(
            classify("run timed out after 5s: ffmpeg -i x.mp4"),
            ErrCode::Timeout
        );
    }

    #[test]
    fn engine_file_not_found() {
        assert_eq!(classify("file not found: /tmp/a.txt"), ErrCode::Resource);
    }

    #[test]
    fn engine_cp_source_not_found() {
        assert_eq!(
            classify("cp: source not found: /data/in.jpg"),
            ErrCode::Resource
        );
    }

    #[test]
    fn engine_read_permission() {
        // io::Error 形态：stat failed: Permission denied (os error 13)
        // permission 必须赢过 resource/data 的弱词
        assert_eq!(
            classify("read failed: Permission denied (os error 13)"),
            ErrCode::Permission
        );
    }

    #[test]
    fn engine_rm_protected_root() {
        assert_eq!(
            classify("rm refused: / is a protected root"),
            ErrCode::Contract
        );
    }

    #[test]
    fn engine_script_not_attached() {
        assert_eq!(
            classify("script 'word_stat' not attached (registered: a, b)"),
            ErrCode::Contract
        );
    }

    #[test]
    fn engine_param_not_in_contract() {
        assert_eq!(
            classify("param 'size' not in contract of 'resize'"),
            ErrCode::Contract
        );
    }

    #[test]
    fn engine_missing_required_param() {
        assert_eq!(classify("missing required param: text"), ErrCode::Contract);
    }

    #[test]
    fn engine_df_output() {
        assert_eq!(classify("disk: unexpected df output"), ErrCode::Format);
    }

    #[test]
    fn engine_launch_failed() {
        assert_eq!(
            classify("search bridge launch failed: No such file or directory (os error 2)"),
            ErrCode::Resource
        );
    }

    #[test]
    fn engine_spawn_failed() {
        assert_eq!(
            classify("spawn failed: Permission denied (os error 13)"),
            ErrCode::Permission
        );
    }

    #[test]
    fn engine_all_paths_failed_is_crash_or_contextual() {
        // All paths failed 本身不带根因特征 → crash 兜底（根因在更底层的原始错误里）
        assert_eq!(
            classify("All paths failed for proc: render"),
            ErrCode::Crash
        );
    }

    // ── 分类：外部脚本 stderr（Python/bash 常见形态） ──

    #[test]
    fn py_traceback_typeerror() {
        let tb = "Traceback (most recent call last):\n  File \"x.py\", line 3, in <module>\nTypeError: can only concatenate str (not \"int\") to str";
        assert_eq!(classify(tb), ErrCode::Schema);
    }

    #[test]
    fn py_traceback_filenotfound() {
        let tb = "Traceback (most recent call last):\n  File \"x.py\", line 2\nFileNotFoundError: [Errno 2] No such file or directory: 'in.csv'";
        // v0.14c：本地文件缺失仍是 resource（enotfound 裸模式已修，不再误伤）
        assert_eq!(classify(tb), ErrCode::Resource);
    }

    #[test]
    fn py_traceback_permissionerror() {
        let tb = "Traceback (most recent call last):\nPermissionError: [Errno 13] Permission denied: '/root/secret'";
        assert_eq!(classify(tb), ErrCode::Permission);
    }

    #[test]
    fn py_json_decode() {
        assert_eq!(
            classify("json.decoder.JSONDecodeError: Expecting value: line 1 column 1"),
            ErrCode::Format
        );
    }

    #[test]
    fn py_keyerror() {
        assert_eq!(classify("KeyError: 'width'"), ErrCode::Schema);
    }

    #[test]
    fn py_unicode_decode() {
        assert_eq!(
            classify("UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff"),
            ErrCode::Format
        );
    }

    #[test]
    fn py_connection_refused() {
        // v0.14c：连接类从 resource 拆出 → network（抖动自愈 vs 确定性缺失）
        assert_eq!(
            classify("requests.exceptions.ConnectionError: HTTPConnectionPool: Max retries exceeded ... Connection refused"),
            ErrCode::Network
        );
    }

    #[test]
    fn py_timeout_error() {
        assert_eq!(
            classify("requests.exceptions.ReadTimeout: HTTPSConnectionPool: Read timed out. (read timeout=30)"),
            ErrCode::Timeout
        );
    }

    #[test]
    fn bash_segfault_is_crash() {
        assert_eq!(
            classify("run failed (exit Some(139)): <empty stderr>"),
            ErrCode::Crash
        );
    }

    #[test]
    fn bash_oOm_kill() {
        // v0.14c：exit 137 = SIGKILL(常为 OOM-kill) → memory（原 crash）
        assert_eq!(classify("run failed (exit Some(137))"), ErrCode::Memory);
    }

    #[test]
    fn empty_is_crash() {
        assert_eq!(classify(""), ErrCode::Crash);
    }

    // ── ErrorRecord 编码 ──

    #[test]
    fn record_encodes_dsl_result_protocol() {
        let r = ErrorRecord::new("render", "ffmpeg", "run timed out after 5s: ffmpeg", 3);
        let enc = r.encode();
        assert_eq!(dslresult::extract_field("err", &enc).as_deref(), Some("1"));
        assert_eq!(
            dslresult::extract_field("err_code", &enc).as_deref(),
            Some("timeout")
        );
        assert_eq!(
            dslresult::extract_field("err_proc", &enc).as_deref(),
            Some("render")
        );
        assert_eq!(
            dslresult::extract_field("err_impl", &enc).as_deref(),
            Some("ffmpeg")
        );
        assert_eq!(
            dslresult::extract_field("attempts", &enc).as_deref(),
            Some("3")
        );
        // RAW 保留根因
        assert!(enc.contains("ffmpeg"));
    }

    #[test]
    fn record_truncates_long_raw() {
        let long = "x".repeat(10_000);
        let r = ErrorRecord::new("p", "i", &long, 1);
        assert!(r.raw.chars().count() <= 4097); // 4096 + ellipsis
        assert!(r.message.chars().count() <= 501);
    }

    #[test]
    fn is_error_value_detects_left() {
        let r = ErrorRecord::new("p", "i", "boom", 1);
        let v = crate::ast::Value::Text(r.encode());
        assert!(is_error_value(&v));
    }

    #[test]
    fn is_error_value_rejects_normal_output() {
        // 正常脚本输出（无 err 字段）
        let normal =
            dslresult::encode_structured_result(&[("score".to_string(), "85".to_string())], "raw");
        assert!(!is_error_value(&crate::ast::Value::Text(normal)));
        assert!(!is_error_value(&crate::ast::Value::Text(
            "plain text".into()
        )));
    }

    // ── 顺序陷阱回归（模式互相干扰的真实形态） ──

    #[test]
    fn upstream_left_detects_dead_dep() {
        let err = ErrorRecord::new("render", "ffmpeg", "boom timeout", 1).encode();
        let mut results = BTreeMap::new();
        results.insert("render".to_string(), crate::ast::Value::Text(err));
        results.insert("search".to_string(), crate::ast::Value::Text("ok".into()));
        assert_eq!(
            upstream_left(&results, &["search".to_string(), "render".to_string()]),
            Some("render".to_string())
        );
        assert_eq!(upstream_left(&results, &["search".to_string()]), None);
    }

    // ── 下游响应策略表（v0.14a3 用户裁定） ──

    #[test]
    fn respond_table() {
        use crate::errflow::Response;
        assert_eq!(respond(ErrCode::Timeout), Response::Wait);
        assert_eq!(respond(ErrCode::Resource), Response::Wait);
        assert_eq!(respond(ErrCode::Data), Response::Switch);
        assert_eq!(respond(ErrCode::Permission), Response::Ignore);
        assert_eq!(respond(ErrCode::Crash), Response::Ignore);
        assert_eq!(respond(ErrCode::Contract), Response::Exit);
    }

    // ── v0.14c 新六类 ──

    #[test]
    fn new_ratelimit() {
        assert_eq!(
            classify("HTTP 429 Too Many Requests: rate limit exceeded"),
            ErrCode::RateLimit
        );
        assert_eq!(
            classify("openai: quota exceeded for this month"),
            ErrCode::RateLimit
        );
    }

    #[test]
    fn new_auth() {
        assert_eq!(
            classify("HTTP 401 Unauthorized: invalid api key"),
            ErrCode::Auth
        );
        assert_eq!(classify("Error code: 403 - forbidden"), ErrCode::Auth);
        assert_eq!(
            classify("the token has expired (token expired)"),
            ErrCode::Auth
        );
    }

    #[test]
    fn new_memory() {
        assert_eq!(
            classify("torch.cuda.OutOfMemoryError: CUDA out of memory. Tried to allocate 2.5 GiB"),
            ErrCode::Memory
        );
        assert_eq!(classify("MemoryError"), ErrCode::Memory);
    }

    #[test]
    fn new_dependency() {
        assert_eq!(
            classify("ModuleNotFoundError: No module named 'numpy'"),
            ErrCode::Dependency
        );
        assert_eq!(
            classify("bash: ffmpeg: command not found"),
            ErrCode::Dependency
        );
        assert_eq!(
            classify("error while loading shared libraries: libcuda.so.1"),
            ErrCode::Dependency
        );
    }

    #[test]
    fn new_network() {
        assert_eq!(
            classify("socket.gaierror: [Errno -2] Name or service not known"),
            ErrCode::Network
        );
        assert_eq!(classify("HTTP 502 Bad Gateway"), ErrCode::Network);
        assert_eq!(classify("HTTP 503 Service Unavailable"), ErrCode::Network);
    }

    #[test]
    fn new_cancelled() {
        assert_eq!(classify("KeyboardInterrupt"), ErrCode::Cancelled);
        assert_eq!(
            classify("process interrupted by user (SIGINT)"),
            ErrCode::Cancelled
        );
    }

    #[test]
    fn respond_table_v14c() {
        use crate::errflow::Response;
        assert_eq!(respond(ErrCode::Cancelled), Response::Exit);
        assert_eq!(respond(ErrCode::RateLimit), Response::Wait);
        assert_eq!(respond(ErrCode::Memory), Response::Wait);
        assert_eq!(respond(ErrCode::Network), Response::Wait);
        assert_eq!(respond(ErrCode::Auth), Response::Switch);
        assert_eq!(respond(ErrCode::Dependency), Response::Ignore);
    }

    #[test]
    fn strategy_table_v14c() {
        use crate::errflow::Action;
        assert_eq!(strategy(ErrCode::Cancelled), Action::Escalate);
        assert_eq!(
            strategy(ErrCode::RateLimit),
            Action::Retry {
                budget: 2,
                backoff: Backoff::Linear
            }
        );
        assert_eq!(strategy(ErrCode::Auth), Action::Escalate);
        assert_eq!(
            strategy(ErrCode::Memory),
            Action::Retry {
                budget: 1,
                backoff: Backoff::Linear
            }
        );
        assert_eq!(strategy(ErrCode::Dependency), Action::Escalate);
    }

    // ── v0.14d data 四分 ──

    #[test]
    fn data_split_truncation() {
        assert_eq!(
            classify("openai response: finish_reason=length, output was truncated"),
            ErrCode::Truncation
        );
        assert_eq!(
            classify("This model's maximum context length is 4096 tokens"),
            ErrCode::Truncation
        );
        assert_eq!(
            classify("output truncated: response cut off at max_tokens"),
            ErrCode::Truncation
        );
    }

    #[test]
    fn data_split_format() {
        assert_eq!(
            classify("json.decoder.JSONDecodeError: Expecting value: line 1 column 1"),
            ErrCode::Format
        );
        assert_eq!(
            classify("UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff"),
            ErrCode::Format
        );
        assert_eq!(
            classify("yaml: mapping values are not allowed here — parse error"),
            ErrCode::Format
        );
    }

    #[test]
    fn data_split_schema() {
        assert_eq!(classify("KeyError: 'width'"), ErrCode::Schema);
        assert_eq!(
            classify("TypeError: can only concatenate str (not \"int\") to str"),
            ErrCode::Schema
        );
        assert_eq!(
            classify("pydantic ValidationError: field is required (missing field 'title')"),
            ErrCode::Schema
        );
    }

    #[test]
    fn data_narrowed_to_value_domain() {
        assert_eq!(
            classify("ValueError: invalid literal for int()"),
            ErrCode::Data
        );
        assert_eq!(
            classify("IndexError: list index out of range"),
            ErrCode::Data
        );
        assert_eq!(
            classify("ZeroDivisionError: division by zero"),
            ErrCode::Data
        );
        assert_eq!(classify("OverflowError: math range error"), ErrCode::Data);
    }

    #[test]
    fn strategy_v14d() {
        use crate::errflow::Action;
        // 截断=概率性失败 → 重采样重试；格式/结构=确定性坏 → 换路径
        assert_eq!(
            strategy(ErrCode::Truncation),
            Action::Retry {
                budget: 2,
                backoff: Backoff::Linear
            }
        );
        assert_eq!(strategy(ErrCode::Format), Action::Reroute);
        assert_eq!(strategy(ErrCode::Schema), Action::Reroute);
    }

    #[test]
    fn respond_v14d() {
        use crate::errflow::Response;
        assert_eq!(respond(ErrCode::Truncation), Response::Wait);
        assert_eq!(respond(ErrCode::Format), Response::Switch);
        assert_eq!(respond(ErrCode::Schema), Response::Switch);
    }

    #[test]
    fn strategy_table() {
        use crate::errflow::Action;
        assert_eq!(
            strategy(ErrCode::Timeout),
            Action::Retry {
                budget: 2,
                backoff: Backoff::Linear
            }
        );
        assert_eq!(strategy(ErrCode::Data), Action::Reroute);
        assert_eq!(strategy(ErrCode::Permission), Action::Escalate);
        assert_eq!(strategy(ErrCode::Contract), Action::Escalate);
        assert_eq!(strategy(ErrCode::Crash), Action::Escalate);
    }

    #[test]
    fn dead_refs_and_code_of() {
        let err =
            ErrorRecord::new("render", "ffmpeg", "Permission denied (os error 13)", 1).encode();
        let mut results = BTreeMap::new();
        results.insert("render".to_string(), crate::ast::Value::Text(err));
        results.insert("search".to_string(), crate::ast::Value::Text("ok".into()));
        let dead = dead_refs(&results, &["search".to_string(), "render".to_string()]);
        assert_eq!(dead, vec!["render".to_string()]);
        assert_eq!(
            code_of(&match results["render"] {
                crate::ast::Value::Text(ref t) => t.clone(),
                _ => String::new(),
            }),
            ErrCode::Permission
        );
        assert_eq!(code_of("plain text"), ErrCode::Crash);
    }

    #[test]
    fn permission_beats_resource_weak_words() {
        // "write failed: ... permission ..." —— resource 的 launch/spawn failed 不该吃掉它
        assert_eq!(
            classify("write failed: /out/x.mp4: Permission denied"),
            ErrCode::Permission
        );
    }

    #[test]
    fn timeout_beats_resource() {
        // 连接类超时：timed out 与 connection 同时出现 → timeout（等待超时是根因）
        assert_eq!(
            classify("Connection to db:5432 timed out. (connect timeout=10)"),
            ErrCode::Timeout
        );
    }

    #[test]
    fn contract_param_quote_pattern() {
        // "missing param" 前缀子串覆盖引擎与脚本两种形态；"param '" 宽模式不误伤
        assert_eq!(classify("missing parameter: width"), ErrCode::Contract);
        assert_eq!(classify("missing required param: text"), ErrCode::Contract);
        assert_eq!(classify("unknown parameter passed"), ErrCode::Crash);
    }

    // ── 关键节点判定（v0.14b） ──

    fn mini_pipeline() -> crate::ast::Pipeline {
        // gen → report → deliver 主链；notify 旁路
        crate::parser::parse_pipeline(
            "Pipeline(\"x\")\n  .proc(\"gen\")\n    .plan(a -> run(\"echo @seed\"))\n  .proc(\"report\")\n    .plan(r -> run(\"echo @gen\"))\n  .proc(\"notify\")\n    .plan(n -> run(\"echo side\"))\n  .proc(\"deliver\")\n    .deliver(@report)\n",
        )
        .unwrap()
    }

    #[test]
    fn critical_set_follows_deliver_closure() {
        let pl = mini_pipeline();
        let cs = critical_set(&pl);
        assert!(cs.contains("deliver"));
        assert!(cs.contains("report"));
        assert!(cs.contains("gen")); // BFS 回溯：deliver→report→gen
        assert!(!cs.contains("notify")); // 旁路不在链上
    }

    #[test]
    fn fatal_left_only_on_critical() {
        let pl = mini_pipeline();
        let mut partial = BTreeMap::new();
        // 旁路死 → 不致命
        let side = ErrorRecord::new("notify", "n", "Connection refused", 1).encode();
        partial.insert("notify".to_string(), crate::ast::Value::Text(side));
        partial.insert("gen".to_string(), crate::ast::Value::Text("ok".into()));
        assert_eq!(fatal_left(&pl, &partial), None);
        // 主链死 → 致命
        let main = ErrorRecord::new("gen", "a", "Connection refused", 1).encode();
        partial.insert("gen".to_string(), crate::ast::Value::Text(main));
        assert_eq!(fatal_left(&pl, &partial), Some("gen".to_string()));
    }

    #[test]
    fn strategy_for_doubles_retry_on_critical() {
        assert_eq!(
            strategy_for(ErrCode::Timeout, true),
            Action::Retry {
                budget: 4,
                backoff: Backoff::Linear
            }
        );
        assert_eq!(
            strategy_for(ErrCode::Timeout, false),
            Action::Retry {
                budget: 2,
                backoff: Backoff::Linear
            }
        );
        // 非 Retry 类不受关键性影响
        assert_eq!(strategy_for(ErrCode::Permission, true), Action::Escalate);
    }

    #[test]
    fn no_deliver_means_all_critical() {
        // 无 deliver proc → 全部关键（v0.9 兼容）
        let pl = crate::parser::parse_pipeline(
            "Pipeline(\"x\")\n  .proc(\"a\")\n    .plan(x -> run(\"echo 1\"))\n  .proc(\"b\")\n    .plan(y -> run(\"echo 2\"))\n",
        )
        .unwrap();
        let cs = critical_set(&pl);
        assert!(cs.contains("a") && cs.contains("b"));
    }
}
