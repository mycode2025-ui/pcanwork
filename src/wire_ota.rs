#[allow(unused_variables)]
fn wire_ota_windows(
    app: Rc<std::cell::RefCell<App>>,
    ui: &AppWindow,
    uds_window: &UdsWindow,
    xcp_window: &XcpWindow,
) {
    {
        let uw = uds_window.as_weak();
        ui.on_open_uds_window(move || {
            if let Some(w) = uw.upgrade() {
                show_child_window(&w);
            }
        });
    }
    {
        let xw = xcp_window.as_weak();
        ui.on_open_xcp_window(move || {
            if let Some(w) = xw.upgrade() {
                show_child_window(&w);
            }
        });
    }
    wire_uds_window(app.clone(), uds_window);
    wire_xcp_window(app, xcp_window);
}

fn wire_uds_window(app: Rc<std::cell::RefCell<App>>, uds_window: &UdsWindow) {
    {
        let uw = uds_window.as_weak();
        uds_window.on_diag_log_clear(move || {
            if let Some(w) = uw.upgrade() {
                w.set_diag_log("".into());
            }
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_tester_present(move || {
            send_uds_single(&app, &uw, |w| uds_window_id(w).map(|id| ota::uds::tester_present(1, id)), "UDS 3E");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_session_send(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let session = parse_u32(&w.get_uds_session()).unwrap_or(3) as u8;
                Some(ota::uds::diagnostic_session(1, id, session))
            }, "UDS 10");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_reset_send(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let ty = parse_u32(&w.get_uds_reset_type()).unwrap_or(1) as u8;
                Some(ota::uds::single_frame(1, id, &[0x11, ty]))
            }, "UDS 11");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_clear_dtc(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let group = parse_u32(&w.get_uds_dtc_group()).unwrap_or(0xFF_FF_FF);
                Some(ota::uds::single_frame(1, id, &[0x14, (group >> 16) as u8, (group >> 8) as u8, group as u8]))
            }, "UDS 14");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_read_dtc(move || {
            send_uds_single_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let sub = parse_u32(&w.get_uds_dtc_subfn()).unwrap_or(2) as u8;
                let mask = parse_u32(&w.get_uds_dtc_status_mask()).unwrap_or(0xFF) as u8;
                Ok(ota::uds::single_frame(1, id, &[0x19, sub, mask]))
            }, "UDS 19");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_read_did(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let did = parse_u32(&w.get_uds_did()).unwrap_or(0xF190);
                Some(ota::uds::single_frame(1, id, &[0x22, (did >> 8) as u8, did as u8]))
            }, "UDS 22");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_write_did(move || {
            send_uds_single_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let did = parse_u32(&w.get_uds_did()).unwrap_or(0xF190);
                let mut payload = vec![0x2E, (did >> 8) as u8, did as u8];
                payload.extend(parse_tx_bytes(&w.get_uds_write_data(), 4));
                if payload.len() > 7 {
                    return Err("UDS 2E data is too long for single frame".into());
                }
                Ok(ota::uds::single_frame(1, id, &payload))
            }, "UDS 2E");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_read_memory(move || {
            send_uds_frames_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let addr = parse_u32(&w.get_uds_mem_addr()).unwrap_or(0);
                let len = parse_u32(&w.get_uds_mem_len()).unwrap_or(0x10);
                let payload = vec![0x23, 0x44, (addr >> 24) as u8, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8, (len >> 24) as u8, (len >> 16) as u8, (len >> 8) as u8, len as u8];
                Ok(uds_payload_frames(1, id, &payload))
            }, "UDS 23");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_write_memory(move || {
            send_uds_frames_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let addr = parse_u32(&w.get_uds_mem_addr()).unwrap_or(0);
                let data = parse_tx_bytes(&w.get_uds_mem_data(), 4085);
                if data.is_empty() {
                    return Err("UDS 3D data is empty".into());
                }
                let len = data.len() as u32;
                let mut payload = vec![0x3D, 0x44, (addr >> 24) as u8, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8, (len >> 24) as u8, (len >> 16) as u8, (len >> 8) as u8, len as u8];
                payload.extend(data);
                Ok(uds_payload_frames(1, id, &payload))
            }, "UDS 3D");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_security_seed(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let level = parse_u32(&w.get_uds_security_level()).unwrap_or(1) as u8;
                Some(ota::uds::security_seed_request(1, id, level))
            }, "UDS 27 seed");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_security_key_send(move || {
            send_uds_single_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let level = parse_u32(&w.get_uds_security_level()).unwrap_or(1) as u8;
                let key_level = level.wrapping_add(1);
                let key_bytes = parse_tx_bytes(&w.get_uds_security_key(), 4);
                if key_bytes.len() == 4 {
                    let key = ((key_bytes[0] as u32) << 24)
                        | ((key_bytes[1] as u32) << 16)
                        | ((key_bytes[2] as u32) << 8)
                        | key_bytes[3] as u32;
                    Ok(ota::uds::security_key_request(1, id, key_level, key))
                } else {
                    let mut payload = vec![0x27, key_level];
                    payload.extend(key_bytes);
                    if payload.len() > 7 {
                        return Err("UDS 27 key data is too long for single frame".into());
                    }
                    Ok(ota::uds::single_frame(1, id, &payload))
                }
            }, "UDS 27 key");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_routine_start(move || {
            send_uds_single(&app, &uw, |w| {
                let id = uds_window_id(w)?;
                let routine = parse_u32(&w.get_uds_routine_id()).unwrap_or(0xCBA5) as u16;
                Some(ota::uds::routine_control_jump(1, id, routine))
            }, "UDS 31");
        });
    }
    {
        let app = app.clone();
        let uw = uds_window.as_weak();
        uds_window.on_uds_custom_send(move || {
            send_uds_single_result(&app, &uw, |w| {
                let id = uds_window_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let payload = parse_tx_bytes(&w.get_uds_custom(), 7);
                if payload.is_empty() {
                    return Err("UDS custom data is empty".into());
                }
                Ok(ota::uds::single_frame(1, id, &payload))
            }, "UDS custom");
        });
    }
}

fn wire_xcp_window(app: Rc<std::cell::RefCell<App>>, xcp_window: &XcpWindow) {
    {
        let xw = xcp_window.as_weak();
        xcp_window.on_diag_log_clear(move || {
            if let Some(w) = xw.upgrade() {
                w.set_diag_log("".into());
            }
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_connect(move || {
            send_xcp_single(&app, &xw, |w| xcp_window_id(w).map(|id| ota::xcp::connect_frame(1, id)), "XCP FF");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_status(move || {
            send_xcp_single(&app, &xw, |w| xcp_window_id(w).map(|id| ota::xcp::get_status_frame(1, id)), "XCP FD");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_disconnect(move || {
            send_xcp_single(&app, &xw, |w| xcp_window_id(w).map(|id| ota::xcp::frame_from_data(1, id, [ota::xcp::DISCONNECT_CMD])), "XCP FE");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_set_mta(move || {
            send_xcp_single_result(&app, &xw, |w| {
                let id = xcp_window_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let addr = parse_u32(&w.get_xcp_mta_addr()).unwrap_or(0);
                let ext = parse_u32(&w.get_xcp_mta_ext()).unwrap_or(0) as u8;
                Ok(ota::xcp::frame_from_data(1, id, [0xF6, 0x00, 0x00, ext, (addr >> 24) as u8, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8]))
            }, "XCP SetMTA");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_upload(move || {
            send_xcp_single_result(&app, &xw, |w| {
                let id = xcp_window_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let len = parse_u32(&w.get_xcp_upload_len()).unwrap_or(7).clamp(1, 7) as u8;
                Ok(ota::xcp::frame_from_data(1, id, [0xF5, len]))
            }, "XCP Upload");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_download(move || {
            send_xcp_single_result(&app, &xw, |w| {
                let id = xcp_window_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let mut data = parse_tx_bytes(&w.get_xcp_download_data(), 6);
                if data.is_empty() {
                    return Err("XCP download data is empty".into());
                }
                let mut payload = vec![0xF0, data.len() as u8];
                payload.append(&mut data);
                Ok(ota::xcp::frame_from_data(1, id, payload))
            }, "XCP Download");
        });
    }
    {
        let app = app.clone();
        let xw = xcp_window.as_weak();
        xcp_window.on_xcp_custom_send(move || {
            send_xcp_single_result(&app, &xw, |w| {
                let id = xcp_window_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let data = parse_tx_bytes(&w.get_xcp_custom(), 8);
                if data.is_empty() {
                    return Err("XCP custom data is empty".into());
                }
                Ok(ota::xcp::frame_from_data(1, id, data))
            }, "XCP custom");
        });
    }
}

#[allow(unused_variables, dead_code)]
fn wire_ota_window(
    app: Rc<std::cell::RefCell<App>>,
    ota_window: &OtaWindow,
) {
    {
        let ow = ota_window.as_weak();
        ota_window.on_ota_pick_file(move || {
            let ow = ow.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("Firmware", &["bin", "hex", "s19", "mot", "dat"])
                    .add_filter("All files", &["*"]);
                if let Some(w) = ow.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.pick_file().await else { return };
                if let Some(w) = ow.upgrade() {
                    w.set_ota_file_path(file.path().to_string_lossy().to_string().into());
                    w.set_ota_status(format!("已选择: {}", file.path().display()).into());
                    w.set_ota_progress(0.0);
                }
            });
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_ota_start(move || {
            let Some(w) = ow.upgrade() else { return };
            let en = app.borrow().lang_en;
            let path = w.get_ota_file_path().to_string();
            if path.trim().is_empty() {
                w.set_ota_status(if en { "Select a firmware file first".into() } else { "请先选择升级文件".into() });
                return;
            }
            let Some(id) = parse_u32(&w.get_ota_can_id()) else {
                w.set_ota_status(if en { "Invalid CAN ID".into() } else { "无效 CAN ID".into() });
                return;
            };
            let mode = w.get_ota_mode();
            let job = if mode == 0 {
                build_xcp_ota_job(&path, id, en)
            } else {
                let addr = parse_u32(&w.get_ota_addr()).unwrap_or(0);
                let crc = parse_u32(&w.get_ota_crc()).unwrap_or(0) as u16;
                build_uds_ota_job(&path, id, addr, crc, en)
            };
            let job = match job {
                Ok(job) => job,
                Err(e) => {
                    w.set_ota_status(e.into());
                    return;
                }
            };
            let steps = job.steps.len();
            let mut a = app.borrow_mut();
            let _ = a.cmd.send(Cmd::OtaRun(job));
            a.log(format!("OTA started from standalone window: {path}, {steps} steps"));
            w.set_ota_progress(0.0);
            w.set_ota_status(if en {
                format!("OTA started ({steps} steps)")
            } else {
                format!("OTA 已启动（{steps} 步）")
            }.into());
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_ota_stop(move || {
            can::cancel_ota();
            if let Some(w) = ow.upgrade() {
                w.set_ota_status(if app.borrow().lang_en { "Cancelling OTA...".into() } else { "正在取消 OTA...".into() });
            }
        });
    }
    {
        let ow = ota_window.as_weak();
        ota_window.on_diag_log_clear(move || {
            if let Some(w) = ow.upgrade() {
                w.set_diag_log("".into());
            }
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_tester_present(move || {
            send_ota_single(&app, &ow, |w| uds_id(w).map(|id| ota::uds::tester_present(1, id)), "UDS 3E");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_session_send(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let session = parse_u32(&w.get_uds_session()).unwrap_or(3) as u8;
                Some(ota::uds::diagnostic_session(1, id, session))
            }, "UDS 10");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_reset_send(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let ty = parse_u32(&w.get_uds_reset_type()).unwrap_or(1) as u8;
                Some(ota::uds::single_frame(1, id, &[0x11, ty]))
            }, "UDS 11");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_clear_dtc(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let group = parse_u32(&w.get_uds_dtc_group()).unwrap_or(0xFF_FF_FF);
                Some(ota::uds::single_frame(1, id, &[0x14, (group >> 16) as u8, (group >> 8) as u8, group as u8]))
            }, "UDS 14");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_read_dtc(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let sub = parse_u32(&w.get_uds_dtc_subfn()).unwrap_or(2) as u8;
                let mask = parse_u32(&w.get_uds_dtc_status_mask()).unwrap_or(0xFF) as u8;
                Ok(ota::uds::single_frame(1, id, &[0x19, sub, mask]))
            }, "UDS 19");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_read_did(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let did = parse_u32(&w.get_uds_did()).unwrap_or(0xF190);
                Some(ota::uds::single_frame(1, id, &[0x22, (did >> 8) as u8, did as u8]))
            }, "UDS 22");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_write_did(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "无效 UDS CAN ID".to_string())?;
                let did = parse_u32(&w.get_uds_did()).unwrap_or(0xF190);
                let mut payload = vec![0x2E, (did >> 8) as u8, did as u8];
                payload.extend(parse_tx_bytes(&w.get_uds_write_data(), 4));
                if payload.len() > 7 {
                    return Err("2E 单帧数据最多 4 字节，请用自定义多帧/脚本发送".into());
                }
                Ok(ota::uds::single_frame(1, id, &payload))
            }, "UDS 2E");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_read_memory(move || {
            send_ota_frames_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let addr = parse_u32(&w.get_uds_mem_addr()).unwrap_or(0);
                let len = parse_u32(&w.get_uds_mem_len()).unwrap_or(0x10);
                let payload = vec![
                    0x23,
                    0x44,
                    (addr >> 24) as u8,
                    (addr >> 16) as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                    (len >> 24) as u8,
                    (len >> 16) as u8,
                    (len >> 8) as u8,
                    len as u8,
                ];
                Ok(uds_payload_frames(1, id, &payload))
            }, "UDS 23");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_write_memory(move || {
            send_ota_frames_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "invalid UDS CAN ID".to_string())?;
                let addr = parse_u32(&w.get_uds_mem_addr()).unwrap_or(0);
                let data = parse_tx_bytes(&w.get_uds_mem_data(), 4085);
                if data.is_empty() {
                    return Err("UDS 3D data is empty".into());
                }
                let len = data.len() as u32;
                let mut payload = vec![
                    0x3D,
                    0x44,
                    (addr >> 24) as u8,
                    (addr >> 16) as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                    (len >> 24) as u8,
                    (len >> 16) as u8,
                    (len >> 8) as u8,
                    len as u8,
                ];
                payload.extend(data);
                Ok(uds_payload_frames(1, id, &payload))
            }, "UDS 3D");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_security_seed(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let level = parse_u32(&w.get_uds_security_level()).unwrap_or(1) as u8;
                Some(ota::uds::security_seed_request(1, id, level))
            }, "UDS 27 seed");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_security_key_send(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "无效 UDS CAN ID".to_string())?;
                let level = parse_u32(&w.get_uds_security_level()).unwrap_or(1) as u8;
                let key_level = level.wrapping_add(1);
                let key_bytes = parse_tx_bytes(&w.get_uds_security_key(), 4);
                if key_bytes.len() == 4 {
                    let key = ((key_bytes[0] as u32) << 24)
                        | ((key_bytes[1] as u32) << 16)
                        | ((key_bytes[2] as u32) << 8)
                        | key_bytes[3] as u32;
                    Ok(ota::uds::security_key_request(1, id, key_level, key))
                } else {
                    let mut payload = vec![0x27, key_level];
                    payload.extend(key_bytes);
                    if payload.len() > 7 {
                        return Err("27 key 单帧最多 5 字节 key 数据".into());
                    }
                    Ok(ota::uds::single_frame(1, id, &payload))
                }
            }, "UDS 27 key");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_routine_start(move || {
            send_ota_single(&app, &ow, |w| {
                let id = uds_id(w)?;
                let routine = parse_u32(&w.get_uds_routine_id()).unwrap_or(0xCBA5) as u16;
                Some(ota::uds::routine_control_jump(1, id, routine))
            }, "UDS 31");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_uds_custom_send(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = uds_id(w).ok_or_else(|| "无效 UDS CAN ID".to_string())?;
                let payload = parse_tx_bytes(&w.get_uds_custom(), 7);
                if payload.is_empty() {
                    return Err("自定义 UDS 数据为空".into());
                }
                Ok(ota::uds::single_frame(1, id, &payload))
            }, "UDS custom");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_connect(move || {
            send_ota_single(&app, &ow, |w| xcp_id(w).map(|id| ota::xcp::connect_frame(1, id)), "XCP FF");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_status(move || {
            send_ota_single(&app, &ow, |w| xcp_id(w).map(|id| ota::xcp::get_status_frame(1, id)), "XCP FD");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_disconnect(move || {
            send_ota_single(&app, &ow, |w| xcp_id(w).map(|id| ota::xcp::frame_from_data(1, id, [ota::xcp::DISCONNECT_CMD])), "XCP FE");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_set_mta(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = xcp_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let addr = parse_u32(&w.get_xcp_mta_addr()).unwrap_or(0);
                let ext = parse_u32(&w.get_xcp_mta_ext()).unwrap_or(0) as u8;
                Ok(ota::xcp::frame_from_data(1, id, [0xF6, 0x00, 0x00, ext, (addr >> 24) as u8, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8]))
            }, "XCP SetMTA");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_upload(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = xcp_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let len = parse_u32(&w.get_xcp_upload_len()).unwrap_or(7).clamp(1, 7) as u8;
                Ok(ota::xcp::frame_from_data(1, id, [0xF5, len]))
            }, "XCP Upload");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_download(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = xcp_id(w).ok_or_else(|| "invalid XCP CAN ID".to_string())?;
                let mut data = parse_tx_bytes(&w.get_xcp_download_data(), 6);
                if data.is_empty() {
                    return Err("XCP download data is empty".into());
                }
                let mut payload = vec![0xF0, data.len() as u8];
                payload.append(&mut data);
                Ok(ota::xcp::frame_from_data(1, id, payload))
            }, "XCP Download");
        });
    }
    {
        let app = app.clone();
        let ow = ota_window.as_weak();
        ota_window.on_xcp_custom_send(move || {
            send_ota_single_result(&app, &ow, |w| {
                let id = xcp_id(w).ok_or_else(|| "无效 XCP CAN ID".to_string())?;
                let data = parse_tx_bytes(&w.get_xcp_custom(), 8);
                if data.is_empty() {
                    return Err("自定义 XCP 数据为空".into());
                }
                Ok(ota::xcp::frame_from_data(1, id, data))
            }, "XCP custom");
        });
    }
}

#[allow(dead_code)]
fn uds_id(w: &OtaWindow) -> Option<u32> {
    parse_u32(&w.get_uds_id())
}

fn uds_window_id(w: &UdsWindow) -> Option<u32> {
    parse_u32(&w.get_uds_id())
}

#[allow(dead_code)]
fn xcp_id(w: &OtaWindow) -> Option<u32> {
    parse_u32(&w.get_xcp_id())
}

fn xcp_window_id(w: &XcpWindow) -> Option<u32> {
    parse_u32(&w.get_xcp_id())
}

fn send_uds_single<F>(app: &Rc<std::cell::RefCell<App>>, uw: &slint::Weak<UdsWindow>, build: F, label: &str)
where
    F: FnOnce(&UdsWindow) -> Option<CanFrame>,
{
    send_uds_single_result(app, uw, |w| build(w).ok_or_else(|| "invalid CAN ID".to_string()), label);
}

fn send_uds_single_result<F>(
    app: &Rc<std::cell::RefCell<App>>,
    uw: &slint::Weak<UdsWindow>,
    build: F,
    label: &str,
) where
    F: FnOnce(&UdsWindow) -> Result<CanFrame, String>,
{
    let Some(w) = uw.upgrade() else { return };
    let frame = match build(&w) {
        Ok(frame) => frame,
        Err(e) => {
            w.set_ota_status(e.into());
            return;
        }
    };
    let mut a = app.borrow_mut();
    let id = frame.id;
    let data_hex = frame.data_hex();
    let _ = a.cmd.send(Cmd::SendOnce(frame));
    a.log(format!("{label}: send 0x{id:X} [{data_hex}]"));
    w.set_ota_status(format!("{label} sent 0x{id:X} [{data_hex}]").into());
    append_uds_log(&w, &format!("TX {label}  ID=0x{id:X}  [{data_hex}]"));
}

fn send_uds_frames_result<F>(
    app: &Rc<std::cell::RefCell<App>>,
    uw: &slint::Weak<UdsWindow>,
    build: F,
    label: &str,
) where
    F: FnOnce(&UdsWindow) -> Result<Vec<CanFrame>, String>,
{
    let Some(w) = uw.upgrade() else { return };
    let frames = match build(&w) {
        Ok(frames) => frames,
        Err(e) => {
            w.set_ota_status(e.into());
            return;
        }
    };
    if frames.is_empty() {
        w.set_ota_status("No frame generated".into());
        return;
    }
    let mut a = app.borrow_mut();
    for frame in &frames {
        let id = frame.id;
        let data_hex = frame.data_hex();
        let _ = a.cmd.send(Cmd::SendOnce(frame.clone()));
        a.log(format!("{label}: send 0x{id:X} [{data_hex}]"));
        append_uds_log(&w, &format!("TX {label}  ID=0x{id:X}  [{data_hex}]"));
    }
    w.set_ota_status(format!("{label} sent {} frames", frames.len()).into());
}

fn send_xcp_single<F>(app: &Rc<std::cell::RefCell<App>>, xw: &slint::Weak<XcpWindow>, build: F, label: &str)
where
    F: FnOnce(&XcpWindow) -> Option<CanFrame>,
{
    send_xcp_single_result(app, xw, |w| build(w).ok_or_else(|| "invalid CAN ID".to_string()), label);
}

fn send_xcp_single_result<F>(
    app: &Rc<std::cell::RefCell<App>>,
    xw: &slint::Weak<XcpWindow>,
    build: F,
    label: &str,
) where
    F: FnOnce(&XcpWindow) -> Result<CanFrame, String>,
{
    let Some(w) = xw.upgrade() else { return };
    let frame = match build(&w) {
        Ok(frame) => frame,
        Err(e) => {
            w.set_ota_status(e.into());
            return;
        }
    };
    let mut a = app.borrow_mut();
    let id = frame.id;
    let data_hex = frame.data_hex();
    let _ = a.cmd.send(Cmd::SendOnce(frame));
    a.log(format!("{label}: send 0x{id:X} [{data_hex}]"));
    w.set_ota_status(format!("{label} sent 0x{id:X} [{data_hex}]").into());
    append_xcp_log(&w, &format!("TX {label}  ID=0x{id:X}  [{data_hex}]"));
}

#[allow(dead_code)]
fn send_ota_single<F>(app: &Rc<std::cell::RefCell<App>>, ow: &slint::Weak<OtaWindow>, build: F, label: &str)
where
    F: FnOnce(&OtaWindow) -> Option<CanFrame>,
{
    send_ota_single_result(app, ow, |w| build(w).ok_or_else(|| "无效 CAN ID".to_string()), label);
}

#[allow(dead_code)]
fn send_ota_single_result<F>(
    app: &Rc<std::cell::RefCell<App>>,
    ow: &slint::Weak<OtaWindow>,
    build: F,
    label: &str,
) where
    F: FnOnce(&OtaWindow) -> Result<CanFrame, String>,
{
    let Some(w) = ow.upgrade() else { return };
    let frame = match build(&w) {
        Ok(frame) => frame,
        Err(e) => {
            w.set_ota_status(e.into());
            return;
        }
    };
    let mut a = app.borrow_mut();
    let id = frame.id;
    let data_hex = frame.data_hex();
    let _ = a.cmd.send(Cmd::SendOnce(frame));
    a.log(format!("{label}: send 0x{id:X} [{data_hex}]"));
    w.set_ota_status(format!("{label} 已发送: 0x{id:X} [{data_hex}]").into());
    append_ota_log(&w, &format!("TX {label}  ID=0x{id:X}  [{data_hex}]"));
}

#[allow(dead_code)]
fn send_ota_frames_result<F>(
    app: &Rc<std::cell::RefCell<App>>,
    ow: &slint::Weak<OtaWindow>,
    build: F,
    label: &str,
) where
    F: FnOnce(&OtaWindow) -> Result<Vec<CanFrame>, String>,
{
    let Some(w) = ow.upgrade() else { return };
    let frames = match build(&w) {
        Ok(frames) => frames,
        Err(e) => {
            w.set_ota_status(e.into());
            return;
        }
    };
    if frames.is_empty() {
        w.set_ota_status("No frame generated".into());
        return;
    }
    let mut a = app.borrow_mut();
    for frame in &frames {
        let id = frame.id;
        let data_hex = frame.data_hex();
        let _ = a.cmd.send(Cmd::SendOnce(frame.clone()));
        a.log(format!("{label}: send 0x{id:X} [{data_hex}]"));
        append_ota_log(&w, &format!("TX {label}  ID=0x{id:X}  [{data_hex}]"));
    }
    w.set_ota_status(format!("{label} sent {} frames", frames.len()).into());
}

fn uds_payload_frames(ch: u8, id: u32, payload: &[u8]) -> Vec<CanFrame> {
    if payload.len() <= 7 {
        return vec![ota::uds::single_frame(ch, id, payload)];
    }
    let len = payload.len().min(0x0FFF);
    let mut frames = Vec::new();
    let first_len = len.min(6);
    let mut first = Vec::with_capacity(8);
    first.push(0x10 | (((len >> 8) as u8) & 0x0F));
    first.push(len as u8);
    first.extend_from_slice(&payload[..first_len]);
    first.resize(8, 0);
    frames.push(ota::uds::frame_from_data(ch, id, first));

    let mut seq = 1u8;
    for chunk in payload[first_len..len].chunks(7) {
        let mut cf = Vec::with_capacity(8);
        cf.push(0x20 | (seq & 0x0F));
        cf.extend_from_slice(chunk);
        cf.resize(8, 0);
        frames.push(ota::uds::frame_from_data(ch, id, cf));
        seq = seq.wrapping_add(1);
    }
    frames
}

#[allow(dead_code)]
fn append_ota_log(w: &OtaWindow, line: &str) {
    let mut text = w.get_diag_log().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
    const CAP: usize = 20_000;
    if text.len() > CAP {
        let cut = text.len() - CAP;
        let cut = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
        text = text[cut..].to_string();
    }
    w.set_diag_log(text.into());
}

fn append_uds_log(w: &UdsWindow, line: &str) {
    let mut text = w.get_diag_log().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
    const CAP: usize = 20_000;
    if text.len() > CAP {
        let cut = text.len() - CAP;
        let cut = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
        text = text[cut..].to_string();
    }
    w.set_diag_log(text.into());
}

fn append_xcp_log(w: &XcpWindow, line: &str) {
    let mut text = w.get_diag_log().to_string();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(line);
    const CAP: usize = 20_000;
    if text.len() > CAP {
        let cut = text.len() - CAP;
        let cut = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
        text = text[cut..].to_string();
    }
    w.set_diag_log(text.into());
}

fn uds_response_note(data: &[u8]) -> String {
    if data.len() >= 4 && data[1] == 0x7F {
        return format!("UDS NRC service=0x{:02X} code=0x{:02X}", data[2], data[3]);
    }
    if data.len() >= 2 {
        return match data[1] {
            0x50 => "UDS positive: DiagnosticSessionControl".into(),
            0x51 => "UDS positive: ECUReset".into(),
            0x54 => "UDS positive: ClearDTC".into(),
            0x62 => "UDS positive: ReadDataByIdentifier".into(),
            0x67 => "UDS positive: SecurityAccess".into(),
            0x6E => "UDS positive: WriteDataByIdentifier".into(),
            0x71 => "UDS positive: RoutineControl".into(),
            0x7E => "UDS positive: TesterPresent".into(),
            _ => String::new(),
        };
    }
    String::new()
}

fn xcp_response_note(data: &[u8]) -> String {
    match data {
        [0xFF] => "XCP ACK".into(),
        [0xFE, rest @ ..] => format!(
            "XCP ERR {}",
            rest.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
        ),
        d if d.len() >= 8 && d[0] == 0xFF && d[1] == 0x10 => {
            format!("XCP CONNECT active={} target={}", d[2], d[3])
        }
        _ => String::new(),
    }
}

#[allow(dead_code)]
pub(crate) fn ota_observe_frame(w: &OtaWindow, f: &CanFrame) {
    let data_hex = f.data_hex();
    if data_hex.is_empty() {
        return;
    }

    let uds_tx = parse_u32(&w.get_uds_id());
    let uds_rx = parse_u32(&w.get_uds_rx_id()).or_else(|| uds_tx.map(|id| id + 8));
    if let Some(id) = uds_rx
        && !f.tx && f.id == id {
            let note = uds_response_note(&f.data);
            append_ota_log(w, &format!("RX UDS ID=0x{:X}  [{}]  {}", f.id, data_hex, note));
            return;
        }
    if let Some(id) = uds_tx
        && f.tx && f.id == id {
            append_ota_log(w, &format!("TX UDS ID=0x{:X}  [{}]", f.id, data_hex));
            return;
        }

    let xcp_tx = parse_u32(&w.get_xcp_id());
    let xcp_rx = parse_u32(&w.get_xcp_rx_id());
    let xcp_match = if let Some(id) = xcp_rx {
        !f.tx && f.id == id
    } else if let Some(id) = xcp_tx {
        ota::xcp::resolve_ids(id)
            .map(|ids| ota::xcp::response_id_matches(ids.response, f.id))
            .unwrap_or(false)
            && !f.tx
    } else {
        false
    };
    if xcp_match {
        let note = xcp_response_note(&f.data);
        append_ota_log(w, &format!("RX XCP ID=0x{:X}  [{}]  {}", f.id, data_hex, note));
    } else if let Some(id) = xcp_tx
        && f.tx && f.id == id {
            append_ota_log(w, &format!("TX XCP ID=0x{:X}  [{}]", f.id, data_hex));
        }
}

pub(crate) fn uds_observe_frame(w: &UdsWindow, f: &CanFrame) {
    let data_hex = f.data_hex();
    if data_hex.is_empty() {
        return;
    }

    let uds_tx = parse_u32(&w.get_uds_id());
    let uds_rx = parse_u32(&w.get_uds_rx_id()).or_else(|| uds_tx.map(|id| id + 8));
    if let Some(id) = uds_rx
        && !f.tx && f.id == id {
            let note = uds_response_note(&f.data);
            append_uds_log(w, &format!("RX UDS ID=0x{:X}  [{}]  {}", f.id, data_hex, note));
        }
}

pub(crate) fn xcp_observe_frame(w: &XcpWindow, f: &CanFrame) {
    let data_hex = f.data_hex();
    if data_hex.is_empty() {
        return;
    }

    let xcp_tx = parse_u32(&w.get_xcp_id());
    let xcp_rx = parse_u32(&w.get_xcp_rx_id());
    let xcp_match = if let Some(id) = xcp_rx {
        !f.tx && f.id == id
    } else if let Some(id) = xcp_tx {
        ota::xcp::resolve_ids(id)
            .map(|ids| ota::xcp::response_id_matches(ids.response, f.id))
            .unwrap_or(false)
            && !f.tx
    } else {
        false
    };
    if xcp_match {
        let note = xcp_response_note(&f.data);
        append_xcp_log(w, &format!("RX XCP ID=0x{:X}  [{}]  {}", f.id, data_hex, note));
    } else if let Some(id) = xcp_tx
        && f.tx && f.id == id {
            append_xcp_log(w, &format!("TX XCP ID=0x{:X}  [{}]", f.id, data_hex));
        }
}
