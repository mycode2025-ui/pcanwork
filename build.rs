fn main() {
    // Rust build entry: compile app.slint, not main.slint.
    // main.slint is only for slint-viewer / live preview.
    slint_build::compile("ui/app.slint").unwrap();

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

    // Copy the Python client library pcanwork.py and example scripts to the executable directory.
    // The script runner adds PCANWORK_CLIENT_DIR, which points to current_exe().parent(),
    // to PYTHONPATH so that `import pcanwork` works.
    // OUT_DIR = target/<profile>/build/<pkg-hash>/out.
    // Going up three levels reaches the executable directory: target/<profile>.
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        let exe_dir = std::path::Path::new(&out_dir).join("..").join("..").join("..");
        let _ = std::fs::copy("pcanwork.py", exe_dir.join("pcanwork.py"));

        let tdir = exe_dir.join("templates");
        let _ = std::fs::create_dir_all(&tdir);

        // Copy all .py example scripts under templates/.
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

    // Rust compile entry.
    println!("cargo:rerun-if-changed=ui/app.slint");

    // Also track all split UI files.
    if let Ok(entries) = std::fs::read_dir("ui") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) == Some("slint") {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}