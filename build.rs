fn main() {
    // Compile only the frequently edited shell plus the two windows that share
    // live row models with it. Other dialogs live in pcanwork-ui-features.
    slint_build::compile("ui/MainWindows.slint").unwrap();

    // Windows target only: embed an application manifest that declares System DPI awareness.
    // When moving the window across monitors with different scaling factors, Windows performs
    // bitmap scaling instead of per-monitor resizing, which eliminates drag jitter at the root.
    // Trade-off: the UI may look slightly blurry on secondary monitors whose scaling factor
    // differs from the primary monitor.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(new_manifest("PcanWork").dpi_awareness(DpiAwareness::System))
            .expect("Failed to embed Windows application manifest");

        // Embed the executable icon for File Explorer and pinned taskbar shortcuts.
        let mut res = winresource::WindowsResource::new();
        res.set_icon("app.ico");
        if let Err(e) = res.compile() {
            // If the resource compiler rc.exe is missing, treat it as non-fatal and skip the icon.
            println!("cargo:warning=Failed to embed exe icon (skipped): {e}");
        }
    }

    println!("cargo:rerun-if-changed=app.ico");

    for path in [
        "ui/MainWindows.slint",
        "ui/AppWindow.slint",
        "ui/ChartWindow.slint",
        "ui/TxWindow.slint",
        "ui/common.slint",
        "ui/design-system.slint",
        "ui/logo.svg",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Ok(entries) = std::fs::read_dir("ui/icons") {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
}
