//! locks.rs — v0.21 资源治理：契约互斥锁。
//!
//! 归层：资源治理是 L0 物理层职责，不是脚本自觉。concurrency 契约
//! （safe/exclusive/serial）此前只是展示字段——executor 无任何强制点，
//! 两条管线可并发跑 exclusive 脚本（GPU 类）把机器 OOM 打死
//! （2026-09-26 planetes 事故实锤）。
//!
//! 语义：
//! - safe      = 无锁（可并行、可 CSE）
//! - serial    = 同一脚本（按绝对路径分键）跨进程互斥
//! - exclusive = 全机互斥（GPU/大内存类单例资源）
//!
//! 实现：Rust 1.89+ `std::fs::File::try_lock()`（Unix 下即 flock(2)
//! LOCK_EX，非阻塞）。争用即 Err fail-closed——GPU 类脚本宁可立刻报错
//! 也不排队六小时（排队/重试语义归调度层 .when/.retry，引擎锁只管互斥）。
//! 锁文件在 XDG_RUNTIME_DIR（缺省 /tmp）：锁是临时态，机器重启蒸发恰是
//! 特性；flock 是 fd 级锁，持锁进程退出（哪怕被 OOM 硬杀）内核自动释放，
//! 不存在陈旧锁死锁。

use crate::core::script_card::Concurrency;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

/// 持锁 guard：Drop（或进程退出）即释放。无需 Send——exec_script_call
/// 在本函数栈上同步等子进程结束，锁的持有范围 = 脚本整个生命周期。
pub struct LockGuard {
    /// 持有 fd 即持锁（Drop 自动解锁）。字段不被读是刻意的——锁的活性
    /// 就是它的全部职责。
    #[allow(dead_code)]
    file: File,
}

impl LockGuard {
    /// 显式释放（等价 Drop）。
    pub fn release(self) {}
}

/// 派生锁文件路径。serial 按脚本绝对路径哈希分键（不同项目同名脚本
/// 互不误撞）；exclusive 固定单例键。
fn lock_file_path(key: &str) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // 键可能含路径分隔符——FNV-1a 哈希成安全文件名，防路径注入
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    base.join(format!("ductile-{}-{:016x}.lock", sanitize(key), h))
}

/// 键名净化：只留字母数字和 _ -，限 32 字符。
fn sanitize(k: &str) -> String {
    let mut s = String::with_capacity(k.len().min(32));
    for c in k.chars() {
        if s.len() >= 32 {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            s.push(c);
        }
    }
    if s.is_empty() {
        s.push('x');
    }
    _ = k;
    s
}

/// 获取契约锁。safe → Ok(None) 无锁。争用 → Err（fail-closed）。
pub fn acquire_concurrency_lock(
    script_path: &str,
    conc: &Concurrency,
) -> Result<Option<LockGuard>, String> {
    match conc {
        Concurrency::Safe => Ok(None),
        Concurrency::Serial | Concurrency::Exclusive => {
            let key = match conc {
                Concurrency::Exclusive => "global-exclusive".to_string(),
                _ => script_path.to_string(),
            };
            let path = lock_file_path(&key);
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|e| format!("lock open failed {}: {}", path.display(), e))?;
            // flock LOCK_EX|LOCK_NB：try_lock 非阻塞，争用即 Err
            if let Err(e) = file.try_lock() {
                return Err(format!(
                    "concurrency conflict: lock '{}' (key={}) — {} held by another \
                     pipeline; fail-closed. GPU/exclusive 类脚本不排队：重试时机 \
                     由调度层决定（.when 探针 / .retry），引擎锁只管互斥 [{}]",
                    path.display(),
                    key,
                    conc.as_str(),
                    e
                ));
            }
            Ok(Some(LockGuard { file }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_never_locks() {
        assert!(acquire_concurrency_lock("/x/y.py", &Concurrency::Safe)
            .unwrap()
            .is_none());
    }

    #[test]
    fn serial_lock_file_derivation_stable() {
        let a = lock_file_path("/home/u/proj/scripts/gen.py");
        let b = lock_file_path("/home/u/proj/scripts/gen.py");
        assert_eq!(a, b);
        let c = lock_file_path("/home/u/other/scripts/gen.py");
        assert_ne!(a, c, "不同路径必须分键");
    }

    #[test]
    fn sanitize_strips_unsafe() {
        assert_eq!(sanitize("/a/b/c.py"), "abcpy");
        assert!(!sanitize("///").is_empty());
    }

    #[test]
    fn second_lock_same_key_conflicts() {
        // flock 同进程语义：同进程不同 fd 互相争用（flock 是 open-file-description
        // 级锁，两个独立 open 的 fd 即两条持锁者）——第二次 try_lock 必须 Err。
        let g = acquire_concurrency_lock("/x/y.py", &Concurrency::Serial).unwrap();
        assert!(g.is_some());
        let r = acquire_concurrency_lock("/x/y.py", &Concurrency::Serial);
        assert!(r.is_err(), "同键二锁必须冲突（flock open-file-description 级）");
    }

    #[test]
    fn drop_releases_lock() {
        // 同键先取后释放再取：第二次必须成功
        {
            let _g = acquire_concurrency_lock("/x/z.py", &Concurrency::Serial).unwrap();
        }
        let g2 = acquire_concurrency_lock("/x/z.py", &Concurrency::Serial);
        assert!(g2.is_ok(), "Drop 后锁必须可再取");
    }
}
