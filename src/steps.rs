//! Steps — 内置函数执行器族（v0.7 resource ops + search/llm/merge + 注册表）。
//!
//! 从 executor.rs 拆出的执行层：step_registry 是唯一注册点（清单即表），
//! 进程管理（spawn/procs/kill/wait + PROC_TABLE）、文件系统操作
//! （exists/stat/ls/rm/cp/mkdir/disk）、桥接脚本调用（search/llm）、
//! 读写与 shell 执行（read/write/run/sh）。

use crate::ast::*;
use crate::dslresult::{encode_structured_result, parse_dsl_result_block};
use crate::textargs::{
    expand_fs_path, extract_all_string_args, extract_first_bare_arg, extract_first_string,
    extract_string_arg, resolve_vars,
};
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Best-effort home directory (HOME on Unix, USERPROFILE on Windows).
fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                r"C:\Users\Public".into()
            } else {
                "/tmp".into()
            }
        })
}

/// Shell gate: default allow (local trusted operator).
/// `DUCTILE_RESTRICT_SHELL=1` blocks run/sh/spawn; `DUCTILE_UNSAFE_SHELL=1` overrides.
pub fn shell_allowed() -> bool {
    shell_allowed_with(
        std::env::var("DUCTILE_UNSAFE_SHELL").ok(),
        std::env::var("DUCTILE_RESTRICT_SHELL").ok(),
    )
}

/// shell_allowed 的参数化内核（测试注入用——set_var 全局 env 在并行测试下
/// 会把 DUCTILE_RESTRICT_SHELL=1 泄漏进并发进程，曾致 run_echo 系列随机挂）。
pub fn shell_allowed_with(unsafe_shell: Option<String>, restrict_shell: Option<String>) -> bool {
    let flag = |v: Option<String>| {
        matches!(
            v.as_deref(),
            Some("1") | Some("true") | Some("yes") | Some("TRUE") | Some("YES")
        )
    };
    if flag(unsafe_shell) {
        return true;
    }
    !flag(restrict_shell)
}

fn deny_if_shell_restricted(op: &str) -> Result<(), String> {
    if shell_allowed() {
        Ok(())
    } else {
        Err(format!(
            "{} blocked by DUCTILE_RESTRICT_SHELL=1 — unset it, or set DUCTILE_UNSAFE_SHELL=1 to allow (local trusted only)",
            op
        ))
    }
}

/// Locate `bash` for `run`/`spawn` (required on all platforms today).
/// Windows: PATH first, then common Git for Windows installs.
pub fn resolve_bash() -> Result<PathBuf, String> {
    if let Some(p) = which_in_path("bash") {
        return Ok(p);
    }
    #[cfg(windows)]
    {
        for c in windows_bash_candidates() {
            if c.is_file() {
                return Ok(c);
            }
        }
        return Err(
            "bash not found — install Git for Windows and ensure bash is on PATH \
             (or use \"Git Bash\"). See docs/WINDOWS.md"
                .into(),
        );
    }
    #[cfg(not(windows))]
    {
        Err("bash not found on PATH".into())
    }
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", "", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{}{}", name, ext));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_bash_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(root) = std::env::var(key) {
            let r = PathBuf::from(root);
            v.push(r.join(r"Git\bin\bash.exe"));
            v.push(r.join(r"Git\usr\bin\bash.exe"));
            if key == "LOCALAPPDATA" {
                v.push(r.join(r"Programs\Git\bin\bash.exe"));
            }
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let h = PathBuf::from(home);
        v.push(h.join(r"scoop\apps\git\current\bin\bash.exe"));
        v.push(h.join(r"scoop\apps\git\current\usr\bin\bash.exe"));
    }
    v
}

/// Prepend dirs so child `ductile` / drills find the same binary as this process.
/// Also sets `DUCTILE_BIN` to `current_exe` when available.
// ── v0.16 管线级执行环境（第三刀）────────────────────────────
// Pipeline(..., cwd="...", env=["K=V"]) 的 thread_local 中继。
// exec_pipeline 入口 set（guard Drop 清理），steps 层 Command 构造时读取。
// 不进全局 env（并行测试竞态），不穿签名（StepFn 面太广）。
use std::cell::RefCell;

thread_local! {
    static PIPELINE_CTX: RefCell<Option<PipelineCtxInner>> = const { RefCell::new(None) };
}

struct PipelineCtxInner {
    cwd: Option<String>,
    env: Vec<(String, String)>,
}

pub struct PipelineCtx;

/// v0.17 auto-prompt 的 per-proc 上下文中继：exec_proc 进门 set，
/// exec_llm 合成 prompt 时读。guard Drop 清理（递归 exec_pipeline 安全）。
pub struct NodeCtxGuard;

impl Drop for NodeCtxGuard {
    fn drop(&mut self) {
        NODE_CTX.with(|c| *c.borrow_mut() = None);
    }
}

thread_local! {
    static NODE_CTX: RefCell<Option<NodeCtxInner>> = const { RefCell::new(None) };
}

struct NodeCtxInner {
    pipeline_name: String,
    pipeline_desc: String,
    proc_name: String,
    proc_desc: String,
    /// 本 proc 声明序之前、且被本 proc（或其 .when）引用过的上游 proc 集合。
    upstream_summaries: Vec<(String, String)>, // (name, desc)
    /// 本 proc 全部 impl 的 .when 条件（继承约束段用）。
    when_conds: Vec<String>,
    /// 上游契约（outputs/invariants）。
    upstream_contracts: Vec<String>,
    /// 下游消费者（引用本 proc 的后续节点）——位置信息：你的产出给谁用。
    downstream: Vec<String>,
}

pub struct NodeCtx;

impl NodeCtx {
    /// exec_proc 入口调用；guard Drop 清理。所有字符串预提取（inner 不借引用，
    /// 生命周期与 guard 绑定，无悬垂）。
    pub fn set(pl: &crate::ast::Pipeline, proc: &crate::ast::Proc) -> NodeCtxGuard {
        let refs_set: std::collections::BTreeSet<String> = proc
            .plan
            .iter()
            .flat_map(|i| i.refs.iter().cloned())
            .collect();
        // .when 里的 @ref 也算上游（门禁引用即数据依赖）
        // v0.17.3：.needs(@ref) 显式数据依赖也进 upstream——audit .when(@brk.tickets)
        // 只声明门禁时，合成器看不到它真正要逐条核对的 req（盲评实证 -7.3 分）。
        let mut upstream = Vec::new();
        for p in &pl.procs {
            if p.name == proc.name {
                break;
            }
            if refs_set.contains(&p.name)
                || when_refs(&p, &proc.plan)
                || proc.needs.contains(&p.name)
            {
                upstream.push((p.name.clone(), p.description.clone()));
            }
        }
        let upstream_contracts = pl
            .procs
            .iter()
            .take_while(|p| p.name != proc.name)
            .filter(|p| refs_set.contains(&p.name))
            .flat_map(|p| {
                let mut v = Vec::new();
                if !p.contract.outputs.is_empty() {
                    v.push(format!(
                        "上游「{}」契约要求输出字段：{}",
                        p.name,
                        p.contract.outputs.join(", ")
                    ));
                }
                for inv in &p.contract.invariants {
                    v.push(format!("上游「{}」不变式：{}", p.name, inv));
                }
                v
            })
            .collect();
        let when_conds = proc
            .plan
            .iter()
            .filter_map(|i| i.when.clone())
            .filter(|w| !w.trim().is_empty())
            .collect();
        // 下游消费者：声明序在本 proc 之后、refs/when 引用本 proc 的节点。
        // 位置语义：你的产出是它们的上游——字段稳定性和粒度要为它们负责。
        let downstream: Vec<String> = pl
            .procs
            .iter()
            .skip_while(|p| p.name != proc.name)
            .skip(1)
            .filter(|p| {
                p.plan
                    .iter()
                    .any(|i| i.refs.iter().any(|r| r == &proc.name))
                    || p.plan.iter().any(|i| {
                        i.when
                            .as_ref()
                            .map(|w| w.contains(&format!("@{}", proc.name)))
                            .unwrap_or(false)
                    })
            })
            .map(|p| {
                if p.description.is_empty() {
                    p.name.clone()
                } else {
                    format!("{}（{}）", p.name, p.description)
                }
            })
            .collect();
        NODE_CTX.with(|c| {
            *c.borrow_mut() = Some(NodeCtxInner {
                pipeline_name: pl.name.clone(),
                pipeline_desc: pl.description.clone(),
                proc_name: proc.name.clone(),
                proc_desc: proc.description.clone(),
                upstream_summaries: upstream,
                when_conds,
                upstream_contracts,
                downstream,
            })
        });
        NodeCtxGuard
    }

    /// exec_llm 调用：读取当前节点上下文合成 auto-prompt。
    /// 返回 None = 无上下文（不在 exec_proc 内）或信息不足不合成。
    pub fn synthesize_auto_prompt(
        topic: &str,
        results: &BTreeMap<String, Value>,
        schema: &str,
        guide: &str,
        system: &str,
    ) -> Option<String> {
        NODE_CTX.with(|c| match &*c.borrow() {
            None => None,
            Some(inner) => {
                let mut b: Vec<String> = Vec::new();
                // 1. 身份
                if inner.proc_desc.is_empty() && system.is_empty() {
                    return None; // 无身份可提炼 → 不合成
                }
                b.push(format!(
                    "# 任务上下文（auto-prompt 自动合成）\n管线「{}」{}节点「{}」{}",
                    inner.pipeline_name,
                    if inner.pipeline_desc.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", inner.pipeline_desc)
                    },
                    inner.proc_name,
                    if inner.proc_desc.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", inner.proc_desc)
                    }
                ));
                // 2. 主题
                if !topic.trim().is_empty() {
                    b.push(format!("# 主题输入\n{}", topic.trim()));
                }
                // 3. 上游摘要
                if !inner.upstream_summaries.is_empty() {
                    let mut lines = Vec::new();
                    let mut used = 0;
                    for (name, desc) in &inner.upstream_summaries {
                        if used >= 8 {
                            lines.push(format!(
                                "…（其余 {} 个上游省略）",
                                inner.upstream_summaries.len() - used
                            ));
                            break;
                        }
                        if let Some(val) = results.get(name) {
                            lines.push(format!(
                                "## 来自「{}」{}：\n{}",
                                name,
                                if desc.is_empty() {
                                    String::new()
                                } else {
                                    format!("（{}）", desc)
                                },
                                preview_value(val, preview_window(schema))
                            ));
                            used += 1;
                        }
                    }
                    if !lines.is_empty() {
                        b.push(format!("# 上游输入（你的原材料）\n{}", lines.join("\n")));
                    }
                }
                // 4. 继承约束
                let mut cons = Vec::new();
                for w in &inner.when_conds {
                    cons.push(format!("本节点仅在条件「{}」成立时执行（引擎门禁）", w));
                }
                cons.extend(inner.upstream_contracts.iter().cloned());
                if !cons.is_empty() {
                    b.push(format!(
                        "# 继承的约束（违反即失败）\n- {}",
                        cons.join("\n- ")
                    ));
                }
                // 4.5 位置信息：你的产出给谁用（下游消费者）——粒度与字段稳定性
                // 为它们负责；deliver 节点语义：这是最终交付物。
                if !inner.downstream.is_empty() {
                    b.push(format!(
                        "# 下游消费者（你的产出是它们的上游）\n{}",
                        inner.downstream.join("、")
                    ));
                }
                // 5. 开放动作（agent guide）
                if !guide.is_empty() {
                    b.push(format!("# 可用动作与检索方式\n{}", guide));
                }
                // 5.5 认知上下文：错误记忆 + 历史统计（cognition spec §4 pull 模型：
                // 指针卡 + 按需展开；L3 冷启动只记不判——不足 K 条历史不注入统计）。
                // 注入是防复发：同节点历史错误模式前置告知，不是全量倾倒。
                let mem = memory_digest(&inner.proc_name);
                if let Some(m) = mem {
                    b.push(m);
                }
                // 6. 输出契约 + 示例
                if !schema.is_empty() {
                    let fields: Vec<&str> = schema.split(',').map(|s| s.trim()).collect();
                    let example = fields
                        .iter()
                        .map(|f| format!("  \"{}\": \"...\"", f))
                        .collect::<Vec<_>>()
                        .join(",\n");
                    b.push(format!(
                        "# 输出契约\n只输出一个 JSON 对象，包含且仅包含这些字段：{}。\n示例：\n{{\n{}\n}}\n不要输出 JSON 以外的任何文字。",
                        schema, example
                    ));
                }
                if b.len() < 2 {
                    return None;
                }
                Some(b.join("\n\n"))
            }
        })
    }
}

/// 上游 p 是否被本 proc 的任一 .when 引用（@p 或 @p.field 形态）。
fn when_refs(p: &crate::ast::Proc, plan: &[crate::ast::Impl]) -> bool {
    plan.iter().any(|i| {
        i.when
            .as_ref()
            .map(|w| w.contains(&format!("@{}", p.name)))
            .unwrap_or(false)
    })
}

/// v0.17.1 上游预览窗口分级（按下游职责）：
/// - 常规：200 字符/字段（原 v0.17 行为，"最少必要信息"）
/// - 细节消费型：2000 字符/字段——schema 声明它要做逐条核对/清单产出
///   （missing/tickets/tasks/notes/findings 类字段），截断 200 会把它的
///   工作对象截没。盲评实证：审计节点拿 200 字符预览 vs 基线拿全文，
///   输掉 10 分且被批"仅两点、缺乏量化分析"。
/// 判定信号用下游自身 schema（声明的是它的职责，非上游内容）。
fn preview_window(schema: &str) -> usize {
    const DETAIL_CONSUMER: [&str; 6] =
        ["missing", "tickets", "tasks", "notes", "findings", "issues"];
    let has = schema
        .split(',')
        .map(|s| s.trim())
        .any(|f| DETAIL_CONSUMER.iter().any(|d| f.contains(d)));
    if has {
        2000
    } else {
        200
    }
}

/// v0.17 认知上下文注入：同节点错误记忆（open incident）+ 历史统计（runs）。
/// cognition spec §4：最小上下文按角色定义——caller 拿意图+结果卡，节点执行
/// 拿「我上次怎么错的」。错误卡是指针卡（id + 信号 + 一句话证据），pull 模型
/// ——全文在 incidents 表，按需 `ductile incident show` 展开。
/// L3 统计冷启动只记不判：<3 条历史不注入（防单样本误导）。
fn memory_digest(proc_name: &str) -> Option<String> {
    // cfg!(test) 守卫：单测不读真库（db 继承 DUCTILE_DATA 泄漏脚枪）
    if cfg!(test) {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    // 错误流：本节点未关闭的 incident（最近 3 条）
    if let Ok(conn) = crate::db::open_try() {
        let open = crate::incident::list_incidents_conn(&conn, Some("open"));
        let mine: Vec<_> = open
            .iter()
            .filter(|i| i.proc_name == proc_name)
            .take(3)
            .collect();
        if !mine.is_empty() {
            let cards: Vec<String> = mine
                .iter()
                .map(|i| {
                    format!(
                        "- [incident#{}] {} 信号:({}) 证据: {}",
                        i.id,
                        i.err_code,
                        trunc_chars(&i.signals, 80),
                        trunc_chars(&i.evidence, 120)
                    )
                })
                .collect();
            lines.push(format!(
                "# 错误记忆（本节点历史事故，防复发——展开用 ductile incident）\n{}",
                cards.join("\n")
            ));
        }
    }
    // 历史统计：本节点最近 runs 的成功率/时延分布（L3 统计带）
    let runs = crate::db::recent_runs_limit(proc_name, 20);
    if runs.len() >= 3 {
        let ok = runs.iter().filter(|r| r.status == "Ok").count();
        let total = runs.len();
        let lats: Vec<i64> = runs
            .iter()
            .map(|r| r.latency_ms)
            .filter(|l| *l > 0)
            .collect();
        if !lats.is_empty() {
            let max_l = *lats.iter().max().unwrap_or(&0);
            let min_l = *lats.iter().min().unwrap_or(&0);
            lines.push(format!(
                "# 历史统计（近 {} 次执行）\n成功率 {}/{}；时延 {}-{}ms。时延逼近上限或成功率异常时优先精简输出。",
                total, ok, total, min_l, max_l
            ));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n\n"))
    }
}

/// v0.17 auto-prompt 的上游值预览：§§FIELDS§§ 编码 → 字段=截断值 行列表；
/// 其他文本 → 截断原文。最少必要信息 ≠ 全文倾倒。
fn preview_value(val: &Value, limit: usize) -> String {
    let text = match val {
        Value::Text(t) => t.clone(),
        Value::File(p) => format!("[文件: {}]", p),
        Value::Null => String::new(),
    };
    if text.starts_with("§§FIELDS§§") {
        let body = &text["§§FIELDS§§".len()..];
        let mut lines = Vec::new();
        for part in body.split("§§") {
            if part.is_empty() || !part.contains('=') {
                continue;
            }
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            if k == "RAW" {
                continue; // 原文全文不进 prompt
            }
            let v_prev = trunc_chars(v, limit);
            lines.push(format!("{} = {}", k, v_prev));
        }
        return lines.join("\n");
    }
    trunc_chars(&text, limit * 4)
}

fn trunc_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_string()
    } else {
        let cut: String = s.chars().take(limit).collect();
        format!("{}…", cut)
    }
}

impl PipelineCtx {
    /// exec_pipeline 入口调用；返回 guard，Drop 时清理。
    /// cwd 规范化（bash -c cd + pwd）：$VAR / $(...) / ~ 展开 + 相对路径判定。
    /// 失败 fail-closed：cwd 坏 = 整流必错，panic 于 exec_pipeline 转硬错误。
    pub fn set(pl: &crate::ast::Pipeline) -> Option<PipelineCtxGuard> {
        if pl.cwd.is_none() && pl.env.is_empty() {
            return None;
        }
        let cwd = match &pl.cwd {
            Some(c) => match resolve_pipeline_cwd(c) {
                Ok(p) => Some(p),
                Err(e) => {
                    let msg = format!("pipeline cwd invalid: {} ({})", c, e);
                    eprintln!("  [env] {}", msg);
                    return Some(PipelineCtxGuard {
                        poisoned: true,
                        msg: Some(msg),
                    });
                }
            },
            None => None,
        };
        let env: Vec<(String, String)> = pl
            .env
            .iter()
            .filter_map(|e| {
                e.find('=')
                    .map(|eq| (e[..eq].to_string(), e[eq + 1..].to_string()))
            })
            .collect();
        PIPELINE_CTX.with(|c| *c.borrow_mut() = Some(PipelineCtxInner { cwd, env }));
        Some(PipelineCtxGuard {
            poisoned: false,
            msg: None,
        })
    }

    /// 当前线程管线 cwd（未设置 = None）
    pub fn cwd() -> Option<String> {
        PIPELINE_CTX.with(|c| c.borrow().as_ref().and_then(|i| i.cwd.clone()))
    }

    /// 当前线程管线 env 对（未设置 = 空）
    pub fn env_pairs() -> Vec<(String, String)> {
        PIPELINE_CTX.with(|c| {
            c.borrow()
                .as_ref()
                .map(|i| i.env.clone())
                .unwrap_or_default()
        })
    }
}

/// Drop 清理 + cwd 失败的延迟 poison（exec_pipeline 检查 guard 后整流退出）
pub struct PipelineCtxGuard {
    poisoned: bool,
    msg: Option<String>,
}

impl PipelineCtxGuard {
    pub fn poison(&self) -> Option<&str> {
        if self.poisoned {
            self.msg.as_deref()
        } else {
            None
        }
    }
}

impl Drop for PipelineCtxGuard {
    fn drop(&mut self) {
        PIPELINE_CTX.with(|c| *c.borrow_mut() = None);
    }
}

/// cwd 规范化：bash `cd <dir> && pwd` 拿绝对路径。
/// 环境变量 / 命令替换 / ~ 全交给 bash（DSL 值无需自实现展开器）。
fn resolve_pipeline_cwd(raw: &str) -> Result<String, String> {
    let bash = resolve_bash()?;
    let out = Command::new(&bash)
        .arg("-c")
        .arg(format!("cd {} && pwd", shell_quote_cwd(raw)))
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// cwd 值单引号包裹（bash 单引号内零展开——$VAR 由 bash 在 cd 参数位置…不行，
/// $VAR 需要展开）。改为：外层单引号剥掉，值原样传给 bash -c（值内引号转义）。
/// 实际策略：值里既可能写 /abs/path 也可能写 $HOME/x 或 $(pwd)——原样交给
/// bash -c 的 cd 参数位置，用双引号包裹并对值内 " \ ` $ 保留（保留 $ 展开语义）。
fn shell_quote_cwd(raw: &str) -> String {
    // 双引号包裹：$VAR/$(...) 展开，" 和 \ 转义。单引号原样。
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn apply_ductile_child_env(command: &mut Command) {
    let mut prepend: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        command.env("DUCTILE_BIN", &exe);
        if let Some(dir) = exe.parent() {
            prepend.push(dir.to_path_buf());
        }
    }
    // v0.16 管线级 env（第三刀）：Pipeline(env=[...]) 经 thread_local 中继注入。
    // 身份变量 PATH/HOME/USER 永不覆盖（与 impl 级 env= 同规则）。
    for (k, v) in PipelineCtx::env_pairs() {
        if k == "PATH" || k == "HOME" || k == "USER" {
            continue;
        }
        command.env(k, v);
    }
    if let Ok(td) = std::env::var("CARGO_TARGET_DIR") {
        let td = PathBuf::from(td);
        prepend.push(td.join("release"));
        prepend.push(td.join("debug"));
    }
    if let Ok(root) = std::env::var("DUCTILE_ROOT") {
        let root = PathBuf::from(root);
        prepend.push(root.join("target").join("release"));
        prepend.push(root.join("target").join("debug"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut cur = Some(cwd);
        while let Some(p) = cur {
            if p.join(".git").exists() || p.join("Cargo.toml").exists() {
                prepend.push(p.join("target").join("release"));
                prepend.push(p.join("target").join("debug"));
                break;
            }
            cur = p.parent().map(|x| x.to_path_buf());
        }
    }
    let sep = if cfg!(windows) { ";" } else { ":" };
    let extra: Vec<String> = prepend
        .into_iter()
        .filter(|d| d.is_dir())
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    if extra.is_empty() {
        return;
    }
    let old = std::env::var("PATH").unwrap_or_default();
    command.env("PATH", format!("{}{}{}", extra.join(sep), sep, old));
}

// ── Function Registry（v0.11.1 重构：Registry + Adapter 模式）──
//
// 旧形态：run_impl_steps 里 19 分支的裸 match。两个真实缺陷：
//   1. fail-closed 错误文案里的 Known 函数清单是手工字符串，已漂移（web_search 写了两遍）；
//   2. 新增函数要同时改 match 和文案，漏一处就静默不一致。
// 新形态：单一注册表 = 函数名 → StepFn（统一签名）。异构签名的 fs 系函数用
// 闭包 **Adapter** 归一；清单文案由表键自动生成——清单即注册表，不可能漂移。
// `cache` 分支保留为显式 Err 表项（测试探针依赖此语义）。

/// 统一步骤签名：所有 exec_* 归一到它。
type StepFn = Box<dyn Fn(&Impl, &str, &str, &BTreeMap<String, Value>) -> Result<Value, String>>;

/// fs 系（无 topic/results）→ 统一签名的 Adapter。
fn fs_adapter(f: fn(&Impl, &str) -> Result<Value, String>) -> StepFn {
    Box::new(move |impl_, _topic, body, _results| f(impl_, body))
}

/// 函数注册表：名字 → 执行器。新增函数只需在此插一行。
pub fn step_registry() -> BTreeMap<&'static str, StepFn> {
    let mut m: BTreeMap<&'static str, StepFn> = BTreeMap::new();
    // search/llm/merge/write/read
    m.insert(
        "search",
        Box::new(|i: &Impl, t: &str, b: &str, r: &BTreeMap<String, Value>| {
            exec_search(i, t, b, r, true)
        }),
    );
    m.insert(
        "mcp_search",
        Box::new(|i: &Impl, t: &str, b: &str, r: &BTreeMap<String, Value>| {
            exec_search(i, t, b, r, true)
        }),
    );
    m.insert(
        "web_search",
        Box::new(|i: &Impl, t: &str, b: &str, r: &BTreeMap<String, Value>| {
            exec_search(i, t, b, r, false)
        }),
    );
    m.insert("llm", Box::new(exec_llm));
    m.insert(
        "merge",
        Box::new(|i: &Impl, _t: &str, b: &str, r: &BTreeMap<String, Value>| exec_merge(i, b, r)),
    );
    m.insert("write", Box::new(exec_write));
    m.insert("read", Box::new(exec_read));
    m.insert("read_file", Box::new(exec_read));
    // run/sh
    m.insert("run", Box::new(exec_run));
    m.insert("sh", Box::new(exec_run));
    // v0.7 resource ops
    m.insert("spawn", Box::new(exec_spawn));
    m.insert("procs", Box::new(exec_procs));
    m.insert("kill", Box::new(exec_kill));
    m.insert("wait", Box::new(exec_wait));
    // fs ops（Adapter 归一）
    m.insert("exists", fs_adapter(exec_fs_exists));
    m.insert("stat", fs_adapter(exec_fs_stat));
    m.insert("ls", fs_adapter(exec_fs_ls));
    m.insert("rm", fs_adapter(exec_fs_rm));
    m.insert("cp", fs_adapter(exec_fs_cp));
    m.insert("mkdir", fs_adapter(exec_fs_mkdir));
    m.insert(
        "disk",
        Box::new(|i: &Impl, _t: &str, b: &str, _r: &BTreeMap<String, Value>| exec_disk(i, b)),
    );
    // v0.12 脚本契约线：script(name, k=v...) — 脚本即 API
    m.insert("script", Box::new(exec_script_call));
    m
}

/// 探针保留语义：cache = 恒 Err("cache miss")（故意失败路径的测试钩子）。
pub fn is_probe_stub(func: &str) -> bool {
    func == "cache"
}

/// 已知函数清单（由注册表键自动生成——清单即表，不可能漂移）。
pub fn known_functions() -> Vec<&'static str> {
    step_registry().keys().copied().collect()
}

// ── Built-in executors ──

fn exec_search(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
    is_mcp: bool,
) -> Result<Value, String> {
    let query = extract_string_arg("query", body);
    let resolved = resolve_vars(&query, topic, results);
    let func_name = if is_mcp { "mcp_search" } else { "web_search" };
    eprintln!("    -> {}: \"{}\"", func_name, resolved);

    let bridge = find_bridge("search_bridge.py");
    let mode = if is_mcp { "mcp_search" } else { "web_search" };
    let engine = extract_string_arg("engine", body);

    let output = Command::new("python3")
        .arg(&bridge)
        .arg(mode)
        .arg(&resolved)
        .args(if engine.is_empty() {
            vec![]
        } else {
            vec![&engine]
        })
        .output()
        .map_err(|e| format!("search bridge launch failed: {}", e))?;

    if output.status.success() {
        Ok(Value::Text(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    } else {
        Err(format!(
            "search failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ))
    }
}

fn exec_llm(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    // v0.16 agent 引用：llm(<name>, ...) 裸首参（与 script(name, k=v) 同工效学）。
    // 首参是裸标识符（非 k=v）且 [agents.<name>] 存在 → 视为 agent 名，从
    // config.toml 取 model/system/schema/timeout。显式实参优先于 agent 配置
    //（实参覆盖配置——局部临时改 model 不用动配置文件）。agent 不存在 →
    // fail-closed 硬错误（防手滑把普通词当 agent 静默落错模型）。
    let first_bare = extract_first_bare_arg(body);
    let mut agent_cfg: Option<crate::config::AgentConfig> = None;
    if let Some(name) = &first_bare {
        let agents = crate::config::load_agents_config();
        match agents.get(name) {
            Some(a) => agent_cfg = Some(a.clone()),
            None => {
                // 真实意图校验：首参裸标识符但无对应 [agents.*] 段——若它不是
                // 任何已知 k=v 参数名，就是想引 agent 写错了名，硬错误。
                let known_keys = [
                    "prompt", "input", "template", "model", "system", "schema", "count",
                ];
                if !known_keys.contains(&name.as_str()) {
                    let available: Vec<String> = agents.agents.keys().cloned().collect();
                    return Err(format!(
                        "llm: unknown agent '{}' (first bare arg) — define [agents.{}] in config.toml or use prompt=/input=. Known agents: [{}]",
                        name, name,
                        if available.is_empty() { "(none)".to_string() } else { available.join(", ") }
                    ));
                }
            }
        }
    }

    // prompt= aliases input= (examples historically used prompt=)
    let prompt_raw = {
        let p = extract_string_arg("prompt", body);
        if p.is_empty() {
            extract_string_arg("input", body)
        } else {
            p
        }
    };
    let template = extract_string_arg("template", body);
    // 合并序：显式实参 > agent 配置 > [llm] 默认（bridge 侧 env 兜底）
    let model = {
        let m = extract_string_arg("model", body);
        if m.is_empty() {
            agent_cfg
                .as_ref()
                .map(|a| a.model.clone())
                .unwrap_or_default()
        } else {
            m
        }
    };
    let system = {
        let s = extract_string_arg("system", body);
        if s.is_empty() {
            agent_cfg
                .as_ref()
                .map(|a| a.system.clone())
                .unwrap_or_default()
        } else {
            s
        }
    };
    let schema = {
        let s = extract_string_arg("schema", body);
        if s.is_empty() {
            agent_cfg
                .as_ref()
                .map(|a| a.schema.clone())
                .unwrap_or_default()
        } else {
            s
        }
    };
    let count = extract_string_arg("count", body);
    let tier_arg = {
        let t = extract_string_arg("tier", body);
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let resolved_prompt = {
        let p = resolve_vars(&prompt_raw, topic, results);
        if p.is_empty() {
            // v0.17 auto-prompt：prompt 缺省 + agent 存在 → 从图上下文合成。
            // 六段结构：身份/主题/上游摘要/继承约束/开放动作(guide)/输出契约+示例。
            // 无 agent 或信息不足 → 维持硬错误（auto-prompt 是糖不是承重墙）。
            let synth = agent_cfg.as_ref().and_then(|a| {
                NodeCtx::synthesize_auto_prompt(
                    topic,
                    results,
                    &resolve_vars(&schema, topic, results),
                    &a.guide,
                    &resolve_vars(&a.system, topic, results),
                )
            });
            match synth {
                Some(p) => {
                    eprintln!(
                        "    -> llm auto-prompt: synthesized {} chars",
                        p.chars().count()
                    );
                    p
                }
                None => return Err("llm requires prompt=\"...\" or input=\"...\" (auto-prompt needs an agent with .desc or system)".into()),
            }
        } else {
            p
        }
    };
    let resolved_template = resolve_vars(&template, topic, results);
    let resolved_system = resolve_vars(&system, topic, results);
    let resolved_schema = resolve_vars(&schema, topic, results);
    let resolved_model = resolve_vars(&model, topic, results);

    // v0.16.1 智力阶梯：agent 声明 tiers 且未显式 model= → 阶梯执行。
    // 档位 i 失败自动升 i+1（Reroute 换路哲学，阶梯有界）。显式 model= 完全
    // 旁路（探测类场景）。
    let schema_fields = if resolved_schema.is_empty() {
        0
    } else {
        resolved_schema.split(',').count()
    };
    let use_ladder = agent_cfg.as_ref().is_some_and(|a| !a.tiers.is_empty())
        && extract_string_arg("model", body).is_empty();
    let mut ladder: Vec<String> = Vec::new();
    let mut tier_idx = 0usize;
    if use_ladder {
        let agent = agent_cfg.as_ref().unwrap();
        let tiers_cfg = crate::config::load_tiers_config();
        let (l, idx) = crate::config::resolve_tier_start(
            agent,
            &tiers_cfg,
            resolved_prompt.len(),
            resolved_system.len(),
            schema_fields,
            tier_arg.as_deref(),
        )?;
        ladder = l;
        tier_idx = idx;
    }
    eprintln!(
        "    -> llm: model={} schema={} input.len={}{}",
        if resolved_model.is_empty() {
            "(default)"
        } else {
            &resolved_model
        },
        !resolved_schema.is_empty(),
        resolved_prompt.len(),
        if let Some(name) = &first_bare {
            format!(" agent={}", name)
        } else {
            String::new()
        }
    );

    let bridge = find_bridge("llm_bridge.py");
    if !Path::new(&bridge).exists() {
        return Err(format!(
            "llm bridge not found at '{}' (place bridge/llm_bridge.py in repo or ~/.local/share/ductile/bridge/)",
            bridge
        ));
    }

    let mut args: Vec<String> = Vec::new();
    args.push("--prompt".into());
    args.push(resolved_prompt.clone());
    if !resolved_template.is_empty() {
        args.push("--template".into());
        args.push(resolved_template);
    }
    // model 不在此注入：阶梯路径按档位动态换，legacy 路径走 resolved_model
    if !resolved_system.is_empty() {
        args.push("--system".into());
        args.push(resolved_system);
    }
    if !resolved_schema.is_empty() {
        args.push("--schema".into());
        args.push(resolved_schema);
    }
    if !count.is_empty() {
        args.push("--count".into());
        args.push(count);
    }

    // 阶梯路径：档位 i 失败 → 升 i+1 重试（阶梯有界 = 自动 Reroute）。
    // 每档注入 [models.<tier>] 的 model/base_url/api_key/timeout（显式覆盖 [llm]）。
    if !ladder.is_empty() {
        let tiers_cfg = crate::config::load_tiers_config();
        let agent_timeout = agent_cfg.as_ref().map(|a| a.timeout_secs).unwrap_or(0);
        let mut last_err = String::new();
        let mut i = tier_idx;
        while i < ladder.len() {
            let tier_name = &ladder[i];
            let Some(tier) = tiers_cfg.tiers.get(tier_name) else {
                // resolve_tier_start 已校验过——此处理论不可达，fail-closed 兜底
                return Err(format!("tier '{}' vanished from config mid-run", tier_name));
            };
            let mut t_args = args.clone();
            t_args.push("--model".into());
            t_args.push(tier.model.clone());
            eprintln!(
                "    -> llm tier [{}/{}] {} -> model={}",
                i + 1,
                ladder.len(),
                tier_name,
                tier.model
            );
            let out = run_python_bridge_tier(&bridge, &t_args, agent_timeout, Some(tier));
            match out {
                Ok(output) if output.status.success() => {
                    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
                    if let Some(kvs) = parse_dsl_result_block(&stdout_text) {
                        if !kvs.is_empty() {
                            eprintln!(
                                "    -> llm result: {} fields (tier={})",
                                kvs.len(),
                                tier_name
                            );
                            let mut enriched = kvs;
                            enriched.push(("meta_tier".into(), tier_name.clone()));
                            enriched.push(("meta_model_id".into(), tier.model.clone()));
                            return Ok(Value::Text(encode_structured_result(
                                &enriched,
                                &stdout_text,
                            )));
                        }
                    }
                    // 成功但无结构化块——原样返回（meta 已在 bridge 侧？没有；补 tier 标注）
                    eprintln!("    -> llm result: plain text (tier={})", tier_name);
                    return Ok(Value::Text(stdout_text));
                }
                Ok(output) => {
                    last_err = format!(
                        "tier '{}' (model={}) failed: {}",
                        tier_name,
                        tier.model,
                        String::from_utf8_lossy(&output.stderr)
                            .chars()
                            .take(200)
                            .collect::<String>()
                    );
                }
                Err(e) => {
                    last_err = format!("tier '{}' bridge launch failed: {}", tier_name, e);
                }
            }
            if i + 1 < ladder.len() {
                eprintln!(
                    "    -> llm escalate: {} -> {} (upgrading intelligence tier)",
                    tier_name,
                    ladder[i + 1]
                );
            }
            i += 1;
        }
        return Err(format!(
            "llm: all ladder tiers [{}] failed. Last: {}",
            ladder[tier_idx..].join(", "),
            last_err
        ));
    }

    // legacy 单模型路径
    let mut args = args;
    if !resolved_model.is_empty() {
        args.push("--model".into());
        args.push(resolved_model);
    }
    let output = if let Some(a) = &agent_cfg {
        run_python_bridge_with_timeout(&bridge, &args, a.timeout_secs)?
    } else {
        run_python_bridge(&bridge, &args)?
    };

    if output.status.success() {
        let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
        if let Some(kvs) = parse_dsl_result_block(&stdout_text) {
            if !kvs.is_empty() {
                eprintln!("    -> llm result: {} fields", kvs.len());
                return Ok(Value::Text(encode_structured_result(&kvs, &stdout_text)));
            }
        }
        Ok(Value::Text(stdout_text))
    } else {
        Err(format!(
            "llm failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        ))
    }
}

/// Prefer `python3`, fall back to `python` (Windows).
/// Injects [llm] from config.toml into child env when process env lacks OPENAI_*.
/// v0.16：agent timeout 注入——[agents.x] timeout_secs 显式覆盖（agent 场景常需
/// 比全局 [llm] 更长的窗口，schema 长输出尤其）。
fn run_python_bridge(bridge: &str, args: &[String]) -> Result<std::process::Output, String> {
    run_python_bridge_with_timeout(bridge, args, 0)
}

fn run_python_bridge_with_timeout(
    bridge: &str,
    args: &[String],
    agent_timeout_secs: u64,
) -> Result<std::process::Output, String> {
    let cfg = crate::config::load_llm_config();
    for py in ["python3", "python"] {
        let mut cmd = Command::new(py);
        cmd.arg(bridge).args(args);
        crate::config::apply_llm_env_from_config(&mut cmd, &cfg);
        if agent_timeout_secs > 0 {
            cmd.env("OPENAI_TIMEOUT_SECS", agent_timeout_secs.to_string());
        }
        match cmd.output() {
            Ok(o) => return Ok(o),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("llm bridge launch failed ({py}): {e}")),
        }
    }
    Err("llm bridge launch failed: neither python3 nor python found on PATH".into())
}

/// v0.16.1 档位桥：tier 的 model 经 --model 已注入 args；此处注入档位的
/// base_url/api_key/timeout（tier 显式值覆盖 [llm]，缺省回落）。
fn run_python_bridge_tier(
    bridge: &str,
    args: &[String],
    agent_timeout_secs: u64,
    tier: Option<&crate::config::ModelTier>,
) -> Result<std::process::Output, String> {
    let cfg = crate::config::load_llm_config();
    for py in ["python3", "python"] {
        let mut cmd = Command::new(py);
        cmd.arg(bridge).args(args);
        crate::config::apply_llm_env_from_config(&mut cmd, &cfg);
        if let Some(t) = tier {
            if !t.base_url.is_empty() {
                cmd.env("OPENAI_BASE_URL", &t.base_url);
            }
            if !t.api_key.is_empty() {
                cmd.env("OPENAI_API_KEY", &t.api_key);
            }
            if t.timeout_secs > 0 {
                cmd.env("OPENAI_TIMEOUT_SECS", t.timeout_secs.to_string());
            }
            // v0.17：tier 级 max_tokens（思考型模型预算——35B 思考 9k+ 的教训）。
            // 显式配置覆盖环境变量；缺省不动（沿用 OPENAI_MAX_TOKENS）。
            if t.max_tokens > 0 {
                cmd.env("OPENAI_MAX_TOKENS", t.max_tokens.to_string());
            }
        }
        if agent_timeout_secs > 0 && tier.is_none() {
            cmd.env("OPENAI_TIMEOUT_SECS", agent_timeout_secs.to_string());
        }
        match cmd.output() {
            Ok(o) => return Ok(o),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("llm bridge launch failed ({py}): {e}")),
        }
    }
    Err("llm bridge launch failed: neither python3 nor python found on PATH".into())
}

fn exec_merge(
    impl_: &Impl,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let refs = &impl_.refs;
    let parts: Vec<String> = refs
        .iter()
        .filter(|r| *r != "merge")
        .filter_map(|r| results.get(r))
        .map(|v| v.as_text().to_string())
        .collect();
    let has_dedup = body.contains("dedup");
    let merged = if has_dedup {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for p in &parts {
            if seen.insert(p.clone()) {
                out.push(p.clone());
            }
        }
        out.join("\n")
    } else {
        parts.join("\n---\n")
    };
    eprintln!(
        "    -> merge: {} sources, {} chars",
        parts.len(),
        merged.len()
    );
    Ok(Value::Text(merged))
}

// ── Resource management helpers (v0.7) ──

/// Best-effort kill of an entire process group (POSIX only; no-op elsewhere).
/// Uses kill(-pid, SIGKILL) via libc-free syscall wrapper: we shell out to
/// `kill` itself to avoid adding a libc dependency.
fn libc_kill_group(pid: i32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", pid))
        .output();
}

/// Extract ALL quoted values for a repeated key: env="A=1" env="B=2" → ["A=1","B=2"]
fn exec_write(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let to_path = extract_string_arg("to", body);
    let content = extract_string_arg("content", body);
    let resolved_path = resolve_vars(&to_path, topic, results);
    let resolved_content = resolve_vars(&content, topic, results);
    let expanded = expand_fs_path(&resolved_path);
    eprintln!("    -> write: {}", expanded);

    let path = Path::new(&expanded);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, &resolved_content).map_err(|e| format!("write failed: {}", e))?;
    Ok(Value::File(expanded))
}

// ── v0.7 Resource management ops: processes ──

use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Global process table: handle name → (pid, start_unix, command)
static PROC_TABLE: OnceLock<Mutex<StdHashMap<String, (u32, u64, String)>>> = OnceLock::new();

fn proc_table() -> &'static Mutex<StdHashMap<String, (u32, u64, String)>> {
    PROC_TABLE.get_or_init(|| Mutex::new(StdHashMap::new()))
}

/// 进程存活检查（僵尸感知）：/proc/pid/stat 第 3 字段为状态，
/// Z = 僵尸（已退出待 reap）→ 视为不在运行。
fn proc_alive(pid: u32) -> bool {
    let stat = match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // 格式：pid (comm) state ...；comm 可含空格，取最后一个 ')' 之后的首个非空字符
    match stat.rfind(')') {
        Some(i) => stat[i + 1..].trim_start().starts_with(|c: char| c != 'Z'),
        None => false,
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// spawn("name", "cmd") [timeout=0] — background launch, returns pid.
/// Handle name is used by wait/kill/procs. Non-blocking.
fn exec_spawn(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    deny_if_shell_restricted("spawn")?;
    let name = extract_string_arg("name", body);
    if name.is_empty() {
        return Err("spawn requires name=\"handle\"".into());
    }
    // cmd: prefer cmd="..." arg; fall back to first quoted string that isn't the name
    let cmd_raw = {
        let c = extract_string_arg("cmd", body);
        if !c.is_empty() {
            c
        } else {
            // strip name="..." then take first quoted string
            let stripped = body.replace(&format!("name=\"{}\"", name), "");
            extract_first_string(&stripped)
        }
    };
    let cmd = resolve_vars(&cmd_raw, topic, results);
    if cmd.is_empty() || cmd == name {
        return Err("spawn requires a command string".into());
    }
    use std::process::Stdio;
    // v0.12.1：Unix 上子进程自立进程组。此前继承父组 → exec_kill 的
    // kill -9 -pid（组杀）目标组不存在，静默无效；且孤孙子进程
    // （cmd 里的 `&`）会在组杀时漏杀。Windows 无 process_group，仅 kill 主进程。
    let mut spawn_cmd = Command::new(resolve_bash()?);
    spawn_cmd
        .arg("-c")
        .arg(&cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_ductile_child_env(&mut spawn_cmd);
    #[cfg(unix)]
    {
        spawn_cmd.process_group(0);
    }
    let child = spawn_cmd
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;
    let pid = child.id();
    eprintln!("    -> spawn[{}]: pid={} cmd={}", name, pid, cmd);
    proc_table()
        .lock()
        .map_err(|e| format!("proc table poisoned: {}", e))?
        .insert(name, (pid, unix_now(), cmd));
    Ok(Value::Text(format!("{}", pid)))
}

/// procs() or procs("name") — list live handles. Output: name pid age_s cmd per line.
fn exec_procs(
    _impl_: &Impl,
    _topic: &str,
    _body: &str,
    _results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let filter = extract_string_arg("name", _body);
    let table = proc_table()
        .lock()
        .map_err(|e| format!("proc table poisoned: {}", e))?;
    let now = unix_now();
    let mut lines = Vec::new();
    for (name, (pid, start, cmd)) in table.iter() {
        if !filter.is_empty() && name != &filter {
            continue;
        }
        // liveness check via /proc（僵尸感知）
        let alive = proc_alive(*pid);
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}",
            name,
            pid,
            now.saturating_sub(*start),
            if alive { "alive" } else { "dead" },
            cmd
        ));
    }
    Ok(Value::Text(lines.join("\n")))
}

/// kill("name") — SIGKILL the handle's process group; also accepts raw pid("1234").
fn exec_kill(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let target = resolve_vars(&extract_string_arg("name", body), topic, results);
    if target.is_empty() {
        return Err("kill requires name=\"handle\" or a pid".into());
    }
    let pid: i32 = if let Ok(p) = target.parse::<i32>() {
        p
    } else {
        let table = proc_table()
            .lock()
            .map_err(|e| format!("proc table poisoned: {}", e))?;
        table
            .get(&target)
            .map(|(pid, _, _)| *pid as i32)
            .ok_or_else(|| format!("unknown handle: {}", target))?
    };
    eprintln!("    -> kill: -{}", pid);
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", pid))
        .output();
    Ok(Value::Text(format!("killed -{}", pid)))
}

/// wait("name", timeout=60) — block until handle exits (or timeout). Returns exit info.
fn exec_wait(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let name = resolve_vars(&extract_string_arg("name", body), topic, results);
    let timeout: u64 = extract_string_arg("timeout", body).parse().unwrap_or(60);
    if name.is_empty() {
        return Err("wait requires name=\"handle\"".into());
    }
    let pid = {
        let table = proc_table()
            .lock()
            .map_err(|e| format!("proc table poisoned: {}", e))?;
        table
            .get(&name)
            .map(|(pid, _, _)| *pid)
            .ok_or_else(|| format!("unknown handle: {}", name))?
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout.max(1));
    loop {
        let alive = proc_alive(pid);
        if !alive {
            return Ok(Value::Text(format!("{} exited", name)));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "wait timeout: {} still running after {}s",
                name, timeout
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

// ── v0.7 Resource management ops: filesystem ──

/// exists("path") → "true"/"false"
fn exec_fs_exists(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    Ok(Value::Text(if Path::new(&path).exists() {
        "true".into()
    } else {
        "false".into()
    }))
}

/// stat("path") → "size_bytes mtime_unix" or error if missing
fn exec_fs_stat(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    let meta = fs::metadata(&path).map_err(|e| format!("stat failed: {}", e))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let kind = if meta.is_dir() {
        "dir"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };
    Ok(Value::Text(format!("{} {} {}", kind, meta.len(), mtime)))
}

/// ls("dir") → lines of entries (name only)
fn exec_fs_ls(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    let entries = fs::read_dir(&path).map_err(|e| format!("ls failed: {}", e))?;
    let mut names: Vec<String> = Vec::new();
    for e in entries.flatten() {
        names.push(e.file_name().to_string_lossy().to_string());
    }
    names.sort();
    Ok(Value::Text(names.join("\n")))
}

/// rm("path") — recursive; refuses / and $HOME roots.
fn exec_fs_rm(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    let p = Path::new(&path);
    let danger = p == Path::new("/") || p == dirs_home();
    if danger {
        return Err(format!("rm refused: {} is a protected root", path));
    }
    if !p.exists() {
        return Ok(Value::Text("noop: not found".into()));
    }
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| format!("rm failed: {}", e))?;
    } else {
        fs::remove_file(p).map_err(|e| format!("rm failed: {}", e))?;
    }
    Ok(Value::Text(format!("removed {}", path)))
}

fn dirs_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| PathBuf::from(home_dir()))
}

/// cp("src", "dst") — file or directory tree (pure Rust, no shell `cp`).
fn exec_fs_cp(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let src = expand_fs_path(&extract_string_arg("from", body));
    let dst = expand_fs_path(&extract_string_arg("to", body));
    if src.is_empty() || dst.is_empty() {
        return Err("cp requires from=\"...\" to=\"...\"".into());
    }
    let s = Path::new(&src);
    if !s.exists() {
        return Err(format!("cp: source not found: {}", src));
    }
    if s.is_dir() {
        copy_dir_recursive(s, Path::new(&dst))?;
    } else {
        if let Some(parent) = Path::new(&dst).parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::copy(&src, &dst).map_err(|e| format!("cp failed: {}", e))?;
    }
    Ok(Value::Text(format!("copied {} -> {}", src, dst)))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("cp mkdir failed: {}", e))?;
    for entry in fs::read_dir(src).map_err(|e| format!("cp read_dir failed: {}", e))? {
        let entry = entry.map_err(|e| format!("cp entry failed: {}", e))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("cp file_type failed: {}", e))?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to).map_err(|e| format!("cp failed: {}", e))?;
        }
    }
    Ok(())
}

/// mkdir("path") — create all parents.
fn exec_fs_mkdir(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    fs::create_dir_all(&path).map_err(|e| format!("mkdir failed: {}", e))?;
    Ok(Value::Text(path))
}

/// disk("dir") → "avail_gb total_gb" via df -BG.
fn exec_disk(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_fs_path(&extract_first_string(body));
    let out = Command::new("df")
        .arg("-BG")
        .arg(&path)
        .output()
        .map_err(|e| format!("df failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "df failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // last line: fs size used avail use% mount
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last().unwrap_or("");
    let fields: Vec<&str> = last.split_whitespace().collect();
    if fields.len() >= 4 {
        return Ok(Value::Text(format!("{} {}", fields[3], fields[1])));
    }
    Err("disk: unexpected df output".into())
}

fn exec_read(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let path = extract_string_arg("from", body);
    let path = if path.is_empty() {
        extract_first_string(body)
    } else {
        path
    };

    if path.starts_with("cache:") {
        return Err("cache miss".into());
    }

    // 与 exec_write 对齐：路径过 {topic}/{hash(topic)}/@ref 解析
    let resolved = resolve_vars(&path, topic, results);
    let expanded = expand_fs_path(&resolved);
    if !Path::new(&expanded).exists() {
        return Err(format!("file not found: {}", expanded));
    }
    let content = fs::read_to_string(&expanded).map_err(|e| format!("read failed: {}", e))?;
    Ok(Value::Text(content))
}

fn exec_run(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    deny_if_shell_restricted("run")?;
    let cmd_raw = extract_first_string(body);
    let cmd = resolve_vars(&cmd_raw, topic, results);
    // v0.7 resource management: optional timeout=seconds arg (default 300s, 0 = no limit)
    let timeout_secs: u64 = extract_string_arg("timeout", body).parse().unwrap_or(300);
    // v0.7 resource management: env vars via env="K=V" args (repeatable)
    let envs = extract_all_string_args("env", body);
    eprintln!("    -> run: {} (timeout={}s)", cmd, timeout_secs);

    let bash = resolve_bash()?;
    let mut command = Command::new(&bash);
    // v0.16 管线级 cwd（第三刀）：Pipeline(..., cwd="...") 生效于此。
    // 值内可写 $VAR/$(...)/~——resolve 阶段已被 bash 规范化为绝对路径。
    if let Some(dir) = PipelineCtx::cwd() {
        command.current_dir(&dir);
    }
    command.arg("-c").arg(&cmd);
    apply_ductile_child_env(&mut command);
    for e in &envs {
        if let Some(eq) = e.find('=') {
            let (k, v) = (&e[..eq], &e[eq + 1..]);
            if k == "PATH" || k == "HOME" || k == "USER" {
                // never clobber identity vars
                continue;
            }
            command.env(k, v);
        }
    }

    // Spawn + deadline: kill the whole process group on timeout so orphaned
    // children (e.g. `foo &`) don't outlive the pipeline.
    use std::process::Stdio;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "run failed to start bash ({}) — on Windows install Git for Windows (see docs/WINDOWS.md): {}",
                bash.display(),
                e
            )
        })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1));
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = s;
                break;
            }
            Ok(None) => {
                if timeout_secs > 0 && std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Best-effort group kill: negative pid of the child.
                    // (child.id() is the group leader under setsid-less bash -c)
                    let pid = child.id() as i32;
                    if pid > 0 {
                        libc_kill_group(pid);
                    }
                    return Err(format!("run timed out after {}s: {}", timeout_secs, cmd));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("run failed: {}", e)),
        }
    }

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    use std::io::Read;
    if let Some(p) = stdout_pipe.as_mut() {
        let _ = p.read_to_end(&mut stdout_buf);
    }
    if let Some(p) = stderr_pipe.as_mut() {
        let _ = p.read_to_end(&mut stderr_buf);
    }

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr_buf);
        return Err(format!(
            "run failed (exit {:?}): {}",
            status.code(),
            &stderr_text.chars().take(300).collect::<String>()
        ));
    }

    let stdout_text = String::from_utf8_lossy(&stdout_buf).to_string();

    // Parse ##DSL_RESULT block
    if let Some(kvs) = parse_dsl_result_block(&stdout_text) {
        if !kvs.is_empty() {
            eprintln!("    -> result: {} fields", kvs.len());
            return Ok(Value::Text(encode_structured_result(&kvs, &stdout_text)));
        }
    }

    // Raw stdout (truncated)
    let trimmed = if stdout_text.len() > 5000 {
        format!("{}...[truncated]", crate::trunc_chars(&stdout_text, 5000))
    } else {
        stdout_text
    };
    Ok(Value::Text(trimmed))
}

fn find_bridge(script: &str) -> String {
    // Search order: current dir, ~/.local/share/ductile/bridge/, project-local
    let home = home_dir();
    let candidates = [
        format!("./{}", script),
        format!("./bridge/{}", script),
        format!("{}/.local/share/ductile/bridge/{}", home, script),
        format!("/usr/local/share/ductile/bridge/{}", script),
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return c.clone();
        }
    }
    // Fallback: return the home path (will fail with a clear error)
    format!("{}/.local/share/ductile/bridge/{}", home, script)
}

// ── v0.12 脚本契约执行线 ──

/// script(name, k=v...) 执行：
/// 1. 契约卡从 scripts 表加载（fail-closed：未注册的脚本名硬错并列出已注册清单）
/// 2. DSL 里的 k=v 覆盖契约 params 默认值；`@proc.field` 引用上游结果
/// 3. 契约头里遗漏的必填参数硬错；未声明的幻觉参数也硬错（契约即接口）
/// 4. 解释器由 lang 决定；timeout/retries 走契约
/// 5. 输出复用 ##DSL_RESULT 协议（脚本自己 echo 结构化字段）
pub fn exec_script_call(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let (name, args) = crate::script::parse_script_body(body)?;

    let card = crate::db::script_get(&name).ok_or_else(|| {
        let known: Vec<String> = crate::db::script_list()
            .iter()
            .map(|c| c.name.clone())
            .collect();
        if known.is_empty() {
            format!(
                "script '{}' not attached — register first: ductile script attach <file>",
                name
            )
        } else {
            format!(
                "script '{}' not attached. Attached scripts: {}",
                name,
                known.join(", ")
            )
        }
    })?;

    // 契约 params 解析：`name(spec)` 按括号分组（spec 内的逗号不切分），
    // 再从 spec 里提取 required / default=X
    let mut declared: BTreeMap<String, String> = BTreeMap::new(); // name -> default(""=无)
    let mut required: Vec<String> = Vec::new();
    {
        let mut parts: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut depth = 0i32;
        for c in card.params.chars() {
            match c {
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    parts.push(cur.clone());
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        if !cur.trim().is_empty() {
            parts.push(cur);
        }
        for part_raw in parts {
            let part = part_raw.trim();
            if part.is_empty() {
                continue;
            }
            let pname = part.split('(').next().unwrap_or(part).trim().to_string();
            if pname.is_empty() {
                continue;
            }
            let spec = part
                .find('(')
                .map(|i| part[i + 1..].trim_end_matches(')').trim().to_string())
                .unwrap_or_default();
            if spec.split(',').any(|s| s.trim() == "required") {
                required.push(pname.clone());
            }
            if let Some(di) = spec.find("default=") {
                let dv = spec[di + "default=".len()..].trim().to_string();
                declared.insert(pname.clone(), dv);
            } else {
                declared.entry(pname.clone()).or_default();
            }
        }
    }

    // 调用参数合并：`@proc` 整值引用 / `@proc.field` 与 {topic} 走 resolve_vars。
    // v0.14.2 fix：纯 `@x.y` 此前只查整值键 → miss → 空串 → 误报 "param is empty"。
    // 现在 miss 时 fallthrough 到 resolve_vars（.field 解析在那里），与 run() 语法对齐。
    let mut call_args: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &args {
        let resolved = if let Some(stripped) = v.strip_prefix('@') {
            match results.get(stripped) {
                Some(Value::Text(t)) => t.clone(),
                Some(Value::File(f)) => f.clone(),
                _ => resolve_vars(v, topic, results),
            }
        } else {
            resolve_vars(v, topic, results)
        };
        call_args.insert(k.clone(), resolved);
    }

    // 必填参数校验（fail-closed）
    for req in &required {
        match call_args.get(req) {
            None => {
                return Err(format!(
                    "script '{}': missing required param '{}' (contract params: {})",
                    name, req, card.params
                ))
            }
            Some(v) if v.is_empty() => {
                return Err(format!(
                    "script '{}': required param '{}' is empty",
                    name, req
                ))
            }
            _ => {}
        }
    }
    // 未在契约声明的参数 → 硬错（契约即接口，防止调用方幻觉传参）
    for k in call_args.keys() {
        if !declared.contains_key(k) {
            return Err(format!(
                "script '{}': param '{}' not in contract (declared: {})",
                name, k, card.params
            ));
        }
    }

    // env 传参：DUCTILE_ARG_<NAME>、DUCTILE_TOPIC；脚本侧 getenv 取参
    let interp = crate::script::lang_interpreter(&card.lang)?;
    let mut command = Command::new(interp);
    command.arg(&card.path);
    for (k, v) in &call_args {
        command.env(format!("DUCTILE_ARG_{}", k.to_uppercase()), v);
    }
    // 未给的参数也传空串（脚本侧可区分"给了空值"与"没这个参数"）
    for (k, v) in &declared {
        if !call_args.contains_key(k) {
            command.env(format!("DUCTILE_ARG_{}", k.to_uppercase()), v.clone());
        }
    }
    command.env("DUCTILE_TOPIC", topic);
    let timeout_secs = card.timeout_secs.max(1);

    eprintln!(
        "    -> script: {} (lang={}, timeout={}s, retries={})",
        name, card.lang, timeout_secs, card.retries
    );

    use std::process::Stdio;
    let mut last_err = String::new();
    for attempt in 0..=card.retries {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("script '{}' launch failed: {}", name, e))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let status;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = s;
                    break;
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "script '{}' timed out after {}s",
                            name, timeout_secs
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(format!("script '{}' wait failed: {}", name, e)),
            }
        }

        // 先收全 stdout/stderr 再判断成功与否
        let mut stdout_buf = Vec::new();
        if let Some(mut io) = child.stdout.take() {
            use std::io::Read;
            let _ = io.read_to_end(&mut stdout_buf);
        }
        let mut stderr_buf = Vec::new();
        if let Some(mut io) = child.stderr.take() {
            use std::io::Read;
            let _ = io.read_to_end(&mut stderr_buf);
        }
        let stdout_text = String::from_utf8_lossy(&stdout_buf).to_string();

        if status.success() {
            // ##DSL_RESULT 协议复用（脚本 echo 结构化字段）
            if let Some(kvs) = parse_dsl_result_block(&stdout_text) {
                if !kvs.is_empty() {
                    eprintln!("    -> script result: {} fields", kvs.len());
                    return Ok(Value::Text(encode_structured_result(&kvs, &stdout_text)));
                }
            }
            let trimmed = if stdout_text.len() > 5000 {
                format!("{}...[truncated]", crate::trunc_chars(&stdout_text, 5000))
            } else {
                stdout_text
            };
            return Ok(Value::Text(trimmed));
        }

        last_err = format!(
            "script '{}' failed (exit {:?}): {}",
            name,
            status.code(),
            String::from_utf8_lossy(&stderr_buf)
                .chars()
                .take(300)
                .collect::<String>()
        );
        eprintln!("    -> retry {}/{} after 2s", attempt + 1, card.retries);
        if attempt < card.retries {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_impl() -> Impl {
        Impl {
            name: String::new(),
            tags: std::collections::BTreeSet::new(),
            cost: Cost::default(),
            enabled: true,
            when: None,
            refs: vec![],
            body_text: String::new(),
            stub: false,
            retry: 0,
            ensure: vec![],
            description: String::new(),
        }
    }

    // ── exec_read with missing file ──
    #[test]
    fn exec_read_missing_file() {
        let impl_ = Impl {
            body_text: r#"read(from="/nonexistent/ductile_test_file.txt")"#.into(),
            ..default_impl()
        };
        let result = exec_read(&impl_, "test", &impl_.body_text, &BTreeMap::new());
        assert!(result.is_err());
    }

    #[test]
    fn exec_read_cache_miss() {
        let impl_ = Impl {
            body_text: r#"read(from="cache:abc")"#.into(),
            ..default_impl()
        };
        let result = exec_read(&impl_, "test", &impl_.body_text, &BTreeMap::new());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cache miss");
    }

    // ── exec_write + exec_read roundtrip ──
    // ── v0.14.2 issue #2：纯 @proc.field 引用走 resolve_vars fallthrough ──

    #[test]
    fn script_arg_pure_at_field_resolves() {
        // `a="@src.ratio"` 此前只查整值键 "src.ratio" → miss → 空 → 误报 param is empty
        let mut results = BTreeMap::new();
        results.insert(
            "src".into(),
            Value::Text(
                "§§FIELDS§§ratio=0.5240§§RAW§§##DSL_RESULT\nratio=0.5240\n##DSL_END".into(),
            ),
        );
        let body = r#"script(fake_probe, a="@src.ratio")"#;
        let (_name, args) = crate::script::parse_script_body(body).unwrap();
        let topic = "t";
        let resolved = if let Some(stripped) = args[0].1.strip_prefix('@') {
            match results.get(stripped) {
                Some(Value::Text(t)) => t.clone(),
                Some(Value::File(f)) => f.clone(),
                _ => resolve_vars(&args[0].1, topic, &results),
            }
        } else {
            resolve_vars(&args[0].1, topic, &results)
        };
        assert_eq!(resolved, "0.5240");
    }

    #[test]
    fn exec_write_then_read() {
        let path = std::env::temp_dir().join("ductile_test_roundtrip.txt");
        let path_s = path.to_string_lossy().replace('\\', "/");
        let _ = std::fs::remove_file(&path);

        // Write
        let write_impl = Impl {
            body_text: format!(r#"write(to="{}", content="hello ductile")"#, path_s),
            ..default_impl()
        };
        let results = BTreeMap::new();
        let w = exec_write(&write_impl, "test", &write_impl.body_text, &results);
        assert!(w.is_ok());

        // Read back
        let read_impl = Impl {
            body_text: format!(r#"read(from="{}")"#, path_s),
            ..default_impl()
        };
        let r = exec_read(&read_impl, "test", &read_impl.body_text, &BTreeMap::new());
        assert!(r.is_ok());
        match r.unwrap() {
            Value::Text(t) => assert_eq!(t, "hello ductile"),
            _ => panic!("expected Text"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_bash_finds_something() {
        // CI / 本机：有 bash 才过；纯 Windows 无 Git 时跳过会让门禁假绿，故要求可解析
        let p = resolve_bash().expect("bash required for ductile run/spawn — see docs/WINDOWS.md");
        assert!(p.is_file(), "{}", p.display());
    }

    #[test]
    fn apply_ductile_child_env_sets_path_or_bin() {
        let mut c = Command::new("true");
        apply_ductile_child_env(&mut c);
        // 至少尝试设置；无 current_exe 时也可能空操作
        let _ = c;
        if let Ok(exe) = std::env::current_exe() {
            assert!(exe.exists() || exe.parent().is_some());
        }
    }

    // ── exec_merge ──
    #[test]
    fn exec_merge_basic() {
        let mut results = BTreeMap::new();
        results.insert("a".into(), Value::Text("data_a".into()));
        results.insert("b".into(), Value::Text("data_b".into()));
        let impl_ = Impl {
            refs: vec!["a".into(), "b".into()],
            body_text: "merge(@a, @b)".into(),
            ..default_impl()
        };
        let result = exec_merge(&impl_, &impl_.body_text, &results);
        assert!(result.is_ok());
        let text = result.unwrap().as_text().to_string();
        assert!(text.contains("data_a"));
        assert!(text.contains("data_b"));
    }

    #[test]
    fn exec_merge_dedup() {
        let mut results = BTreeMap::new();
        results.insert("a".into(), Value::Text("same".into()));
        results.insert("b".into(), Value::Text("same".into()));
        let impl_ = Impl {
            refs: vec!["a".into(), "b".into()],
            body_text: "merge(@a, @b, dedup)".into(),
            ..default_impl()
        };
        let result = exec_merge(&impl_, &impl_.body_text, &results);
        let text = result.unwrap().as_text().to_string();
        // With dedup, should not contain duplicate
        assert_eq!(text.matches("same").count(), 1);
    }

    // ── fs 七件套 ──

    /// Normalize path for embedding in DSL `"..."` literals (forward slashes
    /// avoid Windows `\` escape ambiguity in extract_string_arg / bodies).
    fn dsl_path(p: &std::path::Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    #[test]
    fn fs_exists_true_false() {
        let here = std::env::temp_dir();
        let ok = exec_fs_exists(
            &default_impl(),
            &format!(r#"exists("{}")"#, dsl_path(&here)),
        )
        .unwrap();
        assert_eq!(ok.as_text(), "true");
        let no = exec_fs_exists(&default_impl(), r#"exists("/nonexistent-xyz-ductile")"#).unwrap();
        assert_eq!(no.as_text(), "false");
    }

    #[test]
    fn fs_stat_and_ls() {
        let dir = std::env::temp_dir().join(format!("ductile-steps-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("b.txt"), "world").unwrap();
        let st = exec_fs_stat(
            &default_impl(),
            &format!(r#"stat("{}")"#, dsl_path(&dir.join("a.txt"))),
        )
        .unwrap();
        assert!(st.as_text().starts_with("file "), "{}", st.as_text());
        let ls = exec_fs_ls(&default_impl(), &format!(r#"ls("{}")"#, dsl_path(&dir))).unwrap();
        assert_eq!(ls.as_text(), "a.txt\nb.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_mkdir_cp_rm_roundtrip() {
        let base = std::env::temp_dir().join(format!("ductile-cp-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::create_dir_all(&src);
        std::fs::write(src.join("f.txt"), "data").unwrap();
        // mkdir dst（from/to 形态）
        let m = exec_fs_mkdir(&default_impl(), &format!(r#"mkdir("{}")"#, dsl_path(&dst))).unwrap();
        assert_eq!(m.as_text().replace('\\', "/"), dsl_path(&dst));
        // cp 目录树
        let c = exec_fs_cp(
            &default_impl(),
            &format!(
                r#"cp(from="{}", to="{}")"#,
                dsl_path(&src),
                dsl_path(&dst.join("src"))
            ),
        )
        .unwrap();
        assert!(c.as_text().contains("copied"));
        assert!(dst.join("src").join("f.txt").exists());
        // rm 树
        let r = exec_fs_rm(&default_impl(), &format!(r#"rm("{}")"#, dsl_path(&base))).unwrap();
        assert!(r.as_text().contains("removed"));
        assert!(!base.exists());
    }

    #[test]
    fn fs_rm_refuses_protected_roots() {
        assert!(exec_fs_rm(&default_impl(), r#"rm("/")"#).is_err());
        let home = home_dir().replace('\\', "/");
        assert!(
            exec_fs_rm(&default_impl(), &format!(r#"rm("{}")"#, home)).is_err(),
            "home={}",
            home
        );
    }

    #[test]
    fn fs_rm_missing_is_noop_ok() {
        let r = exec_fs_rm(&default_impl(), r#"rm("/nonexistent-ductile-xyz")"#).unwrap();
        assert!(r.as_text().contains("noop"));
    }

    #[test]
    fn fs_cp_missing_source_err() {
        assert!(exec_fs_cp(&default_impl(), r#"cp(from="/no/such/src", to="/tmp/x")"#).is_err());
    }

    // ── run：DSL_RESULT 集成 + 超时 + env（依赖 bash；Windows 无 bash 时跳过）──

    #[test]
    #[cfg(unix)]
    fn run_echo_returns_stdout() {
        let r = exec_run(
            &default_impl(),
            "t",
            r#"run("echo hello-ductile")"#,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(r.as_text().contains("hello-ductile"));
    }

    #[test]
    #[cfg(unix)]
    fn run_dsl_result_block_structured() {
        let cmd = "echo x; printf '##DSL_RESULT\nscore=85\n##DSL_END\n'";
        let body = format!("run(\"{}\")", cmd);
        let r = exec_run(&default_impl(), "t", &body, &BTreeMap::new()).unwrap();
        let t = r.as_text();
        assert!(t.starts_with("§§FIELDS§§"), "{}", &t[..40.min(t.len())]);
        assert_eq!(
            crate::dslresult::extract_field("score", &t),
            Some("85".into())
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_failing_command_err() {
        let r = exec_run(&default_impl(), "t", r#"run("exit 3")"#, &BTreeMap::new());
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("exit"));
    }

    #[test]
    #[cfg(unix)]
    fn run_timeout_kills() {
        let r = exec_run(
            &default_impl(),
            "t",
            r#"run("sleep 60", timeout=1)"#,
            &BTreeMap::new(),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("timed out"));
    }

    #[test]
    #[cfg(unix)]
    fn run_env_vars_visible() {
        let body = r#"run("echo $DUCTILE_TEST_V", env="DUCTILE_TEST_V=xyz123")"#;
        let r = exec_run(&default_impl(), "t", &body, &BTreeMap::new()).unwrap();
        assert!(r.as_text().contains("xyz123"));
    }

    #[test]
    #[cfg(unix)]
    fn run_topic_resolution() {
        let body = r#"run("echo {topic}")"#;
        let r = exec_run(&default_impl(), "my-topic", body, &BTreeMap::new()).unwrap();
        assert!(r.as_text().contains("my-topic"));
    }

    // ── 进程管理 ──

    #[test]
    #[cfg(unix)]
    fn spawn_wait_kill_cycle() {
        let mut results = BTreeMap::new();
        // spawn 一个 sleep 30 的后台进程
        let sp = exec_spawn(
            &default_impl(),
            "t",
            r#"spawn(name="t30", cmd="setsid sleep 30")"#,
            &results,
        )
        .unwrap();
        let pid_str = sp.as_text().to_string();
        assert!(pid_str.parse::<u32>().is_ok(), "pid: {}", pid_str);
        // procs 表里有它且 alive
        let pl = exec_procs(&default_impl(), "t", r#"procs(name="t30")"#, &results).unwrap();
        assert!(pl.as_text().contains("t30"));
        assert!(pl.as_text().contains("alive"));
        // kill
        let k = exec_kill(&default_impl(), "t", r#"kill(name="t30")"#, &results).unwrap();
        assert!(k.as_text().contains("killed"));
        // kill 后短暂等待进程消失，wait 应立即返回 exited
        std::thread::sleep(std::time::Duration::from_millis(300));
        let w = exec_wait(
            &default_impl(),
            "t",
            r#"wait(name="t30", timeout=5)"#,
            &results,
        )
        .unwrap();
        assert!(w.as_text().contains("exited"));
    }

    #[test]
    fn spawn_missing_name_err() {
        assert!(exec_spawn(
            &default_impl(),
            "t",
            r#"spawn("sleep 1")"#,
            &BTreeMap::new()
        )
        .is_err());
    }

    #[test]
    fn kill_unknown_handle_err() {
        assert!(exec_kill(
            &default_impl(),
            "t",
            r#"kill(name="no-such-handle")"#,
            &BTreeMap::new()
        )
        .is_err());
    }

    #[test]
    fn wait_unknown_handle_err() {
        assert!(exec_wait(
            &default_impl(),
            "t",
            r#"wait(name="ghost")"#,
            &BTreeMap::new()
        )
        .is_err());
    }

    #[test]
    fn restrict_shell_gate_logic() {
        // 纯逻辑测试（参数注入，不碰全局 env——并行测试安全）
        assert!(shell_allowed_with(None, Some("1".into())) == false);
        assert!(shell_allowed_with(Some("1".into()), Some("1".into())) == true);
        assert!(shell_allowed_with(None, None) == true);
        assert!(shell_allowed_with(None, Some("0".into())) == true);
    }

    #[test]
    fn unsafe_shell_overrides_restrict() {
        assert!(shell_allowed_with(Some("1".into()), Some("1".into())));
    }

    #[test]
    fn llm_requires_prompt_or_input() {
        let err = exec_llm(
            &default_impl(),
            "t",
            r#"llm(model="gpt-4o-mini")"#,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("prompt") || err.contains("input"), "{}", err);
    }

    #[test]
    fn llm_prompt_alias_reaches_bridge() {
        // Isolate from user config.toml / env so missing key is deterministic.
        let dir = std::env::temp_dir().join(format!("ductile-llm-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[llm]\napi_key = \"\"\n").unwrap();
        let prev_cfg = std::env::var("DUCTILE_CONFIG").ok();
        let prev_key = std::env::var("OPENAI_API_KEY").ok();
        std::env::set_var("DUCTILE_CONFIG", &cfg_path);
        std::env::remove_var("OPENAI_API_KEY");
        let err = exec_llm(
            &default_impl(),
            "t",
            r#"llm(prompt="ping", model="gpt-4o-mini")"#,
            &BTreeMap::new(),
        )
        .unwrap_err();
        if let Some(k) = prev_key {
            std::env::set_var("OPENAI_API_KEY", k);
        } else {
            std::env::remove_var("OPENAI_API_KEY");
        }
        if let Some(c) = prev_cfg {
            std::env::set_var("DUCTILE_CONFIG", c);
        } else {
            std::env::remove_var("DUCTILE_CONFIG");
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains("api_key")
                || err.contains("OPENAI_API_KEY")
                || err.contains("llm failed")
                || err.contains("bridge"),
            "{}",
            err
        );
    }

    // ── write 边界 ──

    #[test]
    fn write_resolves_topic_and_creates_parents() {
        let base = std::env::temp_dir().join(format!("ductile-w-{}", std::process::id()));
        let path = format!("{}/deep/{}/out.txt", base.display(), "{hash(topic)}");
        let body = format!(r#"write(to="{}", content="data-{{topic}}")"#, path);
        let r = exec_write(&default_impl(), "topicX", &body, &BTreeMap::new()).unwrap();
        let f = r.as_text().to_string();
        assert!(std::path::Path::new(&f).exists(), "file missing: {}", f);
        let content = std::fs::read_to_string(&f).unwrap();
        assert_eq!(content, "data-topicX");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── v0.17 auto-prompt ──

    fn mk_proc(name: &str, desc: &str) -> crate::ast::Proc {
        crate::ast::Proc {
            name: name.into(),
            description: desc.into(),
            plan: Vec::new(),
            checks: Vec::new(),
            contract: crate::ast::Contract::default(),
            deliver: false,
            needs: vec![],
            foreach: None,
            deliver_refs: Vec::new(),
            foreach_var: String::new(),
            pick_by: String::new(),
        }
    }

    #[test]
    #[test]
    fn preview_window_detail_consumer() {
        // 细节消费型 schema（missing/tickets）→ 2000 窗口
        assert_eq!(preview_window("score,missing,notes"), 2000);
        assert_eq!(preview_window("tickets,deps,total_est"), 2000);
        // 常规 schema → 200
        assert_eq!(preview_window("modules,storage,stack,risks"), 200);
        assert_eq!(preview_window(""), 200);
    }

    fn auto_prompt_synthesizes_identity_topic_upstream() {
        let mut pl = crate::ast::Pipeline::default();
        pl.name = "proj_chain".into();
        pl.description = "项目开发全链".into();
        let mut up = mk_proc("req", "S0 需求提炼");
        // 上游有契约 → 继承约束段
        up.contract.outputs = vec!["must_have".into(), "constraints".into()];
        pl.procs.push(up);
        let mut me = mk_proc("arch", "S1 架构设计");
        // 本节点 impl 引用上游 + .when 门禁
        let mut im = default_impl();
        im.refs = vec!["req".into()];
        im.when = Some("@req.must_have".into());
        me.plan.push(im);
        pl.procs.push(me);

        let mut results = BTreeMap::new();
        results.insert(
            "req".to_string(),
            Value::Text("§§FIELDS§§must_have=[\"a\", \"b\"]§§constraints=低预算§§RAW§§xx".into()),
        );

        let _g = NodeCtx::set(&pl, &pl.procs[1]);
        let out = NodeCtx::synthesize_auto_prompt(
            "搞个素材归档",
            &results,
            "modules,storage,stack,risks",
            "可检索 /tmp 目录",
            "",
        )
        .expect("synthesized");
        assert!(out.contains("管线「proj_chain」"), "身份段: {}", out);
        assert!(out.contains("节点「arch」：S1 架构设计"));
        assert!(out.contains("# 主题输入\n搞个素材归档"));
        assert!(out.contains("## 来自「req」"), "上游段: {}", out);
        assert!(
            out.contains("must_have = [\"a\", \"b\"]"),
            "字段预览: {}",
            out
        );
        assert!(!out.contains("RAW"), "RAW 全文不得进 prompt");
        assert!(out.contains("引擎门禁"), "when 约束: {}", out);
        assert!(
            out.contains("上游「req」契约要求输出字段"),
            "契约约束: {}",
            out
        );
        assert!(out.contains("# 可用动作与检索方式"), "guide 段: {}", out);
        assert!(out.contains("\"modules\": \"...\""), "schema 示例: {}", out);
    }

    #[test]
    fn auto_prompt_needs_identity() {
        // 无 desc 无 system → None（不合成空洞 prompt）
        let mut pl = crate::ast::Pipeline::default();
        pl.procs.push(mk_proc("x", ""));
        let _g = NodeCtx::set(&pl, &pl.procs[0]);
        assert!(NodeCtx::synthesize_auto_prompt("t", &BTreeMap::new(), "", "", "").is_none());
    }

    #[test]
    fn auto_prompt_outside_exec_proc_is_none() {
        // 不在 exec_proc 内（无 NODE_CTX）→ None
        let t = std::thread::spawn(|| {
            NodeCtx::synthesize_auto_prompt("t", &BTreeMap::new(), "a,b", "g", "s")
        });
        assert!(t.join().unwrap().is_none());
    }

    #[test]
    fn preview_value_truncates_long_text() {
        let long = "x".repeat(1000);
        let v = Value::Text(long);
        let pv = preview_value(&v, 10);
        assert!(
            pv.chars().count() <= 41,
            "10*4+1 截断: {}",
            pv.chars().count()
        );
        assert!(pv.ends_with('…'));
    }
}
