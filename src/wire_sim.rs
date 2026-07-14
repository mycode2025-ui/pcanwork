// Event-wiring for the wire_sim. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[allow(unused_variables, clippy::too_many_arguments)]
fn wire_sim(
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
    // 仿真面板：添加控件(画布中级联落位, 立即选中)
    {
        let app = app.clone();
        sim_panel_window.on_sim_add(move |k| {
            let kind = SimKind::from_i32(k);
            let mut a = app.borrow_mut();
            let n = a.sim_widgets.len();
            let (w, h) = kind.default_size();
            let widget = SimWidget {
                kind,
                name: format!("{}{}", kind.label_i18n(a.lang_en), n + 1),
                channel: 1,
                frame_id: 0x100,
                signal: String::new(),
                threshold: 0.0,
                min: 0.0,
                max: 100.0,
                gen_mode: GenMode::Sine,
                gen_step: 2.0,
                period_ms: 100,
                x: 30.0 + (n % 6) as f64 * 26.0,
                y: 30.0 + (n % 6) as f64 * 26.0,
                w,
                h,
                enabled: true,
                slider_val: 0.0,
                press_val: 1.0,
                release_val: 0.0,
                align: 1,
                cur: 0.0,
                tick: 0,
                last_fire: None,
            };
            a.sim_widgets.push(widget);
            let new_idx = (a.sim_widgets.len() - 1) as i32;
            a.sim_sel = new_idx;
            a.sim_multi.clear();
            a.sim_multi.insert(new_idx);
            a.log(format!("仿真面板新增: {}", kind.label()));
            refresh_sim(&a);
        });
    }
    // 仿真面板：选中控件(单击=单选; Ctrl+单击=多选切换)
    {
        let app = app.clone();
        sim_panel_window.on_sim_select(move |i, additive| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx >= a.sim_widgets.len() {
                return;
            }
            if additive {
                if a.sim_multi.contains(&i) {
                    if a.sim_sel == i {
                        // 再次 Ctrl 点「模板」本身 → 移出选择
                        a.sim_multi.remove(&i);
                        a.sim_sel = a.sim_multi.iter().copied().next().unwrap_or(-1);
                    } else {
                        // Ctrl 点已选的非模板控件 → 提升为模板(保留多选)
                        a.sim_sel = i;
                    }
                } else {
                    // Ctrl 点未选控件 → 加入并设为模板
                    a.sim_multi.insert(i);
                    a.sim_sel = i;
                }
            } else {
                // 普通单击 → 唯一选中且为模板
                a.sim_multi.clear();
                a.sim_multi.insert(i);
                a.sim_sel = i;
            }
            refresh_sim(&a);
        });
    }
    // 高亮信号（双击工程树触发，2.5 秒内有效）
{
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_panel_window.on_sim_edit(move |i| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx >= a.sim_widgets.len() {
                return;
            }
            a.sim_sel = i;
            a.sim_multi.clear();
            a.sim_multi.insert(i);
            refresh_sim(&a);
            if let Some(win) = ppw.upgrade() {
                let (_, rows) = sim_signal_choices(&a);
                win.set_dbc_signals(ModelRc::from(Rc::new(VecModel::from(rows))));
                sim_fill_props(&win, &a.sim_widgets[idx]);
                show_child_window(&win);
            }
        });
    }
    // 仿真面板：全
{
        let app = app.clone();
        sim_panel_window.on_sim_select_all(move || {
            let mut a = app.borrow_mut();
            a.sim_multi.clear();
            for i in 0..a.sim_widgets.len() as i32 {
                a.sim_multi.insert(i);
            }
            a.sim_sel = if a.sim_widgets.is_empty() { -1 } else { 0 };
            refresh_sim(&a);
        });
    }
    // 仿真面板：对齐选中控件(0左 1右 2顶 3底 4水平居中 5垂直居中)
    {
        let app = app.clone();
        sim_panel_window.on_sim_align(move |mode| {
            let mut a = app.borrow_mut();
            let mut sel: Vec<usize> = a
                .sim_multi
                .iter()
                .map(|&i| i as usize)
                .filter(|&i| i < a.sim_widgets.len())
                .collect();
            sel.sort_unstable();
            if sel.len() < 2 {
                a.log("对齐需至少选中 2 个控件(可点「全选」)".to_string());
                return;
            }
            // 计算包围
let min_x = sel.iter().map(|&i| a.sim_widgets[i].x).fold(f64::MAX, f64::min);
            let max_r = sel.iter().map(|&i| a.sim_widgets[i].x + a.sim_widgets[i].w).fold(f64::MIN, f64::max);
            let min_y = sel.iter().map(|&i| a.sim_widgets[i].y).fold(f64::MAX, f64::min);
            let max_b = sel.iter().map(|&i| a.sim_widgets[i].y + a.sim_widgets[i].h).fold(f64::MIN, f64::max);
            let cx = (min_x + max_r) / 2.0;
            let cy = (min_y + max_b) / 2.0;
            // 等宽/等高: 以「模板」(主控件 sim_sel)的宽/高为基准；无模板则退回取最大
            let ref_idx = if a.sim_sel >= 0 && sel.contains(&(a.sim_sel as usize)) {
                a.sim_sel as usize
            } else {
                usize::MAX
            };
            let ref_w = if ref_idx != usize::MAX {
                a.sim_widgets[ref_idx].w
            } else {
                sel.iter().map(|&i| a.sim_widgets[i].w).fold(f64::MIN, f64::max)
            };
            let ref_h = if ref_idx != usize::MAX {
                a.sim_widgets[ref_idx].h
            } else {
                sel.iter().map(|&i| a.sim_widgets[i].h).fold(f64::MIN, f64::max)
            };
            for &i in &sel {
                let wdg = &mut a.sim_widgets[i];
                match mode {
                    0 => wdg.x = min_x,
                    1 => wdg.x = max_r - wdg.w,
                    2 => wdg.y = min_y,
                    3 => wdg.y = max_b - wdg.h,
                    4 => wdg.x = cx - wdg.w / 2.0,
                    5 => wdg.y = cy - wdg.h / 2.0,
                    6 => wdg.w = ref_w,
                    7 => wdg.h = ref_h,
                    _ => {}
                }
            }
            let basis = if ref_idx == usize::MAX { "取最大".to_string() } else { format!("模板「{}」", a.sim_widgets[ref_idx].name) };
            a.log(format!("已对齐 {} 个控件（等高/等宽基准: {basis}）", sel.len()));
            refresh_sim(&a);
        });
    }
    // 仿真面板：拖动控
{
        let app = app.clone();
        sim_panel_window.on_sim_drag(move |i, dx, dy| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                a.sim_widgets[idx].x = (a.sim_widgets[idx].x + dx as f64).max(0.0);
                a.sim_widgets[idx].y = (a.sim_widgets[idx].y + dy as f64).max(0.0);
                sim_set_row(&a, idx); // 只更新被拖那一
}
        });
    }
    // 仿真面板：缩放控件
    {
        let app = app.clone();
        sim_panel_window.on_sim_resize(move |i, nw, nh| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                a.sim_widgets[idx].w = (nw as f64).max(40.0);
                a.sim_widgets[idx].h = (nh as f64).max(30.0);
                sim_set_row(&a, idx); // 只更新被缩放那一
}
        });
    }
    // 选中控件属性(Rust 在 select 时填充, 应用时回读)
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_remove(move || {
            let mut a = app.borrow_mut();
            let idx = a.sim_sel;
            if idx >= 0 && (idx as usize) < a.sim_widgets.len() {
                a.sim_widgets.remove(idx as usize);
                a.sim_sel = -1;
                a.sim_multi.clear();
                refresh_sim(&a);
                if let Some(win) = ppw.upgrade() {
                    win.set_has_sel(false);
                }
            }
        });
    }
    // 仿真面板：清
{
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_panel_window.on_sim_clear(move || {
            let mut a = app.borrow_mut();
            a.sim_widgets.clear();
            a.sim_sel = -1;
            a.sim_multi.clear();
            refresh_sim(&a);
            if let Some(win) = ppw.upgrade() {
                win.set_has_sel(false);
            }
        });
    }
    // 仿真面板：运行/停止(锁定)
    {
        let app = app.clone();
        let spw = sim_panel_window.as_weak();
        let ppw = sim_prop_window.as_weak();
        sim_panel_window.on_sim_run_toggle(move || {
            let mut a = app.borrow_mut();
            a.sim_running = !a.sim_running;
            if a.sim_running {
                a.sim_sel = -1;
                a.sim_multi.clear();
                for wdg in a.sim_widgets.iter_mut() {
                    wdg.last_fire = None;
                    wdg.tick = 0;
                }
            }
            let running = a.sim_running;
            a.log(if running {
                "仿真面板：运行中（已锁定布局）".to_string()
            } else {
                "仿真面板：已停止（可编辑）".to_string()
            });
            refresh_sim(&a);
            if let Some(win) = spw.upgrade() {
                win.set_running(running);
            }
            // 进入运行模式：关闭属性窗
if running
                && let Some(win) = ppw.upgrade() {
                    win.set_has_sel(false);
                    let _ = win.hide();
                }
        });
    }
    // 仿真面板：应用属性窗口到选中控件
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_apply(move || {
            let Some(win) = ppw.upgrade() else { return };
            let mut a = app.borrow_mut();
            let idx = a.sim_sel;
            if idx < 0 || idx as usize >= a.sim_widgets.len() {
                return;
            }
            let pf = |s: slint::SharedString, d: f64| s.trim().parse::<f64>().unwrap_or(d);
            let gen_mode = match win.get_p_genmode() {
                1 => GenMode::Ramp,
                2 => GenMode::Sine,
                _ => GenMode::Constant,
            };
            let frame_id = u32::from_str_radix(
                win.get_p_frame().trim().trim_start_matches("0x").trim_start_matches("0X"),
                16,
            )
            .unwrap_or(0);
            let (px, py) = (win.get_p_x(), win.get_p_y());
            let (pw, ph) = (win.get_p_w(), win.get_p_h());
            let align = win.get_p_align();
            let chan = win.get_p_chan().trim().parse::<u8>().unwrap_or(1).max(1);
            let press = pf(win.get_p_pressval(), 1.0);
            let release = pf(win.get_p_releaseval(), 0.0);
            let w = &mut a.sim_widgets[idx as usize];
            w.name = win.get_p_name().trim().to_string();
            w.channel = chan;
            w.frame_id = frame_id;
            w.signal = win.get_p_signal().trim().to_string();
            w.min = pf(win.get_p_min(), 0.0);
            w.max = pf(win.get_p_max(), 100.0);
            w.threshold = pf(win.get_p_threshold(), 0.0);
            w.gen_mode = gen_mode;
            w.gen_step = pf(win.get_p_step(), 1.0);
            w.period_ms = win.get_p_period().trim().parse::<u64>().unwrap_or(100).max(10);
            w.x = pf(px, w.x).max(0.0);
            w.y = pf(py, w.y).max(0.0);
            w.w = pf(pw, w.w).max(40.0);
            w.h = pf(ph, w.h).max(30.0);
            w.press_val = press;
            w.release_val = release;
            w.align = align;
            refresh_sim(&a);
            // 回填(尺寸/位置可能被 clamp)
            let wclone = a.sim_widgets[idx as usize].clone();
            sim_fill_props(&win, &wclone);
        });
    }
    // 仿真面板：从 DBC 下拉选信号 → 绑定到选中控件
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_signal(move |idx| {
            let Some(win) = ppw.upgrade() else { return };
            let mut a = app.borrow_mut();
            let sel = a.sim_sel;
            if sel < 0 || sel as usize >= a.sim_widgets.len() {
                return;
            }
            let (choices, _) = sim_signal_choices(&a);
            if let Some((id, sig)) = choices.get(idx as usize).cloned() {
                // 自动带入该信号的 DBC 量程(若有)
                let range = sim_signal_range(&a, id, &sig);
                let w = &mut a.sim_widgets[sel as usize];
                w.frame_id = id;
                w.signal = sig;
                if let Some((mn, mx)) = range {
                    w.min = mn;
                    w.max = mx;
                }
                let wclone = a.sim_widgets[sel as usize].clone();
                a.log(format!(
                    "仿真控件绑定信号: 0x{:X}/{} [{}, {}]",
                    wclone.frame_id, wclone.signal, wclone.min, wclone.max
                ));
                refresh_sim(&a);
                sim_fill_props(&win, &wclone);
            }
        });
    }
    // 选中控件属性(Rust 在 select 时填充, 应用时回读)
    {
        let app = app.clone();
        sim_panel_window.on_sim_button(move |i, pressed| {
            let a = app.borrow();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                let (ch, id, sig, val) = {
                    let wdg = &a.sim_widgets[idx];
                    (
                        wdg.channel,
                        wdg.frame_id,
                        wdg.signal.clone(),
                        if pressed { wdg.press_val } else { wdg.release_val },
                    )
                };
                sim_send(&a, ch, id, &sig, val);
            }
        });
    }
    // 高亮信号（双击工程树触发，2.5 秒内有效）
    {
        let app = app.clone();
        sim_panel_window.on_sim_slider(move |i, frac| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                let (min, max) = (a.sim_widgets[idx].min, a.sim_widgets[idx].max);
                let val = min + (frac as f64).clamp(0.0, 1.0) * (max - min);
                a.sim_widgets[idx].slider_val = val;
                let (ch, id, signal) =
                    (a.sim_widgets[idx].channel, a.sim_widgets[idx].frame_id, a.sim_widgets[idx].signal.clone());
                sim_send(&a, ch, id, &signal, val);
                sim_set_row(&a, idx);
            }
        });
    }
}
