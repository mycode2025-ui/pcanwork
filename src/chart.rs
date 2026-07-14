//! Chart math + the chart-window renderer.
//! Pure helpers (interpolation, windowing, range) plus `refresh_chart`, which
//! turns the live/frozen series into the `ChartSeries` model and axis labels.
//! Extracted from main.rs. Chinese text below lives only in string literals (UI data).

use crate::{App, AppWindow, ChartSeries, ChartWindow, Series, fmt_wall};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::collections::VecDeque;
use std::rc::Rc;

/// Parse the leading numeric part of a string (e.g. "239.00A" -> 239.0).
pub(crate) fn parse_lead(s: &str) -> Option<f64> {
    let t: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    t.parse().ok()
}

/// Linear interpolation over an ascending (time, value) point list.
pub(crate) fn interp_pts(pts: &[(f64, f64)], t: f64) -> f64 {
    if pts.is_empty() {
        return 0.0;
    }
    if t <= pts[0].0 {
        return pts[0].1;
    }
    let last = pts[pts.len() - 1];
    if t >= last.0 {
        return last.1;
    }
    for w in pts.windows(2) {
        let (t0, v0) = w[0];
        let (t1, v1) = w[1];
        if t >= t0 && t <= t1 {
            let f = if (t1 - t0).abs() < 1e-12 {
                0.0
            } else {
                (t - t0) / (t1 - t0)
            };
            return v0 + (v1 - v0) * f;
        }
    }
    last.1
}

/// Take the samples inside the visible window [tmin, tmax] (plus one neighbor on each
/// side so the polyline reaches the borders), then decimate to <= max_pts points.
/// Used to recompute the visible region at full resolution after zoom; the path and
/// the cursor share these points. Samples must be in ascending time order.
pub(crate) fn window_pts(
    samples: &VecDeque<(f64, f64)>,
    tmin: f64,
    tmax: f64,
    max_pts: usize,
) -> Vec<(f64, f64)> {
    if samples.is_empty() {
        return Vec::new();
    }
    let v: Vec<(f64, f64)> = samples.iter().copied().collect();
    let start = v.partition_point(|&(t, _)| t < tmin);
    let end = v.partition_point(|&(t, _)| t <= tmax); // first index > tmax
    let a = start.saturating_sub(1); // include one extra neighbor on the left
    let b = (end + 1).min(v.len()); // include one extra neighbor on the right
    let slice = &v[a..b];
    if slice.is_empty() {
        return Vec::new();
    }
    let step = (slice.len() / max_pts.max(1)).max(1);
    let mut out: Vec<(f64, f64)> = slice.iter().step_by(step).copied().collect();
    if let Some(&last) = slice.last()
        && out.last() != Some(&last)
    {
        out.push(last); // ensure the window end point is present (cursor at right edge)
    }
    out
}

/// Time range covering all visible series (falls back to 0..1 when there is no data).
pub(crate) fn chart_full_range(series: &[Series]) -> (f64, f64) {
    let mut dmin = f64::MAX;
    let mut dmax = f64::MIN;
    for s in series.iter().filter(|s| s.visible) {
        for &(t, _) in &s.samples {
            dmin = dmin.min(t);
            dmax = dmax.max(t);
        }
    }
    if dmax <= dmin {
        (0.0, 1.0)
    } else {
        (dmin, dmax)
    }
}

fn playback_original_time_label(t: f64, precise: bool) -> String {
    if t > 946_684_800.0 {
        fmt_wall(t, false)
    } else if precise {
        format!("{t:.3}s")
    } else {
        format!("{t:.1}s")
    }
}

fn chart_time_label(a: &App, t: f64, rel_base: f64) -> String {
    match (a.chart_time_source, a.chart_time_mode) {
        (0, 1) => a
            .capture_wall_epoch
            .map(|epoch| fmt_wall(epoch + t, false))
            .unwrap_or_else(|| format!("{t:.1}s")),
        (1, 1) => playback_original_time_label(t, false),
        (1, _) => format!("{:.1}s", t - rel_base),
        _ => format!("{t:.1}s"),
    }
}

fn chart_cursor_time_label(a: &App, t: f64, rel_base: f64) -> String {
    match (a.chart_time_source, a.chart_time_mode) {
        (0, 1) => a
            .capture_wall_epoch
            .map(|epoch| fmt_wall(epoch + t, false))
            .unwrap_or_else(|| format!("{t:.3}s")),
        (1, 1) => playback_original_time_label(t, true),
        (1, _) => format!("{:.3}s", t - rel_base),
        _ => format!("{t:.3}s"),
    }
}

/// Rebuild the chart series model + axis labels and push to both windows.
pub(crate) fn refresh_chart(a: &App, ui: &AppWindow, chart_window: &ChartWindow) {
    ui.set_chart_paused(a.chart_paused);
    ui.set_chart_normalize(a.chart_normalize);
    ui.set_chart_cursor(a.chart_cursor);
    chart_window.set_chart_paused(a.chart_paused);
    chart_window.set_chart_cursor(a.chart_cursor);
    chart_window.set_chart_time_mode(a.chart_time_mode);
    chart_window.set_chart_sig_logging(a.sig_log.is_some());
    // Data source: when paused use the frozen snapshot (curves stay still while new
    // samples accumulate in the background unseen); otherwise the live series.
    let series_src: &[Series] = match (a.chart_paused, a.chart_frozen_series.as_ref()) {
        (true, Some(f)) => f.as_slice(),
        _ => &a.series,
    };
    // Full time range of the data (visible series only).
    let (dmin, dmax) = chart_full_range(series_src);
    let has = series_src.iter().any(|s| s.visible) && (dmax - dmin) > 0.0;
    // Visible window:
    // - paused: use the frozen window (absolute time, for measuring on frozen data)
    // - live: zoom only sets the window width; the right edge tracks the latest data
    //   (dmax) so new data keeps flowing in even when zoomed.
    let (mut tmin, mut tmax) = if a.chart_paused {
        a.chart_view.or(a.chart_pause_view).unwrap_or((dmin, dmax))
    } else if let Some((vmin, vmax)) = a.chart_view {
        let span = (vmax - vmin).clamp(1e-9, (dmax - dmin).max(1e-9));
        ((dmax - span).max(dmin), dmax)
    } else {
        (dmin, dmax)
    };
    // Clamp the window into the data range: a window wider than the data would squeeze
    // the waveform to the middle and break the cursor's whole-width frac->time mapping.
    if has {
        let w = (tmax - tmin).min(dmax - dmin).max(1e-9);
        tmin = tmin.max(dmin).min(dmax - w);
        tmax = tmin + w;
    }
    let tspan = (tmax - tmin).max(1e-9);
    // Highlighted signal (double-click on the project tree, valid for 2.5s).
    let hl = a
        .chart_highlight
        .as_ref()
        .filter(|(_, t)| t.elapsed().as_secs_f64() < 2.5)
        .map(|(n, _)| n.clone());
    let cursor_frac = chart_window.get_cursor_frac().clamp(0.0, 1.0) as f64;
    let cursor_time = tmin + cursor_frac * tspan;
    let cursor_frac2 = chart_window.get_cursor_frac2().clamp(0.0, 1.0) as f64;
    let cursor_time2 = tmin + cursor_frac2 * tspan;
    let dual = a.chart_dual;
    chart_window.set_chart_dual(dual);
    // aspect = plot-area width/height for one curve (Slint Path scales uniformly, so the
    // viewbox ratio must match to fill). Computed from the container pixel size Slint
    // reports + the visible-series count (avoids reading element sizes in Slint).
    let cont_w = chart_window.get_plot_w() as f64;
    let cont_h = chart_window.get_plot_h() as f64;
    let n_vis = series_src.iter().filter(|s| s.visible).count().max(1) as f64;
    let plot_w = (cont_w - 88.0).max(1.0); // minus right Y axis (86px) and border
    let row_h = (cont_h / n_vis).max(1.0); // row height = area height / visible count
    let aspect = (plot_w / row_h).clamp(0.2, 1000.0);
    chart_window.set_chart_aspect(aspect as f32);

    let rows: Vec<ChartSeries> = series_src
        .iter()
        .map(|s| {
            // Path and cursor share the same points so the cursor lands on the drawn curve.
            let pts = window_pts(&s.samples, tmin, tmax, 600);
            let mut smin = f64::MAX;
            let mut smax = f64::MIN;
            for &(_, v) in &pts {
                smin = smin.min(v);
                smax = smax.max(v);
            }
            let (dlo, dhi) = if pts.is_empty() {
                (0.0, 1.0)
            } else if smax <= smin {
                (smin - 1.0, smin + 1.0)
            } else {
                (smin, smax)
            };
            // Add 8% margin so the curve does not touch the edges / overflow the plot.
            let margin = (dhi - dlo) * 0.08;
            let lo = dlo - margin;
            let hi = dhi + margin;
            let span = (hi - lo).max(1e-9);
            let mut cmd = String::new();
            for (k, &(t, v)) in pts.iter().enumerate() {
                let x = (t - tmin) / tspan * 100.0 * aspect;
                let y = 100.0 - (v - lo) / span * 100.0;
                if k == 0 {
                    cmd.push_str(&format!("M {x:.2} {y:.2} "));
                } else {
                    cmd.push_str(&format!("L {x:.2} {y:.2} "));
                }
            }
            // Cursor value: linear interpolation on the decimated polyline at cursor_time.
            let cursor_val = if a.chart_cursor && has && !pts.is_empty() {
                Some(interp_pts(&pts, cursor_time))
            } else {
                None
            };
            let (cursor_value, cursor_y, cursor_valid) = match cursor_val {
                Some(v) => (
                    format!("{:.2}{}", v, s.unit),
                    (100.0 - (v - lo) / span * 100.0).clamp(0.0, 100.0) as f32,
                    true,
                ),
                None => (String::new(), 0.0, false),
            };
            // Second cursor value (dual-cursor mode).
            let cursor_val2 = if a.chart_cursor && dual && has && !pts.is_empty() {
                Some(interp_pts(&pts, cursor_time2))
            } else {
                None
            };
            let (cursor2_value, cursor2_y, cursor2_valid) = match cursor_val2 {
                Some(v) => (
                    format!("{:.2}{}", v, s.unit),
                    (100.0 - (v - lo) / span * 100.0).clamp(0.0, 100.0) as f32,
                    true,
                ),
                None => (String::new(), 0.0, false),
            };
            ChartSeries {
                name: s.name.clone().into(),
                commands: cmd.into(),
                clr: s.color,
                cur: format!("{:.2}", s.cur).into(),
                unit: s.unit.clone().into(),
                axis_max: format!("{dhi:.2}").into(),
                axis_mid: format!("{:.2}", (dlo + dhi) / 2.0).into(),
                axis_min: format!("{dlo:.2}").into(),
                cursor_value: cursor_value.into(),
                cursor_y,
                cursor_valid,
                cursor2_value: cursor2_value.into(),
                cursor2_y,
                cursor2_valid,
                visible: s.visible,
                highlight: hl.as_deref() == Some(s.name.as_str()),
                smin: if pts.is_empty() {
                    "-".into()
                } else {
                    format!("{smin:.2}").into()
                },
                smax: if pts.is_empty() {
                    "-".into()
                } else {
                    format!("{smax:.2}").into()
                },
            }
        })
        .collect();
    // Cursor readout (matches the curve points exactly: reuse each curve's interpolated value).
    let readout = if a.chart_cursor && has && dual {
        let dt = (cursor_time2 - cursor_time).abs();
        let cursor_time_text = chart_cursor_time_label(a, cursor_time, dmin);
        let cursor_time2_text = chart_cursor_time_label(a, cursor_time2, dmin);
        let mut parts = vec![format!(
            "dt={dt:.3}s  (cursor1 {cursor_time_text} / cursor2 {cursor_time2_text})"
        )];
        for r in &rows {
            if r.visible
                && r.cursor_valid
                && r.cursor2_valid
                && let (Some(v1), Some(v2)) = (
                    parse_lead(r.cursor_value.as_str()),
                    parse_lead(r.cursor2_value.as_str()),
                )
            {
                parts.push(format!(
                    "{}: {} -> {}  d{:.2}",
                    r.name,
                    r.cursor_value,
                    r.cursor2_value,
                    v2 - v1
                ))
            }
        }
        parts.join("   |   ")
    } else if a.chart_cursor && has {
        let mut parts = vec![format!(
            "t={}",
            chart_cursor_time_label(a, cursor_time, dmin)
        )];
        for r in &rows {
            if r.visible && r.cursor_valid {
                parts.push(format!("{}={}", r.name, r.cursor_value));
            }
        }
        parts.join("  |  ")
    } else {
        String::new()
    };
    // Update the resident chart model in place (avoid swapping the model each frame).
    {
        let m = &a.chart_model;
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

    // Axis labels.
    let has = series_src.iter().any(|s| s.visible) && tspan > 0.0;
    ui.set_chart_ymax_label("".into());
    ui.set_chart_ymid_label("".into());
    ui.set_chart_ymin_label("".into());
    if has {
        let chart_xstart = chart_time_label(a, tmin, dmin);
        let chart_xend = chart_time_label(a, tmax, dmin);
        ui.set_chart_xstart_label(format!("{tmin:.1}s").into());
        ui.set_chart_xend_label(format!("{tmax:.1}s").into());
        chart_window.set_chart_xstart_label(chart_xstart.into());
        chart_window.set_chart_xend_label(chart_xend.into());
        // 5 evenly spaced time tick labels.
        let xlabels: Vec<SharedString> = (0..5)
            .map(|k| chart_time_label(a, tmin + (tmax - tmin) * (k as f64) / 4.0, dmin).into())
            .collect();
        chart_window.set_chart_xlabels(ModelRc::from(Rc::new(VecModel::from(xlabels))));
    } else {
        ui.set_chart_xstart_label("".into());
        ui.set_chart_xend_label("".into());
        chart_window.set_chart_xstart_label("".into());
        chart_window.set_chart_xend_label("".into());
        chart_window.set_chart_xlabels(ModelRc::from(Rc::new(VecModel::from(
            Vec::<SharedString>::new(),
        ))));
    }

    // Cursor readout.
    ui.set_chart_cursor_readout(readout.clone().into());
    chart_window.set_chart_cursor_readout(readout.into());
}
