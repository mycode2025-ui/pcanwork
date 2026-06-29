// Event-wiring for the wire_chart. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_chart(
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
        chart_window.on_chart_toggle_pause(move || {
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
        let cw = chart_window.as_weak();
        chart_window.on_chart_topmost_toggle(move |on| {
            if let Some(w) = cw.upgrade() {
                set_window_topmost(w.window(), on);
            }
        });
    }
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let chartw = chart_window.as_weak();
        chart_window.on_chart_set_time_mode(move |mode| {
            let mut a = app.borrow_mut();
            a.chart_time_mode = mode.clamp(0, 1);
            let (Some(ui), Some(cw)) = (uiw.upgrade(), chartw.upgrade()) else {
                return;
            };
            refresh_chart(&a, &ui, &cw);
        });
    }
    {
        let app = app.clone();
        chart_window.on_clear_chart(move || {
            let mut a = app.borrow_mut();
            a.series.clear();
            a.chart_view = None;
            a.log("已清空曲线");
        });
    }
    // 时间轴滚轮缩
{
        let app = app.clone();
        chart_window.on_chart_zoom(move |delta, frac| {
            let mut a = app.borrow_mut();
            // 当前窗口（未缩放则取「当前显示的数据集」全程：暂停时用冻结快照，否则用实时数据
let (mut vmin, mut vmax) = a.chart_view.unwrap_or_else(|| {
                let src: &[Series] = if a.chart_paused {
                    a.chart_frozen_series.as_deref().unwrap_or(&a.series)
                } else {
                    &a.series
                };
                chart_full_range(src)
            });
            let span = (vmax - vmin).max(1e-6);
            let center = vmin + (frac as f64).clamp(0.0, 1.0) * span;
            // delta<0（向上滚）放大，>0 缩小
            let factor = if delta < 0.0 { 0.6 } else { 1.67 };
            // 数据范围：窗口不得超出数据，否则波形挤在中间、游标按整宽走就对不
let src: &[Series] = if a.chart_paused {
                a.chart_frozen_series.as_deref().unwrap_or(&a.series)
            } else {
                &a.series
            };
            let (dmin, dmax) = chart_full_range(src);
            let data_span = (dmax - dmin).max(1e-6);
            let mut new_span = (span * factor).clamp(0.02, data_span); // 不能比数据全程还
new_span = new_span.min(data_span);
            vmin = center - (frac as f64) * new_span;
            vmax = vmin + new_span;
            // 平移回数据范围内，保证波形始终铺满绘图区宽度
            if vmin < dmin {
                vmin = dmin;
                vmax = dmin + new_span;
            }
            if vmax > dmax {
                vmax = dmax;
                vmin = dmax - new_span;
            }
            a.chart_view = Some((vmin, vmax));
        });
    }
    // 适应（重置缩放为全程
{
        let app = app.clone();
        chart_window.on_chart_fit(move || {
            app.borrow_mut().chart_view = None;
        });
    }
    {
        let app = app.clone();
        chart_window.on_chart_cursor_toggle(move || {
            let mut a = app.borrow_mut();
            a.chart_cursor = !a.chart_cursor;
        });
    }
    {
        let app = app.clone();
        chart_window.on_chart_dual_toggle(move || {
            let mut a = app.borrow_mut();
            a.chart_dual = !a.chart_dual;
            if a.chart_dual {
                a.chart_cursor = true; // 双游标需先有游标
            }
        });
    }
    // 游标随鼠标移动即时重算（不等 100ms 定时器，消除拖动滞后
{
        let app = app.clone();
        let uiw = ui.as_weak();
        let chartw = chart_window.as_weak();
        chart_window.on_chart_cursor_move(move || {
            let (Some(ui), Some(cw)) = (uiw.upgrade(), chartw.upgrade()) else {
                return;
            };
            let a = app.borrow();
            refresh_chart(&a, &ui, &cw);
        });
    }
    {
        let app = app.clone();
        chart_window.on_chart_toggle_series(move |i| {
            let mut a = app.borrow_mut();
            if let Some(s) = a.series.get_mut(i as usize) {
                s.visible = !s.visible;
            }
        });
    }
    {
        let app = app.clone();
        chart_window.on_chart_remove_series(move |i| {
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
        let cww = chart_window.as_weak();
        chart_window.on_chart_export_csv(move || {
            if app.borrow().series.is_empty() {
                app.borrow_mut().log("曲线为空，无可导出数据".to_string());
                return;
            }
            let app = app.clone();
            let cww = cww.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("chart_data.csv");
                if let Some(w) = cww.upgrade() {
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
    // 宽表导出：时间为行、每个信号一列，按全体时间戳并集对齐（保持最近值）
    {
        let app = app.clone();
        let cww = chart_window.as_weak();
        chart_window.on_chart_export_wide_csv(move || {
            if app.borrow().series.is_empty() {
                app.borrow_mut().log("曲线为空，无可导出数据".to_string());
                return;
            }
            let app = app.clone();
            let cww = cww.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("chart_wide.csv");
                if let Some(w) = cww.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                let mut a = app.borrow_mut();
                // 1) 所有信号时间戳的有序并集（去重），上限保护
                let mut ts: Vec<f64> = a
                    .series
                    .iter()
                    .flat_map(|s| s.samples.iter().map(|&(t, _)| t))
                    .collect();
                ts.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                ts.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
                const MAX_ROWS: usize = 200_000;
                let truncated = ts.len() > MAX_ROWS;
                if truncated {
                    ts.truncate(MAX_ROWS);
                }
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        let mut w = std::io::BufWriter::new(f);
                        // 表头：Time + 每个信号名(单位)
                        let mut header = String::from("Time");
                        for s in &a.series {
                            if s.unit.is_empty() {
                                header.push_str(&format!(",{}", s.name));
                            } else {
                                header.push_str(&format!(",{}({})", s.name, s.unit));
                            }
                        }
                        let _ = writeln!(w, "{header}");
                        // 每个信号一个游标，沿时间前进取最近已知
let mut idx = vec![0usize; a.series.len()];
                        let mut last: Vec<Option<f64>> = vec![None; a.series.len()];
                        for &t in &ts {
                            let mut line = format!("{t:.6}");
                            for (si, s) in a.series.iter().enumerate() {
                                let sm = &s.samples;
                                while idx[si] < sm.len() && sm[idx[si]].0 <= t + 1e-9 {
                                    last[si] = Some(sm[idx[si]].1);
                                    idx[si] += 1;
                                }
                                match last[si] {
                                    Some(v) => line.push_str(&format!(",{v}")),
                                    None => line.push(','),
                                }
                            }
                            let _ = writeln!(w, "{line}");
                        }
                        let note = if truncated {
                            format!("（已截断至 {MAX_ROWS} 行）")
                        } else {
                            String::new()
                        };
                        let nsig = a.series.len();
                        let nrow = ts.len();
                        a.log(format!(
                            "曲线宽表已导出({nsig}信号 × {nrow}行){note}: {}",
                            path.display()
                        ));
                    }
                    Err(e) => a.log(format!("导出失败: {e}")),
                }
            });
        });
    }
    {
        let app = app.clone();
        let cww = chart_window.as_weak();
        chart_window.on_chart_sig_log_toggle(move || {
            {
                let mut a = app.borrow_mut();
                if a.sig_log.is_some() {
                    // 停止记录
                    if let Some(mut w) = a.sig_log.take() {
                        let _ = w.flush();
                    }
                    a.log("已停止信号记录".to_string());
                    return;
                }
                if a.series.is_empty() {
                    a.log("请先把要记录的信号加入曲线".to_string());
                    return;
                }
            }
            // 异步对话框（见 toggle_record 注释）。
            let app = app.clone();
            let cww = cww.clone();
            let _ = slint::spawn_local(async move {
                let mut dlg = rfd::AsyncFileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("signal_log.csv");
                if let Some(w) = cww.upgrade() {
                    dlg = dlg.set_parent(&w.window().window_handle());
                }
                let Some(file) = dlg.save_file().await else { return };
                let path = file.path().to_path_buf();
                let mut a = app.borrow_mut();
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        let mut w = std::io::BufWriter::new(f);
                        let _ = writeln!(w, "Time,Signal,Value,Unit");
                        let nsig = a.series.len();
                        a.sig_log = Some(w);
                        a.log(format!(
                            "开始信号记录(流式): {} —— 记录曲线中 {nsig} 个信号",
                            path.display()
                        ));
                    }
                    Err(e) => a.log(format!("无法创建记录文件: {e}")),
                }
            });
        });
    }
    {
        let app = app.clone();
        signal_window.on_signal_pick_search(move |s| {
            let mut a = app.borrow_mut();
            a.signal_pick_filter = s.to_string();
            if !a.signal_pick_filter.trim().is_empty() {
                a.signal_pick_root_open = true;
                a.signal_pick_messages_open = true;
            }
        });
    }
    {
        let app = app.clone();
        let picker = signal_window.as_weak();
        signal_window.on_signal_pick_row_clicked(move |i| {
            let mut a = app.borrow_mut();
            let Some(item) = a.signal_pick_items.get(i as usize).cloned() else {
                return;
            };
            match item {
                SignalPickItem::DbcRoot => a.signal_pick_root_open = !a.signal_pick_root_open,
                SignalPickItem::MessagesRoot => {
                    a.signal_pick_messages_open = !a.signal_pick_messages_open
                }
                SignalPickItem::Message(id) => {
                    if !a.signal_pick_msg_expanded.insert(id) {
                        a.signal_pick_msg_expanded.remove(&id);
                    }
                }
                SignalPickItem::Signal(id, signal) => a.signal_pick_selected = Some((id, signal)),
                SignalPickItem::ExprVar(name) => {
                    a.signal_pick_expr_selected = Some(name.clone());
                    // 选中即把该表达式填进编辑栏, 方便修改
                    if let Some(w) = picker.upgrade()
                        && let Some(ev) = a.expr_vars.iter().find(|e| e.name == name)
                    {
                        w.set_expr_name(ev.name.clone().into());
                        w.set_expr_formula(ev.formula.clone().into());
                        w.set_expr_unit(ev.unit.clone().into());
                        w.set_expr_error("".into());
                    }
                }
            }
        });
    }
    {
        let app = app.clone();
        signal_window.on_signal_pick_row_double_clicked(move |i| {
            let mut a = app.borrow_mut();
            let Some(item) = a.signal_pick_items.get(i as usize).cloned() else {
                return;
            };
            match item {
                SignalPickItem::Signal(id, signal) => {
                    a.signal_pick_selected = Some((id, signal.clone()));
                    let msg = add_signal_to_chart(&mut a, id, &signal);
                    a.log(msg);
                }
                SignalPickItem::ExprVar(name) => {
                    a.signal_pick_expr_selected = Some(name.clone());
                    let msg = add_expr_to_chart(&mut a, &name);
                    a.log(msg);
                }
                SignalPickItem::Message(id) => {
                    if !a.signal_pick_msg_expanded.insert(id) {
                        a.signal_pick_msg_expanded.remove(&id);
                    }
                }
                SignalPickItem::DbcRoot => a.signal_pick_root_open = !a.signal_pick_root_open,
                SignalPickItem::MessagesRoot => {
                    a.signal_pick_messages_open = !a.signal_pick_messages_open
                }
            }
        });
    }
    {
        let app = app.clone();
        let picker = signal_window.as_weak();
        signal_window.on_signal_pick_ok(move || {
            let mut a = app.borrow_mut();
            let added = if a.sig_cat == 3 {
                match a.signal_pick_expr_selected.clone() {
                    Some(name) => { let m = add_expr_to_chart(&mut a, &name); a.log(m); true }
                    None => { a.log("请先选择一个表达式"); false }
                }
            } else {
                match a.signal_pick_selected.clone() {
                    Some((id, signal)) => { let m = add_signal_to_chart(&mut a, id, &signal); a.log(m); true }
                    None => { a.log("请先选择一个 DBC 信号"); false }
                }
            };
            if added && let Some(picker) = picker.upgrade() {
                let _ = picker.hide();
            }
        });
    }
    {
        let app = app.clone();
        signal_window.on_signal_pick_apply(move || {
            let mut a = app.borrow_mut();
            if a.sig_cat == 3 {
                match a.signal_pick_expr_selected.clone() {
                    Some(name) => { let m = add_expr_to_chart(&mut a, &name); a.log(m); }
                    None => a.log("请先选择一个表达式"),
                }
            } else {
                match a.signal_pick_selected.clone() {
                    Some((id, signal)) => { let m = add_signal_to_chart(&mut a, id, &signal); a.log(m); }
                    None => a.log("请先选择一个 DBC 信号"),
                }
            }
        });
    }
    {
        let app = app.clone();
        signal_window.on_sig_cat_changed(move |cat| {
            app.borrow_mut().sig_cat = cat;
        });
    }
    {
        let app = app.clone();
        let picker = signal_window.as_weak();
        signal_window.on_expr_save(move || {
            let Some(w) = picker.upgrade() else { return; };
            let name = w.get_expr_name().trim().to_string();
            let formula = w.get_expr_formula().trim().to_string();
            let unit = w.get_expr_unit().trim().to_string();
            if name.is_empty() || formula.is_empty() {
                w.set_expr_error("名称和表达式不能为空".into());
                return;
            }
            let refs = match crate::expr::refs(&formula) {
                Ok(r) => r,
                Err(e) => {
                    w.set_expr_error(format!("表达式错误: {e}").into());
                    return;
                }
            };
            let mut a = app.borrow_mut();
            let unknown: Vec<String> = refs.into_iter().filter(|r| !a.dbc_has_signal(r)).collect();
            // 添加或按名更新
            if let Some(ev) = a.expr_vars.iter_mut().find(|e| e.name == name) {
                ev.formula = formula.clone();
                ev.unit = unit.clone();
            } else {
                a.expr_vars.push(ExprVar { name: name.clone(), formula: formula.clone(), unit: unit.clone() });
            }
            // 已在曲线里的同名表达式系列同步公式/单位
            for s in a.series.iter_mut().filter(|s| s.expr.is_some() && s.name == name) {
                s.expr = Some(formula.clone());
                s.unit = unit.clone();
            }
            recompute_expr_ids(&mut a);
            a.signal_pick_expr_selected = Some(name.clone());
            let warn = if unknown.is_empty() { String::new() } else { format!("（DBC 中暂无: {}）", unknown.join(", ")) };
            a.log(format!("表达式已保存: {name} = {formula}{warn}"));
            drop(a);
            w.set_expr_error(if unknown.is_empty() { "".into() } else { format!("已保存。DBC 中暂无信号: {}（取 0）", unknown.join(", ")).into() });
        });
    }
    {
        let app = app.clone();
        let picker = signal_window.as_weak();
        signal_window.on_expr_delete(move || {
            let mut a = app.borrow_mut();
            let Some(name) = a.signal_pick_expr_selected.clone() else {
                a.log("请先在列表里选择要删除的表达式");
                return;
            };
            a.expr_vars.retain(|e| e.name != name);
            a.signal_pick_expr_selected = None;
            recompute_expr_ids(&mut a);
            a.log(format!("已删除表达式: {name}"));
            drop(a);
            if let Some(w) = picker.upgrade() {
                w.set_expr_name("".into());
                w.set_expr_formula("".into());
                w.set_expr_unit("".into());
                w.set_expr_error("".into());
            }
        });
    }
    {
        let picker = signal_window.as_weak();
        signal_window.on_signal_pick_cancel(move || {
            if let Some(picker) = picker.upgrade() {
                let _ = picker.hide();
            }
        });
    }
}
