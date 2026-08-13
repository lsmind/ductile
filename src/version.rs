//! Version — pipeline 版本快照。
//!
//! `ductile version save <file> "desc"` — 保存当前 pipeline 文件快照
//! `ductile version log <file>`          — 查看历史
//! `ductile version diff <file> v1 v2`   — 对比两个版本

use std::fs;
use std::path::PathBuf;

fn version_dir(pipeline_name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(&home)
        .join(".local/share/ductile/versions")
        .join(pipeline_name);
    let _ = fs::create_dir_all(&dir);
    dir
}

fn meta_file(pipeline_name: &str) -> PathBuf {
    version_dir(pipeline_name).join("meta.log")
}

fn snapshot_file(pipeline_name: &str, version: usize) -> PathBuf {
    version_dir(pipeline_name).join(format!("v{}.snapshot", version))
}

fn read_current_version(pipeline_name: &str) -> usize {
    let meta = meta_file(pipeline_name);
    if !meta.exists() {
        return 0;
    }
    let content = fs::read_to_string(&meta).unwrap_or_default();
    content
        .lines()
        .filter(|l| l.starts_with('v'))
        .count()
}

pub fn save_version(pipeline_name: &str, content: &str, change: &str) -> usize {
    let current = read_current_version(pipeline_name);
    let new_v = current + 1;

    // Save snapshot
    fs::write(snapshot_file(pipeline_name, new_v), content).ok();

    // Append meta
    let entry = format!("v{}|{}\n", new_v, change);
    let meta = meta_file(pipeline_name);
    if meta.exists() {
        fs::OpenOptions::new()
            .append(true)
            .open(&meta)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry.as_bytes())
            })
            .ok();
    } else {
        fs::write(&meta, &entry).ok();
    }

    new_v
}

pub fn read_log(pipeline_name: &str) -> Option<String> {
    let meta = meta_file(pipeline_name);
    if meta.exists() {
        fs::read_to_string(&meta).ok()
    } else {
        None
    }
}

pub fn diff_versions(pipeline_name: &str, v1: usize, v2: usize) -> Option<String> {
    let s1 = read_snapshot(pipeline_name, v1)?;
    let s2 = read_snapshot(pipeline_name, v2)?;

    if s1 == s2 {
        Some("identical".to_string())
    } else {
        // Simple line-by-line diff
        let lines1: Vec<&str> = s1.lines().collect();
        let lines2: Vec<&str> = s2.lines().collect();
        let mut diff = format!("v{} → v{}\n", v1, v2);
        let max_len = lines1.len().max(lines2.len());
        for i in 0..max_len {
            let l1 = lines1.get(i).copied().unwrap_or("");
            let l2 = lines2.get(i).copied().unwrap_or("");
            if l1 != l2 {
                if !l1.is_empty() && l2.is_empty() {
                    diff.push_str(&format!("  - {}\n", l1.trim()));
                } else if l1.is_empty() && !l2.is_empty() {
                    diff.push_str(&format!("  + {}\n", l2.trim()));
                } else {
                    diff.push_str(&format!("  - {}\n", l1.trim()));
                    diff.push_str(&format!("  + {}\n", l2.trim()));
                }
            }
        }
        Some(diff)
    }
}

fn read_snapshot(pipeline_name: &str, v: usize) -> Option<String> {
    let path = snapshot_file(pipeline_name, v);
    if path.exists() {
        fs::read_to_string(&path).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_dir_creates_parent() {
        let dir = version_dir("__test_version");
        assert!(dir.exists());
        // Cleanup
        let _ = fs::remove_dir_all(version_dir("__test_version").parent().unwrap().join("__test_version"));
    }
}
