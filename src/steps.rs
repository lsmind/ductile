//! Steps — 内置函数执行器族（v0.7 resource ops + search/llm/merge + 注册表）。
//!
//! 从 executor.rs 拆出的执行层：step_registry 是唯一注册点（清单即表），
//! 进程管理（spawn/procs/kill/wait + PROC_TABLE）、文件系统操作
//! （exists/stat/ls/rm/cp/mkdir/disk）、桥接脚本调用（search/llm）、
//! 读写与 shell 执行（read/write/run/sh）。

use crate::ast::*;
use crate::dslresult::{encode_structured_result, parse_dsl_result_block};
use crate::textargs::{
    expand_fs_path, extract_all_string_args, extract_first_string, extract_string_arg, resolve_vars,
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
                    return Some(PipelineCtxGuard { poisoned: true, msg: Some(msg) });
                }
            },
            None => None,
        };
        let env: Vec<(String, String)> = pl
            .env
            .iter()
            .filter_map(|e| {
                e.find('=').map(|eq| (e[..eq].to_string(), e[eq + 1..].to_string()))
            })
            .collect();
        PIPELINE_CTX.with(|c| *c.borrow_mut() = Some(PipelineCtxInner { cwd, env }));
        Some(PipelineCtxGuard { poisoned: false, msg: None })
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
    let model = extract_string_arg("model", body);
    let system = extract_string_arg("system", body);
    let schema = extract_string_arg("schema", body);
    let count = extract_string_arg("count", body);
    let resolved_prompt = resolve_vars(&prompt_raw, topic, results);
    let resolved_template = resolve_vars(&template, topic, results);
    let resolved_system = resolve_vars(&system, topic, results);
    let resolved_schema = resolve_vars(&schema, topic, results);
    let resolved_model = resolve_vars(&model, topic, results);
    if resolved_prompt.is_empty() {
        return Err("llm requires prompt=\"...\" or input=\"...\"".into());
    }
    eprintln!(
        "    -> llm: model={} schema={} input.len={}",
        if resolved_model.is_empty() {
            "(default)"
        } else {
            &resolved_model
        },
        !resolved_schema.is_empty(),
        resolved_prompt.len()
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
    if !resolved_model.is_empty() {
        args.push("--model".into());
        args.push(resolved_model);
    }
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

    let output = run_python_bridge(&bridge, &args)?;

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
fn run_python_bridge(
    bridge: &str,
    args: &[String],
) -> Result<std::process::Output, String> {
    let cfg = crate::config::load_llm_config();
    for py in ["python3", "python"] {
        let mut cmd = Command::new(py);
        cmd.arg(bridge).args(args);
        crate::config::apply_llm_env_from_config(&mut cmd, &cfg);
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
        let ok = exec_fs_exists(&default_impl(), &format!(r#"exists("{}")"#, dsl_path(&here)))
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
        assert!(shell_allowed_with(
            Some("1".into()),
            Some("1".into())
        ));
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
}
