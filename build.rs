// build.rs — v0.23：把 git commit hash 烤进二进制。
// 用途：`ductile help <topic> src` 与 `--version` 显示构建版本；
// help src 给出的 `git show <hash>:<file>` 永远钉在构建时的代码状态。
// 无 git 或脏树（CI/cache 场景）退化为 "dev"。
use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=DUCTILE_BUILD_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
}
