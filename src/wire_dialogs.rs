// Event-wiring for the wire_dialogs. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
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
        channel_window.on_chan_add(move || {
            let mut a = app.borrow_mut();
            let mut cfg = a.channels.last().cloned().unwrap_or_else(default_channel);
            cfg.channel_index += 1;
            a.channels.push(cfg);
            renumber_channels(&mut a);
            a.channel_sel = a.channels.len() as i32 - 1;
            if let Some(chw) = chw.upgrade() {
                chw.set_chan_sel(a.channel_sel);
                chw.set_channels(ModelRc::from(Rc::new(VecModel::from(chan_list_strings(&a)))));
                if let Some(c) = a.channels.get(a.channel_sel as usize) {
                    set_chan_form(&chw, c);
                }
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_clone(move || {
            let mut a = app.borrow_mut();
            let sel = a.channel_sel;
            if let Some(c) = a.channels.get(sel as usize).cloned() {
                a.channels.push(c);
                renumber_channels(&mut a);
                a.channel_sel = a.channels.len() as i32 - 1;
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(a.channel_sel);
                    if let Some(c) = a.channels.get(a.channel_sel as usize) {
                        set_chan_form(&chw, c);
                    }
                }
                a.log("已克隆通道（请修改硬件通道索引）");
            }
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_chan_remove(move |i| {
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i < a.channels.len() && a.channels.len() > 1 {
                a.channels.remove(i);
                renumber_channels(&mut a);
                a.channel_sel = (a.channel_sel.min(a.channels.len() as i32 - 1)).max(0);
                if let Some(chw) = chw.upgrade() {
                    chw.set_chan_sel(a.channel_sel);
                    chw.set_channels(ModelRc::from(Rc::new(VecModel::from(chan_list_strings(&a)))));
                    if let Some(c) = a.channels.get(a.channel_sel as usize) {
                        set_chan_form(&chw, c);
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
            a.channel_sel = i;
            if let Some(chw) = chw.upgrade()
                && let Some(c) = a.channels.get(i as usize) {
                    set_chan_form(&chw, c);
                }
        });
    }
    {
        let app = app.clone();
        channel_window.on_chan_edit(move |field, value| {
            let mut a = app.borrow_mut();
            let sel = a.channel_sel;
            if let Some(c) = a.channels.get_mut(sel as usize) {
                match field.as_str() {
                    "device_type" => c.device_type = value.to_string(),
                    "device_index" => c.device_index = value.trim().parse().unwrap_or(c.device_index),
                    "channel_index" => c.channel_index = value.trim().parse().unwrap_or(c.channel_index),
                    "baud" => c.baud = value.to_string(),
                    "data_baud" => c.data_baud = value.to_string(),
                    "is_fd" => c.is_fd = value == "1",
                    "termination" => c.termination = value == "1",
                    "net_server" => c.net_server = value == "1",
                    "ip" => c.ip = value.to_string(),
                    "port" => c.port = value.to_string(),
                    _ => {}
                }
            }
        });
    }
    {
        let app = app.clone();
        channel_window.on_save_all(move || {
            let mut a = app.borrow_mut();
            if let Some(c) = a.channels.first().cloned() {
                a.baud = c.baud.clone();
                a.device_cfg = c;
            }
            let n = a.channels.len();
            a.log(format!("已保存 {n} 个通道配置"));
        });
    }
    {
        let app = app.clone();
        let chw = channel_window.as_weak();
        channel_window.on_connect_all(move || {
            let mut a = app.borrow_mut();
            if let Some(c) = a.channels.first().cloned() {
                a.baud = c.baud.clone();
                a.device_cfg = c;
            }
            let _ = a.cmd.send(Cmd::ConnectChannels(a.channels.clone()));
            if let Some(chw) = chw.upgrade() {
                let _ = chw.hide();
            }
        });
    }
    {
        let chw = channel_window.as_weak();
        channel_window.on_cancel(move || {
            if let Some(chw) = chw.upgrade() {
                let _ = chw.hide();
            }
        });
    }
    {
        let pw = playback_window.as_weak();
        ui.on_open_playback_window(move || {
            if let Some(w) = pw.upgrade() {
                let _ = w.show();
            }
        });
    }
    // ---- Record file conversion window ----
    {
        let cvw = convert_window.as_weak();
        ui.on_open_convert_window(move || {
            if let Some(w) = cvw.upgrade() {
                let _ = w.show();
            }
        });
    }
    {
        let cvw = convert_window.as_weak();
        convert_window.on_pick_src_file(move || {
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("记录文件 (CSV/ASC/BLF)", &["csv", "asc", "blf"]);
                dlg = dlg.set_parent(&w.window().window_handle());
                let Some(file) = dlg.pick_file().await else { return };
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
            let w = cvw.unwrap();
            let en = app.borrow().lang_en;
            let src = w.get_src_file().to_string();
            if src.is_empty() {
                w.set_status1(if en { "Select a source file first".into() } else { "请先选择源文件".into() });
                return;
            }
            let fmt = convert::LogFmt::from_index(w.get_fmt1());
            let stem = std::path::Path::new(&src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("out")
                .to_string();
            let app = app.clone();
            let cvw = cvw.clone();
            let _ = slint::spawn_local(async move {
                let Some(w) = cvw.upgrade() else { return };
                let mut dlg = rfd::AsyncFileDialog::new().set_file_name(format!("{stem}.{}", fmt.ext()));
                if let Some(dir) = std::path::Path::new(&src).parent() {
                    dlg = dlg.set_directory(dir);
                }
                dlg = dlg.add_filter(fmt.ext(), &[fmt.ext()]).set_parent(&w.window().window_handle());
                let Some(file) = dlg.save_file().await else { return };
                let dst = file.path().to_path_buf();
                match convert::convert(&src, &dst.to_string_lossy(), fmt) {
                    Ok(n) => {
                        let msg = if en { format!("Done: {n} frames → {}", dst.display()) } else { format!("转换完成：{n} 帧 → {}", dst.display()) };
                        w.set_status1(msg.clone().into());
                        app.borrow_mut().log(msg);
                    }
                    Err(e) => {
                        let msg = if en { format!("Failed: {e}") } else { format!("转换失败：{e}") };
                        w.set_status1(msg.clone().into());
                        app.borrow_mut().log(msg);
                    }
                }
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
                let Some(folder) = dlg.pick_folder().await else { return };
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
                let Some(folder) = dlg.pick_folder().await else { return };
                w.set_out_dir(folder.path().to_string_lossy().to_string().into());
            });
        });
    }
    {
        let app = app.clone();
        let cvw = convert_window.as_weak();
        convert_window.on_do_batch(move || {
            let w = cvw.unwrap();
            let en = app.borrow().lang_en;
            let sdir = w.get_src_dir().to_string();
            let odir = w.get_out_dir().to_string();
            if sdir.is_empty() || odir.is_empty() {
                w.set_status2(if en { "Select source and target folders first".into() } else { "请先选择源目录和目标目录".into() });
                return;
            }
            let fmt = convert::LogFmt::from_index(w.get_fmt2());
            let (ok, fail, lines) = convert::convert_dir(&sdir, &odir, fmt);
            let text = if en { format!("Batch done: {ok} ok, {fail} failed\n{}", lines.join("\n")) } else { format!("批量转换完成：成功 {ok}，失败 {fail}\n{}", lines.join("\n")) };
            w.set_status2(text.into());
            app.borrow_mut().log(format!("批量转换：成功 {ok}，失败 {fail}"));
        });
    }
    {
        let app = app.clone();
        let cw = cache_window.as_weak();
        ui.on_open_cache_config(move || {
            if let Some(w) = cw.upgrade() {
                let a = app.borrow();
                w.set_trace_cap(a.trace_cap.to_string().into());
                w.set_chart_cap(a.chart_cap.to_string().into());
                let _ = w.show();
            }
        });
    }
    {
        let app = app.clone();
        let cw = cache_window.as_weak();
        cache_window.on_apply_cache(move |trace_s, chart_s| {
            let mut a = app.borrow_mut();
            let tc = trace_s.trim().parse::<usize>().unwrap_or(a.trace_cap).clamp(1_000, 5_000_000);
            let cc = chart_s.trim().parse::<usize>().unwrap_or(a.chart_cap).clamp(500, 1_000_000);
            a.trace_cap = tc;
            a.chart_cap = cc;
            // 立即按新上限裁剪现有缓存
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
    // 触发器窗口：打开
    {
        let app = app.clone();
        let tgw = trigger_window.as_weak();
        ui.on_open_trigger_window(move || {
            if let Some(w) = tgw.upgrade() {
                w.set_armed(app.borrow().trigger.is_some());
                let _ = w.show();
            }
        });
    }
    // 仿真面板：打开
    {
        let app = app.clone();
        let spw = sim_panel_window.as_weak();
        ui.on_open_sim_panel_window(move || {
            if let Some(w) = spw.upgrade() {
                w.set_running(app.borrow().sim_running);
                let _ = w.show();
            }
        });
    }
    // 串口工具：作为独立 exe 启动(同目录的 serial-tool.exe / 旧名兼容)
    {
        let app = app.clone();
        ui.on_open_serial_tool(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.to_path_buf()));
            let mut launched = false;
            if let Some(dir) = dir.as_ref() {
                for name in ["serial-tool.exe", "xcharge-serial-tool.exe", "serial.exe"] {
                    let p = dir.join(name);
                    if p.exists() {
                        use std::os::windows::process::CommandExt;
                        launched = std::process::Command::new(&p)
                            .current_dir(dir)
                            // CREATE_NO_WINDOW：启动子进程时不弹出控制台命令行窗口
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
    // Modbus 工具：作为独立 exe 启动(同目录的 modbus-tools.exe)，与串口工具同一模式
    {
        let app = app.clone();
        ui.on_open_modbus_tool(move || {
            let dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.to_path_buf()));
            let mut launched = false;
            if let Some(dir) = dir.as_ref() {
                let p = dir.join("modbus-tools.exe");
                if p.exists() {
                    use std::os::windows::process::CommandExt;
                    launched = std::process::Command::new(&p)
                        .current_dir(dir)
                        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：不弹控制台窗口
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
    // 触发器：布防/应用
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let tgw = trigger_window.as_weak();
        trigger_window.on_trig_apply(move || {
            let Some(w) = tgw.upgrade() else { return };
            let cond = match w.get_trig_cond() {
                0 => {
                    let s = w.get_trig_id();
                    let id = u32::from_str_radix(s.trim().trim_start_matches("0x").trim_start_matches("0X"), 16)
                        .unwrap_or(0);
                    TrigCond::IdEquals(id)
                }
                1 => {
                    let off = w.get_trig_off().trim().parse::<usize>().unwrap_or(0);
                    let val = u8::from_str_radix(
                        w.get_trig_val().trim().trim_start_matches("0x").trim_start_matches("0X"),
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
            // 仅 SendFrame 用到的响应帧参数
            let send_id = u32::from_str_radix(
                w.get_send_id().trim().trim_start_matches("0x").trim_start_matches("0X"),
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
    // 触发器：撤防
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
