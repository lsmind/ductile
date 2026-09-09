# Windows

`run` / `spawn` 通过 **bash** 执行。请安装 [Git for Windows](https://git-scm.com/download/win)（自带 bash）。

## 构建与 PATH

```powershell
cargo build --release
# 推荐：把 target\release 加入用户 PATH，或始终用完整路径
.\target\release\ductile.exe run .\selftest.pipeline
```

引擎会为子进程自动前置：

- 当前 `ductile` 可执行文件所在目录（并设置 `DUCTILE_BIN`）
- `CARGO_TARGET_DIR` / 仓库 `target/{release,debug}`（若存在）

狗粮管线不必再手写长 `PATH=`。

## 路径

- DSL 里的 `~/...`：读 `HOME` 或 `USERPROFILE`
- DSL 里的 `/tmp/...`：映射到系统临时目录（原生 `write`/`read`/`cp`）
- bash 脚本内的 `/tmp`：仍由 Git Bash 解析

`disk-watch` 等会扫 `$HOME`：自测已设 `DUCTILE_HOME` 到仓库 `examples`，避免扫整个用户盘。

## 自测

在仓库根（Git Bash 或已能解析 bash 的环境）：

```bash
cargo build --release
./target/release/ductile.exe run ./selftest.pipeline
```

缺 bash 时错误会指向本文档。
