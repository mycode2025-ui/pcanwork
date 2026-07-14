//! Renderers for the signal-selector tree and the statistics tables.
//! Extracted from main.rs. Chinese text below lives only in string literals (UI data).

use crate::dbc::MessageDef;
use crate::{
    App, AppWindow, ChanStat, IdStat, LastInfo, SigRow, SignalPickItem, SignalPickRow,
    SignalSelectWindow, fmtf,
};
use slint::{ModelRc, VecModel};
use std::rc::Rc;

/// Rebuild the signal-selector window's tree (DBC -> messages -> signals) and summary.
pub(crate) fn refresh_signal_picker(a: &mut App, signal_window: &SignalSelectWindow) {
    let en = a.lang_en;
    let mut rows: Vec<SignalPickRow> = Vec::new();
    let mut items: Vec<SignalPickItem> = Vec::new();
    let filter_text = a.signal_pick_filter.trim().to_ascii_lowercase();
    let filtering = !filter_text.is_empty();
    let selected = a.signal_pick_selected.clone();

    // Append a picker row plus its backing item (kept index-aligned).
    #[allow(clippy::too_many_arguments)]
    fn push(
        rows: &mut Vec<SignalPickRow>,
        items: &mut Vec<SignalPickItem>,
        item: SignalPickItem,
        level: i32,
        name: String,
        desc: String,
        kind: &str,
        expandable: bool,
        expanded: bool,
        selectable: bool,
        selected: bool,
    ) {
        rows.push(SignalPickRow {
            level,
            name: name.into(),
            desc: desc.into(),
            kind: kind.into(),
            expandable,
            expanded,
            selectable,
            selected,
        });
        items.push(item);
    }

    signal_window.set_sig_cat(a.sig_cat);
    if a.sig_cat == 3 {
        // ---- Expression Variables: 列出用户定义的派生信号 ----
        signal_window.set_signal_pick_dbc_name(if en {
            "Expression".into()
        } else {
            "表达式".into()
        });
        for ev in &a.expr_vars {
            if filtering && !ev.name.to_ascii_lowercase().contains(&filter_text) {
                continue;
            }
            let sel = a.signal_pick_expr_selected.as_deref() == Some(ev.name.as_str());
            let unit = if ev.unit.is_empty() {
                String::new()
            } else {
                format!(" [{}]", ev.unit)
            };
            push(
                &mut rows,
                &mut items,
                SignalPickItem::ExprVar(ev.name.clone()),
                0,
                ev.name.clone(),
                format!("= {}{}", ev.formula, unit),
                "curve",
                false,
                false,
                true,
                sel,
            );
        }
        signal_window.set_signal_pick_summary(
            if en {
                format!("Expressions: {}", a.expr_vars.len())
            } else {
                format!("表达式: {} 个", a.expr_vars.len())
            }
            .into(),
        );
    } else if a.sig_cat != 0 {
        // ---- System / User Defined / Function Signals: 占位(未实现) ----
        let label = match a.sig_cat {
            1 => "System Variables",
            2 => "User Defined Variables",
            _ => "Function Signals",
        };
        signal_window.set_signal_pick_dbc_name(label.into());
        push(
            &mut rows,
            &mut items,
            SignalPickItem::DbcRoot,
            0,
            if en {
                "(not implemented)".to_string()
            } else {
                "(未实现)".to_string()
            },
            if en {
                "This category is a placeholder".to_string()
            } else {
                "该分类暂未实现".to_string()
            },
            "folder",
            false,
            false,
            false,
            false,
        );
        signal_window.set_signal_pick_summary("".into());
    } else if a.dbc_loaded() {
        let dbc_label = match a.dbcs.len() {
            1 => a.dbcs[0].file_name.clone(),
            n => {
                if en {
                    format!("{} +{} more", a.dbcs[0].file_name, n - 1)
                } else {
                    format!("{} 等 {n} 个", a.dbcs[0].file_name)
                }
            }
        };
        signal_window.set_signal_pick_dbc_name(dbc_label.clone().into());
        push(
            &mut rows,
            &mut items,
            SignalPickItem::DbcRoot,
            0,
            dbc_label.clone(),
            if en {
                "Mapped to: CAN 1".to_string()
            } else {
                "关联至: CAN 1".to_string()
            },
            "dbc",
            true,
            a.signal_pick_root_open,
            false,
            false,
        );

        if a.signal_pick_root_open {
            push(
                &mut rows,
                &mut items,
                SignalPickItem::MessagesRoot,
                1,
                "Messages".to_string(),
                "".to_string(),
                "folder",
                true,
                a.signal_pick_messages_open,
                false,
                false,
            );
        }

        let mut visible_signals = 0usize;
        if a.signal_pick_root_open && a.signal_pick_messages_open {
            // Merge messages from all DBCs (dedup by id, first loaded wins).
            let mut messages: Vec<&MessageDef> = Vec::new();
            let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for d in &a.dbcs {
                for m in d.messages() {
                    if seen.insert(m.id) {
                        messages.push(m);
                    }
                }
            }
            messages.sort_by_key(|m| m.id);
            for m in messages {
                let id_hex = format!("0x{:X}", m.id);
                let msg_label = format!("{} ({id_hex})", m.name);
                let msg_match = filtering
                    && (m.name.to_ascii_lowercase().contains(&filter_text)
                        || id_hex.to_ascii_lowercase().contains(&filter_text));
                let matching_signals: Vec<_> = m
                    .signals
                    .iter()
                    .filter(|s| {
                        !filtering
                            || msg_match
                            || s.name.to_ascii_lowercase().contains(&filter_text)
                            || s.unit.to_ascii_lowercase().contains(&filter_text)
                    })
                    .collect();
                if filtering && matching_signals.is_empty() && !msg_match {
                    continue;
                }

                let msg_open = filtering || a.signal_pick_msg_expanded.contains(&m.id);
                push(
                    &mut rows,
                    &mut items,
                    SignalPickItem::Message(m.id),
                    2,
                    msg_label,
                    if en {
                        format!("{} signals", m.signals.len())
                    } else {
                        format!("信号 {}", m.signals.len())
                    },
                    "message",
                    !m.signals.is_empty(),
                    msg_open,
                    false,
                    false,
                );

                if msg_open {
                    let signal_iter: Vec<_> = if filtering && !msg_match {
                        matching_signals
                    } else {
                        m.signals.iter().collect()
                    };
                    for s in signal_iter {
                        visible_signals += 1;
                        let is_selected = selected
                            .as_ref()
                            .map(|(id, sig)| *id == m.id && sig == &s.name)
                            .unwrap_or(false);
                        let unit = if s.unit.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", s.unit)
                        };
                        push(
                            &mut rows,
                            &mut items,
                            SignalPickItem::Signal(m.id, s.name.clone()),
                            3,
                            s.name.clone(),
                            format!(
                                "start {} len {} factor {} offset {}{}",
                                s.start_bit,
                                s.size,
                                fmtf(s.factor),
                                fmtf(s.offset),
                                unit
                            ),
                            "signal",
                            false,
                            false,
                            true,
                            is_selected,
                        );
                    }
                }
            }
        }

        let summary = match selected {
            Some((id, signal)) => {
                if en {
                    format!("Selected: groups 1, signals 1  0x{id:X} / {signal}")
                } else {
                    format!("已选中: 分组 1, 信号 1  0x{id:X} / {signal}")
                }
            }
            None => {
                if en {
                    format!("Selected: groups 0, signals 0    visible signals {visible_signals}")
                } else {
                    format!("已选中: 分组 0, 信号 0    可见信号 {visible_signals}")
                }
            }
        };
        signal_window.set_signal_pick_summary(summary.into());
    } else {
        signal_window.set_signal_pick_dbc_name(if en { "(none)".into() } else { "(无)".into() });
        push(
            &mut rows,
            &mut items,
            SignalPickItem::DbcRoot,
            0,
            if en {
                "(no DBC loaded)".to_string()
            } else {
                "(未加载 DBC)".to_string()
            },
            if en {
                "Load a DBC file first".to_string()
            } else {
                "请先加载 DBC 文件".to_string()
            },
            "dbc",
            false,
            false,
            false,
            false,
        );
        signal_window.set_signal_pick_summary(if en {
            "Selected: groups 0, signals 0".into()
        } else {
            "已选中: 分组 0, 信号 0".into()
        });
    }

    a.signal_pick_items = items;
    signal_window.set_signal_pick_filter(a.signal_pick_filter.clone().into());

    // 仅在内容变化时重建 ListView 模型: 全局渲染每 100ms 调本函数, 若每次都 set 新 VecModel,
    // ListView 的委托会被反复销毁重建 → 点击选中/悬停高亮瞬间被重置, 视觉上"闪烁"。
    // 用所有行字段算一个签名, 与上次相同则跳过 set。
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rows.len().hash(&mut hasher);
    for r in &rows {
        r.level.hash(&mut hasher);
        r.name.as_str().hash(&mut hasher);
        r.desc.as_str().hash(&mut hasher);
        r.kind.as_str().hash(&mut hasher);
        r.expandable.hash(&mut hasher);
        r.expanded.hash(&mut hasher);
        r.selectable.hash(&mut hasher);
        r.selected.hash(&mut hasher);
    }
    let sig = hasher.finish();
    if sig != a.signal_pick_cache {
        a.signal_pick_cache = sig;
        signal_window.set_signal_pick_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
}

/// Rebuild the per-channel and per-ID statistics tables.
pub(crate) fn build_stats(a: &App, ui: &AppWindow) {
    let active_ids = a.last.len();
    let chan = vec![ChanStat {
        ch: "CAN1".into(),
        state: if a.running { "Running" } else { "Stopped" }.into(),
        rx: a.rx.to_string().into(),
        tx: a.tx.to_string().into(),
        err: a.err.to_string().into(),
        load: format!("{:.1}%", a.bus_load).into(),
        fps: format!("{:.0}", a.fps).into(),
        active_ids: active_ids.to_string().into(),
    }];
    ui.set_chan_stats(ModelRc::from(Rc::new(VecModel::from(chan))));

    let mut ids: Vec<(&u64, &LastInfo)> = a.last.iter().collect();
    ids.sort_by_key(|(k, _)| **k & 0xFFFF_FFFF);
    // "现在"时刻：实时采集时用墙钟(总线全静默也能判超时)，否则用最新帧时间(回放/停止)。
    let max_t = a.last.values().map(|li| li.t).fold(0.0_f64, f64::max);
    let now_t = match (a.running, a.capture_wall_epoch) {
        (true, Some(epoch)) => {
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(max_t + epoch);
            (wall - epoch).max(max_t)
        }
        _ => max_t,
    };
    let mut id_rows: Vec<IdStat> = Vec::new();
    for (k, li) in ids {
        let id = (*k & 0xFFFF_FFFF) as u32;
        let ch = ((*k >> 40) & 0xFF) as u8;
        // 真均值 = 周期累加 / 周期个数(count-1)，而非 min/max 中点。
        let avg = if li.count > 1 {
            li.sum_cycle / (li.count - 1) as f64
        } else {
            0.0
        };
        // Timed out: has a stable cycle and no refresh for > 3x avg cycle (at least 50ms).
        let timed_out = avg > 0.0 && (now_t - li.t) > (avg * 3.0).max(0.05);
        id_rows.push(IdStat {
            ch: format!("CAN{}", if ch == 0 { 1 } else { ch }).into(),
            id: format!("0x{id:X}").into(),
            name: {
                // tx 帧(发送/回显)在统计里加 (Tx) 标记，与同 ID 的 rx 行区分。
                let base = a.dbc_message_name(id).unwrap_or("");
                if (*k >> 39) & 1 == 1 {
                    if base.is_empty() {
                        "(Tx)".into()
                    } else {
                        format!("{base} (Tx)").into()
                    }
                } else {
                    base.into()
                }
            },
            count: li.count.to_string().into(),
            avg_cycle: format!("{:.0}ms", avg * 1000.0).into(),
            min: if li.min_cycle < f64::MAX {
                format!("{:.0}ms", li.min_cycle * 1000.0).into()
            } else {
                "-".into()
            },
            max: format!("{:.0}ms", li.max_cycle * 1000.0).into(),
            last_time: format!("{:.3}", li.t).into(),
            timeout: if timed_out {
                if a.lang_en { "Yes" } else { "是" }
            } else if a.lang_en {
                "No"
            } else {
                "否"
            }
            .into(),
        });
    }
    ui.set_id_stats(ModelRc::from(Rc::new(VecModel::from(id_rows))));
}

/// Rebuild the signal-parse panel for the currently selected message: decode its latest
/// frame via the DBC and list each signal (raw / physical / unit / range / status).
/// `a.sig_panel` is kept index-aligned so "add to chart" can map a row back to (id, signal).
pub(crate) fn build_signal_panel(a: &mut App, ui: &AppWindow) {
    let en = a.lang_en;
    let mut sig_rows: Vec<SigRow> = Vec::new();
    a.sig_panel.clear();
    let mut sig_title = if en {
        "No message selected".to_string()
    } else {
        "未选择报文".to_string()
    };
    if let Some(k) = a.selected_key
        && let Some(li) = a.last.get(&k)
    {
        let id = (k & 0xFFFF_FFFF) as u32;
        let data = li.data.clone();
        let count = li.count;
        let name = a.dbc_message_name(id).unwrap_or("").to_string();
        sig_title = if en {
            format!(
                "Current: 0x{id:X} {name} / Len={} / Count={count}",
                data.len()
            )
        } else {
            format!(
                "当前报文: 0x{id:X} {name} / Len={} / Count={count}",
                data.len()
            )
        };
        if !a.dbc_loaded() {
            sig_title = if en {
                "DBC not loaded".into()
            } else {
                "未加载 DBC".into()
            };
        } else {
            let decoded = a.dbc_decode(id, &data);
            if decoded.is_empty() {
                sig_title = if en {
                    format!("0x{id:X} message not matched in DBC")
                } else {
                    format!("0x{id:X} 当前报文未匹配 DBC")
                };
            }
            for s in decoded {
                a.sig_panel.push((id, s.name.clone()));
                sig_rows.push(SigRow {
                    signal: s.name.into(),
                    raw: s.raw.to_string().into(),
                    physical: format!("{:.3}", s.physical).into(),
                    unit: s.unit.into(),
                    min: fmtf(s.min).into(),
                    max: fmtf(s.max).into(),
                    enum_txt: s.enum_txt.into(),
                    startbit: s.start_bit.to_string().into(),
                    length: s.size.to_string().into(),
                    byteorder: if s.little_endian { "Intel" } else { "Motorola" }.into(),
                    signed: if s.signed { "Yes" } else { "No" }.into(),
                    factor: fmtf(s.factor).into(),
                    offset: fmtf(s.offset).into(),
                    status: if s.out_of_range {
                        if en { "Out of range" } else { "越界" }
                    } else {
                        "Normal"
                    }
                    .into(),
                    out_of_range: s.out_of_range,
                });
            }
        }
    }
    ui.set_sig_title(sig_title.into());
    ui.set_sigs(ModelRc::from(Rc::new(VecModel::from(sig_rows))));
}
