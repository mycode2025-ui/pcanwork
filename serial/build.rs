fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=assets/app.png");
    println!("cargo:rerun-if-changed=assets/app.ico");

    // Use the default (light fluent) widget style, same as the PcanWork main app,
    // so std-widget colors/text match the light academic-blue theme.
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");

    // 嵌入 exe 图标（资源管理器/桌面快捷方式显示用）
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("ProductName", "Serial Tool");
        res.set("FileDescription", "串口/网络/SSH 调试工具");
        res.compile().expect("failed to embed Windows resources");
    }
}
