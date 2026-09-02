// Event-wiring for the wire_tx. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_tx(
    app: Rc<std::cell::RefCell<App>>,
    ui: &AppWindow,
    tx_window: &TxWindow,
) {
    tx_window
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    {
        let app = app.clone();
        tx_window.on_tx_add(move || {
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
        let txw = tx_window.as_weak();
        let app = app.clone();
        tx_window.on_tx_file_send(move || {
            if !app.borrow_mut().license_allows("can-transmit") { return; }
            let Some(w) = txw.upgrade() else { return };
            let en = app.borrow().lang_en;
            let path = w.get_tx_file_path().to_string();
            if path.trim().is_empty() {
                w.set_tx_file_status(if en { "Select a file first".into() } else { "请选择文件".into() });
                return;
            }
            let id = parse_u32(&w.get_tx_file_id()).unwrap_or(0);
            let channel = ch_from_name(&w.get_tx_file_ch());
            let mode = w.get_tx_file_mode();
            let repeat = w.get_tx_file_repeat().trim().parse::<u32>().unwrap_or(1).max(1);
            let max_len = w
                .get_tx_file_len()
                .trim()
                .parse::<usize>()
                .unwrap_or(8)
                .clamp(1, 64);
            let address = parse_u32(&w.get_tx_file_addr()).unwrap_or(0);
            let crc = parse_u32(&w.get_tx_file_crc()).unwrap_or(0) as u16;
            let worker = app.borrow().worker_tx.clone();
            w.set_tx_file_progress(0.0);
            w.set_tx_file_status(if en {
                "Preparing file...".into()
            } else {
                "正在解析文件...".into()
            });
            std::thread::spawn(move || {
                let result = if mode == 1 || mode == 2 {
                    build_ota_job(&path, channel, id, mode, address, crc, en)
                        .map(TxFilePayload::Ota)
                } else {
                    build_normal_file_frames(&path, channel, id, max_len, en)
                        .map(TxFilePayload::Frames)
                };
                let _ = worker.send(WorkerEvent::TxFilePrepared {
                    path,
                    repeat,
                    english: en,
                    result,
                });
            });
        });
    }
    {
        let app = app.clone();
        tx_window.on_tx_remove(move |i| {
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
        tx_window.on_tx_send_once(move |i| {
            let mut a = app.borrow_mut();
            if !a.license_allows("can-transmit") { return; }
            if i >= 0 && (i as usize) < a.txs.len() {
                let f = tx_frame(&a.txs[i as usize]);
                let _ = a.cmd.send(Cmd::SendOnce(f));
                a.txs[i as usize].sent += 1;
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_toggle_periodic(move |i| {
            let mut a = app.borrow_mut();
            if i >= 0 && (i as usize) < a.txs.len() {
                let idx = i as usize;
                if !a.txs[idx].periodic && !a.license_allows("can-transmit") { return; }
                a.txs[idx].periodic = !a.txs[idx].periodic;
                toggle_task_periodic(&mut a, idx);
                // instant feedback: refresh the list now instead of waiting for the 100ms tick
                if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                    push_tx_list(&mut a, &u, &w);
                }
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_all(move |start| {
            let mut a = app.borrow_mut();
            if start && !a.license_allows("can-transmit") { return; }
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
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    // 应用「次数/间隔」到全部任务
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_apply_all(move |i, field| {
            let mut a = app.borrow_mut();
            let i = i as usize;
            if i >= a.txs.len() {
                return;
            }
            let rep = a.txs[i].repeat;
            let per = a.txs[i].period_ms;
            for t in a.txs.iter_mut() {
                match field.as_str() {
                    "repeat" => t.repeat = rep,
                    "period" => t.period_ms = per,
                    _ => {}
                }
            }
            // 重新下发正在运行的非变化任务（变化任务由 app 循环按新值步进）
            for idx in 0..a.txs.len() {
                if a.txs[idx].periodic {
                    configure_task_periodic(&mut a, idx);
                }
            }
            a.log(format!(
                "已把第 {} 行的{}套用到全部任务",
                i + 1,
                if field == "repeat" { "发送次数" } else { "间隔" }
            ));
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    // 设置发送速度倍率
    {
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_speed_set(move |s| {
            let mut a = app.borrow_mut();
            let sp = s
                .trim()
                .trim_end_matches('×')
                .trim_end_matches('x')
                .parse::<f64>()
                .unwrap_or(1.0);
            a.tx_speed = if sp <= 0.0 { 1.0 } else { sp };
            let speed = a.tx_speed;
            // 重新下发正在运行的非变化任务以应用新间隔
            for idx in 0..a.txs.len() {
                if a.txs[idx].periodic {
                    configure_task_periodic(&mut a, idx);
                }
            }
            a.log(format!("发送速度倍率: {}×", speed));
            if let Some(w) = txw2.upgrade() {
                w.set_tx_speed(s);
            }
        });
    }
    {
        let app = app.clone();
        tx_window.on_tx_clear(move || {
            let mut a = app.borrow_mut();
            // 停掉所有周期任务再清空
            let tasks = a.txs.clone();
            for task in &tasks {
                stop_task_periodic(&a, task);
            }
            a.txs.clear();
            a.tx_checked.clear();
            a.log("已清空发送列表".to_string());
        });
    }
    // ---- 列表勾选 / 上移下移 / 批量删除 ----
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_toggle_check(move |i| {
            let mut a = app.borrow_mut();
            if let Some(t) = a.txs.get(i as usize) {
                let h = t.handle;
                if !a.tx_checked.remove(&h) {
                    a.tx_checked.insert(h);
                }
            }
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_select_all(move || {
            let mut a = app.borrow_mut();
            let handles: Vec<u64> = a.txs.iter().map(|t| t.handle).collect();
            // 全选按钮兼作切换：已全选则取消，否则全选
            let all = !handles.is_empty() && handles.iter().all(|h| a.tx_checked.contains(h));
            a.tx_checked.clear();
            if !all {
                a.tx_checked.extend(handles);
            }
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_invert(move || {
            let mut a = app.borrow_mut();
            let handles: Vec<u64> = a.txs.iter().map(|t| t.handle).collect();
            for h in handles {
                if !a.tx_checked.remove(&h) {
                    a.tx_checked.insert(h);
                }
            }
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_move_up(move || {
            let mut a = app.borrow_mut();
            let n = a.txs.len();
            // top-to-bottom: a checked row swaps up past an unchecked predecessor (blocks move together)
            for i in 1..n {
                if a.tx_checked.contains(&a.txs[i].handle)
                    && !a.tx_checked.contains(&a.txs[i - 1].handle)
                {
                    a.txs.swap(i - 1, i);
                }
            }
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_move_down(move || {
            let mut a = app.borrow_mut();
            let n = a.txs.len();
            if n > 1 {
                for i in (0..n - 1).rev() {
                    if a.tx_checked.contains(&a.txs[i].handle)
                        && !a.tx_checked.contains(&a.txs[i + 1].handle)
                    {
                        a.txs.swap(i, i + 1);
                    }
                }
            }
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_remove_checked(move || {
            let mut a = app.borrow_mut();
            let checked: HashSet<u64> = a.tx_checked.clone();
            if checked.is_empty() {
                a.log("未勾选任何任务".to_string());
                return;
            }
            // stop periodic on the tasks about to be removed
            let stops: Vec<TxTask> = a
                .txs
                .iter()
                .filter(|t| checked.contains(&t.handle))
                .cloned()
                .collect();
            for task in &stops {
                stop_task_periodic(&a, task);
            }
            let cnt = checked.len();
            a.txs.retain(|t| !checked.contains(&t.handle));
            a.tx_checked.clear();
            a.log(format!("已删除 {cnt} 个发送任务"));
            if let (Some(u), Some(w)) = (uiw.upgrade(), txw2.upgrade()) {
                push_tx_list(&mut a, &u, &w);
            }
        });
    }
    {
        let app = app.clone();
        let txww = tx_window.as_weak();
        tx_window.on_tx_save_list(move || {
            let a = app.borrow();
            if a.txs.is_empty() {
                drop(a);
                app.borrow_mut().log("发送列表为空，无需保存".to_string());
                return;
            }
            let dto: Vec<TxTaskDto> = a.txs.iter().map(TxTaskDto::from_task).collect();
            drop(a);
            let app = app.clone();
            let txww = txww.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("发送列表", &["json"])
                    .set_file_name("send_list.json");
                if let Some(w) = txww.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                let worker = app.borrow().worker_tx.clone();
                std::thread::spawn(move || {
                    let result = serde_json::to_string_pretty(&dto)
                        .map_err(|error| format!("序列化失败: {error}"))
                        .and_then(|text| {
                            std::fs::write(&path, text)
                                .map_err(|error| format!("保存失败: {error}"))
                        });
                    let message = match result {
                        Ok(()) => format!("已保存发送列表: {}", path.display()),
                        Err(error) => error,
                    };
                    let _ = worker.send(WorkerEvent::Log(message));
                });
            });
        });
    }
    {
        let app = app.clone();
        let txww = tx_window.as_weak();
        tx_window.on_tx_load_list(move || {
            let app = app.clone();
            let txww = txww.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new().add_filter("发送列表", &["json"]);
                if let Some(w) = txww.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.pick_file().await else { return };
                let path = file.path().to_path_buf();
                let worker = app.borrow().worker_tx.clone();
                std::thread::spawn(move || {
                    let result = std::fs::read_to_string(&path)
                        .map_err(|error| format!("读取失败: {error}"))
                        .and_then(|text| {
                            serde_json::from_str::<Vec<TxTaskDto>>(&text)
                                .map_err(|error| format!("解析失败: {error}"))
                        });
                    let _ = worker.send(WorkerEvent::TxListLoaded(result));
                });
            });
        });
    }
    {
        let txw = tx_window.as_weak();
        let app = app.clone();
        tx_window.on_tx_file_pick(move || {
            let txw = txw.clone();
            let app = app.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("帧数据(文本)", &["txt", "asc", "log", "dat"])
                    .add_filter("所有文件", &["*"]);
                if let Some(w) = txw.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.pick_file().await else { return };
                let path = file.path().to_path_buf();
                if let Some(w) = txw.upgrade() {
                    w.set_tx_file_path(path.to_string_lossy().to_string().into());
                    w.set_tx_file_status(if app.borrow().lang_en { format!("Selected: {}", path.display()) } else { format!("已选择: {}", path.display()) }.into());
                }
            });
        });
    }
    {
        let txw = tx_window.as_weak();
        let app = app.clone();
        tx_window.on_tx_file_stop(move || {
            if let Some(w) = txw.upgrade() {
                w.set_tx_file_status(if app.borrow().lang_en { "Stopped".into() } else { "已停止".into() });
            }
        });
    }
    {
        let app = app.clone();
        tx_window.on_tx_update(move |i, field, value| {
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
        let txw = tx_window.as_weak();
        tx_window.on_tx_data_editor_show(move |i| {
            let Some(w) = txw.upgrade() else { return };
            if i < 0 {
                let fd = w.get_tx_form_fd() == "Yes";
                let max_len = if fd { 64 } else { 8 };
                let dlc = w
                    .get_tx_form_dlc()
                    .trim()
                    .parse::<usize>()
                    .unwrap_or(8)
                    .min(max_len);
                let data = parse_tx_bytes(&w.get_tx_form_data(), max_len);
                w.set_tx_data_editor_row(-1);
                w.set_tx_data_editor_title(
                    format!("普通发送 · DLC {dlc} · {}", if fd { "CAN FD" } else { "CAN" })
                        .into(),
                );
                w.set_tx_data_editor_rows(build_tx_data_editor_rows(&data, dlc));
                w.set_tx_data_editor_paste_value("".into());
                w.set_tx_data_editor_error("".into());
                w.set_tx_data_editor_open(true);
                return;
            }
            let a = app.borrow();
            let Some(task) = a.txs.get(i as usize) else { return };
            w.set_tx_data_editor_row(i);
            w.set_tx_data_editor_title(
                format!(
                    "{} · ID {} · DLC {}",
                    task.name,
                    id_str(task.id, task.ext),
                    task.data.len()
                )
                .into(),
            );
            w.set_tx_data_editor_rows(build_tx_data_editor_rows(&task.data, task.data.len()));
            w.set_tx_data_editor_paste_value("".into());
            w.set_tx_data_editor_error("".into());
            w.set_tx_data_editor_open(true);
        });
    }
    {
        let txw = tx_window.as_weak();
        tx_window.on_tx_data_editor_byte_edited(move |i, value| {
            let Some(w) = txw.upgrade() else { return };
            edit_tx_data_editor_byte(&w.get_tx_data_editor_rows(), i as usize, &value);
            w.set_tx_data_editor_error("".into());
        });
    }
    {
        let app = app.clone();
        let txw = tx_window.as_weak();
        tx_window.on_tx_data_editor_paste(move |value| {
            let Some(w) = txw.upgrade() else { return };
            let row = w.get_tx_data_editor_row();
            let dlc = if row < 0 {
                let max_len = if w.get_tx_form_fd() == "Yes" { 64 } else { 8 };
                w.get_tx_form_dlc().trim().parse::<usize>().unwrap_or(8).min(max_len)
            } else {
                app.borrow().txs.get(row as usize).map(|task| task.data.len()).unwrap_or(0)
            };
            let filled = paste_tx_data_editor(&w.get_tx_data_editor_rows(), &value, dlc);
            if filled == 0 {
                w.set_tx_data_editor_error("没有识别到有效的十六进制字节".into());
            } else {
                w.set_tx_data_editor_error("".into());
            }
        });
    }
    {
        let app = app.clone();
        let txw = tx_window.as_weak();
        tx_window.on_tx_data_editor_apply(move || {
            let Some(w) = txw.upgrade() else { return false };
            let row = w.get_tx_data_editor_row();
            let dlc = if row < 0 {
                let max_len = if w.get_tx_form_fd() == "Yes" { 64 } else { 8 };
                w.get_tx_form_dlc().trim().parse::<usize>().unwrap_or(8).min(max_len)
            } else {
                app.borrow().txs.get(row as usize).map(|task| task.data.len()).unwrap_or(0)
            };
            let data = match collect_tx_data_editor(&w.get_tx_data_editor_rows(), dlc) {
                Ok(data) => data,
                Err(index) => {
                    w.set_tx_data_editor_error(
                        format!("字节 {:02X} 必须是两位十六进制数", index).into(),
                    );
                    return false;
                }
            };
            if row < 0 {
                let value = data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
                w.set_tx_form_data(value.into());
                return true;
            }
            let mut a = app.borrow_mut();
            let Some(task) = a.txs.get_mut(row as usize) else { return false };
            task.data = data;
            if task.periodic {
                configure_task_periodic(&mut a, row as usize);
            }
            a.tx_list_cache = u64::MAX;
            true
        });
    }
    // DBC 发送：双击报文添加
    {
        let app = app.clone();
        let txw = tx_window.as_weak();
        tx_window.on_tx_dbc_add(move |i| {
            let mut a = app.borrow_mut();
            let Some((id, ext, name)) = a.tx_dbc_order.get(i as usize).cloned() else {
                return;
            };
            let ch = txw
                .upgrade()
                .map(|w| ch_from_name(&w.get_tx_dbc_ch()))
                .unwrap_or(1);
            let sig_values: Vec<(String, f64)> = a
                .dbc_message_frame(id, ext)
                .map(|m| m.signals.iter().map(|s| (s.name.clone(), 0.0)).collect())
                .unwrap_or_default();
            let map: std::collections::HashMap<String, f64> = sig_values.iter().cloned().collect();
            let data = a
                .dbc_encode_frame(id, ext, &map)
                .unwrap_or_else(|| vec![0u8; 8]);
            // a DBC message longer than 8 bytes can only be carried by CAN FD
            let fd = data.len() > 8;
            let h = a.next_handle;
            a.next_handle += 1;
            a.txs.push(TxTask {
                name: name.clone(),
                ch,
                id,
                ext,
                fd,
                brs: false,
                remote: false,
                data,
                periodic: false,
                period_ms: 100,
                repeat: -1,
                sent: 0,
                handle: h,
                dbc_id: Some(id),
                sig_values,
                varies: Vec::new(),
            });
            a.tx_sel = (a.txs.len() - 1) as i32;
            a.tx_sig_cache = u64::MAX;
            a.log(format!("已添加 DBC 发送: 0x{id:X} {name}"));
        });
    }
    // DBC 发送：选中任务
{
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_row_sel(move |i| {
            app.borrow_mut().tx_sel = i;
            // 切任务时清空信号选择/信息面板
            if let Some(w) = txw2.upgrade() {
                w.set_tx_sig_sel(-1);
                w.set_tx_sel_sig_name("".into());
            }
        });
    }
    // DBC 发送：编辑信号实际值 → 重新编码
    {
        let app = app.clone();
        tx_window.on_tx_sig_edit(move |i, val| {
            let mut a = app.borrow_mut();
            let sel = a.tx_sel;
            if sel < 0 || sel as usize >= a.txs.len() {
                return;
            }
            let Ok(v) = val.trim().parse::<f64>() else {
                return;
            };
            let Some(id) = a.txs[sel as usize].dbc_id else {
                return;
            };
            let ext = a.txs[sel as usize].ext;
            let sig_name = a
                .dbc_message_frame(id, ext)
                .and_then(|m| m.signals.get(i as usize).map(|s| s.name.clone()));
            let Some(sig_name) = sig_name else {
                return;
            };
            {
                let t = &mut a.txs[sel as usize];
                if let Some(e) = t.sig_values.iter_mut().find(|(n, _)| n == &sig_name) {
                    e.1 = v;
                } else {
                    t.sig_values.push((sig_name, v));
                }
            }
            let map: std::collections::HashMap<String, f64> =
                a.txs[sel as usize].sig_values.iter().cloned().collect();
            let data_opt = a.dbc_encode_frame(id, ext, &map);
            if let Some(data) = data_opt {
                a.txs[sel as usize].data = data;
                let idx = sel as usize;
                if a.txs[idx].periodic {
                    configure_task_periodic(&mut a, idx);
                }
            }
        });
    }
    // 信号变化页：选中信号 → 载入信息 + 变化配置面板
    {
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_sig_row_sel(move |i| {
            let Some(w) = txw2.upgrade() else { return };
            w.set_tx_sig_sel(i);
            populate_sig_panel(&app.borrow(), &w, i);
        });
    }
    // 编辑选中信号的实际值
    {
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_sig_edit_phys(move |v| {
            let Some(w) = txw2.upgrade() else { return };
            let Ok(phys) = v.trim().parse::<f64>() else { return };
            let sig_idx = w.get_tx_sig_sel();
            let mut a = app.borrow_mut();
            if let Some((name, factor, offset)) = selected_signal(&a, sig_idx) {
                set_signal_value(&mut a, &name, phys);
                let raw = if factor != 0.0 { ((phys - offset) / factor).round() } else { 0.0 };
                w.set_tx_sel_sig_raw(fmtf(raw).into());
                let idx = a.tx_sel;
                if idx >= 0 && a.txs.get(idx as usize).is_some_and(|t| t.periodic) {
                    configure_task_periodic(&mut a, idx as usize);
                }
            }
        });
    }
    // 编辑选中信号的原始值（实际值 = raw*比例 + 偏移）
    {
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_sig_edit_raw(move |v| {
            let Some(w) = txw2.upgrade() else { return };
            let Ok(raw) = v.trim().parse::<f64>() else { return };
            let sig_idx = w.get_tx_sig_sel();
            let mut a = app.borrow_mut();
            if let Some((name, factor, offset)) = selected_signal(&a, sig_idx) {
                let phys = raw * factor + offset;
                set_signal_value(&mut a, &name, phys);
                w.set_tx_sel_sig_phys(fmtf(phys).into());
                let idx = a.tx_sel;
                if idx >= 0 && a.txs.get(idx as usize).is_some_and(|t| t.periodic) {
                    configure_task_periodic(&mut a, idx as usize);
                }
            }
        });
    }
    // 应用变化方式到选中信号
    {
        let app = app.clone();
        let txw2 = tx_window.as_weak();
        tx_window.on_tx_vary_apply(move |mode, p1, p2, p3, p4, p5, p6, seq| {
            let Some(w) = txw2.upgrade() else { return };
            let sig_idx = w.get_tx_sig_sel();
            let mut a = app.borrow_mut();
            let sel = a.tx_sel;
            let Some((name, _, _)) = selected_signal(&a, sig_idx) else {
                a.log("请先选中一个信号".to_string());
                return;
            };
            let params = [
                p1.to_string(),
                p2.to_string(),
                p3.to_string(),
                p4.to_string(),
                p5.to_string(),
                p6.to_string(),
            ];
            let vmode = ui_to_vary(mode, &params, &seq);
            {
                let t = &mut a.txs[sel as usize];
                if t.period_ms == 0 {
                    t.period_ms = 100;
                }
                if !t.sig_values.iter().any(|(s, _)| s == &name) {
                    t.sig_values.push((name.clone(), 0.0));
                }
                if let Some(sv) = t.varies.iter_mut().find(|v| v.signal == name) {
                    sv.mode = vmode;
                } else {
                    t.varies.push(SignalVary {
                        signal: name.clone(),
                        mode: vmode,
                    });
                }
            }
            if a.txs[sel as usize].periodic {
                configure_task_periodic(&mut a, sel as usize);
            }
            a.tx_sig_cache = u64::MAX;
            a.log(format!("已设置信号变化: {name}"));
        });
    }
    // 选中行操作条（基于 selected_key，行顺序变化也不会错）
{
        let app = app.clone();
        let txw = tx_window.as_weak();
        tx_window.on_tx_form_add(move |ch, id, kind, fd, brs, dlc, data, period, name| {
            let mut a = app.borrow_mut();
            let remote = txw
                .upgrade()
                .map(|w| w.get_tx_form_remote() == "Yes")
                .unwrap_or(false);
            match tx_task_from_form(
                &mut a, &ch, &id, &kind, &fd, &brs, &dlc, &data, &period, &name, remote,
            ) {
                Ok(t) => {
                    a.log(format!("添加发送报文: {} {}", t.name, id_str(t.id, t.ext)));
                    a.txs.push(t);
                }
                Err(e) => a.log(format!("自动加载 sample.dbc 失败: {e}")),
            }
        });
    }
    {
        let app = app.clone();
        let txw = tx_window.as_weak();
        tx_window.on_tx_form_send(move |ch, id, kind, fd, brs, dlc, data, period, name| {
            let mut a = app.borrow_mut();
            let remote = txw
                .upgrade()
                .map(|w| w.get_tx_form_remote() == "Yes")
                .unwrap_or(false);
            match tx_task_from_form(
                &mut a, &ch, &id, &kind, &fd, &brs, &dlc, &data, &period, &name, remote,
            ) {
                Ok(t) => {
                    // 读取 每次帧数(burst)/发送次数(repeat)/ID递增/数据递增
                    let (burst, repeat, id_inc, data_inc) = match txw.upgrade() {
                        Some(w) => (
                            w.get_tx_form_count().trim().parse::<u32>().unwrap_or(1).max(1),
                            w.get_tx_form_repeat().trim().parse::<u32>().unwrap_or(1).max(1),
                            w.get_tx_form_id_inc(),
                            w.get_tx_form_data_inc(),
                        ),
                        None => (1, 1, false, false),
                    };
                    let total = (burst as u64) * (repeat as u64);
                    let cap = total.min(100_000); // 防误操作刷爆总线
                    let frame = tx_frame(&t);
                    if a
                        .cmd
                        .send(Cmd::SendSequence {
                            frame,
                            count: cap,
                            id_increment: id_inc,
                            data_increment: data_inc,
                        })
                        .is_ok()
                    {
                        a.log(format!(
                            "发送任务: {} {} ×{}帧{}{}",
                            t.name,
                            id_str(t.id, t.ext),
                            cap,
                            if id_inc { " ID递增" } else { "" },
                            if data_inc { " 数据递增" } else { "" }
                        ));
                    } else {
                        a.log("发送任务提交失败: 命令队列已满，请稍后重试");
                    }
                }
                Err(e) => a.log(format!("自动加载 sample.dbc 失败: {e}")),
            }
        });
    }
    // Preserve the user's draft while changing frame type/protocol/DLC.
    // DLC controls the transmitted length when the task is created; it must not
    // destructively pad/truncate the data editor merely because focus changed.
    {
        let txw = tx_window.as_weak();
        tx_window.on_tx_form_dlc_edited(move |v| {
            let Some(w) = txw.upgrade() else { return };
            w.set_tx_form_dlc(v);
        });
    }
    {
        let txw = tx_window.as_weak();
        tx_window.on_tx_form_dlc_committed(move |v| {
            let Some(w) = txw.upgrade() else { return };
            let maxb = if w.get_tx_form_fd() == "Yes" { 64 } else { 8 };
            let n = v.trim().parse::<usize>().unwrap_or(0).min(maxb);
            w.set_tx_form_dlc(n.to_string().into());
        });
    }
    {
        let txw = tx_window.as_weak();
        tx_window.on_tx_form_data_edited(move |v| {
            let Some(w) = txw.upgrade() else { return };
            w.set_tx_form_data(v);
        });
    }
}
fn build_ota_job(
    path: &str,
    channel: u8,
    id: u32,
    mode: i32,
    addr: u32,
    crc: u16,
    en: bool,
) -> Result<OtaJob, String> {
    match mode {
        1 => build_xcp_ota_job(path, channel, id, en),
        2 => build_uds_ota_job(path, channel, id, addr, crc, en),
        _ => Err("unsupported OTA mode".into()),
    }
}

fn build_normal_file_frames(
    path: &str,
    channel: u8,
    id: u32,
    max_len: usize,
    en: bool,
) -> Result<Vec<CanFrame>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| if en { format!("Read failed: {e}") } else { format!("读取失败: {e}") })?;
    let ext = id > 0x7FF;
    let mut frames = Vec::new();
    for line in text.lines() {
        let data = parse_tx_bytes(line, max_len);
        if !data.is_empty() {
            frames.push(CanFrame {
                t: 0.0,
                ch: channel,
                tx: true,
                id,
                ext,
                fd: max_len > 8,
                brs: false,
                remote: false,
                error: false,
                data,
            });
        }
    }
    if frames.is_empty() {
        return Err(if en { "No valid frame data in file".into() } else { "文件无有效帧数据".into() });
    }
    Ok(frames)
}

fn xcp_response_to_can(response: ota::xcp::XcpResponseId) -> OtaResponseId {
    match response {
        ota::xcp::XcpResponseId::Exact(id) => OtaResponseId::Exact(id),
        ota::xcp::XcpResponseId::WildcardBase(base) => OtaResponseId::WildcardBase(base),
    }
}

fn build_xcp_ota_job(path: &str, channel: u8, id: u32, en: bool) -> Result<OtaJob, String> {
    let ids = ota::xcp::resolve_ids(id)
        .ok_or_else(|| if en { "Invalid XCP CAN ID, expected 0x1821xx10".to_string() } else { "无效 XCP CAN ID，应为 0x1821xx10".to_string() })?;
    let response = xcp_response_to_can(ids.response);
    let raw = std::fs::read(path)
        .map_err(|e| if en { format!("Read failed: {e}") } else { format!("读取失败: {e}") })?;
    let image = ota::xcp::padded_image(raw)?;
    let mut steps = Vec::new();
    steps.push(OtaStep {
        frame: ota::xcp::connect_frame(channel, ids.send_id),
        ack: OtaAck::XcpConnect { response },
        timeout_ms: 100,
        retries: 200,
    });
    for prepared in ota::xcp::prepare_download(&image) {
        steps.push(OtaStep {
            frame: ota::xcp::prepared_frame_to_can(channel, ids.send_id, &prepared),
            ack: OtaAck::XcpAck { response },
            timeout_ms: if matches!(prepared, ota::xcp::XcpPreparedFrame::End) { 500 } else { 60 },
            retries: 3,
        });
    }
    Ok(OtaJob {
        name: "XCP OTA".into(),
        steps,
        timeout_ms: 60,
        retries: 3,
    })
}
fn push_uds_multiframe_steps(
    steps: &mut Vec<OtaStep>,
    frames: impl IntoIterator<Item = CanFrame>,
    final_ack: OtaAck,
) {
    let frames: Vec<CanFrame> = frames.into_iter().collect();
    let last = frames.len().saturating_sub(1);
    for (i, frame) in frames.into_iter().enumerate() {
        let ack = if i == 0 {
            OtaAck::UdsFlowControl
        } else if i == last {
            final_ack
        } else {
            OtaAck::None
        };
        steps.push(OtaStep {
            frame,
            ack,
            timeout_ms: if matches!(ack, OtaAck::None) { 1 } else { 1000 },
            retries: 3,
        });
    }
}

fn build_uds_ota_job(
    path: &str,
    channel: u8,
    id: u32,
    addr: u32,
    crc: u16,
    en: bool,
) -> Result<OtaJob, String> {
    if id == 0 {
        return Err(if en { "UDS CAN ID is empty".into() } else { "UDS CAN ID 为空".into() });
    }
    let image = std::fs::read(path)
        .map_err(|e| if en { format!("Read failed: {e}") } else { format!("读取失败: {e}") })?;
    if image.is_empty() {
        return Err(if en { "UDS file is empty".into() } else { "UDS 文件为空".into() });
    }

    let mut steps = Vec::new();
    steps.push(OtaStep {
        frame: ota::uds::diagnostic_session(channel, id, 0x03),
        ack: OtaAck::UdsPositive { service: ota::uds::DIAGNOSTIC_SESSION_CONTROL },
        timeout_ms: 1000,
        retries: 3,
    });
    push_uds_multiframe_steps(
        &mut steps,
        ota::uds::request_download(channel, id, addr, image.len() as u32),
        OtaAck::UdsPositive { service: ota::uds::REQUEST_DOWNLOAD },
    );
    for (i, chunk) in image.chunks(256).enumerate() {
        push_uds_multiframe_steps(
            &mut steps,
            ota::uds::transfer_data_block(channel, id, i as u8, chunk),
            OtaAck::UdsPositive { service: ota::uds::TRANSFER_DATA },
        );
    }
    steps.push(OtaStep {
        frame: ota::uds::request_transfer_exit(channel, id, crc),
        ack: OtaAck::UdsPositive { service: ota::uds::REQUEST_TRANSFER_EXIT },
        timeout_ms: 1000,
        retries: 3,
    });
    Ok(OtaJob {
        name: "UDS OTA".into(),
        steps,
        timeout_ms: 1000,
        retries: 3,
    })
}
