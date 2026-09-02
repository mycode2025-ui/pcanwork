// Event-wiring for the wire_main. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
fn begin_shutdown(app: &Rc<std::cell::RefCell<App>>, ui: &AppWindow) {
    if let Err(error) = settings::save(&gather_settings(&app.borrow(), ui)) {
        eprintln!("Failed to save settings during shutdown: {error}");
    }

    let (cmd, recorder_join, mut signal_log) = {
        let mut state = app.borrow_mut();
        if state.shutdown_requested {
            return;
        }
        state.shutdown_requested = true;
        state.recording = false;
        state
            .ipc_subs
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut child) = state.py_child.take()
            && let Err(error) = child.kill()
        {
            state.log(format!("Failed to stop script process: {error}"));
        }
        (
            state.cmd.clone(),
            state.recorder.begin_shutdown(),
            state.sig_log.take(),
        )
    };

    std::thread::spawn(move || {
        if let Some(join) = recorder_join
            && join.join().is_err()
        {
            eprintln!("Recorder thread panicked during shutdown");
        }
        if let Some(writer) = signal_log.as_mut()
            && let Err(error) = std::io::Write::flush(writer)
        {
            eprintln!("Failed to flush signal log during shutdown: {error}");
        }
        if cmd
            .send_critical(Cmd::Shutdown, std::time::Duration::from_secs(1))
            .is_err()
        {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
    });
}

fn queue_project_load(path: std::path::PathBuf, worker: WorkerSender<WorkerEvent>) {
    std::thread::spawn(move || {
        let result = (|| -> Result<_, String> {
            let text =
                std::fs::read_to_string(&path).map_err(|error| format!("读取工程失败: {error}"))?;
            let project: Project =
                serde_json::from_str(&text).map_err(|error| format!("解析工程失败: {error}"))?;
            let dbc_paths: Vec<String> = if project.settings.dbc_paths.is_empty() {
                project.settings.dbc_path.clone().into_iter().collect()
            } else {
                project.settings.dbc_paths.clone()
            };
            let replace_dbcs = !dbc_paths.is_empty();
            let mut loaded = Vec::new();
            let mut errors = Vec::new();
            for dbc_path in dbc_paths {
                match DbcDb::load(&dbc_path) {
                    Ok(database) => loaded.push((dbc_path, database)),
                    Err(error) => errors.push(format!("加载 DBC 失败 {dbc_path}: {error}")),
                }
            }
            Ok((project, loaded, errors, replace_dbcs))
        })();
        let _ = worker.send(WorkerEvent::ProjectLoaded {
            path,
            result: Box::new(result),
        });
    });
}

fn refresh_dbc_diagnostics(
    app: &App,
    window: &DbcDiagnosticsWindow,
    model: &VecModel<DbcDiagnosticRow>,
) {
    let mut rows = Vec::new();
    let mut errors = 0;
    let mut warnings = 0;
    let mut infos = 0;
    let active_filter = window.get_severity_filter();
    for database in &app.dbcs {
        for diagnostic in database.diagnostics() {
            let severity = match diagnostic.severity {
                dbc::DbcDiagnosticSeverity::Error => {
                    errors += 1;
                    2
                }
                dbc::DbcDiagnosticSeverity::Warning => {
                    warnings += 1;
                    1
                }
                dbc::DbcDiagnosticSeverity::Info => {
                    infos += 1;
                    0
                }
            };
            if active_filter != 0
                && !((active_filter == 1 && severity == 2)
                    || (active_filter == 2 && severity == 1)
                    || (active_filter == 3 && severity == 0))
            {
                continue;
            }
            let frame_kind = if diagnostic.extended { "EXT" } else { "STD" };
            let signal = if diagnostic.signal_name.is_empty() {
                String::new()
            } else {
                format!(" / {}", diagnostic.signal_name)
            };
            rows.push(DbcDiagnosticRow {
                severity,
                code: diagnostic.code.into(),
                location: format!(
                    "{} · 0x{:X} {} · {}{}",
                    database.file_name,
                    diagnostic.message_id,
                    frame_kind,
                    diagnostic.message_name,
                    signal
                )
                .into(),
                title_zh: diagnostic.title_zh.into(),
                title_en: diagnostic.title_en.into(),
                detail_zh: diagnostic.detail_zh.into(),
                detail_en: diagnostic.detail_en.into(),
            });
        }
    }
    sync_vec_model(model, rows);
    window.set_file_count(app.dbcs.len().min(i32::MAX as usize) as i32);
    window.set_error_count(errors);
    window.set_warning_count(warnings);
    window.set_info_count(infos);
    window.set_action_status(
        if app.dbcs.is_empty() {
            if window.global::<FeatureI18n>().get_en() {
                "No DBC loaded"
            } else {
                "尚未加载 DBC"
            }
        } else if errors == 0 && warnings == 0 {
            if window.global::<FeatureI18n>().get_en() {
                "Diagnostics completed: no blocking issue"
            } else {
                "诊断完成：未发现阻断性问题"
            }
        } else if window.global::<FeatureI18n>().get_en() {
            "Diagnostics completed; review the items below"
        } else {
            "诊断完成，请检查下列问题"
        }
        .into(),
    );
}

fn dbc_diagnostic_report(app: &App, english: bool) -> String {
    let mut report = String::new();
    report.push_str(if english {
        "PcanWork DBC Diagnostics\n"
    } else {
        "PcanWork DBC 诊断报告\n"
    });
    report.push_str(&format!(
        "{}: {}\n\n",
        if english { "Files" } else { "文件数" },
        app.dbcs.len()
    ));
    let mut total = 0usize;
    for database in &app.dbcs {
        let diagnostics = database.diagnostics();
        report.push_str(&format!("== {} ==\n", database.file_name));
        if diagnostics.is_empty() {
            report.push_str(if english { "PASS\n\n" } else { "通过\n\n" });
            continue;
        }
        total += diagnostics.len();
        for diagnostic in diagnostics {
            let severity = match diagnostic.severity {
                dbc::DbcDiagnosticSeverity::Error => {
                    if english {
                        "ERROR"
                    } else {
                        "错误"
                    }
                }
                dbc::DbcDiagnosticSeverity::Warning => {
                    if english {
                        "WARN"
                    } else {
                        "警告"
                    }
                }
                dbc::DbcDiagnosticSeverity::Info => {
                    if english {
                        "INFO"
                    } else {
                        "提示"
                    }
                }
            };
            let signal = if diagnostic.signal_name.is_empty() {
                String::new()
            } else {
                format!("/{}", diagnostic.signal_name)
            };
            report.push_str(&format!(
                "[{severity}] {} 0x{:X} {} {}/{}{}\n{}\n\n",
                diagnostic.code,
                diagnostic.message_id,
                if diagnostic.extended { "EXT" } else { "STD" },
                database.file_name,
                diagnostic.message_name,
                signal,
                if english {
                    diagnostic.detail_en
                } else {
                    diagnostic.detail_zh
                }
            ));
        }
    }
    if app.dbcs.is_empty() {
        report.push_str(if english {
            "No DBC loaded.\n"
        } else {
            "尚未加载 DBC。\n"
        });
    } else {
        report.push_str(&format!(
            "{}: {}\n",
            if english {
                "Total findings"
            } else {
                "问题总数"
            },
            total
        ));
    }
    report
}

fn wire_main_children(app: Rc<std::cell::RefCell<App>>, windows: &ChildWindows) {
    {
        let app = app.clone();
        let spw = windows.sim_panel.as_weak();
        windows.sim_panel.window().on_close_requested(move || {
            let mut a = app.borrow_mut();
            if a.sim_running {
                let _ = configure_sim_generators(&a, false);
                a.sim_running = false;
                for wdg in a.sim_widgets.iter_mut() {
                    wdg.last_fire = None;
                }
                a.log("仿真面板关闭：已停止仿真".to_string());
                if let Some(w) = spw.upgrade() {
                    w.set_running(false);
                }
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    macro_rules! hide_child_on_close {
        ($window:expr) => {
            $window
                .window()
                .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
        };
    }
    hide_child_on_close!(windows.chart);
    hide_child_on_close!(windows.signal);
    hide_child_on_close!(windows.uds);
    hide_child_on_close!(windows.xcp);
    {
        let app = app.clone();
        windows.channel.window().on_close_requested(move || {
            let mut state = app.borrow_mut();
            if state.channel_connect_pending {
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                state.channel_edit = None;
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
    hide_child_on_close!(windows.playback);
    hide_child_on_close!(windows.convert);
    hide_child_on_close!(windows.cache);
    hide_child_on_close!(windows.trigger);
    hide_child_on_close!(windows.sim_prop);
    hide_child_on_close!(windows.console_help);
    hide_child_on_close!(windows.script_runner);
    hide_child_on_close!(windows.dbc_diagnostics);

    {
        let picker = windows.signal.as_weak();
        windows.chart.on_open_signal_selector(move || {
            if let Some(picker) = picker.upgrade() {
                show_child_window(&picker);
            }
        });
    }
    {
        let app = app.clone();
        let window = windows.dbc_diagnostics.as_weak();
        let model = windows.dbc_diagnostics_model.clone();
        windows.dbc_diagnostics.on_rescan(move || {
            if let Some(window) = window.upgrade() {
                refresh_dbc_diagnostics(&app.borrow(), &window, &model);
            }
        });
    }
    {
        let app = app.clone();
        let window = windows.dbc_diagnostics.as_weak();
        windows.dbc_diagnostics.on_copy_report(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let report =
                dbc_diagnostic_report(&app.borrow(), window.global::<FeatureI18n>().get_en());
            let status = match arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(report))
            {
                Ok(()) => if window.global::<FeatureI18n>().get_en() {
                    "Report copied"
                } else {
                    "诊断报告已复制"
                }
                .to_string(),
                Err(error) => {
                    if window.global::<FeatureI18n>().get_en() {
                        format!("Copy failed: {error}")
                    } else {
                        format!("复制失败：{error}")
                    }
                }
            };
            window.set_action_status(status.into());
        });
    }
    {
        let app = app.clone();
        let window = windows.dbc_diagnostics.as_weak();
        windows.dbc_diagnostics.on_export_report(move || {
            let Some(dialog_window) = window.upgrade() else {
                return;
            };
            let window = window.clone();
            let app = app.clone();
            let _ = slint::spawn_local(async move {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .add_filter("Text report", &["txt"])
                    .set_file_name("PcanWork-DBC-Diagnostics.txt")
                    .set_parent(&dialog_window.window().window_handle())
                    .save_file()
                    .await
                else {
                    return;
                };
                let report = dbc_diagnostic_report(
                    &app.borrow(),
                    dialog_window.global::<FeatureI18n>().get_en(),
                );
                let status = match std::fs::write(file.path(), report) {
                    Ok(()) => if dialog_window.global::<FeatureI18n>().get_en() {
                        "Report exported"
                    } else {
                        "诊断报告已导出"
                    }
                    .to_string(),
                    Err(error) => {
                        if dialog_window.global::<FeatureI18n>().get_en() {
                            format!("Export failed: {error}")
                        } else {
                            format!("导出失败：{error}")
                        }
                    }
                };
                if let Some(window) = window.upgrade() {
                    window.set_action_status(status.into());
                }
            });
        });
    }
    {
        let window = windows.dbc_diagnostics.as_weak();
        windows.dbc_diagnostics.on_dismiss(move || {
            if let Some(window) = window.upgrade() {
                let _ = window.window().hide();
            }
        });
    }
}

fn wire_main(app: Rc<std::cell::RefCell<App>>, ui: &AppWindow, child_windows: ChildWindowStore) {
    {
        let app_s = app.clone();
        let uiw = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = uiw.upgrade() {
                begin_shutdown(&app_s, &ui);
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

    {
        let app = app.clone();
        ui.on_connect(move || {
            let mut a = app.borrow_mut();
            if !a.license_allows("can-connect") {
                return;
            }
            refresh_and_reconcile_pcan(&mut a);
            let _ = a.cmd.send(Cmd::ConnectChannels(a.channels.clone()));
        });
    }
    {
        let app = app.clone();
        ui.on_disconnect(move || {
            let _ = app.borrow().cmd.send(Cmd::Disconnect);
        });
    }
    {
        let app = app.clone();
        ui.on_start_rx(move || {
            let mut a = app.borrow_mut();
            if !a.license_allows("can-capture") {
                return;
            }
            a.capture_wall_epoch = None;
            if !a.connected {
                refresh_and_reconcile_pcan(&mut a);
                let _ = a.cmd.send(Cmd::ConnectChannels(a.channels.clone()));
            }
            let _ = a.cmd.send(Cmd::Start);
        });
    }
    {
        let app = app.clone();
        ui.on_stop_rx(move || {
            let _ = app.borrow().cmd.send(Cmd::Stop);
        });
    }
    {
        let app = app.clone();
        ui.on_clear_msgs(move || {
            let mut a = app.borrow_mut();
            a.trace.clear();
            a.last.clear();
            a.last_dirty = true;
            a.selected_key = None;
            a.selected_index = -1;
            a.display_items.clear();
            a.expanded_keys.clear();
            a.expanded_signal_cache.clear();
            a.capture_wall_epoch = None;
            a.no_counter = 0;
            a.last_msg_sig = u64::MAX;
            a.log("已清空显示缓存");
        });
    }
    {
        let app = app.clone();
        ui.on_set_mode(move |trace| {
            app.borrow_mut().mode_trace = trace;
        });
    }
    {
        let app = app.clone();
        ui.on_set_time_mode(move |mode| {
            app.borrow_mut().time_mode = mode;
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_toggle_col(move |key| {
            let Some(ui) = uiw.upgrade() else { return };
            let k = key.to_string();
            let mut a = app.borrow_mut();
            if !a.cols_hidden.remove(&k) {
                a.cols_hidden.insert(k);
            }
            apply_col_widths(&ui, &a.cols_hidden);
        });
    }
    {
        let app = app.clone();
        ui.on_toggle_pause(move || {
            let mut a = app.borrow_mut();
            a.paused = !a.paused;
        });
    }
    {
        let app = app.clone();
        ui.on_toggle_autoscroll(move || {
            let mut a = app.borrow_mut();
            a.autoscroll = !a.autoscroll;
        });
    }
    {
        let app = app.clone();
        ui.on_console_set_enabled(move |en| {
            let mut a = app.borrow_mut();
            a.console_enabled = en;
            a.log(if en {
                "报文日志: 已启用捕获"
            } else {
                "报文日志: 已停止捕获"
            });
        });
    }
    {
        let app = app.clone();
        ui.on_console_set_id(move |s| {
            let mut a = app.borrow_mut();
            let t = s.trim();
            if t.is_empty() {
                a.console_id = None;
            } else {
                let h = t.trim_start_matches("0x").trim_start_matches("0X");
                match u32::from_str_radix(h, 16) {
                    Ok(id) => a.console_id = Some(id),
                    Err(_) => a.log(format!("报文日志: 无效 ID '{t}'(按十六进制解析)")),
                }
            }
        });
    }
    {
        let app = app.clone();
        ui.on_console_set_ch(move |ch| {
            app.borrow_mut().console_ch = ch.clamp(0, 255) as u8;
        });
    }
    {
        let app = app.clone();
        ui.on_console_clear(move || {
            app.borrow_mut().console.clear();
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_console_export(move || {
            let text = app.borrow().console.export_text();
            if text.is_empty() {
                app.borrow_mut().log("报文日志为空, 无可导出");
                return;
            }
            let worker = app.borrow().worker_tx.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("文本 (txt)", &["txt"])
                    .add_filter("日志 (log)", &["log"])
                    .set_file_name("can_console.txt");
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else {
                    return;
                };
                let path = file.path().to_path_buf();
                std::thread::spawn(move || {
                    let message = match std::fs::write(&path, text.as_bytes()) {
                        Ok(()) => format!("已导出报文日志: {}", path.display()),
                        Err(error) => format!("导出报文日志失败: {error}"),
                    };
                    let _ = worker.send(WorkerEvent::Log(message));
                });
            });
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_toggle_record(move || {
            let recording = app.borrow().recording;
            if recording {
                let mut a = app.borrow_mut();
                a.recording = false;
                if let Err(error) = a.recorder.stop() {
                    a.log(format!("停止记录失败: {error}"));
                }
            } else {
                if !app.borrow_mut().license_allows("record-export") {
                    return;
                }
                let app = app.clone();
                let uiw = uiw.clone();
                let _ = slint::spawn_local(async move {
                    let mut dlg = rfd::AsyncFileDialog::new()
                        .add_filter("CSV (本软件/通用)", &["csv"])
                        .add_filter("Vector ASC (CANoe/ZXDoc 可打开)", &["asc"])
                        .add_filter("Vector BLF (二进制)", &["blf"])
                        .set_file_name("can_record.csv");
                    if let Some(w) = uiw.upgrade() {
                        dlg = dlg.set_parent(&w.window().window_handle());
                    }
                    let Some(file) = dlg.save_file().await else {
                        return;
                    };
                    let path = file.path().to_path_buf();
                    let mut a = app.borrow_mut();
                    let ext = path
                        .extension()
                        .map(|e| e.to_ascii_lowercase())
                        .unwrap_or_default();
                    let fmt = if ext == "asc" {
                        RecFmt::Asc
                    } else if ext == "blf" {
                        RecFmt::Blf
                    } else {
                        RecFmt::Csv
                    };
                    match a.recorder.start(path.clone(), fmt) {
                        Ok(()) => {
                            a.rec_fmt = fmt;
                            a.rec_path = Some(path);
                            a.recording = true;
                        }
                        Err(error) => {
                            a.log(format!("开始记录失败: {error}"));
                        }
                    }
                });
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_load_dbc(move || {
            let app = app.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new().add_filter("DBC", &["dbc"]);
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.pick_file().await else {
                    return;
                };
                let path = file.path().to_path_buf();
                let p = path.to_string_lossy().to_string();
                if app.borrow().dbc_paths.iter().any(|x| x == &p) {
                    app.borrow_mut().log(format!("该 DBC 已加载: {p}"));
                    return;
                }
                let worker = app.borrow().worker_tx.clone();
                std::thread::spawn(move || {
                    let result = DbcDb::load(&p);
                    let _ = worker.send(WorkerEvent::DbcLoaded { path: p, result });
                });
            });
        });
    }
    {
        let app = app.clone();
        ui.on_reload_dbc(move || {
            let paths = app.borrow().dbc_paths.clone();
            if paths.is_empty() {
                app.borrow_mut()
                    .log("尚未加载 DBC，无法重新加载".to_string());
                return;
            }
            let worker = app.borrow().worker_tx.clone();
            std::thread::spawn(move || {
                let mut loaded = Vec::new();
                let mut errors = Vec::new();
                for path in paths {
                    match DbcDb::load(&path) {
                        Ok(db) => loaded.push((path, db)),
                        Err(error) => errors.push(format!("重新加载失败 {path}: {error}")),
                    }
                }
                let _ = worker.send(WorkerEvent::DbcReloaded { loaded, errors });
            });
        });
    }
    {
        let app = app.clone();
        ui.on_clear_dbc(move || {
            let mut a = app.borrow_mut();
            let n = a.dbcs.len();
            a.dbcs.clear();
            a.dbc_paths.clear();
            a.expanded_signal_cache.clear();
            rebuild_dbc_snap(&mut a);
            a.log(format!("已清除全部 DBC（{n} 个）"));
        });
    }
    {
        let app = app.clone();
        ui.on_tree_remove_dbc(move |i| {
            let mut a = app.borrow_mut();
            let Some(&dbc_i) = a.tree_dbc_index.get(i as usize) else {
                return;
            };
            let dbc_i = dbc_i as usize;
            if dbc_i >= a.dbcs.len() {
                return;
            }
            let name = a.dbcs[dbc_i].file_name.clone();
            a.dbcs.remove(dbc_i);
            a.expanded_signal_cache.clear();
            if dbc_i < a.dbc_paths.len() {
                a.dbc_paths.remove(dbc_i);
            }
            a.last_tree_sig = u64::MAX;
            a.signal_pick_cache = u64::MAX;
            a.tx_msgs_cache = u64::MAX;
            a.tx_sig_cache = u64::MAX;
            rebuild_dbc_snap(&mut a);
            a.log(format!("已删除 DBC: {name}"));
        });
    }
    {
        let app = app.clone();
        ui.on_select_row(move |i| {
            let mut a = app.borrow_mut();
            a.selected_index = i;
            if let Some(k) = display_key(&a, i) {
                a.selected_key = Some(k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_view_signals(move |i| {
            let mut a = app.borrow_mut();
            a.selected_index = i;
            if let Some(k) = display_key(&a, i) {
                a.selected_key = Some(k);
                let id = (k & 0xFFFF_FFFF) as u32;
                a.log(format!("查看 0x{id:X} 信号解析"));
            }
        });
    }
    {
        let app = app.clone();
        ui.on_toggle_expand(move |i| {
            let mut a = app.borrow_mut();
            a.selected_index = i;
            let Some(k) = display_key(&a, i) else {
                return;
            };
            a.selected_key = Some(k);
            if !a.expanded_keys.insert(k) {
                a.expanded_keys.remove(&k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_msg_sig_to_chart(move |i| {
            let mut a = app.borrow_mut();
            let Some((k, signal)) = display_signal(&a, i) else {
                return;
            };
            a.selected_index = i;
            a.selected_key = Some(k);
            let id = (k & 0xFFFF_FFFF) as u32;
            let msg = add_signal_to_chart(&mut a, id, &signal);
            a.log(msg);
        });
    }
    {
        let app = app.clone();
        ui.on_add_sig_to_chart(move |i| {
            let mut a = app.borrow_mut();
            if let Some((id, signal)) = a.sig_panel.get(i as usize).cloned() {
                let msg = add_signal_to_chart(&mut a, id, &signal);
                a.log(msg);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_apply_filter(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let mut a = app.borrow_mut();
            a.filter = parse_filter(&ui.get_f_id(), &ui.get_f_name(), &ui.get_f_data());
            a.filter.dir_filter = dir_idx_to_opt(ui.get_dir_filter());
            a.last_msg_sig = u64::MAX;
            a.log("已应用过滤");
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_clear_filter(move || {
            let Some(ui) = uiw.upgrade() else { return };
            ui.set_f_id(SharedString::new());
            ui.set_f_name(SharedString::new());
            ui.set_f_data(SharedString::new());
            ui.set_dir_filter(0);
            let mut a = app.borrow_mut();
            a.filter = Filter::default();
            a.last_msg_sig = u64::MAX;
        });
    }
    {
        let app = app.clone();
        ui.on_set_dir_filter(move |idx| {
            let mut a = app.borrow_mut();
            a.filter.dir_filter = dir_idx_to_opt(idx);
            a.last_msg_sig = u64::MAX;
        });
    }
    {
        let app = app.clone();
        ui.on_tx_add(move || {
            let mut a = app.borrow_mut();
            let h = a.next_handle;
            a.next_handle += 1;
            let n = a.txs.len() + 1;
            a.txs.push(TxTask {
                name: format!("Tx_{n}"),
                ch: 1,
                id: 0x200,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                data: vec![0x01, 0, 0, 0, 0, 0, 0, 0],
                periodic: false,
                period_ms: 100,
                repeat: -1,
                sent: 0,
                handle: h,
                dbc_id: None,
                sig_values: Vec::new(),
                varies: Vec::new(),
            });
        });
    }
    {
        let app = app.clone();
        ui.on_tx_remove(move |i| {
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i < a.txs.len() {
                let t = a.txs.remove(i);
                stop_task_periodic(&a, &t);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_tx_send_once(move |i| {
            let mut a = app.borrow_mut();
            if !a.license_allows("can-transmit") {
                return;
            }
            let i = i as usize;
            if i < a.txs.len() {
                let f = tx_frame(&a.txs[i]);
                let _ = a.cmd.send(Cmd::SendOnce(f));
                a.txs[i].sent += 1;
            }
        });
    }
    {
        let app = app.clone();
        ui.on_tx_toggle_periodic(move |i| {
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i < a.txs.len() {
                if !a.txs[i].periodic && !a.license_allows("can-transmit") {
                    return;
                }
                a.txs[i].periodic = !a.txs[i].periodic;
                toggle_task_periodic(&mut a, i);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_tree_clicked(move |i| {
            let mut a = app.borrow_mut();
            let Some(key) = a.tree_row_keys.get(i as usize).cloned() else {
                return;
            };
            if key.is_empty() {
                return;
            }
            if key.starts_with("dbcfile:") {
                if !a.tree_collapsed.remove(&key) {
                    a.tree_collapsed.insert(key);
                }
            } else if !a.tree_collapsed.insert(key.clone()) {
                a.tree_collapsed.remove(&key);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_tree_dblclick(move |i| {
            let mut a = app.borrow_mut();
            if let Some(Some(name)) = a.tree_curve_sig.get(i as usize).cloned() {
                a.chart_highlight = Some((name.clone(), std::time::Instant::now()));
                for s in a.series.iter_mut() {
                    if s.name == name {
                        s.visible = true;
                    }
                }
                a.log(format!("高亮曲线信号: {name}"));
                drop(a);
                if let Some(ui) = uiw.upgrade() {
                    ui.invoke_open_chart_window();
                }
            }
        });
    }
    {
        let app = app.clone();
        ui.on_clear_chart(move || {
            let mut a = app.borrow_mut();
            a.series.clear();
            a.log("已清空曲线");
        });
    }
    {
        let app = app.clone();
        ui.on_tx_all(move |start| {
            let mut a = app.borrow_mut();
            if start && !a.license_allows("can-transmit") {
                return;
            }
            for task in &mut a.txs {
                task.periodic = start;
                if start {
                    task.sent = 0;
                }
            }
            for idx in 0..a.txs.len() {
                configure_task_periodic(&mut a, idx);
            }
            a.log(if start {
                "启动全部发送"
            } else {
                "停止全部发送"
            });
        });
    }
    {
        let app = app.clone();
        ui.on_tx_update(move |i, field, value| {
            let mut a = app.borrow_mut();
            update_tx_task(&mut a, i, &field, &value);
            let idx = i as usize;
            if a.txs.get(idx).is_some_and(|t| t.periodic) {
                configure_task_periodic(&mut a, idx);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_menu_info(move |s| {
            app.borrow_mut().log(format!("[菜单] {s}"));
        });
    }
    {
        let app = app.clone();
        ui.on_sort_by(move |col| {
            let mut a = app.borrow_mut();
            if a.sort_col == col {
                if !a.sort_desc {
                    a.sort_desc = true;
                } else {
                    a.sort_col = -1;
                    a.sort_desc = false;
                }
            } else {
                a.sort_col = col;
                a.sort_desc = false;
            }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_only_id(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) {
                act_only_id(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_hide_id(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) {
                act_hide_id(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_to_tx(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) {
                act_to_tx(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_send_now(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) {
                act_send_now(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_add_all_signals(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) {
                act_add_all_signals(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_chart_toggle_pause(move || {
            let mut a = app.borrow_mut();
            a.chart_paused = !a.chart_paused;
            if a.chart_paused {
                a.chart_pause_view =
                    Some(a.chart_view.unwrap_or_else(|| chart_full_range(&a.series)));
                a.chart_frozen_series = Some(a.series.clone());
            } else {
                a.chart_pause_view = None;
                a.chart_frozen_series = None;
            }
        });
    }
    {
        let app = app.clone();
        ui.on_chart_autoscale(move || {
            app.borrow_mut().chart_normalize = false;
        });
    }
    {
        let app = app.clone();
        ui.on_chart_normalize_toggle(move || {
            let mut a = app.borrow_mut();
            a.chart_normalize = !a.chart_normalize;
        });
    }
    {
        let app = app.clone();
        ui.on_chart_cursor_toggle(move || {
            let mut a = app.borrow_mut();
            a.chart_cursor = !a.chart_cursor;
        });
    }
    {
        let app = app.clone();
        ui.on_chart_toggle_series(move |i| {
            let mut a = app.borrow_mut();
            if let Some(s) = a.series.get_mut(i as usize) {
                s.visible = !s.visible;
            }
        });
    }
    {
        let app = app.clone();
        ui.on_chart_remove_series(move |i| {
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i < a.series.len() {
                let name = a.series.remove(i).name;
                a.log(format!("已移除曲线信号 {name}"));
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_chart_export_csv(move || {
            let snapshot = chart_export_snapshot(&app.borrow());
            if snapshot.is_empty() {
                app.borrow_mut().log("曲线为空，无可导出数据".to_string());
                return;
            }
            let worker = app.borrow().worker_tx.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("chart_data.csv");
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else {
                    return;
                };
                let path = file.path().to_path_buf();
                spawn_chart_export(snapshot, path, false, worker);
            });
        });
    }
    {
        let app = app.clone();
        ui.on_chart_add_dbc_signal(move |i| {
            let mut a = app.borrow_mut();
            let Some((id, signal)) = a.dbc_signal_choices.get(i as usize).cloned() else {
                a.log("没有可添加的 DBC 信号".to_string());
                return;
            };
            let msg = add_signal_to_chart(&mut a, id, &signal);
            a.log(msg);
        });
    }
    {
        let app = app.clone();
        ui.on_sel_only_id(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key {
                act_only_id(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_hide_id(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key {
                act_hide_id(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_to_tx(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key {
                act_to_tx(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_send_now(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key {
                act_send_now(&mut a, k);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_add_all_signals(move || {
            let mut a = app.borrow_mut();
            match a.selected_key {
                Some(k) => act_add_all_signals(&mut a, k),
                None => a.log("请先单击选中一行报文".to_string()),
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_save_project(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let (proj, sim_revision) = {
                let a = app.borrow();
                (
                    Project {
                        name: a.project_name.clone(),
                        settings: gather_settings(&a, &ui),
                        txs: a.txs.iter().map(TxTaskDto::from_task).collect(),
                    },
                    a.sim_revision,
                )
            };
            let default_file_name = format!("{}.pcprj", proj.name);
            let worker = app.borrow().worker_tx.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("PcanWork 工程", &["pcprj"])
                    .add_filter("旧工程/JSON", &["zcp", "json"])
                    .set_file_name(&default_file_name);
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else {
                    return;
                };
                let path = file.path().to_path_buf();
                std::thread::spawn(move || {
                    let result = serde_json::to_string_pretty(&proj)
                        .map_err(|error| format!("序列化工程失败: {error}"))
                        .and_then(|text| {
                            std::fs::write(&path, text)
                                .map_err(|error| format!("保存工程失败: {error}"))
                        });
                    let _ = worker.send(WorkerEvent::ProjectSaved {
                        path,
                        sim_revision,
                        result,
                    });
                });
            });
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_open_project(move || {
            let app = app.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let Some(ui) = uiw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("PcanWork 工程", &["pcprj"])
                    .add_filter("旧工程/JSON", &["zcp", "json"]);
                dlg = dlg.set_parent(&ui.window().window_handle());
                let Some(file) = dlg.pick_file().await else {
                    return;
                };
                let path = file.path().to_path_buf();
                let worker = app.borrow().worker_tx.clone();
                queue_project_load(path, worker);
            });
        });
    }
    {
        let app = app.clone();
        ui.on_open_recent_project(move |index| {
            let (path, worker) = {
                let a = app.borrow();
                let Some(path) = a.recent_project_paths.get(index as usize) else {
                    return;
                };
                (std::path::PathBuf::from(path), a.worker_tx.clone())
            };
            if path.is_file() {
                queue_project_load(path, worker);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_new_project(move || {
            {
                let mut a = app.borrow_mut();
                a.project_name = format!("Project_{}", chrono::Local::now().format("%H%M%S"));
                a.project_path = None;
                a.sim_dirty = false;
                a.sim_revision = 0;
                let _ = configure_sim_generators(&a, false);
                a.sim_running = false;
                a.sim_sel = -1;
                a.sim_multi.clear();
                let tasks = a.txs.clone();
                for task in &tasks {
                    stop_task_periodic(&a, task);
                }
                a.txs.clear();
                a.filter = Filter::default();
                a.series.clear();
                a.trace.clear();
                a.last.clear();
                a.last_dirty = true;
                a.selected_key = None;
                a.selected_index = -1;
                a.display_items.clear();
                a.expanded_keys.clear();
                a.expanded_signal_cache.clear();
                a.sim_widgets.clear();
                a.sim_tx_frames.clear();
                a.dbcs.clear();
                a.dbc_paths.clear();
                rebuild_dbc_snap(&mut a);
                refresh_sim(&a);
                a.last_tree_sig = u64::MAX;
                let project_name = a.project_name.clone();
                a.log(format!("已新建工程: {project_name}"));
            }
            if let Some(ui) = uiw.upgrade() {
                ui.set_project_open(true);
                ui.set_f_id("".into());
                ui.set_f_name("".into());
                ui.set_f_data("".into());
                ui.set_dir_filter(0);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_apply_renderer(move |requested| {
            let Some(ui) = uiw.upgrade() else { return };
            let next = match requested.as_str() {
                "gpu" => "gpu",
                "cpu" => "cpu",
                _ => "auto",
            };
            let label = match next {
                "gpu" => "GPU",
                "cpu" => "CPU",
                _ => "Auto",
            };
            ui.set_renderer_mode(next.into());
            let mut a = app.borrow_mut();
            if let Err(error) = settings::save(&gather_settings(&a, &ui)) {
                a.log(format!("保存设置失败: {error}"));
            }
            a.log(format!("渲染器已设为 {label}，正在重启程序"));
            if let Err(e) = restart_current_process() {
                a.log(format!("自动重启失败: {e}"));
            }
        });
    }
    {
        let uiw = ui.as_weak();
        let child_windows = child_windows.clone();
        ui.on_toggle_dark(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let dark = !ui.global::<Theme>().get_dark();
            ui.global::<Theme>().set_dark(dark);
            if let Some(windows) = child_windows.get() {
                windows.set_dark(dark);
            }
        });
    }
    {
        let uiw = ui.as_weak();
        let child_windows = child_windows.clone();
        ui.on_toggle_big(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let big = !ui.global::<Theme>().get_big();
            ui.global::<Theme>().set_big(big);
            if let Some(windows) = child_windows.get() {
                windows.set_big(big);
            }
        });
    }
    {
        let uiw = ui.as_weak();
        let child_windows = child_windows.clone();
        let app = app.clone();
        ui.on_toggle_lang(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let en = !ui.global::<I18n>().get_en();
            {
                let mut a = app.borrow_mut();
                a.lang_en = en;
                a.last_tree_sig = u64::MAX;
            }
            ui.global::<I18n>().set_en(en);
            if let Some(windows) = child_windows.get() {
                windows.set_language(en);
                refresh_dbc_diagnostics(
                    &app.borrow(),
                    &windows.dbc_diagnostics,
                    &windows.dbc_diagnostics_model,
                );
            }
        });
    }
}
