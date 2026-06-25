// Event-wiring for the wire_playback. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_playback(
    app: Rc<std::cell::RefCell<App>>,
    ui: &AppWindow,
    chart_window: &ChartWindow,
    signal_window: &SignalSelectWindow,
    tx_window: &TxWindow,
    channel_window: &ChannelConfigWindow,
    playback_window: &PlaybackWindow,
    convert_window: &ConvertWindow,
    cache_window: &CacheConfigWindow,
    trigger_window: &TriggerWindow,
    sim_panel_window: &SimPanelWindow,
    sim_prop_window: &SimPropWindow,
) {
    // ---------------- 数据回放窗口 ----------------
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_open_file(move || {
            let app = app.clone();
            let pw = pw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = pw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("回放文件 (CSV/ASC/BLF)", &["csv", "asc", "blf"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(files) = dlg.pick_files().await else { return };
                let paths: Vec<std::path::PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
                let mut a = app.borrow_mut();
                a.pb_files.clear(); // 「打开」=替换；追加用「添加」
                let mut total = 0usize;
                for path in &paths {
                    let p = path.to_string_lossy().to_string();
                    match convert::parse_playback_file(&p) {
                        Ok(frames) => {
                            total += frames.len();
                            let name = path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or(p);
                            a.pb_files.push((name, frames));
                        }
                        Err(e) => a.log(format!("载入回放文件失败 {}: {e}", path.display())),
                    }
                }
                let nf = a.pb_files.len();
                a.log(format!("已载入 {nf} 个回放文件（共 {total} 帧）"));
                pb_apply_files(&mut a, &w);
            });
        });
    }
    // 回放：追加文件（不清空已载入的）
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_add_file(move || {
            let app = app.clone();
            let pw = pw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = pw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("回放文件 (CSV/ASC/BLF)", &["csv", "asc", "blf"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(files) = dlg.pick_files().await else { return };
                let paths: Vec<std::path::PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
                let mut a = app.borrow_mut();
                for path in &paths {
                    let p = path.to_string_lossy().to_string();
                    match convert::parse_playback_file(&p) {
                        Ok(frames) => {
                            let name = path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or(p);
                            a.pb_files.push((name, frames));
                        }
                        Err(e) => a.log(format!("添加回放文件失败 {}: {e}", path.display())),
                    }
                }
                pb_apply_files(&mut a, &w);
            });
        });
    }
    // 回放：切换合并方式后重组
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_remerge(move || {
            let w = pw.unwrap();
            let mut a = app.borrow_mut();
            pb_apply_files(&mut a, &w);
        });
    }
    // 回放：清空已载入文件
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_clear_files(move || {
            let w = pw.unwrap();
            let mut a = app.borrow_mut();
            a.pb_files.clear();
            pb_apply_files(&mut a, &w);
        });
    }
    // 回放：移除单个文件
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_remove_file(move |i| {
            let w = pw.unwrap();
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i < a.pb_files.len() {
                a.pb_files.remove(i);
                pb_apply_files(&mut a, &w);
            }
        });
    }
    // 回放：上移文件（影响顺序拼接的先后）
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_move_file_up(move |i| {
            let w = pw.unwrap();
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i > 0 && i < a.pb_files.len() {
                a.pb_files.swap(i - 1, i);
                pb_apply_files(&mut a, &w);
            }
        });
    }
    // 回放：下移文件
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_move_file_down(move |i| {
            let w = pw.unwrap();
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i + 1 < a.pb_files.len() {
                a.pb_files.swap(i, i + 1);
                pb_apply_files(&mut a, &w);
            }
        });
    }
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_apply_settings(move || {
            let w = pw.unwrap();
            let a = app.borrow();
            pb_build_and_load(&a, &w);
        });
    }
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        playback_window.on_play(move || {
            let w = pw.unwrap();
            let a = app.borrow();
            let online = w.get_online();
            let speed = if w.get_speed_fast() {
                0.0
            } else {
                w.get_rate().to_string().trim().parse::<f64>().unwrap_or(1.0).max(0.01)
            };
            let _ = a.cmd.send(Cmd::PlaybackPlay {
                online,
                speed,
                loop_play: w.get_loop_play(),
            });
        });
    }
    {
        let app = app.clone();
        playback_window.on_seek(move |frac| {
            let _ = app.borrow().cmd.send(Cmd::PlaybackSeek(frac as f64));
        });
    }
    {
        let app = app.clone();
        playback_window.on_step(move || {
            let _ = app.borrow().cmd.send(Cmd::PlaybackStep);
        });
    }
    {
        let app = app.clone();
        playback_window.on_pause(move || {
            let _ = app.borrow().cmd.send(Cmd::PlaybackPause);
        });
    }
    {
        let app = app.clone();
        playback_window.on_cancel(move || {
            let _ = app.borrow().cmd.send(Cmd::PlaybackCancel);
        });
    }
}
