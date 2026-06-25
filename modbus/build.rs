fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // 仅 Windows: 嵌入 exe 图标(资源管理器/任务栏固定项显示), 与 pcanwork 同款品牌。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        if let Err(e) = res.compile() {
            // 缺 rc.exe 时不致命, 仅跳过 exe 图标
            println!("cargo:warning=嵌入 exe 图标失败(跳过): {e}");
        }
    }
    println!("cargo:rerun-if-changed=app.ico");

    // 把自签名测试证书拷到 exe 同目录的 certs/，使从 PcanWork 拉起(cwd=exe 目录)时
    // TLS 功能仍能按 "certs/xxx" 相对路径找到。OUT_DIR 上溯三级即 target/<profile>。
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let exe_dir = std::path::Path::new(&out_dir).join("..").join("..").join("..");
        let dst = exe_dir.join("certs");
        let _ = std::fs::create_dir_all(&dst);
        if let Ok(entries) = std::fs::read_dir("certs") {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() {
                    if let Some(name) = p.file_name() {
                        let _ = std::fs::copy(&p, dst.join(name));
                    }
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=certs");
    println!("cargo:rerun-if-changed=ui/app.slint");
}
