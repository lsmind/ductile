//! Ductile v0.24 run adapter — 行动③ D5 后半。
//!
//! 规范 §2：run<F>(argv, stdin, env, cwd) —— argv 直传，**不隐式 shell**；
//! stdout 保留原始文本（fd1，≤8MiB）；结构化字段走 fd3（DCTF3 帧）；
//! 超时杀整个进程组；退出码非零 → E302（完整帧 Success 仍 E302，补丁③）。
//!
//! 判决兑现（审稿 B"run() 无字段"）：
//!   - @x.status/@x.ok/@x.stdout/@x.fields.* 永远合法（Outcome 统一协议）
//!   - 子进程写 fd3 → 解帧 → 字段；无帧 → fields 空（Success 仍成立）
//!   - 帧损坏 → E304/E306；帧与 fd1 stdout 摘要不符 → E501

use crate::kernel::fd3::{self, FrameError, WireOutcome};
use crate::kernel::types::{ErrCode, Outcome, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 资源/超时规格（E406 骨架；cgroup 执行在 D6 nsjail 层接入）。
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    /// 墙钟超时；超时杀进程组 → E303。
    pub timeout: Option<Duration>,
    /// fd1 上限（默认 8MiB，超出 → E406 ResourceQuota）。
    pub stdout_cap: usize,
}

impl Default for RunSpec {
    fn default() -> Self {
        RunSpec {
            argv: Vec::new(),
            stdin: None,
            env: BTreeMap::new(),
            cwd: None,
            timeout: None,
            stdout_cap: fd3::STDOUT_MAX,
        }
    }
}

/// 执行结果（原始材料；由调用方组装 Outcome）。
pub struct RunRaw {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub frame: Result<Option<WireOutcome>, FrameError>,
    pub timed_out: bool,
    pub truncated: bool,
}

/// 僵尸收割器：scope 结束时 kill(-pid) + waitpid，防孤儿进程组。
struct ReapOnDrop {
    pid: i32,
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
            let mut status: libc::c_int = 0;
            let _ = libc::waitpid(self.pid, &mut status, 0);
        }
    }
}

#[cfg(unix)]
fn spawn_with_fd3(spec: &RunSpec) -> std::io::Result<(std::process::Child, i32)> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..])
        .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // stderr 不进结果（审计单独记录）
        .process_group(0); // 新进程组：超时可杀全组

    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }

    // fd3：裸 fd 管道；pre_exec 闭包按值捕获 i32（Copy）
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (rd_fd, wr_fd) = (fds[0], fds[1]);
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(wr_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn();
    // 父进程立即关写端（否则读端永不 EOF）；失败时两端全关
    unsafe { libc::close(wr_fd); }
    match child {
        Ok(c) => Ok((c, rd_fd)),
        Err(e) => {
            unsafe { libc::close(rd_fd); }
            Err(e)
        }
    }
}

/// 执行 run：双流并发排空（fd1 + fd3），超时杀组。
pub fn run_raw(spec: &RunSpec) -> Result<RunRaw, Outcome> {
    if spec.argv.is_empty() {
        return Err(Outcome::failed(ErrCode::E312, "argv empty"));
    }

    let started = Instant::now();
    let (mut child, frame_pipe) = match spawn_with_fd3(spec) {
        Ok(x) => x,
        Err(e) => {
            return Err(Outcome::failed(
                ErrCode::E301,
                format!("spawn {} failed: {}", spec.argv[0], e.kind()),
            ))
        }
    };

    let pid = child.id() as i32;
    let _reaper = ReapOnDrop { pid };

    // stdin 喂入后立即关写端
    if let Some(input) = &spec.stdin {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(input.as_bytes());
        }
    }

    // 双流并发排空：fd3 单独线程；fd1 主线程（带超时轮询）
    // rd_fd 所有权移入线程（from_raw_fd 接管 close 职责）
    let rd_fd = frame_pipe;
    let mut frame_reader = unsafe { std::fs::File::from_raw_fd(rd_fd) };
    let frame_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            match frame_reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > fd3::P_MAX + fd3::HEADER_LEN + 64 {
                        break; // 物理上限防滥用
                    }
                }
                Err(_) => break,
            }
        }
        buf
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stdout: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16384];
    let mut timed_out = false;

    loop {
        if let Some(t) = spec.timeout {
            if started.elapsed() > t {
                timed_out = true;
                break;
            }
        }
        // 非阻塞轮询：设置 O_NONBLOCK
        set_nonblock(stdout_pipe.as_raw_fd());
        match stdout_pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.len() + n > spec.stdout_cap {
                    stdout.extend_from_slice(&chunk[..spec.stdout_cap - stdout.len().min(spec.stdout_cap)]);
                    truncated = true;
                } else {
                    stdout.extend_from_slice(&chunk[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }

    // 超时：杀进程组（ReapOnDrop 兜底）
    if timed_out {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = frame_handle.join();
        return Ok(RunRaw {
            exit: None,
            stdout,
            frame: Ok(None),
            timed_out: true,
            truncated,
        });
    }

    let exit = child.wait().ok().and_then(|s| s.code());

    let frame_bytes = frame_handle.join().unwrap_or_default();
    let frame = if frame_bytes.is_empty() {
        Ok(None)
    } else {
        fd3::decode_frame(&frame_bytes)
            .and_then(|dec| fd3::parse_payload(&dec.payload))
            .map(Some)
    };

    Ok(RunRaw { exit, stdout, frame, timed_out: false, truncated })
}

fn set_nonblock(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// RunRaw → Outcome（补丁③判定顺序：
/// Spawn/Permission → Timeout/Cancel → Protocol/Codec → Exit；
/// 完整帧 Success 但非零退出仍 E302）。
pub fn raw_to_outcome(raw: RunRaw, fields_schema: Option<&crate::kernel::types::Schema>) -> Outcome {
    if raw.timed_out {
        return Outcome::failed(ErrCode::E303, "wall clock timeout; process group killed");
    }
    if raw.truncated {
        return Outcome::failed(ErrCode::E406, format!("stdout > cap"));
    }
    let wire = match raw.frame {
        Err(e) => {
            return Outcome::failed(e.code(), format!("fd3 frame: {:?}", e));
        }
        Ok(None) => None,
        Ok(Some(w)) => Some(w),
    };
    // fd3 语义优先；无帧时退化为退出码判定
    match wire {
        Some(WireOutcome::Success { fields, stdout_sha256, stdout_len }) => {
            // fd1 摘要核对（补丁③：H 校验 fd1 原始 stdout）
            let actual_len = raw.stdout.len() as u64;
            let actual_sha = crate::kernel::hash::sha256_hex(&raw.stdout);
            if stdout_len != actual_len || stdout_sha256 != actual_sha {
                return Outcome::failed(
                    ErrCode::E501,
                    "fd3 stdout digest mismatch vs fd1",
                );
            }
            // 非零退出仍 E302（补丁③）
            if raw.exit != Some(0) {
                return Outcome::failed(
                    ErrCode::E302,
                    format!("exit {:?} with valid frame", raw.exit),
                );
            }
            // schema 校验
            if let Some(schema) = fields_schema {
                let v = Value::Map(fields.clone());
                if let Err(mismatch) = schema.check(&v) {
                    let mut o = Outcome::success(fields, String::new());
                    o = o.contract_fail(false);
                    let detail = format!("{} vs {}", mismatch.expected, mismatch.got);
                    let _ = detail;
                    return Outcome::failed(ErrCode::E501, format!("fields schema: expected {}", mismatch.expected));
                }
            }
            let stdout_str = String::from_utf8_lossy(&raw.stdout).to_string();
            Outcome::Success {
                fields,
                stdout: stdout_str,
                meta: BTreeMap::new(),
            }
        }
        Some(WireOutcome::Failed { code, name, detail }) => {
            // 子进程自报错误码：必须认识，否则 E304
            let known = code_from_str(&code);
            match known {
                Some(c) => Outcome::failed(c, format!("{} ({})", detail, name)),
                None => Outcome::failed(ErrCode::E304, format!("unknown error code in frame: {}", code)),
            }
        }
        Some(WireOutcome::Skipped { reason, detail }) => {
            let sr = match reason.as_str() {
                "Ineligible" => crate::kernel::types::SkipReason::Ineligible(detail),
                "UpstreamSkipped" => crate::kernel::types::SkipReason::UpstreamSkipped,
                "NotSelected" => crate::kernel::types::SkipReason::NotSelected,
                "EmptyInput" => crate::kernel::types::SkipReason::EmptyInput,
                _ => {
                    return Outcome::failed(ErrCode::E304, format!("unknown skip reason: {}", reason))
                }
            };
            Outcome::skipped(sr)
        }
        None => {
            // 无帧：退出码即真相
            match raw.exit {
                Some(0) => {
                    let stdout_str = String::from_utf8_lossy(&raw.stdout).to_string();
                    Outcome::Success {
                        fields: BTreeMap::new(),
                        stdout: stdout_str,
                        meta: BTreeMap::new(),
                    }
                }
                Some(c) => Outcome::failed(ErrCode::E302, format!("exit {}", c)),
                None => Outcome::failed(ErrCode::E409, "no exit code (worker crash?)"),
            }
        }
    }
}

fn code_from_str(s: &str) -> Option<ErrCode> {
    use ErrCode::*;
    Some(match s {
        "E101" => E101, "E102" => E102, "E103" => E103, "E104" => E104,
        "E201" => E201, "E202" => E202, "E203" => E203, "E204" => E204,
        "E205" => E205, "E206" => E206, "E207" => E207,
        "E301" => E301, "E302" => E302, "E303" => E303, "E304" => E304,
        "E305" => E305, "E306" => E306, "E307" => E307, "E308" => E308,
        "E309" => E309, "E310" => E310, "E311" => E311, "E312" => E312,
        "E313" => E313, "E314" => E314, "E315" => E315, "E316" => E316,
        "E317" => E317, "E318" => E318,
        "E401" => E401, "E402" => E402, "E403" => E403, "E404" => E404,
        "E405" => E405, "E406" => E406, "E407" => E407, "E408" => E408,
        "E409" => E409,
        "E501" => E501, "E502" => E502, "E503" => E503, "E504" => E504,
        _ => return None,
    })
}

/// 便捷入口：跑 + 组装。
pub fn run(spec: &RunSpec, schema: Option<&crate::kernel::types::Schema>) -> Outcome {
    match run_raw(spec) {
        Ok(raw) => raw_to_outcome(raw, schema),
        Err(o) => o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python(code: &str) -> Vec<String> {
        vec!["python3".into(), "-c".into(), code.into()]
    }

    #[test]
    fn plain_exit0_no_frame() {
        let spec = RunSpec { argv: python("print('hello')"), ..Default::default() };
        let out = run(&spec, None);
        assert!(out.ok(), "got {:?}", out.status());
        assert_eq!(out.stdout().trim(), "hello");
        assert!(out.fields().is_empty());
    }

    #[test]
    fn nonzero_exit_is_e302() {
        let spec = RunSpec {
            argv: python("import sys; sys.exit(3)"),
            ..Default::default()
        };
        let out = run(&spec, None);
        assert_eq!(out.error().unwrap().code, ErrCode::E302);
    }

    #[test]
    fn frame_success_path() {
        // 子进程写 DCTF3 帧到 fd3 + stdout 到 fd1
        let code = r#"
import os, sys, hashlib, struct, json
out = b'hello'
payload = json.dumps({
    "fields": {"n": 1},
    "status": "Success",
    "stdout": {"bytes": len(out), "sha256": hashlib.sha256(out).hexdigest()},
}, separators=(",", ":"), sort_keys=True).encode()
schema = json.dumps({"n": "int"}, separators=(",", ":"), sort_keys=True).encode()
hdr = b"DCTF3\n" + struct.pack(">HH", 1, 0) + struct.pack(">I", len(schema)) + struct.pack(">Q", len(payload))
body = hdr + schema + payload
digest = hashlib.sha256(body).digest()
os.write(3, body + digest)
sys.stdout.buffer.write(out)
"#;
        let spec = RunSpec { argv: python(code), ..Default::default() };
        let out = run(&spec, None);
        assert!(out.ok(), "status={:?} err={:?}", out.status(), out.error().map(|e| &e.detail));
        assert_eq!(out.field("n"), Ok(&Value::Int(1)));
        assert_eq!(out.stdout(), "hello");
    }

    #[test]
    fn frame_digest_mismatch_vs_fd1_is_e501() {
        // 帧声称 stdout="XXXX" 实际输出 "hello" → E501
        let code = r#"
import os, sys, hashlib, struct, json
out = b'hello'
fake = b'XXXX'
payload = json.dumps({
    "fields": {},
    "status": "Success",
    "stdout": {"bytes": len(fake), "sha256": hashlib.sha256(fake).hexdigest()},
}, separators=(",", ":"), sort_keys=True).encode()
schema = b"{}"
hdr = b"DCTF3\n" + struct.pack(">HH", 1, 0) + struct.pack(">I", len(schema)) + struct.pack(">Q", len(payload))
body = hdr + schema + payload
os.write(3, body + hashlib.sha256(body).digest())
sys.stdout.buffer.write(out)
"#;
        let spec = RunSpec { argv: python(code), ..Default::default() };
        let out = run(&spec, None);
        assert_eq!(out.error().unwrap().code, ErrCode::E501);
    }

    #[test]
    fn frame_success_but_nonzero_exit_is_e302() {
        let code = r#"
import os, sys, hashlib, struct, json
out = b''
payload = json.dumps({
    "fields": {},
    "status": "Success",
    "stdout": {"bytes": 0, "sha256": hashlib.sha256(out).hexdigest()},
}, separators=(",", ":"), sort_keys=True).encode()
schema = b"{}"
hdr = b"DCTF3\n" + struct.pack(">HH", 1, 0) + struct.pack(">I", len(schema)) + struct.pack(">Q", len(payload))
body = hdr + schema + payload
os.write(3, body + hashlib.sha256(body).digest())
sys.exit(7)
"#;
        let spec = RunSpec { argv: python(code), ..Default::default() };
        let out = run(&spec, None);
        assert_eq!(out.error().unwrap().code, ErrCode::E302);
    }

    #[test]
    fn corrupted_frame_is_e306() {
        let code = r#"
import os, sys
os.write(3, b"DCTF3\nGARBAGEGARBAGE")
"#;
        let spec = RunSpec { argv: python(code), ..Default::default() };
        let out = run(&spec, None);
        let code = out.error().unwrap().code;
        assert!(matches!(code, ErrCode::E304 | ErrCode::E306), "got {}", code);
    }

    #[test]
    fn timeout_kills_group_e303() {
        let spec = RunSpec {
            argv: python("import time; time.sleep(30)"),
            timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let t0 = Instant::now();
        let out = run(&spec, None);
        assert!(t0.elapsed() < Duration::from_secs(5), "timeout must kill fast");
        assert_eq!(out.error().unwrap().code, ErrCode::E303);
    }

    #[test]
    fn argv_direct_no_shell_interpolation() {
        // argv 含 shell 元字符也不经 shell——原样传给 python 的 sys.argv
        let code = r#"
import sys
print(sys.argv[1])
"#;
        let spec = RunSpec {
            argv: python(code),
            stdin: None,
            env: BTreeMap::new(),
            cwd: None,
            timeout: None,
            stdout_cap: fd3::STDOUT_MAX,
        };
        let spec = RunSpec { argv: spec.argv, ..Default::default() };
        let mut s = spec;
        s.argv.push("$(echo pwned); rm -rf /".into());
        let out = run(&s, None);
        assert!(out.ok());
        assert_eq!(out.stdout().trim(), "$(echo pwned); rm -rf /");
    }

    #[test]
    fn stdin_piped() {
        let spec = RunSpec {
            argv: python("import sys; sys.stdout.write(sys.stdin.read().upper())"),
            stdin: Some("ductile".into()),
            ..Default::default()
        };
        let out = run(&spec, None);
        assert!(out.ok());
        assert_eq!(out.stdout(), "DUCTILE");
    }

    #[test]
    fn stdout_cap_is_e406() {
        let code = format!("print('x' * {})", 64 * 1024);
        let spec = RunSpec {
            argv: python(&code),
            stdout_cap: 1024,
            ..Default::default()
        };
        let out = run(&spec, None);
        assert_eq!(out.error().unwrap().code, ErrCode::E406);
    }

    #[test]
    fn spawn_failure_is_e301() {
        let spec = RunSpec {
            argv: vec!["/nonexistent/binary/xyz".into()],
            ..Default::default()
        };
        let out = run(&spec, None);
        assert_eq!(out.error().unwrap().code, ErrCode::E301);
    }

    #[test]
    fn empty_argv_is_e312() {
        let out = run(&RunSpec::default(), None);
        assert_eq!(out.error().unwrap().code, ErrCode::E312);
    }
}
