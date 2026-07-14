// Event-wiring for the wire_main. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_main(
    app: Rc<std::cell::RefCell<App>>,
    ui: &AppWindow,
    chart_window: &ChartWindow,
    signal_window: &SignalSelectWindow,
    tx_window: &TxWindow,
    uds_window: &UdsWindow,
    xcp_window: &XcpWindow,
    channel_window: &ChannelConfigWindow,
    playback_window: &PlaybackWindow,
    convert_window: &ConvertWindow,
    cache_window: &CacheConfigWindow,
    trigger_window: &TriggerWindow,
    sim_panel_window: &SimPanelWindow,
    sim_prop_window: &SimPropWindow,
    console_help_window: &ConsoleHelpWindow,
    script_runner_window: &ScriptRunnerWindow,
) {
    {
        let app = app.clone();
        let spw = sim_panel_window.as_weak();
        sim_panel_window.window().on_close_requested(move || {
            let mut a = app.borrow_mut();
            if a.sim_running {
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
    hide_child_on_close!(chart_window);
    hide_child_on_close!(signal_window);
    hide_child_on_close!(uds_window);
    hide_child_on_close!(xcp_window);
    hide_child_on_close!(channel_window);
    hide_child_on_close!(playback_window);
    hide_child_on_close!(convert_window);
    hide_child_on_close!(cache_window);
    hide_child_on_close!(trigger_window);
    hide_child_on_close!(sim_prop_window);
    hide_child_on_close!(console_help_window);
    hide_child_on_close!(script_runner_window);
{
        let cmd_quit = app.borrow().cmd.clone();
        let app_s = app.clone();
        let uiw = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = uiw.upgrade() {
                settings::save(&gather_settings(&app_s.borrow(), &ui));
            }
            {
                let mut a = app_s.borrow_mut();
                if a.recording && a.rec_fmt == RecFmt::Blf
                    && let Some(p) = a.rec_path.clone()
                {
                    let buf = std::mem::take(&mut a.rec_blf_buf);
                    let n = buf.len();
                    match blf::write(&p.to_string_lossy(), &buf) {
                        Ok(()) => a.log(format!("退出前已保存 BLF: {} ({n} 帧)", p.display())),
                        Err(e) => a.log(format!("退出前写 BLF 失败: {e}")),
                    }
                }
                if let Some(mut c) = a.py_child.take() {
                    let _ = c.kill();
                }
                a.ipc_subs.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = cmd_quit.send(Cmd::Disconnect);
            let _ = cmd_quit.send(Cmd::Quit);
std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }

    {
        let app = app.clone();
        ui.on_connect(move || {
            let a = app.borrow();
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
            a.capture_wall_epoch = None;
if !a.connected {
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
            a.capture_wall_epoch = None;
            a.no_counter = 0;
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
            a.log(if en { "报文日志: 已启用捕获" } else { "报文日志: 已停止捕获" });
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
        let hw = console_help_window.as_weak();
        ui.on_console_help(move || {
            if let Some(w) = hw.upgrade() {
                show_child_window(&w);
            }
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
            let app = app.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("文本 (txt)", &["txt"])
                    .add_filter("日志 (log)", &["log"])
                    .set_file_name("can_console.txt");
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                match std::fs::write(&path, text.as_bytes()) {
                    Ok(()) => app.borrow_mut().log(format!("已导出报文日志: {}", path.display())),
                    Err(e) => app.borrow_mut().log(format!("导出报文日志失败: {e}")),
                }
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
                a.rec_file = None;
                if a.rec_fmt == RecFmt::Blf {
                    if let Some(p) = a.rec_path.clone() {
                        let buf = std::mem::take(&mut a.rec_blf_buf);
                        let n = buf.len();
                        match blf::write(&p.to_string_lossy(), &buf) {
                            Ok(()) => a.log(format!("已保存 BLF: {} ({n} 帧)", p.display())),
                            Err(e) => a.log(format!("写 BLF 失败: {e}")),
                        }
                    }
                }
                a.log("停止记录");
            } else {
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
                    let Some(file) = dlg.save_file().await else { return };
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
                a.rec_fmt = fmt;
                a.rec_path = Some(path.clone());
                if fmt == RecFmt::Blf {
a.rec_blf_buf.clear();
                    a.rec_file = None;
                    a.recording = true;
                    a.log(format!("开始记录(BLF): {}", path.display()));
                } else {
                    match std::fs::File::create(&path) {
                        Ok(f) => {
                            let mut w = std::io::BufWriter::new(f);
                            match fmt {
                                RecFmt::Csv => {
                                    let _ = writeln!(w, "Time,Ch,Dir,ID,Len,Data");
                                }
                                RecFmt::Asc => {
                                    let _ = writeln!(w, "date Tue Jun 17 10:00:00 am 2026");
                                    let _ = writeln!(w, "base hex  timestamps absolute");
                                    let _ = writeln!(w, "no internal events logged");
                                }
                                RecFmt::Blf => {}
                            }
                            a.rec_file = Some(w);
                            a.recording = true;
                            a.log(format!(
                                "开始记录({}): {}",
                                if fmt == RecFmt::Asc { "ASC" } else { "CSV" },
                                path.display()
                            ));
                        }
                        Err(e) => a.log(format!("创建记录文件失败: {e}")),
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
                let Some(file) = dlg.pick_file().await else { return };
                let path = file.path().to_path_buf();
                let mut a = app.borrow_mut();
                let p = path.to_string_lossy().to_string();
                if a.dbc_paths.iter().any(|x| x == &p) {
                    a.log(format!("该 DBC 已加载: {p}"));
                    return;
                }
                match DbcDb::load(&p) {
                    Ok(db) => {
                        let n = db.messages().count();
                        let total = a.dbcs.len() + 1;
                        a.log(format!(
                            "已加载 DBC: {} ({n} 条报文)，当前共 {total} 个 DBC",
                            db.file_name
                        ));
                        a.dbcs.push(db);
                        a.dbc_paths.push(p);
                        rebuild_dbc_snap(&mut a);
                    }
                    Err(e) => a.log(format!("加载 DBC 失败: {e}")),
                }
            });
        });
    }
    {
        let app = app.clone();
        ui.on_reload_dbc(move || {
            let mut a = app.borrow_mut();
            if a.dbc_paths.is_empty() {
                a.log("尚未加载 DBC，无法重新加载".to_string());
                return;
            }
            let paths = a.dbc_paths.clone();
            a.dbcs.clear();
            a.dbc_paths.clear();
            let mut ok = 0;
            for p in paths {
                match DbcDb::load(&p) {
                    Ok(db) => {
                        a.dbcs.push(db);
                        a.dbc_paths.push(p);
                        ok += 1;
                    }
                    Err(e) => a.log(format!("重新加载失败 {p}: {e}")),
                }
            }
            rebuild_dbc_snap(&mut a);
            a.log(format!("已重新加载 {ok} 个 DBC"));
        });
    }
    {
        let app = app.clone();
        ui.on_clear_dbc(move || {
            let mut a = app.borrow_mut();
            let n = a.dbcs.len();
            a.dbcs.clear();
            a.dbc_paths.clear();
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
            let ui = uiw.unwrap();
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
            let ui = uiw.unwrap();
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
                if t.periodic {
                    let _ = a.cmd.send(Cmd::SetPeriodic {
                        handle: t.handle,
                        frame: tx_frame(&t),
                        period_ms: t.period_ms,
                        repeat: t.repeat,
                        enable: false,
                    });
                }
            }
        });
    }
    {
        let app = app.clone();
        ui.on_tx_send_once(move |i| {
            let mut a = app.borrow_mut();
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
        let cw = chart_window.as_weak();
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
                if let Some(w) = cw.upgrade() {
                    show_child_window(&w);
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
            let speed = a.tx_speed;
            let cmds: Vec<Cmd> = a
                .txs
                .iter_mut()
                .map(|t| {
                    t.periodic = start;
                    if start {
                        t.sent = 0;
                    }
                    Cmd::SetPeriodic {
                        handle: t.handle,
                        frame: tx_frame(t),
                        period_ms: eff_period(t.period_ms, speed),
                        repeat: t.repeat,
                        enable: start,
                    }
                })
                .collect();
            for c in cmds {
                let _ = a.cmd.send(c);
            }
            a.log(if start {
                "启动全部发送"
            } else {
                "停止全部发送"
            });
        });
    }
    {
        let chart = chart_window.as_weak();
        ui.on_open_chart_window(move || {
            if let Some(chart) = chart.upgrade() {
                show_child_window(&chart);
            }
        });
    }
    {
        let picker = signal_window.as_weak();
        chart_window.on_open_signal_selector(move || {
            if let Some(picker) = picker.upgrade() {
                show_child_window(&picker);
            }
        });
    }
    {
        let txw = tx_window.as_weak();
        ui.on_open_tx_window(move || {
            if let Some(txw) = txw.upgrade() {
                show_child_window(&txw);
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        ui.on_open_channel_config(move || {
            let mut a = app.borrow_mut();
            a.pcan_devices = can::pcan_attached_channels();
            if let Some(chw) = chw.upgrade() {
                let sel = a.channel_sel.clamp(0, a.channels.len() as i32 - 1).max(0);
                chw.set_chan_sel(sel);
                refresh_channel_window_lists(&chw, &a);
                if let Some(c) = a.channels.get(sel as usize) {
                    set_chan_form(&chw, c);
                }
                show_child_window(&chw);
            }
        });
    }
    {
        let app = app.clone();
        ui.on_tx_update(move |i, field, value| {
            let mut a = app.borrow_mut();
            update_tx_task(&mut a, i, &field, &value);
            if let Some(t) = a.txs.get(i as usize)
                && t.periodic {
                    let _ = a.cmd.send(Cmd::SetPeriodic {
                        handle: t.handle,
                        frame: tx_frame(t),
                        period_ms: t.period_ms,
                        repeat: t.repeat,
                        enable: true,
                    });
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
        let cmd_quit = app.borrow().cmd.clone();
        let app_s = app.clone();
        let uiw = ui.as_weak();
        ui.on_quit_app(move || {
            if let Some(ui) = uiw.upgrade() {
                settings::save(&gather_settings(&app_s.borrow(), &ui));
            }
            let _ = cmd_quit.send(Cmd::Disconnect);
            let _ = cmd_quit.send(Cmd::Quit);
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = slint::quit_event_loop();
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
            if let Some(k) = display_key(&a, i) { act_only_id(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_hide_id(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) { act_hide_id(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_to_tx(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) { act_to_tx(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_send_now(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) { act_send_now(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_ctx_add_all_signals(move |i| {
            let mut a = app.borrow_mut();
            if let Some(k) = display_key(&a, i) { act_add_all_signals(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_chart_toggle_pause(move || {
            let mut a = app.borrow_mut();
            a.chart_paused = !a.chart_paused;
            if a.chart_paused {
                a.chart_pause_view = Some(a.chart_view.unwrap_or_else(|| chart_full_range(&a.series)));
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
            if app.borrow().series.is_empty() {
                app.borrow_mut().log("曲线为空，无可导出数据".to_string());
                return;
            }
            let app = app.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("chart_data.csv");
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                let mut a = app.borrow_mut();
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        let mut w = std::io::BufWriter::new(f);
                        let _ = writeln!(w, "Time,Signal,Value,Unit");
                        for s in &a.series {
                            for &(t, v) in &s.samples {
                                let _ = writeln!(w, "{t:.6},{},{v},{}", s.name, s.unit);
                            }
                        }
                        a.log(format!("曲线数据已导出: {}", path.display()));
                    }
                    Err(e) => a.log(format!("导出失败: {e}")),
                }
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
            if let Some(k) = a.selected_key { act_only_id(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_hide_id(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key { act_hide_id(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_to_tx(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key { act_to_tx(&mut a, k); }
        });
    }
    {
        let app = app.clone();
        ui.on_sel_send_now(move || {
            let mut a = app.borrow_mut();
            if let Some(k) = a.selected_key { act_send_now(&mut a, k); }
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
            let proj = {
                let a = app.borrow();
                Project {
                    name: a.project_name.clone(),
                    settings: gather_settings(&a, &ui),
                    txs: a.txs.iter().map(TxTaskDto::from_task).collect(),
                }
            };
            let default_file_name = format!("{}.pcprj", proj.name);
            let app = app.clone();
            let uiw = uiw.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("PcanWork 工程", &["pcprj"])
                    .add_filter("旧工程/JSON", &["zcp", "json"])
                    .set_file_name(&default_file_name);
                if let Some(w) = uiw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                match serde_json::to_string_pretty(&proj) {
                    Ok(txt) => match std::fs::write(&path, txt) {
                        Ok(_) => app
                            .borrow_mut()
                            .log(format!("已保存工程: {}", path.display())),
                        Err(e) => app.borrow_mut().log(format!("保存工程失败: {e}")),
                    },
                    Err(e) => app.borrow_mut().log(format!("序列化失败: {e}")),
                }
            });
        });
    }
{
        let app = app.clone();
        let uiw = ui.as_weak();
        let cw = chart_window.as_weak();
        let sw = signal_window.as_weak();
        let txw = tx_window.as_weak();
        let chw = channel_window.as_weak();
        let pw = playback_window.as_weak();
        let tgw = trigger_window.as_weak();
        let ccw = cache_window.as_weak();
        let spw = sim_panel_window.as_weak();
        let ppw = sim_prop_window.as_weak();
        ui.on_open_project(move || {
            let app = app.clone();
            let uiw = uiw.clone();
            let cw = cw.clone();
            let sw = sw.clone();
            let txw = txw.clone();
            let chw = chw.clone();
            let pw = pw.clone();
            let tgw = tgw.clone();
            let ccw = ccw.clone();
            let spw = spw.clone();
            let ppw = ppw.clone();
            let _ = slint::spawn_local(async move {
                let Some(ui) = uiw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("PcanWork 工程", &["pcprj"])
                    .add_filter("旧工程/JSON", &["zcp", "json"]);
                dlg = dlg.set_parent(&ui.window().window_handle());
                let Some(file) = dlg.pick_file().await else { return };
                let path = file.path().to_path_buf();
            let txt = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    app.borrow_mut().log(format!("读取工程失败: {e}"));
                    return;
                }
            };
            let proj: Project = match serde_json::from_str(&txt) {
                Ok(p) => p,
                Err(e) => {
                    app.borrow_mut().log(format!("解析工程失败: {e}"));
                    return;
                }
            };
            let dark = proj.settings.dark;
            let big = proj.settings.big;
            {
                let mut a = app.borrow_mut();
                a.project_name = if proj.name.trim().is_empty() {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("CAN_Test_Project")
                        .to_string()
                } else {
                    proj.name.clone()
                };
                apply_settings(&mut a, &ui, &proj.settings);
                let stops: Vec<Cmd> = a
                    .txs
                    .iter()
                    .map(|t| Cmd::SetPeriodic {
                        handle: t.handle,
                        frame: tx_frame(t),
                        period_ms: t.period_ms,
                        repeat: t.repeat,
                        enable: false,
                    })
                    .collect();
                for c in stops {
                    let _ = a.cmd.send(c);
                }
                a.txs.clear();
                a.change_next.clear();
                let n = proj.txs.len();
                for dto in proj.txs {
                    let h = a.next_handle;
                    a.next_handle += 1;
                    a.txs.push(dto.into_task(h));
                }
                a.last_tree_sig = u64::MAX;
                a.log(format!(
                    "已打开工程: {}（发送任务 {n} 条，默认停发）",
                    path.display()
                ));
            }
ui.global::<Theme>().set_dark(dark);
            ui.global::<Theme>().set_big(big);
            if let Some(w) = cw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = sw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = txw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = chw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = pw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = tgw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = ccw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = spw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
            if let Some(w) = ppw.upgrade() { w.global::<Theme>().set_dark(dark); w.global::<Theme>().set_big(big); }
refresh_sim(&app.borrow());
            });
        });
    }
{
        let app = app.clone();
        let uiw = ui.as_weak();
        ui.on_new_project(move || {
            {
                let mut a = app.borrow_mut();
                a.project_name = format!("Project_{}", chrono::Local::now().format("%H%M%S"));
                let stops: Vec<Cmd> = a
                    .txs
                    .iter()
                    .map(|t| Cmd::SetPeriodic {
                        handle: t.handle,
                        frame: tx_frame(t),
                        period_ms: t.period_ms,
                        repeat: t.repeat,
                        enable: false,
                    })
                    .collect();
                for c in stops {
                    let _ = a.cmd.send(c);
                }
                a.txs.clear();
                a.change_next.clear();
                a.filter = Filter::default();
                a.series.clear();
                a.trace.clear();
                a.last.clear();
                a.last_dirty = true;
                a.selected_key = None;
                a.selected_index = -1;
                a.display_items.clear();
                a.expanded_keys.clear();
                a.sim_widgets.clear();
                refresh_sim(&a);
                a.last_tree_sig = u64::MAX;
                let project_name = a.project_name.clone();
                a.log(format!("已新建工程: {project_name}"));
            }
            if let Some(ui) = uiw.upgrade() {
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
        ui.on_cycle_renderer(move || {
            let Some(ui) = uiw.upgrade() else { return };
            let old = ui.get_renderer_mode().to_string();
            let next = match ui.get_renderer_mode().as_str() {
                "auto" => "gpu",
                "gpu" => "cpu",
                _ => "auto",
            };
            let label = match next {
                "gpu" => "GPU",
                "cpu" => "CPU",
                _ => "Auto",
            };
            let confirmed = matches!(
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("切换渲染模式")
                    .set_description(format!(
                        "将切换到 {label} 渲染模式。\n\n该设置需要重启程序后生效，现在保存设置并自动重启吗？"
                    ))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show(),
                rfd::MessageDialogResult::Yes
            );
            if !confirmed {
                ui.set_renderer_mode(old.into());
                return;
            }
            ui.set_renderer_mode(next.into());
            let mut a = app.borrow_mut();
            settings::save(&gather_settings(&a, &ui));
            a.log(format!("渲染器已设为 {label}，正在重启程序"));
            if let Err(e) = restart_current_process() {
                a.log(format!("自动重启失败: {e}"));
            }
        });
    }
    {
        let uiw = ui.as_weak();
        let cw = chart_window.as_weak();
        let sw = signal_window.as_weak();
        let txw = tx_window.as_weak();
        let chw = channel_window.as_weak();
        let pw = playback_window.as_weak();
        let tgw = trigger_window.as_weak();
        let spw = sim_panel_window.as_weak();
        let ppw = sim_prop_window.as_weak();
        let cvw = convert_window.as_weak();
        ui.on_toggle_dark(move || {
            let dark = !uiw.unwrap().global::<Theme>().get_dark();
            uiw.unwrap().global::<Theme>().set_dark(dark);
            if let Some(w) = cw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = sw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = txw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = chw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = pw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = tgw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = spw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = ppw.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = cvw.upgrade() { w.global::<Theme>().set_dark(dark); }
        });
    }
    {
        let uiw = ui.as_weak();
        let cw = chart_window.as_weak();
        let sw = signal_window.as_weak();
        let txw = tx_window.as_weak();
        let chw = channel_window.as_weak();
        let pw = playback_window.as_weak();
        let tgw = trigger_window.as_weak();
        let spw = sim_panel_window.as_weak();
        let ppw = sim_prop_window.as_weak();
        let cvw = convert_window.as_weak();
        ui.on_toggle_big(move || {
            let big = !uiw.unwrap().global::<Theme>().get_big();
            uiw.unwrap().global::<Theme>().set_big(big);
            if let Some(w) = cw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = sw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = txw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = chw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = pw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = tgw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = spw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = ppw.upgrade() { w.global::<Theme>().set_big(big); }
            if let Some(w) = cvw.upgrade() { w.global::<Theme>().set_big(big); }
        });
    }
{
        let uiw = ui.as_weak();
        let cw = chart_window.as_weak();
        let sw = signal_window.as_weak();
        let txw = tx_window.as_weak();
        let udsw = uds_window.as_weak();
        let xcpw = xcp_window.as_weak();
        let chw = channel_window.as_weak();
        let pw = playback_window.as_weak();
        let tgw = trigger_window.as_weak();
        let spw = sim_panel_window.as_weak();
        let ppw = sim_prop_window.as_weak();
        let ccw = cache_window.as_weak();
        let cvw = convert_window.as_weak();
        let srw = script_runner_window.as_weak();
        let app = app.clone();
        ui.on_toggle_lang(move || {
            let en = !uiw.unwrap().global::<I18n>().get_en();
            {
                let mut a = app.borrow_mut();
                a.lang_en = en;
                a.last_tree_sig = u64::MAX;
            }
            uiw.unwrap().global::<I18n>().set_en(en);
            if let Some(w) = cw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = sw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = txw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = chw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = pw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = tgw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = spw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = ppw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = ccw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = cvw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = udsw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = xcpw.upgrade() { w.global::<I18n>().set_en(en); }
            if let Some(w) = srw.upgrade() { w.global::<I18n>().set_en(en); }
        });
    }
}
