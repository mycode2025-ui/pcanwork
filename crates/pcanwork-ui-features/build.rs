use std::path::{Path, PathBuf};

fn track_tree(path: &Path) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                track_tree(&entry.path());
            }
        }
    } else {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    let ui = PathBuf::from("../../ui");
    slint_build::compile(ui.join("FeatureWindows.slint")).unwrap();

    for name in [
        "FeatureWindows.slint",
        "SignalSelectWindow.slint",
        "OtaWindow.slint",
        "UdsWindow.slint",
        "XcpWindow.slint",
        "ChannelConfigWindow.slint",
        "PlaybackWindow.slint",
        "ConsoleHelpWindow.slint",
        "ConvertWindow.slint",
        "CacheConfigWindow.slint",
        "TriggerWindow.slint",
        "SimPanelWindow.slint",
        "SimPropWindow.slint",
        "ScriptRunnerWindow.slint",
        "DbcDiagnosticsWindow.slint",
        "common.slint",
        "design-system.slint",
        "logo.svg",
    ] {
        track_tree(&ui.join(name));
    }
    track_tree(&ui.join("icons"));
}
