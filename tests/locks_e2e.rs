//! locks_e2e.rs — 跨进程互斥 E2E（v0.21 资源治理的真验收）。
//! 场景 = 2026-09-26 planetes 事故形态：两条管线并发调用同一
//! exclusive 脚本，第二条必须被 fail-closed 拦下。

use std::process::Command;
use std::time::Duration;

#[test]
fn exclusive_script_cross_process_mutex() {
    let tmp = std::env::temp_dir().join(format!("dt-lock-e2e-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("probe")).unwrap();
    let ductile = format!("{}/target/release/ductile", env!("CARGO_MANIFEST_DIR"));
    assert!(std::path::Path::new(&ductile).exists(), "release 二进制不存在——先 cargo build --release");

    // 探针：exclusive + 睡眠（模拟 GPU 脚本持卡时长）
    let probe = tmp.join("probe/excl_sleep.py");
    std::fs::write(
        &probe,
        concat!(
            "# ductile: v1\n# name: excl_sleep\n# desc: mutex probe\n",
            "# lang: python\n# params: dur(str, default=6)\n# output: done\n",
            "# pure: false\n# idempotent: true\n# concurrency: exclusive\n",
            "# effects: process\n# timeout: 60\n",
            "import os, time\n",
            "time.sleep(float(os.environ.get(\"DUCTILE_ARG_DUR\", \"6\")))\n",
            "print(\"##DSL_RESULT\\ndone=1\\n##DSL_END\")\n"
        ),
    )
    .unwrap();
    let out = Command::new(&ductile)
        .args(["script", "attach"])
        .arg(&probe)
        .env("DUCTILE_DATA", &tmp)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "attach 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 两条管线文件
    let t_hold = tmp.join("t_hold.pipeline");
    let t_grab = tmp.join("t_grab.pipeline");
    std::fs::write(
        &t_hold,
        "Pipeline(\"hold_t\", \"hold 8s\")\n  .proc(\"hold\", script(excl_sleep, dur=\"8\"))\n  .proc(\"deliver\").deliver(@hold)\n",
    )
    .unwrap();
    std::fs::write(
        &t_grab,
        "Pipeline(\"grab_t\", \"grab while holding\")\n  .proc(\"grab\", script(excl_sleep, dur=\"2\"))\n  .proc(\"deliver\").deliver(@grab)\n",
    )
    .unwrap();

    // hold 后台先跑，2s 后 grab 并发争用
    let mut hold = Command::new(&ductile)
        .arg("run")
        .arg(&t_hold)
        .env("DUCTILE_DATA", &tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let grab = Command::new(&ductile)
        .arg("run")
        .arg(&t_grab)
        .env("DUCTILE_DATA", &tmp)
        .output()
        .unwrap();
    let grab_text = format!(
        "{}{}",
        String::from_utf8_lossy(&grab.stdout),
        String::from_utf8_lossy(&grab.stderr)
    );
    let hold_status = hold.wait().unwrap();

    // grab 必须被互斥拦下（fail-closed），hold 必须正常完成
    assert!(
        grab_text.contains("concurrency conflict"),
        "grab 未被互斥拦下！输出：\n{}",
        grab_text
    );
    assert!(!grab_text.contains("done=1"), "grab 不应跑完：\n{}", grab_text);
    assert!(hold_status.success(), "hold 应正常完成，exit={:?}", hold_status.code());
    println!("EXCLUSIVE-MUTEX-PASS");
    let _ = std::fs::remove_dir_all(&tmp);
}
