//! Message-table rendering: row builders, sort comparator, render signature, and the
//! per-frame table rebuild (`build_msg_table`). Extracted from main.rs.

use crate::dbc::Decoded;
use crate::{
    App, AppWindow, ByteCell, DISPLAY_CAP, DisplayItem, FrameRec, MsgRow, fmt_wall, id_str,
};
use slint::{Model, ModelRc, VecModel};
use std::collections::HashMap;
use std::rc::Rc;

fn sort_decoded_for_display(signals: &mut [Decoded]) {
    signals.sort_by(|left, right| {
        left.start_bit
            .cmp(&right.start_bit)
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Build a message-table row from a frame record.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_msgrow(
    r: &FrameRec,
    hot: &[bool],
    can_expand: bool,
    expanded: bool,
    now_t: f64,
    mode_trace: bool,
    time_mode: i32,
    capture_wall_epoch: Option<f64>,
) -> MsgRow {
    let age = now_t - r.t;
    // Trace mode: briefly highlight a just-arrived frame (the new top row flashes).
    let is_new = mode_trace && (0.0..0.15).contains(&age);
    // Grouped mode: an ID not updated for ~3x its cycle is flagged as timed out.
    let timeout = !mode_trace && r.delta > 0.0 && age > (r.delta * 3.0).max(0.1);
    let frame_type = if r.error {
        "Error"
    } else if r.remote {
        "Remote"
    } else {
        "Data"
    };
    let cells: Vec<ByteCell> = r
        .data
        .iter()
        .enumerate()
        .map(|(i, b)| ByteCell {
            hex: format!("{b:02X}").into(),
            hot: hot.get(i).copied().unwrap_or(false),
        })
        .collect();
    // pre-format hex with a newline every 16 bytes, used to wrap long CAN FD payloads
    let data_text: String = r
        .data
        .chunks(16)
        .map(|chunk| {
            chunk
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    MsgRow {
        no: r.no.to_string().into(),
        time: match time_mode {
            1 => match capture_wall_epoch {
                Some(e) => fmt_wall(e + r.t, false),
                None => format!("{:.6}", r.t),
            },
            2 => match capture_wall_epoch {
                Some(e) => fmt_wall(e + r.t, true),
                None => format!("{:.6}", r.t),
            },
            _ => format!("{:.6}", r.t),
        }
        .into(),
        delta: if r.delta > 0.0 {
            format!("{:.6}", r.delta).into()
        } else {
            "".into()
        },
        ch: format!("CAN{}", r.ch).into(),
        dir: if r.tx { "Tx" } else { "Rx" }.into(),
        id: id_str(r.id, r.ext).into(),
        name: r.name.clone().into(),
        kind: if r.ext { "Ext" } else { "Std" }.into(),
        fd: if r.fd { "Yes" } else { "No" }.into(),
        brs: if r.brs { "Yes" } else { "No" }.into(),
        dlc: format!("{}", r.data.len()).into(),
        len: format!("{}", r.data.len()).into(),
        data_bytes: ModelRc::from(Rc::new(VecModel::from(cells))),
        data_text: data_text.into(),
        cycle: if r.delta > 0.0 {
            format!("{:.1}ms", r.delta * 1000.0).into()
        } else {
            "".into()
        },
        count: format!("{}", r.count).into(),
        comment: frame_type.into(),
        is_tx: r.tx,
        is_error: r.error,
        is_new,
        timeout,
        is_signal: false,
        can_expand,
        expanded,
        signal_name: "".into(),
        signal_out_of_range: false,
    }
}

/// Build an expanded-signal sub-row (shown under its message in grouped mode).
pub(crate) fn make_signal_row(s: &Decoded, has_valid_value: bool) -> MsgRow {
    let mut value_text = if has_valid_value {
        let mut text = format!("{:.3}", s.physical);
        if !s.unit.is_empty() {
            text.push(' ');
            text.push_str(&s.unit);
        }
        text.push_str("  (Raw: ");
        text.push_str(&s.raw_text);
        text.push(')');
        if !s.enum_txt.is_empty() {
            text.push_str("  ");
            text.push_str(&s.enum_txt);
        }
        text
    } else {
        "—".to_string()
    };
    if let Some(mux) = s.mux_value {
        if s.mux_active {
            value_text.push_str(&format!("  [MUX={mux} 当前]"));
        } else if has_valid_value {
            value_text.push_str(&format!("  [MUX={mux} 上次值]"));
        } else {
            value_text.push_str(&format!("  [MUX={mux} 等待]"));
        }
    }

    MsgRow {
        no: "".into(),
        time: "".into(),
        delta: "".into(),
        ch: "".into(),
        dir: "".into(),
        id: "".into(),
        name: format!("  {}", s.name).into(),
        kind: "Sig".into(),
        fd: "".into(),
        brs: "".into(),
        dlc: "".into(),
        len: "".into(),
        data_bytes: ModelRc::from(Rc::new(VecModel::from(Vec::<ByteCell>::new()))),
        data_text: value_text.into(),
        cycle: "".into(),
        count: format!("Bit {}:{}", s.start_bit, s.size).into(),
        comment: if !s.mux_active && s.mux_value.is_some() {
            "MuxInactive"
        } else if s.out_of_range {
            "OutOfRange"
        } else {
            "Normal"
        }
        .into(),
        is_tx: false,
        is_error: false,
        is_new: false,
        timeout: false,
        is_signal: true,
        can_expand: false,
        expanded: false,
        signal_name: s.name.clone().into(),
        signal_out_of_range: s.out_of_range,
    }
}

/// Sort comparator: `col` matches the table header column index.
pub(crate) fn cmp_rec(a: &FrameRec, b: &FrameRec, col: i32) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match col {
        0 => a.no.cmp(&b.no),
        1 => a.t.partial_cmp(&b.t).unwrap_or(Ordering::Equal),
        2 => a.delta.partial_cmp(&b.delta).unwrap_or(Ordering::Equal),
        3 => a.ch.cmp(&b.ch),
        4 => a.tx.cmp(&b.tx),
        5 => a.id.cmp(&b.id),
        6 => a.name.cmp(&b.name),
        7 => a.ext.cmp(&b.ext),
        10 | 11 => a.data.len().cmp(&b.data.len()),
        14 => a.count.cmp(&b.count),
        _ => Ordering::Equal,
    }
}

/// Render signature: changes whenever any input affecting the table changes, else the
/// whole-table rebuild is skipped.
pub(crate) fn msg_view_signature(a: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.no_counter.hash(&mut h); // new frame arrived
    a.trace.len().hash(&mut h); // cleared / ring-buffer eviction
    a.mode_trace.hash(&mut h);
    a.sort_col.hash(&mut h);
    a.sort_desc.hash(&mut h);
    // filter conditions
    a.filter.allow.hash(&mut h);
    a.filter.deny.hash(&mut h);
    a.filter.name.hash(&mut h);
    a.filter.name_exclude.hash(&mut h);
    a.filter.name_prefix.hash(&mut h);
    a.filter.name_suffix.hash(&mut h);
    a.filter.data.hash(&mut h);
    a.filter.dir_filter.hash(&mut h); // 方向过滤(此前漏入签名→工程加载只改方向时表不刷新)
    a.time_mode.hash(&mut h); // 时间显示模式(相对/绝对/系统)切换需重建
    // expanded set
    a.expanded_keys.len().hash(&mut h);
    let exp_sum: u64 = a
        .expanded_keys
        .iter()
        .fold(0u64, |acc, k| acc ^ k.wrapping_mul(0x9E3779B97F4A7C15));
    exp_sum.hash(&mut h);
    // whether a DBC is loaded (affects the Name column and expandability)
    a.dbcs.len().hash(&mut h);
    // 实时采集时混入 ~500ms 粗时间桶，使总线全静默期也能刷新"超时/陈旧"判断。
    if a.running
        && let Some(epoch) = a.capture_wall_epoch
        && let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
    {
        (((d.as_secs_f64() - epoch) * 2.0) as i64).hash(&mut h);
    }
    h.finish()
}

fn should_rebuild(paused: bool, current_signature: u64, previous_signature: u64) -> bool {
    let forced = previous_signature == u64::MAX;
    forced || (!paused && current_signature != previous_signature)
}

/// Rebuild the central message table when data changes, or when a UI action explicitly
/// requests a refresh. Pause freezes incoming-data refreshes but must not block filter/reset
/// and clear operations.
pub(crate) fn build_msg_table(a: &mut App, ui: &AppWindow) {
    let msg_sig = msg_view_signature(a);
    if !should_rebuild(a.paused, msg_sig, a.last_msg_sig) {
        return;
    }
    a.last_msg_sig = msg_sig;
    let (rows, items, shown) = {
        let mut recs: Vec<&FrameRec> = Vec::new();
        if a.mode_trace {
            for r in a.trace.iter().rev() {
                if a.filter.accept(r.id, &r.name, &r.data, r.tx) {
                    recs.push(r);
                    if recs.len() >= DISPLAY_CAP {
                        break;
                    }
                }
            }
            recs.reverse();
        } else {
            let mut latest: HashMap<u64, &FrameRec> = HashMap::new();
            let mut order: Vec<u64> = Vec::new();
            for r in a.trace.iter() {
                if a.filter.accept(r.id, &r.name, &r.data, r.tx)
                    && latest.insert(r.key, r).is_none()
                {
                    order.push(r.key);
                }
            }
            order.sort();
            for k in order {
                recs.push(latest[&k]);
            }
        }
        // sorting (only applied to the current display set)
        if a.sort_col >= 0 {
            let col = a.sort_col;
            let desc = a.sort_desc;
            recs.sort_by(|x, y| {
                let o = cmp_rec(x, y, col);
                if desc { o.reverse() } else { o }
            });
        }
        let mut rows = Vec::with_capacity(recs.len());
        let mut items = Vec::with_capacity(recs.len());
        // "现在"时刻：实时采集用墙钟(总线静默也能判超时/陈旧)，否则用最新帧时间。
        let max_t = a.last.values().map(|li| li.t).fold(0.0_f64, f64::max);
        let now_t = match (a.running, a.capture_wall_epoch) {
            (true, Some(epoch)) => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| (d.as_secs_f64() - epoch).max(max_t))
                .unwrap_or(max_t),
            _ => max_t,
        };
        for r in &recs {
            let hot: Vec<bool> = if a.mode_trace {
                r.changed_mask.clone()
            } else {
                a.last
                    .get(&r.key)
                    .map(|li| {
                        li.byte_change_t
                            .iter()
                            .map(|&ct| {
                                let dt = r.t - ct;
                                (0.0..0.5).contains(&dt)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            // Cheap message-name lookup to decide expandability (avoid full decode per row/frame).
            let can_expand = !a.mode_trace && a.dbc_message_name_frame(r.id, r.ext).is_some();
            let expanded = can_expand && a.expanded_keys.contains(&r.key);
            rows.push(make_msgrow(
                r,
                &hot,
                can_expand,
                expanded,
                now_t,
                a.mode_trace,
                a.time_mode,
                a.capture_wall_epoch,
            ));
            items.push(DisplayItem::Message(r.key));
            if expanded {
                // Only truly-expanded rows are decoded (rare).
                let mut decoded = a.dbc_decode_all_frame(r.id, r.ext, &r.data);
                sort_decoded_for_display(&mut decoded);
                for current in decoded
                    .iter()
                    .filter(|signal| signal.mux_active && signal.mux_value.is_some())
                {
                    a.expanded_signal_cache
                        .insert((r.key, current.name.clone()), current.clone());
                }
                for current in decoded {
                    let (display, has_valid_value) = if current.mux_active {
                        (current, true)
                    } else if let Some(previous) = a
                        .expanded_signal_cache
                        .get(&(r.key, current.name.clone()))
                        .cloned()
                    {
                        let mut previous = previous;
                        previous.mux_active = false;
                        (previous, true)
                    } else {
                        (current, false)
                    };
                    items.push(DisplayItem::Signal {
                        key: r.key,
                        signal: display.name.clone(),
                    });
                    rows.push(make_signal_row(&display, has_valid_value));
                }
            }
        }
        let shown = rows.len();
        (rows, items, shown)
    };
    a.display_items = items;
    ui.set_shown_count(shown.to_string().into());
    ui.set_sort_col(a.sort_col);
    ui.set_sort_desc(a.sort_desc);
    // Update the resident model in place: align row count + per-row set_row_data,
    // preserving row delegates and click areas.
    let m = &a.msg_model;
    while m.row_count() > rows.len() {
        m.remove(m.row_count() - 1);
    }
    for (i, row) in rows.into_iter().enumerate() {
        if i < m.row_count() {
            m.set_row_data(i, row);
        } else {
            m.push(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_signal_places_value_in_data_column() {
        let decoded = Decoded {
            name: "PackCurrent".into(),
            raw: 125,
            raw_unsigned: Some(125),
            raw_text: "125".into(),
            physical: 12.5,
            unit: "A".into(),
            min: -100.0,
            max: 100.0,
            start_bit: 16,
            size: 16,
            little_endian: true,
            signed: true,
            factor: 0.1,
            offset: 0.0,
            out_of_range: false,
            enum_txt: String::new(),
            mux_active: true,
            mux_value: None,
        };

        let row = make_signal_row(&decoded, true);
        assert!(row.is_signal);
        assert_eq!(row.no.as_str(), "");
        assert_eq!(row.data_text.as_str(), "12.500 A  (Raw: 125)");
        assert_eq!(row.dlc.as_str(), "");
        assert_eq!(row.len.as_str(), "");
        assert_eq!(row.count.as_str(), "Bit 16:16");
    }

    #[test]
    fn explicit_refresh_is_not_blocked_while_paused() {
        assert!(should_rebuild(true, 42, u64::MAX));
        assert!(!should_rebuild(true, 42, 41));
        assert!(should_rebuild(false, 42, 41));
        assert!(!should_rebuild(false, 42, 42));
    }

    #[test]
    fn expanded_signals_sort_by_start_bit_then_name() {
        let make = |name: &str, start_bit: u64| Decoded {
            name: name.into(),
            raw: 0,
            raw_unsigned: Some(0),
            raw_text: "0".into(),
            physical: 0.0,
            unit: String::new(),
            min: 0.0,
            max: 0.0,
            start_bit,
            size: 1,
            little_endian: true,
            signed: false,
            factor: 1.0,
            offset: 0.0,
            out_of_range: false,
            enum_txt: String::new(),
            mux_active: true,
            mux_value: None,
        };
        let mut signals = vec![make("Z", 24), make("B", 8), make("A", 8), make("M", 0)];
        sort_decoded_for_display(&mut signals);
        assert_eq!(
            signals
                .iter()
                .map(|signal| signal.name.as_str())
                .collect::<Vec<_>>(),
            vec!["M", "A", "B", "Z"]
        );
    }
}
