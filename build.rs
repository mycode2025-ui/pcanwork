fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    // 仅 Windows 目标：嵌入声明 System DPI 感知的应用清单。
    // 跨不同缩放比屏幕时由系统位图缩放，不再逐屏 resize，从根本上消除拖动抖动
    // （代价：在与主屏缩放比不同的副屏上界面会略微发虚）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("PcanWork").dpi_awareness(DpiAwareness::System))
            .expect("嵌入 Windows 应用清单失败");

        // 嵌入 exe 文件图标（资源管理器、任务栏固定项显示）
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        if let Err(e) = res.compile() {
            // 缺少资源编译器(rc.exe)时不致命，仅跳过 exe 图标
            println!("cargo:warning=嵌入 exe 图标失败(跳过): {e}");
        }
    }
    // 把 Python 客户端库 pcanwork.py（+ 示例脚本）拷到 exe 同目录，
    // 脚本运行器经 PCANWORK_CLIENT_DIR(=current_exe().parent()) 加入 PYTHONPATH 供 `import pcanwork`。
    // OUT_DIR = target/<profile>/build/<pkg-hash>/out → 上溯三级即 exe 所在的 target/<profile>。
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let exe_dir = std::path::Path::new(&out_dir).join("..").join("..").join("..");
        let _ = std::fs::copy("pcanwork.py", exe_dir.join("pcanwork.py"));
        let tdir = exe_dir.join("templates");
        let _ = std::fs::create_dir_all(&tdir);
        // 拷贝 templates/ 下所有 .py 示例
        if let Ok(entries) = std::fs::read_dir("templates") {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("py")
                    && let Some(name) = p.file_name()
                {
                    let _ = std::fs::copy(&p, tdir.join(name));
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=pcanwork.py");
    println!("cargo:rerun-if-changed=templates");
    println!("cargo:rerun-if-changed=app.ico");
}
