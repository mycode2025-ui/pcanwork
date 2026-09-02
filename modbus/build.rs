fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // 仅 Windows: 嵌入 exe 图标(资源管理器/任务栏固定项显示), 与 pcanwork 同款品牌。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("ModbusTools").dpi_awareness(DpiAwareness::System))
            .expect("failed to embed Windows application manifest");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        if let Err(e) = res.compile() {
            // 缺 rc.exe 时不致命, 仅跳过 exe 图标
            println!("cargo:warning=嵌入 exe 图标失败(跳过): {e}");
        }
    }
    println!("cargo:rerun-if-changed=app.ico");

    // TLS identity files are runtime inputs and are never build or installer inputs.
    println!("cargo:rerun-if-changed=ui/app.slint");
}
