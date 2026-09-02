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
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("SerialTool").dpi_awareness(DpiAwareness::System))
            .expect("failed to embed Windows application manifest");

        let mut res = winresource::WindowsResource::new();
        let version = std::env::var("CARGO_PKG_VERSION").expect("missing package version");
        let mut parts = version
            .split('.')
            .map(|part| part.parse::<u64>().expect("invalid package version"));
        let version_number = (parts.next().unwrap_or(0) << 48)
            | (parts.next().unwrap_or(0) << 32)
            | (parts.next().unwrap_or(0) << 16);
        res.set_icon("assets/app.ico");
        res.set("ProductName", "Serial Tool");
        res.set("FileDescription", "串口/网络/SSH 调试工具");
        res.set("FileVersion", &version);
        res.set("ProductVersion", &version);
        res.set("CompanyName", "XCharge");
        res.set_version_info(winresource::VersionInfo::FILEVERSION, version_number);
        res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, version_number);
        res.compile().expect("failed to embed Windows resources");
    }
}
