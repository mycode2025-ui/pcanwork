//! Send-task (TX) helpers: build a task from the form, inline field edits, signal value
//! get/set, VaryMode <-> UI conversion, the signal-info panel, and the task-list model.
//! Extracted from main.rs. Chinese text below lives only in user-facing string literals.

use crate::{
    fmtf, id_str, parse_tx_bytes, parse_u32, vary, App, AppWindow, TxRow, TxSigRow, TxTask,
    TxWindow,
};
use slint::{ModelRc, SharedString, VecModel};
use std::rc::Rc;

/// Build a `TxTask` from the "normal send" form fields.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tx_task_from_form(
    a: &mut App,
    ch_s: &str,
    id_s: &str,
    kind_s: &str,
    fd_s: &str,
    brs_s: &str,
    dlc_s: &str,
    data_s: &str,
    period_s: &str,
    name_s: &str,
    remote_flag: bool,
) -> Result<TxTask, String> {
    let id = parse_u32(id_s).ok_or_else(|| format!("无效 ID: {id_s}"))?;
    let ch_txt = ch_s
        .trim()
        .trim_start_matches("CAN")
        .trim_start_matches("can");
    let ch = ch_txt.parse::<u8>().unwrap_or(1).max(1);
    let ext = kind_s.eq_ignore_ascii_case("ext") || kind_s.contains("扩展");
    let fd =
        fd_s.eq_ignore_ascii_case("yes") || fd_s.eq_ignore_ascii_case("fd") || fd_s.contains("FD");
    let brs = brs_s.eq_ignore_ascii_case("yes")
        || brs_s.eq_ignore_ascii_case("brs")
        || brs_s.contains("BRS");
    let brs = brs && fd; // BRS only meaningful for CAN FD
    let remote = remote_flag && !fd; // RTR remote frame: classic CAN only
    let max_len = if fd { 64 } else { 8 };
    let dlc = dlc_s
        .trim()
        .parse::<usize>()
        .unwrap_or(max_len.min(8))
        .min(max_len);
    let mut data = parse_tx_bytes(data_s, max_len);
    data.resize(dlc, 0);
    let period_ms = period_s
        .trim()
        .trim_end_matches("ms")
        .trim_end_matches("MS")
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let handle = a.next_handle;
    a.next_handle += 1;
    let name = if name_s.trim().is_empty() {
        format!("Tx_{}", a.txs.len() + 1)
    } else {
        name_s.trim().to_string()
    };
    Ok(TxTask {
        name,
        ch,
        id,
        ext,
        fd,
        brs,
        remote,
        data,
        periodic: false,
        period_ms: period_ms.max(1),
        repeat: -1,
        sent: 0,
        handle,
        dbc_id: None,
        sig_values: Vec::new(),
        varies: Vec::new(),
    })
}

/// Apply an inline edit to one field of the task at `row` (list cell editing).
pub(crate) fn update_tx_task(a: &mut App, row: i32, field: &str, value: &str) {
    let Some(t) = a.txs.get_mut(row as usize) else {
        return;
    };
    let value = value.trim();
    match field {
        "name" => t.name = value.to_string(),
        "ch" => {
            let v = value.trim_start_matches("CAN").trim_start_matches("can");
            if let Ok(ch) = v.parse::<u8>() {
                t.ch = ch.max(1);
            }
        }
        "id" => {
            if let Some(id) = parse_u32(value) {
                t.id = id;
            }
        }
        "type" => t.ext = value.eq_ignore_ascii_case("ext"),
        "fd" => t.fd = value.eq_ignore_ascii_case("yes") || value == "1",
        "brs" => t.brs = value.eq_ignore_ascii_case("yes") || value == "1",
        "dlc" => {
            if let Ok(dlc) = value.parse::<usize>() {
                let len = dlc.min(if t.fd { 64 } else { 8 });
                t.data.resize(len, 0);
            }
        }
        "data" => {
            let compact = value.replace([',', ';'], " ");
            let mut data = Vec::new();
            let parts: Vec<&str> = compact.split_whitespace().collect();
            if parts.len() == 1 && parts[0].len() > 2 {
                let s = parts[0].trim_start_matches("0x").trim_start_matches("0X");
                for chunk in s.as_bytes().chunks(2) {
                    if chunk.len() == 2
                        && let Ok(hex) = std::str::from_utf8(chunk)
                            && let Ok(b) = u8::from_str_radix(hex, 16) {
                                data.push(b);
                            }
                }
            } else {
                for p in parts {
                    if let Ok(b) =
                        u8::from_str_radix(p.trim_start_matches("0x").trim_start_matches("0X"), 16)
                    {
                        data.push(b);
                    }
                }
            }
            let max_len = if t.fd { 64 } else { 8 };
            data.truncate(max_len);
            if !data.is_empty() {
                t.data = data;
            }
        }
        "period" => {
            let v = value
                .trim_end_matches("ms")
                .trim_end_matches("MS")
                .trim()
                .parse::<u64>();
            if let Ok(ms) = v {
                t.period_ms = ms.max(1);
            }
        }
        "repeat" => {
            // Send count: -1 / empty / <=0 -> infinite loop; N>=1 -> send N times.
            if value.is_empty() {
                t.repeat = -1;
            } else if let Ok(n) = value.parse::<i64>() {
                t.repeat = if n <= 0 { -1 } else { n };
            }
        }
        _ => {}
    }
}

/// Resolve the selected task's `sig_idx`-th signal to (name, factor, offset).
pub(crate) fn selected_signal(a: &App, sig_idx: i32) -> Option<(String, f64, f64)> {
    let sel = a.tx_sel;
    if sel < 0 || sel as usize >= a.txs.len() || sig_idx < 0 {
        return None;
    }
    let id = a.txs[sel as usize].dbc_id?;
    let m = a.dbc_message(id)?;
    let s = m.signals.get(sig_idx as usize)?;
    Some((s.name.clone(), s.factor, s.offset))
}

/// Set a signal's edited actual (physical) value on the selected task.
pub(crate) fn set_signal_value(a: &mut App, name: &str, phys: f64) {
    let sel = a.tx_sel;
    if sel < 0 || sel as usize >= a.txs.len() {
        return;
    }
    {
        let t = &mut a.txs[sel as usize];
        if let Some(e) = t.sig_values.iter_mut().find(|(n, _)| n == name) {
            e.1 = phys;
        } else {
            t.sig_values.push((name.to_string(), phys));
        }
    }
    a.tx_sig_cache = u64::MAX;
}

/// Load a VaryMode into the UI's (mode index, 6 param strings, sequence string).
pub(crate) fn vary_to_ui(m: Option<&vary::VaryMode>) -> (i32, [String; 6], String) {
    let e = String::new;
    let f = |x: f64| fmtf(x);
    match m {
        Some(vary::VaryMode::Arithmetic { diff, lo, hi }) => {
            (1, [f(*diff), f(*lo), f(*hi), e(), e(), e()], e())
        }
        Some(vary::VaryMode::Geometric { init, ratio, lo, hi }) => {
            (2, [f(*init), f(*ratio), f(*lo), f(*hi), e(), e()], e())
        }
        Some(vary::VaryMode::Sine { amp, omega, phase, offset, lo, hi }) => (
            3,
            [f(*amp), f(*omega), f(*phase), f(*offset), f(*lo), f(*hi)],
            e(),
        ),
        Some(vary::VaryMode::Triangle { period, amp, h_off, v_off, lo, hi }) => (
            4,
            [f(*period), f(*amp), f(*h_off), f(*v_off), f(*lo), f(*hi)],
            e(),
        ),
        Some(vary::VaryMode::Rect { period, duty, high, low }) => {
            (5, [f(*period), f(*duty), f(*high), f(*low), e(), e()], e())
        }
        Some(vary::VaryMode::Random { lo, hi }) => (6, [f(*lo), f(*hi), e(), e(), e(), e()], e()),
        Some(vary::VaryMode::Sequence { values }) => (
            7,
            [e(), e(), e(), e(), e(), e()],
            values.iter().map(|v| fmtf(*v)).collect::<Vec<_>>().join(", "),
        ),
        _ => (0, [e(), e(), e(), e(), e(), e()], e()),
    }
}

/// Build a VaryMode from the UI's mode index + param strings + sequence text.
pub(crate) fn ui_to_vary(mode: i32, p: &[String; 6], seq: &str) -> vary::VaryMode {
    let pf = |i: usize| p[i].trim().parse::<f64>().unwrap_or(0.0);
    match mode {
        1 => vary::VaryMode::Arithmetic { diff: pf(0), lo: pf(1), hi: pf(2) },
        2 => vary::VaryMode::Geometric { init: pf(0), ratio: pf(1), lo: pf(2), hi: pf(3) },
        3 => vary::VaryMode::Sine {
            amp: pf(0),
            omega: pf(1),
            phase: pf(2),
            offset: pf(3),
            lo: pf(4),
            hi: pf(5),
        },
        4 => vary::VaryMode::Triangle {
            period: pf(0),
            amp: pf(1),
            h_off: pf(2),
            v_off: pf(3),
            lo: pf(4),
            hi: pf(5),
        },
        5 => vary::VaryMode::Rect { period: pf(0), duty: pf(1), high: pf(2), low: pf(3) },
        6 => vary::VaryMode::Random { lo: pf(0), hi: pf(1) },
        7 => vary::VaryMode::Sequence { values: vary::parse_sequence(seq) },
        _ => vary::VaryMode::None,
    }
}

/// Fill the signal-info + signal-value-variation panels for the selected task's i-th signal.
pub(crate) fn populate_sig_panel(a: &App, w: &TxWindow, sig_idx: i32) {
    let sel = a.tx_sel;
    if sel < 0 || sel as usize >= a.txs.len() || sig_idx < 0 {
        w.set_tx_sel_sig_name("".into());
        return;
    }
    let t = &a.txs[sel as usize];
    let sig = t
        .dbc_id
        .and_then(|id| a.dbc_message(id))
        .and_then(|m| m.signals.get(sig_idx as usize));
    let Some(s) = sig else {
        w.set_tx_sel_sig_name("".into());
        return;
    };
    let phys = t
        .sig_values
        .iter()
        .find(|(n, _)| n == &s.name)
        .map(|(_, v)| *v)
        .unwrap_or(0.0);
    let raw = if s.factor != 0.0 {
        ((phys - s.offset) / s.factor).round()
    } else {
        0.0
    };
    w.set_tx_sel_sig_name(s.name.clone().into());
    w.set_tx_sel_sig_phys(fmtf(phys).into());
    w.set_tx_sel_sig_raw(fmtf(raw).into());
    w.set_tx_sel_sig_unit(s.unit.clone().into());
    w.set_tx_sel_sig_factor(fmtf(s.factor).into());
    w.set_tx_sel_sig_offset(fmtf(s.offset).into());
    w.set_tx_sel_sig_startbit(s.start_bit.to_string().into());
    w.set_tx_sel_sig_length(s.size.to_string().into());
    w.set_tx_sel_sig_comment(if a.lang_en { format!("Valid range [{}, {}]", fmtf(s.min), fmtf(s.max)) } else { format!("有效范围 [{}, {}]", fmtf(s.min), fmtf(s.max)) }.into());
    let vm = t.varies.iter().find(|v| v.signal == s.name).map(|v| &v.mode);
    let (mi, p, seq) = vary_to_ui(vm);
    w.set_tx_vary_mode(mi);
    w.set_tx_vp1(p[0].clone().into());
    w.set_tx_vp2(p[1].clone().into());
    w.set_tx_vp3(p[2].clone().into());
    w.set_tx_vp4(p[3].clone().into());
    w.set_tx_vp5(p[4].clone().into());
    w.set_tx_vp6(p[5].clone().into());
    w.set_tx_vseq(seq.into());
}

/// Build the send-task display rows from the current tasks.
pub(crate) fn build_tx_rows(a: &App) -> Vec<TxRow> {
    let mut rows: Vec<TxRow> = Vec::new();
    for t in &a.txs {
        rows.push(TxRow {
            enabled: true,
            checked: a.tx_checked.contains(&t.handle),
            name: t.name.clone().into(),
            ch: format!("CAN{}", t.ch).into(),
            id: id_str(t.id, t.ext).into(),
            dbcname: a.dbc_message_name(t.id).unwrap_or("").into(),
            kind: if t.ext { "Ext" } else { "Std" }.into(),
            fd: if t.fd { "Yes" } else { "No" }.into(),
            brs: if t.brs { "Yes" } else { "No" }.into(),
            dlc: t.data.len().to_string().into(),
            data: t
                .data
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
                .into(),
            mode: if t.periodic { "Periodic" } else { "Single" }.into(),
            period: format!("{}", t.period_ms).into(),
            count: if t.repeat < 0 { "-1".into() } else { t.repeat.to_string().into() },
            sent: t.sent.to_string().into(),
            status: if t.periodic { "Running" } else { "Stopped" }.into(),
            running: t.periodic,
        });
    }
    rows
}

/// Signature of everything shown in the send-task list (so we only rebuild on change).
pub(crate) fn tx_list_sig(a: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.txs.len().hash(&mut h);
    for t in &a.txs {
        t.id.hash(&mut h);
        t.ch.hash(&mut h);
        t.ext.hash(&mut h);
        t.fd.hash(&mut h);
        t.brs.hash(&mut h);
        t.periodic.hash(&mut h);
        t.sent.hash(&mut h);
        t.period_ms.hash(&mut h);
        t.repeat.hash(&mut h);
        t.name.hash(&mut h);
        t.data.hash(&mut h);
        a.tx_checked.contains(&t.handle).hash(&mut h);
    }
    h.finish()
}

/// Rebuild and push the send-task list model to both windows, and record its signature.
pub(crate) fn push_tx_list(a: &mut App, ui: &AppWindow, tx_window: &TxWindow) {
    a.tx_list_cache = tx_list_sig(a);
    let model = ModelRc::from(Rc::new(VecModel::from(build_tx_rows(a))));
    ui.set_txs(model.clone());
    tx_window.set_txs(model);
}

/// Refresh the DBC-send page: the message-picker list (left, rebuilt only when the merged
/// DBC message set changes) and the signal editor (right, rebuilt only when the selected
/// task or its signal values change — so the user's in-progress edits aren't overwritten).
pub(crate) fn build_tx_dbc_page(a: &mut App, tx_window: &TxWindow) {
    let en = a.lang_en;
    // Message-picker list: merge messages across all DBCs (dedup by id, first loaded wins).
    let mut order: Vec<(u32, String)> = Vec::new();
    let mut picker: Vec<SharedString> = Vec::new();
    {
        let mut msgs: Vec<(u32, String, usize)> = Vec::new();
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for d in &a.dbcs {
            for m in d.messages() {
                if seen.insert(m.id) {
                    msgs.push((m.id, m.name.clone(), d.message_len(m.id)));
                }
            }
        }
        msgs.sort_by_key(|(id, _, _)| *id);
        for (id, name, len) in msgs {
            order.push((id, name.clone()));
            picker.push(format!("{name}  0x{id:X}  {len}B").into());
        }
    }
    a.tx_dbc_order = order;
    {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        picker.hash(&mut h);
        let sig = h.finish();
        if sig != a.tx_msgs_cache {
            a.tx_msgs_cache = sig;
            tx_window.set_tx_dbc_msgs(ModelRc::from(Rc::new(VecModel::from(picker))));
        }
    }
    tx_window.set_tx_sel(a.tx_sel);

    // Signal editor: gate the rebuild on (selection + bound id + signal values) so typing
    // into a field isn't clobbered by the per-frame refresh.
    let sel = a.tx_sel;
    let mut title = if en { "No DBC send task selected".to_string() } else { "未选择 DBC 发送任务".to_string() };
    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        sel.hash(&mut h);
        if sel >= 0 && (sel as usize) < a.txs.len() {
            let t = &a.txs[sel as usize];
            t.dbc_id.hash(&mut h);
            for (n, v) in &t.sig_values {
                n.hash(&mut h);
                v.to_bits().hash(&mut h);
            }
        }
        h.finish()
    };
    if sig != a.tx_sig_cache {
        a.tx_sig_cache = sig;
        let mut sig_rows: Vec<TxSigRow> = Vec::new();
        if sel >= 0 && (sel as usize) < a.txs.len() {
            let t = &a.txs[sel as usize];
            if let Some(id) = t.dbc_id {
                let name = a.dbc_message_name(id).unwrap_or("");
                title = if en { format!("Current: 0x{id:X} {name}") } else { format!("当前: 0x{id:X} {name}") };
                if let Some(m) = a.dbc_message(id) {
                    for s in &m.signals {
                        let phys = t
                            .sig_values
                            .iter()
                            .find(|(n, _)| n == &s.name)
                            .map(|(_, v)| *v)
                            .unwrap_or(0.0);
                        let raw = ((phys - s.offset) / s.factor).round() as i64;
                        sig_rows.push(TxSigRow {
                            signal: s.name.clone().into(),
                            phys: fmtf(phys).into(),
                            raw: raw.to_string().into(),
                            unit: s.unit.clone().into(),
                            factor: fmtf(s.factor).into(),
                            offset: fmtf(s.offset).into(),
                            startbit: s.start_bit.to_string().into(),
                            length: s.size.to_string().into(),
                            factor_num: s.factor as f32,
                            offset_num: s.offset as f32,
                        });
                    }
                }
            } else {
                title = if en { "Selected task is not a DBC message (no signals to edit)".to_string() } else { "选中任务不是 DBC 报文（无信号可编辑）".to_string() };
            }
        }
        tx_window.set_tx_sel_title(title.into());
        tx_window.set_tx_dbc_sigs(ModelRc::from(Rc::new(VecModel::from(sig_rows))));
    }
}
