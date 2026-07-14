//! Simulation panel helpers: signal generation, per-tick stepping, row/model build,
//! property fill, and DBC signal choices. Extracted from main.rs.

use crate::can::{CanFrame, Cmd};
use crate::dbc::DbcDb;
use crate::{App, GenMode, SimKind, SimPropWindow, SimRow, SimWidget, fmtf, key_of};
use slint::Model;

/// Current generator value for a SignalGen widget at its tick counter.
pub(crate) fn sim_gen_value(w: &SimWidget) -> f64 {
    match w.gen_mode {
        GenMode::Constant => w.min,
        GenMode::Ramp => {
            // Triangle wave: sweep back and forth within [min, max].
            let span = (w.max - w.min).abs().max(1e-9);
            let step = w.gen_step.abs().max(1e-9);
            let pos = (w.tick as f64 * step) % (2.0 * span);
            if pos <= span {
                w.min + pos
            } else {
                w.min + 2.0 * span - pos
            }
        }
        GenMode::Sine => w.min + (w.max - w.min) * (0.5 + 0.5 * (w.tick as f64 * 0.2).sin()),
    }
}

/// Build one frame and send it (encode via DBC signal if given, else write byte0).
pub(crate) fn sim_send(a: &App, ch: u8, id: u32, signal: &str, val: f64) {
    let data = if !signal.is_empty() {
        let mut m = std::collections::HashMap::new();
        m.insert(signal.to_string(), val);
        a.dbc_encode(id, &m).unwrap_or_else(|| {
            let mut d = vec![0u8; 8];
            d[0] = val.clamp(0.0, 255.0) as u8;
            d
        })
    } else {
        let mut d = vec![0u8; 8];
        d[0] = val.clamp(0.0, 255.0) as u8;
        d
    };
    let f = CanFrame {
        t: 0.0,
        ch: ch.max(1),
        tx: true,
        id,
        ext: id > 0x7FF,
        fd: false,
        brs: false,
        remote: false,
        error: false,
        data,
    };
    let _ = a.cmd.send(Cmd::SendOnce(f));
}

/// Sim panel step (every ~100ms): in run mode signal generators fire periodically;
/// RX widgets always read the latest value.
pub(crate) fn sim_tick(a: &mut App) {
    let now = std::time::Instant::now();
    // TX: signal generators (auto-send only in run mode)
    if a.sim_running {
        for i in 0..a.sim_widgets.len() {
            {
                let w = &a.sim_widgets[i];
                if !w.enabled || w.kind != SimKind::SignalGen {
                    continue;
                }
                let due = w.last_fire.is_none_or(|t| {
                    now.duration_since(t).as_millis() >= w.period_ms.max(10) as u128
                });
                if !due {
                    continue;
                }
            }
            a.sim_widgets[i].tick = a.sim_widgets[i].tick.wrapping_add(1);
            a.sim_widgets[i].last_fire = Some(now);
            let (ch, val, id, signal) = {
                let w = &a.sim_widgets[i];
                (w.channel, sim_gen_value(w), w.frame_id, w.signal.clone())
            };
            sim_send(a, ch, id, &signal, val);
        }
    }
    // RX: indicator / dial / bar / numeric read the latest value
    for i in 0..a.sim_widgets.len() {
        let kind = a.sim_widgets[i].kind;
        if !matches!(
            kind,
            SimKind::Indicator | SimKind::Dial | SimKind::Bar | SimKind::Numeric
        ) {
            continue;
        }
        let id = a.sim_widgets[i].frame_id;
        let sig = a.sim_widgets[i].signal.clone();
        let key = key_of(a.sim_widgets[i].channel, false, id);
        let data = a.last.get(&key).map(|li| li.data.clone());
        if let Some(data) = data
            && let Some(v) = sim_decode_value(&a.dbcs, id, &sig, &data)
        {
            a.sim_widgets[i].cur = v;
        }
    }
}

/// Decode a widget's value from a frame: empty signal -> byte0, else DBC physical value.
pub(crate) fn sim_decode_value(dbcs: &[DbcDb], id: u32, signal: &str, data: &[u8]) -> Option<f64> {
    if signal.is_empty() {
        return data.first().map(|b| *b as f64);
    }
    for d in dbcs {
        if let Some(dec) = d.decode(id, data).into_iter().find(|x| x.name == signal) {
            return Some(dec.physical);
        }
    }
    None
}

/// Build the display row for one widget.
pub(crate) fn sim_make_row(w: &SimWidget, selected: bool, primary: bool) -> SimRow {
    let info = if w.signal.is_empty() {
        format!("0x{:X}", w.frame_id)
    } else {
        format!("0x{:X}/{}", w.frame_id, w.signal)
    };
    let span = (w.max - w.min).abs().max(1e-9);
    let level = match w.kind {
        SimKind::Dial | SimKind::Bar => ((w.cur - w.min) / span).clamp(0.0, 1.0) as f32,
        SimKind::Slider => ((w.slider_val - w.min) / span).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    };
    let status = match w.kind {
        SimKind::Numeric | SimKind::Dial | SimKind::Bar => format!("{:.2}", w.cur),
        SimKind::Slider => format!("{:.1}", w.slider_val),
        SimKind::SignalGen => format!("{:.1}", sim_gen_value(w)),
        _ => String::new(),
    };
    SimRow {
        kind: w.kind.to_i32(),
        name: w.name.clone().into(),
        info: info.into(),
        status: status.into(),
        x: w.x as f32,
        y: w.y as f32,
        w: w.w as f32,
        h: w.h as f32,
        on: w.kind == SimKind::Indicator && w.cur > w.threshold,
        level,
        selected,
        primary,
        align: w.align,
    }
}

/// Build the sim panel rows and update the resident model in place (canvas pos/size/selection).
pub(crate) fn refresh_sim(a: &App) {
    let m = &a.sim_model;
    while m.row_count() > a.sim_widgets.len() {
        m.remove(m.row_count() - 1);
    }
    for (i, w) in a.sim_widgets.iter().enumerate() {
        let row = sim_make_row(w, a.sim_multi.contains(&(i as i32)), a.sim_sel == i as i32);
        if i < m.row_count() {
            m.set_row_data(i, row);
        } else {
            m.push(row);
        }
    }
}

/// Update a single sim row in place.
pub(crate) fn sim_set_row(a: &App, i: usize) {
    if i < a.sim_widgets.len() && i < a.sim_model.row_count() {
        let row = sim_make_row(
            &a.sim_widgets[i],
            a.sim_multi.contains(&(i as i32)),
            a.sim_sel == i as i32,
        );
        a.sim_model.set_row_data(i, row);
    }
}

/// Fill the standalone property window (p-* properties) from the selected widget.
pub(crate) fn sim_fill_props(win: &SimPropWindow, w: &SimWidget) {
    win.set_has_sel(true);
    win.set_p_name(w.name.clone().into());
    win.set_p_frame(format!("{:X}", w.frame_id).into());
    win.set_p_signal(w.signal.clone().into());
    win.set_p_min(fmtf(w.min).into());
    win.set_p_max(fmtf(w.max).into());
    win.set_p_threshold(fmtf(w.threshold).into());
    win.set_p_genmode(match w.gen_mode {
        GenMode::Constant => 0,
        GenMode::Ramp => 1,
        GenMode::Sine => 2,
    });
    win.set_p_step(fmtf(w.gen_step).into());
    win.set_p_period(w.period_ms.to_string().into());
    win.set_p_x(format!("{:.0}", w.x).into());
    win.set_p_y(format!("{:.0}", w.y).into());
    win.set_p_w(format!("{:.0}", w.w).into());
    win.set_p_h(format!("{:.0}", w.h).into());
    win.set_p_align(w.align);
    win.set_p_kind(w.kind.to_i32());
    win.set_p_chan(w.channel.to_string().into());
    win.set_p_pressval(fmtf(w.press_val).into());
    win.set_p_releaseval(fmtf(w.release_val).into());
    win.set_p_bind(if w.signal.is_empty() {
        format!("CAN{} 0x{:X} / byte0", w.channel, w.frame_id).into()
    } else {
        format!("CAN{} 0x{:X} / {}", w.channel, w.frame_id, w.signal).into()
    });
}

/// Look up the [min, max] range of (id, signal) in the loaded DBCs (only when max > min).
pub(crate) fn sim_signal_range(a: &App, id: u32, signal: &str) -> Option<(f64, f64)> {
    if signal.is_empty() {
        return None;
    }
    for d in &a.dbcs {
        if let Some(m) = d.message(id)
            && let Some(s) = m.signals.iter().find(|s| s.name == signal)
            && s.max > s.min
        {
            return Some((s.min, s.max));
        }
    }
    None
}

/// Build the signal dropdown list for the sim panel (display string + (id, signal) option),
/// from all loaded DBCs.
pub(crate) fn sim_signal_choices(a: &App) -> (Vec<(u32, String)>, Vec<slint::SharedString>) {
    let mut choices: Vec<(u32, String)> = Vec::new();
    let mut rows: Vec<slint::SharedString> = Vec::new();
    let mut seen: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();
    for d in &a.dbcs {
        for m in d.messages() {
            for s in &m.signals {
                if seen.insert((m.id, s.name.clone())) {
                    rows.push(format!("0x{:X} {} / {}", m.id, m.name, s.name).into());
                    choices.push((m.id, s.name.clone()));
                }
            }
        }
    }
    (choices, rows)
}
