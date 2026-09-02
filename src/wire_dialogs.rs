// Event-wiring for the wire_dialogs. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
fn wire_external_tools(app: Rc<std::cell::RefCell<App>>, ui: &AppWindow) {
    {
        let app = app.clone();
        ui.on_open_serial_tool(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|executable| executable.parent().map(|path| path.to_path_buf()));
            let mut launched = false;
            if let Some(dir) = dir.as_ref() {
                for name in ["serial-tool.exe", "xcharge-serial-tool.exe", "serial.exe"] {
                    let path = dir.join(name);
                    if path.exists() {
                        use std::os::windows::process::CommandExt;
                        launched = std::process::Command::new(&path)
                            .current_dir(dir)
                            .creation_flags(0x0800_0000)
                            .spawn()
                            .is_ok();
                        break;
                    }
                }
            }
            app.borrow_mut().log(if launched {
                "已启动串口工具".to_string()
            } else {
                "未找到串口工具 exe (serial-tool.exe)".to_string()
            });
        });
    }
    {
        let app = app.clone();
        ui.on_open_modbus_tool(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|executable| executable.parent().map(|path| path.to_path_buf()));
            let mut launched = false;
            if let Some(dir) = dir.as_ref() {
                let path = dir.join("modbus-tools.exe");
                if path.exists() {
                    use std::os::windows::process::CommandExt;
                    launched = std::process::Command::new(&path)
                        .current_dir(dir)
                        .creation_flags(0x0800_0000)
                        .spawn()
                        .is_ok();
                }
            }
            app.borrow_mut().log(if launched {
                "已启动 Modbus 工具".to_string()
            } else {
                "未找到 Modbus 工具 exe (modbus-tools.exe)".to_string()
            });
        });
    }
}

#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_dialogs(
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
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_pcan_rescan(move || {
            let mut a = app.borrow_mut();
            scan_attached_hardware(&mut a);
            if let Some(chw) = chw.upgrade() {
                refresh_channel_window_lists(&chw, &a);
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_pcan_select(move |i| {
            let mut a = app.borrow_mut();
            if i < 0 {
                return;
            }
            if a.last_hardware_scan.is_none() {
                scan_attached_hardware(&mut a);
            }
            ensure_channel_edit_session(&mut a);
            let devices = a.pcan_devices.clone();
            let zcan_devices = a.zcan_devices.clone();
            let idx = i as usize;
            if let Some(hw) = devices.get(idx) {
                let stable_id = pcan_hardware_id(hw);
                let session = a.channel_edit.as_mut().expect("channel edit session");
                if let Some(existing) = session.channels.iter().position(|c| {
                    (!c.hardware_id.is_empty() && c.hardware_id == stable_id)
                        || (c.device_type.eq_ignore_ascii_case("PCAN")
                            && c.channel_index == hw.channel_index)
                }) {
                    session.selected = existing as i32;
                } else {
                    let mut cfg = default_channel();
                    cfg.device_type = "PCAN".to_string();
                    cfg.hardware_label = hw.device_name.clone();
                    cfg.hardware_id = stable_id;
                    cfg.device_index = 0;
                    cfg.channel_index = hw.channel_index;
                    cfg.is_fd = hw.fd_capable;
                    if hw.fd_capable {
                        cfg.data_baud = "2M".to_string();
                    }
                    session.channels.push(cfg);
                    renumber_channel_slice(&mut session.channels);
                    session.selected = session.channels.len() as i32 - 1;
                    session.dirty = true;
                }
                let selected = session.selected;
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(selected);
                    refresh_channel_window_lists(&chw, &a);
                    if let Some(c) = channel_configs(&a).get(selected as usize) {
                        set_chan_form(&chw, c, &a);
                    }
                }
            } else if let Some(hw) = idx
                .checked_sub(devices.len())
                .and_then(|zcan_index| zcan_devices.get(zcan_index))
            {
                let stable_id = zcan_hardware_id(hw);
                let session = a.channel_edit.as_mut().expect("channel edit session");
                if let Some(existing) = session.channels.iter().position(|c| {
                    (!c.hardware_id.is_empty() && c.hardware_id == stable_id)
                        || (c.device_type.eq_ignore_ascii_case(&hw.device_type)
                            && c.device_index == hw.device_index
                            && c.channel_index == hw.channel_index)
                }) {
                    session.selected = existing as i32;
                } else {
                    let mut cfg = default_channel();
                    cfg.device_type = hw.device_type.clone();
                    cfg.hardware_label = hw.hardware_label.clone();
                    cfg.hardware_id = stable_id;
                    cfg.device_index = hw.device_index;
                    cfg.channel_index = hw.channel_index;
                    cfg.is_fd = hw.fd_capable;
                    cfg.data_baud = "2M".to_string();
                    session.channels.push(cfg);
                    renumber_channel_slice(&mut session.channels);
                    session.selected = session.channels.len() as i32 - 1;
                    session.dirty = true;
                }
                let selected = session.selected;
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(selected);
                    refresh_channel_window_lists(&chw, &a);
                    if let Some(c) = channel_configs(&a).get(selected as usize) {
                        set_chan_form(&chw, c, &a);
                    }
                }
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_add(move || {
            let mut a = app.borrow_mut();
            ensure_channel_edit_session(&mut a);
            let manual_label = if a.lang_en { "Manual channel" } else { "手动通道" };
            let session = a.channel_edit.as_mut().expect("channel edit session");
            let mut cfg = session.channels.last().cloned().unwrap_or_else(default_channel);
            cfg.channel_index += 1;
            cfg.hardware_id.clear();
            cfg.hardware_label = manual_label.into();
            session.channels.push(cfg);
            renumber_channel_slice(&mut session.channels);
            session.selected = session.channels.len() as i32 - 1;
            session.dirty = true;
            let selected = session.selected;
            if let Some(chw) = chw.upgrade() {
                chw.set_chan_sel(selected);
                refresh_channel_window_lists(&chw, &a);
                if let Some(c) = channel_configs(&a).get(selected as usize) {
                    set_chan_form(&chw, c, &a);
                }
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_clone(move || {
            let mut a = app.borrow_mut();
            ensure_channel_edit_session(&mut a);
            let session = a.channel_edit.as_mut().expect("channel edit session");
            let sel = session.selected;
            if let Some(mut c) = session.channels.get(sel as usize).cloned() {
                c.hardware_id.clear();
                c.channel_index = c.channel_index.saturating_add(1);
                session.channels.push(c);
                renumber_channel_slice(&mut session.channels);
                session.selected = session.channels.len() as i32 - 1;
                session.dirty = true;
                let selected = session.selected;
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(selected);
                    refresh_channel_window_lists(&chw, &a);
                    if let Some(c) = channel_configs(&a).get(selected as usize) {
                        set_chan_form(&chw, c, &a);
                    }
                }
                a.log("Channel cloned as manual mapping; check the hardware channel index");
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_remove(move |i| {
            let mut a = app.borrow_mut();
            ensure_channel_edit_session(&mut a);
            let i = i as usize;
            let session = a.channel_edit.as_mut().expect("channel edit session");
            if i < session.channels.len() {
                session.channels.remove(i);
                renumber_channel_slice(&mut session.channels);
                session.selected = if session.channels.is_empty() {
                    0
                } else {
                    (session.selected.min(session.channels.len() as i32 - 1)).max(0)
                };
                session.dirty = true;
                let selected = session.selected;
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(selected);
                    refresh_channel_window_lists(&chw, &a);
                    if let Some(c) = channel_configs(&a).get(selected as usize) {
                        set_chan_form(&chw, c, &a);
                    } else {
                        set_chan_form(&chw, &default_channel(), &a);
                    }
                }
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_select(move |i| {
            let mut a = app.borrow_mut();
            ensure_channel_edit_session(&mut a);
            if let Some(session) = a.channel_edit.as_mut() {
                session.selected = i;
            }
            if let Some(chw) = chw.upgrade()
                && let Some(c) = channel_configs(&a).get(i as usize)
            {
                set_chan_form(&chw, c, &a);
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_edit(move |field, value| {
            let mut a = app.borrow_mut();
            ensure_channel_edit_session(&mut a);
            let session = a.channel_edit.as_mut().expect("channel edit session");
            let sel = session.selected;
            if let Some(c) = session.channels.get_mut(sel as usize) {
                match field.as_str() {
                    "device_type" => {
                        c.device_type = value.to_string();
                        c.hardware_id.clear();
                        if !c.device_type.to_ascii_uppercase().contains("CANFD") {
                            c.is_fd = false;
                            c.fd_non_iso = false;
                            c.termination = false;
                        }
                    }
                    "hardware_label" => c.hardware_label = value.to_string(),
                    "manual_mode" => {
                        if value == "1" {
                            c.hardware_id.clear();
                        }
                    }
                    "device_index" => {
                        c.device_index = value.trim().parse().unwrap_or(c.device_index)
                    }
                    "channel_index" => {
                        c.channel_index = value
                            .trim()
                            .parse::<u32>()
                            .ok()
                            .filter(|index| *index > 0)
                            .map(|index| index - 1)
                            .unwrap_or(c.channel_index)
                    }
                    "baud" => c.baud = value.to_string(),
                    "data_baud" => c.data_baud = value.to_string(),
                    "custom_bitrate" => c.custom_bitrate = value.to_string(),
                    "is_fd" => c.is_fd = value == "1",
                    "termination" => c.termination = value == "1",
                    "listen_only" => c.listen_only = value == "1",
                    "fd_non_iso" => c.fd_non_iso = value == "1",
                    "net_server" => c.net_server = value == "1",
                    "ip" => c.ip = value.to_string(),
                    "port" => c.port = value.to_string(),
                    _ => {}
                }
                if !c.is_fd {
                    c.fd_non_iso = false;
                    c.custom_bitrate.clear();
                }
                session.dirty = true;
            }
            if matches!(
                field.as_str(),
                "device_type"
                    | "hardware_label"
                    | "device_index"
                    | "channel_index"
                    | "baud"
                    | "data_baud"
                    | "custom_bitrate"
                    | "is_fd"
                    | "termination"
                    | "listen_only"
                    | "fd_non_iso"
                    | "manual_mode"
                    | "ip"
                    | "port"
            ) && let Some(chw) = chw.upgrade()
            {
                chw.set_validation_message("".into());
                refresh_channel_window_lists(&chw, &a);
                if let Some(c) = channel_configs(&a).get(sel as usize) {
                    set_chan_form(&chw, c, &a);
                }
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        let uiw = ui.as_weak();
        channel_window.on_save_all(move || {
            let mut a = app.borrow_mut();
            let Some(ui) = uiw.upgrade() else { return };
            let count = match commit_channel_edit(&mut a) {
                Ok(count) => count,
                Err(error) => {
                a.log(format!("Channel configuration invalid: {error}"));
                if let Some(chw) = chw.upgrade() {
                    chw.set_validation_is_error(true);
                    chw.set_validation_message(error.into());
                }
                return;
                }
            };
            persist_project_if_open(&mut a, &ui);
            a.log(format!("Saved {count} channel configuration(s)"));
            if let Some(chw) = chw.upgrade() {
                chw.set_validation_is_error(false);
                chw.set_validation_message(
                    if a.lang_en { "Configuration saved" } else { "配置已保存" }.into(),
                );
                refresh_channel_window_lists(&chw, &a);
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        let uiw = ui.as_weak();
        channel_window.on_connect_all(move || {
            let mut a = app.borrow_mut();
            let Some(ui) = uiw.upgrade() else { return };
            refresh_and_reconcile_pcan(&mut a);
            if let Err(error) = commit_channel_edit(&mut a) {
                a.log(format!("Channel configuration invalid: {error}"));
                if let Some(chw) = chw.upgrade() {
                    chw.set_validation_is_error(true);
                    chw.set_validation_message(error.into());
                }
                return;
            }
            persist_project_if_open(&mut a, &ui);
            a.channel_connect_pending = true;
            a.channel_connect_expected = a.channels.len();
            if a.cmd.send(Cmd::ConnectChannels(a.channels.clone())).is_err() {
                a.channel_connect_pending = false;
                a.channel_connect_expected = 0;
                if let Some(chw) = chw.upgrade() {
                    chw.set_connecting(false);
                    chw.set_validation_is_error(true);
                    chw.set_validation_message(
                        if a.lang_en { "CAN backend has stopped" } else { "CAN 后台线程已退出" }.into(),
                    );
                }
                return;
            }
            if let Some(chw) = chw.upgrade() {
                chw.set_connecting(true);
                chw.set_validation_is_error(false);
                chw.set_validation_message(
                    if a.lang_en { "Connecting all channels..." } else { "正在连接全部通道..." }.into(),
                );
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_cancel(move || {
            let mut a = app.borrow_mut();
            if a.channel_connect_pending {
                return;
            }
            a.channel_edit = None;
            if let Some(chw) = chw.upgrade() {
                let _ = chw.hide();
            }
        });
    }
    // ---- Record file conversion window ----
    {
        let cvw = convert_window.as_weak();
        convert_window.on_pick_src_file(move || {
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("记录文件 (CSV/ASC/BLF)", &["csv", "asc", "blf"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(file) = dlg.pick_file().await else {
                    return;
                };
                let p = file.path().to_path_buf();
                w.set_src_file(p.to_string_lossy().to_string().into());
                w.set_status1("".into());
            });
        });
    }
    {
        let app = app.clone();
        let cvw = convert_window.as_weak();
        convert_window.on_do_convert(move || {
            let Some(w) = cvw.upgrade() else { return };
            let en = app.borrow().lang_en;
            let src = w.get_src_file().to_string();
            if src.is_empty() {
                w.set_status1(if en {
                    "Select a source file first".into()
                } else {
                    "请先选择源文件".into()
                });
                return;
            }
            let fmt = convert::LogFmt::from_index(w.get_fmt1());
            let stem = std::path::Path::new(&src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("out")
                .to_string();
            let worker = app.borrow().worker_tx.clone();
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg =
                    rfd::AsyncFileDialog::new().set_file_name(format!("{stem}.{}", fmt.ext()));
                if let Some(dir) = std::path::Path::new(&src).parent() {
                    dlg = dlg.set_directory(dir);
                }
                dlg = dlg
                    .add_filter(fmt.ext(), &[fmt.ext()])
                    .set_parent(&w.window().window_handle());
                let Some(file) = dlg.save_file().await else {
                    return;
                };
                let dst = file.path().to_path_buf();
                w.set_status1(if en {
                    "Converting...".into()
                } else {
                    "正在转换...".into()
                });
                std::thread::spawn(move || {
                    let status = match convert::convert(&src, &dst.to_string_lossy(), fmt) {
                        Ok(count) if en => format!("Done: {count} frames → {}", dst.display()),
                        Ok(count) => format!("转换完成：{count} 帧 → {}", dst.display()),
                        Err(error) if en => format!("Failed: {error}"),
                        Err(error) => format!("转换失败：{error}"),
                    };
                    let _ = worker.send(WorkerEvent::ConversionFinished {
                        batch: false,
                        log: status.clone(),
                        status,
                    });
                });
            });
        });
    }
    {
        let cvw = convert_window.as_weak();
        convert_window.on_pick_src_dir(move || {
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new();
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(folder) = dlg.pick_folder().await else {
                    return;
                };
                w.set_src_dir(folder.path().to_string_lossy().to_string().into());
            });
        });
    }
    {
        let cvw = convert_window.as_weak();
        convert_window.on_pick_out_dir(move || {
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new();
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(folder) = dlg.pick_folder().await else {
                    return;
                };
                w.set_out_dir(folder.path().to_string_lossy().to_string().into());
            });
        });
    }
    {
        let app = app.clone();
        let cvw = convert_window.as_weak();
        convert_window.on_do_batch(move || {
            let Some(w) = cvw.upgrade() else { return };
            let en = app.borrow().lang_en;
            let sdir = w.get_src_dir().to_string();
            let odir = w.get_out_dir().to_string();
            if sdir.is_empty() || odir.is_empty() {
                w.set_status2(if en {
                    "Select source and target folders first".into()
                } else {
                    "请先选择源目录和目标目录".into()
                });
                return;
            }
            let fmt = convert::LogFmt::from_index(w.get_fmt2());
            w.set_status2(if en {
                "Converting...".into()
            } else {
                "正在批量转换...".into()
            });
            let worker = app.borrow().worker_tx.clone();
            std::thread::spawn(move || {
                let (ok, fail, lines) = convert::convert_dir(&sdir, &odir, fmt);
                let status = if en {
                    format!("Batch done: {ok} ok, {fail} failed\n{}", lines.join("\n"))
                } else {
                    format!("批量转换完成：成功 {ok}，失败 {fail}\n{}", lines.join("\n"))
                };
                let _ = worker.send(WorkerEvent::ConversionFinished {
                    batch: true,
                    status,
                    log: format!("批量转换：成功 {ok}，失败 {fail}"),
                });
            });
        });
    }
    {
        let app = app.clone();
        let cw = cache_window.as_weak();
        cache_window.on_apply_cache(move |trace_s, chart_s| {
            let mut a = app.borrow_mut();
            let tc = trace_s
                .trim()
                .parse::<usize>()
                .unwrap_or(a.trace_cap)
                .clamp(1_000, 5_000_000);
            let cc = chart_s
                .trim()
                .parse::<usize>()
                .unwrap_or(a.chart_cap)
                .clamp(500, 1_000_000);
            a.trace_cap = tc;
            a.chart_cap = cc;
            while a.trace.len() > tc {
                a.trace.pop_front();
            }
            for s in a.series.iter_mut() {
                while s.samples.len() > cc {
                    s.samples.pop_front();
                }
            }
            a.log(format!("缓存上限已更新: 报文 {tc} 帧 / 曲线 {cc} 点"));
            if let Some(w) = cw.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let cw = cache_window.as_weak();
        cache_window.on_cancel(move || {
            if let Some(w) = cw.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let weak = trigger_window.as_weak();
        trigger_window.on_hex_editor_show(move |_field, value, max_len| {
            let Some(w) = weak.upgrade() else { return };
            let max_len = max_len.max(1) as usize;
            let bytes = parse_tx_bytes(&value, max_len);
            let len = if bytes.is_empty() { max_len.min(8) } else { bytes.len() };
            w.set_hex_editor_title(format!("触发器发送数据 · {len}/{max_len} 字节").into());
            w.set_hex_editor_length(len as i32);
            w.set_hex_editor_rows(build_feature_hex_rows(&bytes, len));
            w.set_hex_editor_paste("".into());
            w.set_hex_editor_error("".into());
            w.set_hex_editor_open(true);
        });
    }
    {
        let weak = trigger_window.as_weak();
        trigger_window.on_hex_editor_byte_edited(move |index, value| {
            let Some(w) = weak.upgrade() else { return };
            edit_feature_hex_byte(&w.get_hex_editor_rows(), index as usize, &value);
            w.set_hex_editor_error("".into());
        });
    }
    {
        let weak = trigger_window.as_weak();
        trigger_window.on_hex_editor_fill(move |value| {
            let Some(w) = weak.upgrade() else { return };
            let max_len = if w.get_send_fd() { 64 } else { 8 };
            if let Some((rows, len)) = fill_feature_hex_rows(&value, max_len) {
                w.set_hex_editor_rows(rows);
                w.set_hex_editor_length(len as i32);
                w.set_hex_editor_error("".into());
            } else {
                w.set_hex_editor_error("没有识别到有效的十六进制字节".into());
            }
        });
    }
    {
        let weak = trigger_window.as_weak();
        trigger_window.on_hex_editor_apply(move || {
            let Some(w) = weak.upgrade() else { return false };
            let data = match collect_feature_hex_rows(&w.get_hex_editor_rows(), w.get_hex_editor_length().max(0) as usize) {
                Ok(data) => data,
                Err(index) => {
                    w.set_hex_editor_error(format!("字节 {index:02X} 必须是两位十六进制数").into());
                    return false;
                }
            };
            w.set_send_data(data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ").into());
            true
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let tgw = trigger_window.as_weak();
        trigger_window.on_trig_apply(move || {
            let Some(w) = tgw.upgrade() else { return };
            let cond = match w.get_trig_cond() {
                0 => {
                    let s = w.get_trig_id();
                    let id = u32::from_str_radix(
                        s.trim().trim_start_matches("0x").trim_start_matches("0X"),
                        16,
                    )
                    .unwrap_or(0);
                    TrigCond::IdEquals(id)
                }
                1 => {
                    let off = w.get_trig_off().trim().parse::<usize>().unwrap_or(0);
                    let val = u8::from_str_radix(
                        w.get_trig_val()
                            .trim()
                            .trim_start_matches("0x")
                            .trim_start_matches("0X"),
                        16,
                    )
                    .unwrap_or(0);
                    TrigCond::ByteEquals { off, val }
                }
                _ => TrigCond::ErrorFrame,
            };
            let action = match w.get_trig_action() {
                1 => TrigAction::StartRecord,
                2 => TrigAction::StopRecord,
                3 => TrigAction::SendFrame,
                _ => TrigAction::Alarm,
            };
            let send_id = u32::from_str_radix(
                w.get_send_id()
                    .trim()
                    .trim_start_matches("0x")
                    .trim_start_matches("0X"),
                16,
            )
            .unwrap_or(0);
            let send_ext = w.get_send_ext();
            let send_fd = w.get_send_fd();
            let send_data = parse_tx_bytes(&w.get_send_data(), if send_fd { 64 } else { 8 });
            let desc = match &cond {
                TrigCond::IdEquals(id) => format!("ID=0x{id:X}"),
                TrigCond::ByteEquals { off, val } => format!("数据[{off}]=0x{val:02X}"),
                TrigCond::ErrorFrame => "错误帧".to_string(),
            };
            let act_desc = match action {
                TrigAction::Alarm => "报警".to_string(),
                TrigAction::StartRecord => "开始记录".to_string(),
                TrigAction::StopRecord => "停止记录".to_string(),
                TrigAction::SendFrame => format!("发送报文 0x{send_id:X}"),
            };
            {
                let mut a = app.borrow_mut();
                a.trigger = Some(Trigger {
                    cond,
                    action,
                    last: None,
                    send_ch: 1,
                    send_id,
                    send_ext,
                    send_fd,
                    send_data,
                });
                a.log(format!("触发器已布防: 当 {desc} 时 → {act_desc}"));
            }
            w.set_armed(true);
            if let Some(u) = uiw.upgrade() {
                u.set_trigger_armed(true);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let tgw = trigger_window.as_weak();
        trigger_window.on_trig_disarm(move || {
            {
                let mut a = app.borrow_mut();
                if a.trigger.take().is_some() {
                    a.log("触发器已撤防".to_string());
                }
                a.trig_stop_record();
            }
            if let Some(w) = tgw.upgrade() {
                w.set_armed(false);
            }
            if let Some(u) = uiw.upgrade() {
                u.set_trigger_armed(false);
            }
        });
    }
}
