//! v0.14 错误分类学 — Either 错误流的分类层。
//!
//! 设计（docs/v0.14_error_flow_spec.md §2）：
//! - 六类错误码：timeout / resource / permission / data / contract / crash
//! - 分类是引擎事实：纯函数启发式，`crash` 兜底永不过时（新模式先落 crash，库可迭代）
//! - 判定顺序（先具体后兜底）：contract → timeout → permission → resource → data → crash
//! - ErrorRecord 编码为 `§§FIELDS§§` 协议文本 → when.rs 现有解释器可直接路由
//!   `@proc.err_code == "timeout"`（零新求值器）
//!
//! 模式库来源：steps.rs / script.rs / executor.rs 全量错误字符串归纳
//! + 外部脚本（Python/bash）常见 stderr 形态。

use crate::dslresult;

/// 错误码（六类）。`code()` 返回静态串供编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrCode {
    Timeout,
    Resource,
    Permission,
    Data,
    Contract,
    Crash,
}

impl ErrCode {
    pub fn code(&self) -> &'static str {
        match self {
            ErrCode::Timeout => "timeout",
            ErrCode::Resource => "resource",
            ErrCode::Permission => "permission",
            ErrCode::Data => "data",
            ErrCode::Contract => "contract",
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

const PAT_PERMISSION: &[&str] = &[
    "permission denied",
    "permissionerror",
    "eacces",
    "eperm",
    "operation not permitted",
    "read-only file system",
];

const PAT_RESOURCE: &[&str] = &[
    "not found",
    "no such file",
    "nosuchfile",
    "filenotfounderror",
    "connection refused",
    "connection reset",
    "connectionerror",
    "enotfound",
    "econnrefused",
    "ehostunreach",
    "enetunreach",
    "launch failed",
    "spawn failed",
    "disk full",
    "enospc",
    "address in use",
    "eaddrinuse",
    "service unavailable",
    "503 service",
    "502 bad gateway",
    "504 gateway",
];

const PAT_DATA: &[&str] = &[
    "typeerror",
    "valueerror",
    "jsondecodeerror",
    "keyerror",
    "indexerror",
    "unicodedecodeerror",
    "unicodeencodeerror",
    "attributeerror",
    "validation",
    "invalid utf",
    "unexpected df output",
    "parse error",
    "syntaxerror",
    "overflow",
];

fn matches_any(lower: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| lower.contains(p))
}

/// 分类原始错误串。判定顺序：contract → timeout → permission → resource → data → crash。
///
/// 顺序理由：
/// - contract 在前：契约错误是引擎层概念，串形态固定（`param 'x' not in contract`），
///   不会与其他类的子串歧义
/// - permission 在 resource 前：`stat failed: Permission denied (os error 13)` 这类
///   io::Error 同时含 "failed" 弱词，权限是更强的语义信号
/// - resource 在 data 前：`read failed: No such file` 的 NotFound 属资源不可得
/// - data 的宽模式（validation/parse error）放后，只兜剩余
pub fn classify(raw: &str) -> ErrCode {
    let lower = raw.to_lowercase();
    if matches_any(&lower, PAT_CONTRACT) {
        ErrCode::Contract
    } else if matches_any(&lower, PAT_TIMEOUT) {
        ErrCode::Timeout
    } else if matches_any(&lower, PAT_PERMISSION) {
        ErrCode::Permission
    } else if matches_any(&lower, PAT_RESOURCE) {
        ErrCode::Resource
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

/// 值是否为错误值（Left）。true = 上游失败。
pub fn is_error_value(val: &crate::ast::Value) -> bool {
    match val {
        crate::ast::Value::Text(t) => dslresult::extract_field("err", t)
            .map(|v| v == "1")
            .unwrap_or(false),
        _ => false,
    }
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
        assert_eq!(classify("disk: unexpected df output"), ErrCode::Data);
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
        assert_eq!(classify(tb), ErrCode::Data);
    }

    #[test]
    fn py_traceback_filenotfound() {
        let tb = "Traceback (most recent call last):\n  File \"x.py\", line 2\nFileNotFoundError: [Errno 2] No such file or directory: 'in.csv'";
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
            ErrCode::Data
        );
    }

    #[test]
    fn py_keyerror() {
        assert_eq!(classify("KeyError: 'width'"), ErrCode::Data);
    }

    #[test]
    fn py_unicode_decode() {
        assert_eq!(
            classify("UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff"),
            ErrCode::Data
        );
    }

    #[test]
    fn py_connection_refused() {
        assert_eq!(
            classify("requests.exceptions.ConnectionError: HTTPConnectionPool: Max retries exceeded ... Connection refused"),
            ErrCode::Resource
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
        assert_eq!(classify("run failed (exit Some(137))"), ErrCode::Crash);
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
}
