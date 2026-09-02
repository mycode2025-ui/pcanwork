// Event-wiring for the wire_sim. Included into main.rs via include!(); lives in the
// crate-root module, sharing main.rs's imports/private items (no use, no vis changes).
// Windows are passed by reference; app is an owned Rc clone. Unused params are by design.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SimBindingTreeItem {
    Raw,
    Dbc(String),
    Message(String, u32, bool),
    Signal(String, u32, bool, String),
}

type SimBindingMessageKey = (String, u32, bool);
type SharedSimBindingItems = Rc<std::cell::RefCell<Vec<SimBindingTreeItem>>>;
type SharedSimBindingDbcs = Rc<std::cell::RefCell<std::collections::HashSet<String>>>;
type SharedSimBindingMessages =
    Rc<std::cell::RefCell<std::collections::HashSet<SimBindingMessageKey>>>;
type SharedSimBindingFilter = Rc<std::cell::RefCell<String>>;
type SharedSimBindingSelection =
    Rc<std::cell::RefCell<std::collections::HashSet<SimBindingTreeItem>>>;

fn sim_binding_tree_model(
    app: &App,
    expanded_dbcs: &std::collections::HashSet<String>,
    expanded_messages: &std::collections::HashSet<(String, u32, bool)>,
    filter: &str,
) -> (Vec<SimBindingTreeItem>, Vec<SignalPickRow>) {
    let query = filter.trim().to_lowercase();
    let searching = !query.is_empty();
    let current = app
        .sim_widgets
        .get(app.sim_sel.max(0) as usize)
        .map(|widget| {
            (
                widget.dbc_path.clone(),
                widget.frame_id,
                widget.frame_extended,
                widget.signal.clone(),
            )
        });
    let mut items = Vec::new();
    let mut rows = Vec::new();
    let raw_selected = current
        .as_ref()
        .is_some_and(|(path, _, _, signal)| path.is_empty() && signal.is_empty());
    items.push(SimBindingTreeItem::Raw);
    rows.push(SignalPickRow {
        level: 0,
        name: if app.lang_en {
            "Raw byte binding (no DBC)".into()
        } else {
            "原始字节绑定（不使用 DBC）".into()
        },
        desc: if app.lang_en {
            "Configure CAN ID and payload manually".into()
        } else {
            "手动配置 CAN ID 与数据".into()
        },
        kind: "dbc".into(),
        expandable: false,
        expanded: false,
        selectable: true,
        selected: raw_selected,
        marked: false,
    });

    for (dbc_index, path) in app.dbc_paths.iter().enumerate() {
        let Some(db) = app.dbcs.get(dbc_index) else {
            continue;
        };
        let dbc_name = if db.file_name.is_empty() {
            std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string()
        } else {
            db.file_name.clone()
        };
        let dbc_matches = searching
            && (dbc_name.to_lowercase().contains(&query) || path.to_lowercase().contains(&query));
        let mut messages: Vec<_> = db.messages().collect();
        messages.sort_by(|left, right| {
            (left.id, left.extended, left.name.as_str()).cmp(&(
                right.id,
                right.extended,
                right.name.as_str(),
            ))
        });
        let has_descendant_match = messages.iter().any(|message| {
            message.name.to_lowercase().contains(&query)
                || format!("{:X}", message.id).to_lowercase().contains(&query)
                || message
                    .signals
                    .iter()
                    .any(|signal| signal.name.to_lowercase().contains(&query))
        });
        if searching && !dbc_matches && !has_descendant_match {
            continue;
        }
        let dbc_expanded = searching || expanded_dbcs.contains(path);
        items.push(SimBindingTreeItem::Dbc(path.clone()));
        rows.push(SignalPickRow {
            level: 0,
            name: dbc_name.into(),
            desc: if app.lang_en {
                format!("{} · {} messages", path, messages.len()).into()
            } else {
                format!("{} · {} 条报文", path, messages.len()).into()
            },
            kind: "dbc".into(),
            expandable: !messages.is_empty(),
            expanded: dbc_expanded,
            selectable: false,
            selected: current
                .as_ref()
                .is_some_and(|(selected_path, _, _, _)| sim::dbc_path_eq(selected_path, path)),
            marked: false,
        });
        if !dbc_expanded {
            continue;
        }
        for message in messages {
            let message_key = (path.clone(), message.id, message.extended);
            let message_matches = searching
                && (dbc_matches
                    || message.name.to_lowercase().contains(&query)
                    || format!("{:X}", message.id).to_lowercase().contains(&query));
            let signal_match = message
                .signals
                .iter()
                .any(|signal| signal.name.to_lowercase().contains(&query));
            if searching && !message_matches && !signal_match {
                continue;
            }
            let message_expanded = searching || expanded_messages.contains(&message_key);
            items.push(SimBindingTreeItem::Message(
                path.clone(),
                message.id,
                message.extended,
            ));
            rows.push(SignalPickRow {
                level: 1,
                name: format!("0x{:X}  {}", message.id, message.name).into(),
                desc: if app.lang_en {
                    format!(
                        "{} · DLC {} · {} signals",
                        if message.extended { "EXT" } else { "STD" },
                        message.size,
                        message.signals.len()
                    )
                    .into()
                } else {
                    format!(
                        "{} · DLC {} · {} 个信号",
                        if message.extended { "EXT" } else { "STD" },
                        message.size,
                        message.signals.len()
                    )
                    .into()
                },
                kind: "message".into(),
                expandable: !message.signals.is_empty(),
                expanded: message_expanded,
                selectable: false,
                selected: current.as_ref().is_some_and(
                    |(selected_path, id, extended, _)| {
                        sim::dbc_path_eq(selected_path, path)
                            && *id == message.id
                            && *extended == message.extended
                    },
                ),
                marked: false,
            });
            if !message_expanded {
                continue;
            }
            for signal in &message.signals {
                if searching
                    && !message_matches
                    && !signal.name.to_lowercase().contains(&query)
                {
                    continue;
                }
                let selected = current.as_ref().is_some_and(
                    |(selected_path, id, extended, selected_signal)| {
                        sim::dbc_path_eq(selected_path, path)
                            && *id == message.id
                            && *extended == message.extended
                            && selected_signal == &signal.name
                    },
                );
                items.push(SimBindingTreeItem::Signal(
                    path.clone(),
                    message.id,
                    message.extended,
                    signal.name.clone(),
                ));
                rows.push(SignalPickRow {
                    level: 2,
                    name: signal.name.clone().into(),
                    desc: format!(
                        "{} {}:{}{}{}",
                        if app.lang_en { "Bit" } else { "位" },
                        signal.start_bit,
                        signal.size,
                        if signal.unit.is_empty() { "" } else { " · " },
                        signal.unit
                    )
                    .into(),
                    kind: "signal".into(),
                    expandable: false,
                    expanded: false,
                    selectable: true,
                    selected,
                    marked: false,
                });
            }
        }
    }
    (items, rows)
}

fn refresh_sim_binding_tree(
    window: &SimPropWindow,
    app: &App,
    items: &SharedSimBindingItems,
    expanded_dbcs: &SharedSimBindingDbcs,
    expanded_messages: &SharedSimBindingMessages,
    filter: &SharedSimBindingFilter,
) {
    let (next_items, rows) = sim_binding_tree_model(
        app,
        &expanded_dbcs.borrow(),
        &expanded_messages.borrow(),
        &filter.borrow(),
    );
    *items.borrow_mut() = next_items;
    window.set_binding_tree_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn refresh_sim_signal_library(
    window: &SimPanelWindow,
    app: &App,
    items: &SharedSimBindingItems,
    expanded_dbcs: &SharedSimBindingDbcs,
    expanded_messages: &SharedSimBindingMessages,
    filter: &SharedSimBindingFilter,
    marked: &SharedSimBindingSelection,
) {
    let (mut next_items, mut rows) = sim_binding_tree_model(
        app,
        &expanded_dbcs.borrow(),
        &expanded_messages.borrow(),
        &filter.borrow(),
    );
    if matches!(next_items.first(), Some(SimBindingTreeItem::Raw)) {
        next_items.remove(0);
        rows.remove(0);
    }
    let available: std::collections::HashSet<_> = next_items.iter().cloned().collect();
    marked.borrow_mut().retain(|item| available.contains(item));
    let selected = marked.borrow();
    for (row, item) in rows.iter_mut().zip(&next_items) {
        row.marked = selected.contains(item);
    }
    window.set_signal_library_marked_count(selected.len() as i32);
    drop(selected);
    *items.borrow_mut() = next_items;
    window.set_signal_library_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn make_sim_widget(
    kind: SimKind,
    lang_en: bool,
    ordinal: usize,
    x: f64,
    y: f64,
) -> SimWidget {
    let (w, h) = kind.default_size();
    SimWidget {
        kind,
        name: format!("{}{}", kind.label_i18n(lang_en), ordinal),
        channel: 1,
        dbc_path: String::new(),
        frame_id: 0x100,
        frame_extended: false,
        frame_fd: false,
        frame_brs: false,
        frame_dlc: 8,
        frame_profile_explicit: true,
        signal: String::new(),
        threshold: 0.0,
        min: 0.0,
        max: 100.0,
        gen_mode: GenMode::Sine,
        gen_step: 2.0,
        period_ms: 100,
        x,
        y,
        w,
        h,
        enabled: true,
        slider_val: 0.0,
        press_val: 1.0,
        release_val: 0.0,
        align: 1,
        trace_signals: Vec::new(),
        trace_window_secs: 30,
        trace_auto_range: true,
        alarm_message: if lang_en {
            "Signal value is outside the allowed range".to_string()
        } else {
            "信号值超出允许范围".to_string()
        },
        image_path: String::new(),
        cur: 0.0,
        tick: 0,
        last_fire: None,
        binding_error_reported: false,
        switch_on: false,
        trace_history: Vec::new(),
        trace_paused: false,
        group_values: Vec::new(),
        image_cache: slint::Image::default(),
        image_cache_path: String::new(),
        image_load_ok: false,
    }
}

fn bind_sim_widget_to_signal(
    app: &mut App,
    widget_index: usize,
    item: &SimBindingTreeItem,
) -> Result<(), String> {
    if widget_index >= app.sim_widgets.len() {
        return Err("目标控件不存在".to_string());
    }
    if matches!(app.sim_widgets[widget_index].kind, SimKind::Label | SimKind::Image) {
        return Err("标签和图片背景不能绑定 CAN 信号".to_string());
    }
    apply_sim_signal_binding(
        &app.dbcs,
        &app.dbc_paths,
        &mut app.sim_widgets[widget_index],
        item,
    )
}

fn apply_sim_signal_binding(
    dbcs: &[dbc::DbcDb],
    dbc_paths: &[String],
    widget: &mut SimWidget,
    item: &SimBindingTreeItem,
) -> Result<(), String> {
    let SimBindingTreeItem::Signal(path, id, extended, signal) = item else {
        return Err("所选条目不是 DBC 信号".to_string());
    };
    let range = sim::sim_signal_range_in(dbcs, dbc_paths, path, *id, signal);
    let profile = sim_binding_frame_profile(dbcs, dbc_paths, path, *id, signal)?;
    widget.dbc_path = path.clone();
    widget.frame_id = *id;
    widget.frame_extended = *extended;
    widget.signal = signal.clone();
    widget.trace_signals.clear();
    widget.frame_extended = profile.extended;
    widget.frame_fd = profile.fd;
    widget.frame_brs = profile.brs;
    widget.frame_dlc = profile.dlc;
    widget.frame_profile_explicit = true;
    widget.binding_error_reported = false;
    if let Some((minimum, maximum)) = range {
        widget.min = minimum;
        widget.max = maximum;
    }
    Ok(())
}

fn append_bound_sim_widget(
    app: &mut App,
    kind: SimKind,
    item: &SimBindingTreeItem,
    drop_position: Option<(f64, f64)>,
) -> Result<usize, String> {
    let (width, height) = kind.default_size();
    let (x, y) = drop_position.map_or_else(
        || {
            sim::sim_find_free_position_from(
                &app.sim_widgets,
                width,
                height,
                app.sim_canvas_w,
                app.sim_canvas_h,
                350.0,
            )
        },
        |(drop_x, drop_y)| (drop_x - width / 2.0, drop_y - height / 2.0),
    );
    let mut widget = make_sim_widget(kind, app.lang_en, app.sim_widgets.len() + 1, x, y);
    if let SimBindingTreeItem::Signal(_, _, _, signal) = item {
        widget.name = signal.clone();
    }
    app.sim_widgets.push(widget);
    let index = app.sim_widgets.len() - 1;
    if let Err(error) = bind_sim_widget_to_signal(app, index, item) {
        app.sim_widgets.pop();
        return Err(error);
    }
    let (canvas_w, canvas_h) = (app.sim_canvas_w, app.sim_canvas_h);
    constrain_sim_widget(&mut app.sim_widgets[index], canvas_w, canvas_h);
    Ok(index)
}

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
    // 仿真工作区只提供保存主工程的快捷入口；新建和打开由主窗口统一管理。
    {
        let ui = ui.as_weak();
        sim_panel_window.on_project_save(move || {
            if let Some(window) = ui.upgrade() {
                window.invoke_save_project();
            }
        });
    }
    // 图片控件只保存文件路径；加载与缩放由 Slint 图像缓存完成。
    {
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_image(move || {
            let ppw = ppw.clone();
            let _ = slint::spawn_local(async move {
                let Some(window) = ppw.upgrade() else { return };
                let mut dialog = rfd::AsyncFileDialog::new()
                    .add_filter("Image", &["png", "jpg", "jpeg", "bmp", "svg"]);
                dialog = dialog.set_parent(&window.window().window_handle());
                let Some(file) = dialog.pick_file().await else { return };
                window.set_p_image_path(file.path().to_string_lossy().to_string().into());
            });
        });
    }

    // 仿真面板内置 DBC 信号库。与属性窗口共用同一棵树和同一套绑定校验，
    // 以保证搜索、拖拽、双击与批量创建不会产生不同的帧属性。
    let signal_library_items: SharedSimBindingItems = Rc::new(std::cell::RefCell::new(Vec::new()));
    let signal_library_dbcs: SharedSimBindingDbcs = Rc::new(std::cell::RefCell::new(
        std::collections::HashSet::new(),
    ));
    let signal_library_messages: SharedSimBindingMessages = Rc::new(std::cell::RefCell::new(
        std::collections::HashSet::new(),
    ));
    let signal_library_filter: SharedSimBindingFilter =
        Rc::new(std::cell::RefCell::new(String::new()));
    let signal_library_marked: SharedSimBindingSelection = Rc::new(std::cell::RefCell::new(
        std::collections::HashSet::new(),
    ));
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_refresh(move || {
            let Some(window) = window.upgrade() else { return };
            refresh_sim_signal_library(
                &window,
                &app.borrow(),
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_filter_changed(move |value| {
            let Some(window) = window.upgrade() else { return };
            *filter.borrow_mut() = value.to_string();
            window.set_signal_library_cursor(-1);
            refresh_sim_signal_library(
                &window,
                &app.borrow(),
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_row_clicked(move |index, additive| {
            let Some(window) = window.upgrade() else { return };
            let Some(item) = items.borrow().get(index.max(0) as usize).cloned() else {
                return;
            };
            window.set_signal_library_cursor(index);
            match &item {
                SimBindingTreeItem::Dbc(path) => {
                    let mut expanded = expanded_dbcs.borrow_mut();
                    if !expanded.remove(path) {
                        expanded.insert(path.clone());
                    }
                }
                SimBindingTreeItem::Message(path, id, extended) => {
                    let key = (path.clone(), *id, *extended);
                    let mut expanded = expanded_messages.borrow_mut();
                    if !expanded.remove(&key) {
                        expanded.insert(key);
                    }
                }
                SimBindingTreeItem::Signal(..) => {
                    let mut selected = marked.borrow_mut();
                    if additive {
                        if !selected.remove(&item) {
                            selected.insert(item);
                        }
                    } else {
                        selected.clear();
                        selected.insert(item);
                    }
                }
                SimBindingTreeItem::Raw => return,
            }
            refresh_sim_signal_library(
                &window,
                &app.borrow(),
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_activate(move |index| {
            let Some(window) = window.upgrade() else { return };
            let Some(item) = items.borrow().get(index.max(0) as usize).cloned() else {
                return;
            };
            if !matches!(item, SimBindingTreeItem::Signal(..)) {
                return;
            }
            marked.borrow_mut().clear();
            marked.borrow_mut().insert(item.clone());
            let mut a = app.borrow_mut();
            let target = usize::try_from(a.sim_sel)
                .ok()
                .filter(|target| *target < a.sim_widgets.len())
                .filter(|target| {
                    !matches!(a.sim_widgets[*target].kind, SimKind::Label | SimKind::Image)
                });
            let result = if let Some(target) = target {
                bind_sim_widget_to_signal(&mut a, target, &item).map(|_| target)
            } else {
                append_bound_sim_widget(&mut a, SimKind::Numeric, &item, None)
            };
            match result {
                Ok(target) => {
                    a.sim_sel = target as i32;
                    a.sim_multi.clear();
                    a.sim_multi.insert(target as i32);
                    a.mark_sim_dirty();
                    let signal = a.sim_widgets[target].signal.clone();
                    a.log(format!("仿真快捷绑定: {signal}"));
                    refresh_sim(&a);
                }
                Err(error) => a.log(format!("仿真快捷绑定失败: {error}")),
            }
            refresh_sim_signal_library(
                &window,
                &a,
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_drop(move |index, x, y| {
            let Some(window) = window.upgrade() else { return };
            let Some(item) = items.borrow().get(index.max(0) as usize).cloned() else {
                return;
            };
            if !matches!(item, SimBindingTreeItem::Signal(..)) {
                return;
            }
            let mut a = app.borrow_mut();
            if x < 0.0 || y < 0.0 || x as f64 > a.sim_canvas_w || y as f64 > a.sim_canvas_h {
                return;
            }
            let target = a.sim_widgets.iter().enumerate().rev().find_map(|(target, widget)| {
                let inside = x as f64 >= widget.x
                    && x as f64 <= widget.x + widget.w
                    && y as f64 >= widget.y
                    && y as f64 <= widget.y + widget.h;
                inside.then_some(target)
            });
            let result = match target {
                Some(target)
                    if !matches!(a.sim_widgets[target].kind, SimKind::Label | SimKind::Image) =>
                {
                    bind_sim_widget_to_signal(&mut a, target, &item).map(|_| target)
                }
                Some(_) => Err("标签和图片背景不能绑定 CAN 信号".to_string()),
                None => append_bound_sim_widget(
                    &mut a,
                    SimKind::Numeric,
                    &item,
                    Some((x as f64, y as f64)),
                ),
            };
            match result {
                Ok(target) => {
                    marked.borrow_mut().clear();
                    marked.borrow_mut().insert(item);
                    a.sim_sel = target as i32;
                    a.sim_multi.clear();
                    a.sim_multi.insert(target as i32);
                    a.mark_sim_dirty();
                    let signal = a.sim_widgets[target].signal.clone();
                    a.log(format!("仿真拖拽绑定: {signal}"));
                    refresh_sim(&a);
                }
                Err(error) => a.log(format!("仿真拖拽绑定失败: {error}")),
            }
            refresh_sim_signal_library(
                &window,
                &a,
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    {
        let app = app.clone();
        let window = sim_panel_window.as_weak();
        let items = signal_library_items.clone();
        let expanded_dbcs = signal_library_dbcs.clone();
        let expanded_messages = signal_library_messages.clone();
        let filter = signal_library_filter.clone();
        let marked = signal_library_marked.clone();
        sim_panel_window.on_signal_library_create(move |index, kind| {
            let Some(window) = window.upgrade() else { return };
            let requested = if index >= 0 {
                items
                    .borrow()
                    .get(index as usize)
                    .filter(|item| matches!(item, SimBindingTreeItem::Signal(..)))
                    .cloned()
                    .into_iter()
                    .collect::<Vec<_>>()
            } else {
                let selected = marked.borrow();
                items
                    .borrow()
                    .iter()
                    .filter(|item| selected.contains(*item))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if requested.is_empty() {
                return;
            }
            let kind = SimKind::from_i32(kind);
            let mut a = app.borrow_mut();
            let mut created = Vec::new();
            if kind == SimKind::Trend {
                let mut groups: Vec<Vec<SimBindingTreeItem>> = Vec::new();
                for item in requested {
                    let SimBindingTreeItem::Signal(path, id, extended, _) = &item else {
                        continue;
                    };
                    if let Some(group) = groups.iter_mut().find(|group| {
                        matches!(group.first(), Some(SimBindingTreeItem::Signal(group_path, group_id, group_extended, _))
                            if sim::dbc_path_eq(group_path, path) && group_id == id && group_extended == extended)
                    }) {
                        group.push(item);
                    } else {
                        groups.push(vec![item]);
                    }
                }
                for group in groups {
                    for chunk in group.chunks(4) {
                        match append_bound_sim_widget(&mut a, SimKind::Trend, &chunk[0], None) {
                            Ok(target) => {
                                a.sim_widgets[target].trace_signals = chunk[1..]
                                    .iter()
                                    .filter_map(|item| match item {
                                        SimBindingTreeItem::Signal(_, _, _, signal) => Some(signal.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                created.push(target);
                            }
                            Err(error) => a.log(format!("创建趋势图失败: {error}")),
                        }
                    }
                }
            } else {
                for item in requested {
                    match append_bound_sim_widget(&mut a, kind, &item, None) {
                        Ok(target) => created.push(target),
                        Err(error) => a.log(format!("创建仿真控件失败: {error}")),
                    }
                }
            }
            if let Some(&target) = created.last() {
                a.sim_sel = target as i32;
                a.sim_multi = created.iter().map(|target| *target as i32).collect();
                a.mark_sim_dirty();
                a.log(format!("已从 DBC 信号库创建 {} 个控件", created.len()));
                refresh_sim(&a);
            }
            refresh_sim_signal_library(
                &window,
                &a,
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
                &marked,
            );
        });
    }
    refresh_sim_signal_library(
        sim_panel_window,
        &app.borrow(),
        &signal_library_items,
        &signal_library_dbcs,
        &signal_library_messages,
        &signal_library_filter,
        &signal_library_marked,
    );

    // 仿真面板：添加控件(画布中级联落位, 立即选中)
    {
        let app = app.clone();
        sim_panel_window.on_sim_add(move |k| {
            let kind = SimKind::from_i32(k);
            let mut a = app.borrow_mut();
            let n = a.sim_widgets.len();
            let (w, h) = kind.default_size();
            let (x, y) = sim_find_free_position(
                &a.sim_widgets,
                w,
                h,
                a.sim_canvas_w,
                a.sim_canvas_h,
            );
            let widget = make_sim_widget(kind, a.lang_en, n + 1, x, y);
            let new_idx = if kind == SimKind::Image {
                // 背景图片始终位于控件数组最前端，Slint 先绘制，动态控件自然叠在其上。
                a.sim_widgets.insert(0, widget);
                0
            } else {
                a.sim_widgets.push(widget);
                (a.sim_widgets.len() - 1) as i32
            };
            a.sim_sel = new_idx;
            a.sim_multi.clear();
            a.sim_multi.insert(new_idx);
            a.mark_sim_dirty();
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
            } else if a.sim_multi.len() > 1 && a.sim_multi.contains(&i) {
                a.sim_sel = i;
            } else {
                // 普通单击 → 唯一选中且为模板
                a.sim_multi.clear();
                a.sim_multi.insert(i);
                a.sim_sel = i;
            }
            refresh_sim(&a);
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_clear_selection(move || {
            let mut a = app.borrow_mut();
            a.sim_sel = -1;
            a.sim_multi.clear();
            refresh_sim(&a);
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_constrain(move |canvas_w, canvas_h| {
            if canvas_w < 200.0 || canvas_h < 150.0 {
                return;
            }
            let mut a = app.borrow_mut();
            a.sim_canvas_w = canvas_w as f64;
            a.sim_canvas_h = canvas_h as f64;
            let mut changed = false;
            for widget in &mut a.sim_widgets {
                let before = (widget.x, widget.y, widget.w, widget.h);
                constrain_sim_widget(widget, canvas_w as f64, canvas_h as f64);
                changed |= before != (widget.x, widget.y, widget.w, widget.h);
            }
            if changed {
                a.mark_sim_dirty();
                refresh_sim(&a);
            }
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
                sim_prepare_props(&win, &a, &a.sim_widgets[idx]);
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
            let (canvas_w, canvas_h) = (a.sim_canvas_w, a.sim_canvas_h);
            if canvas_w > 0.0 && canvas_h > 0.0 {
                for &i in &sel {
                    constrain_sim_widget(&mut a.sim_widgets[i], canvas_w, canvas_h);
                }
            }
            a.mark_sim_dirty();
            let basis = if ref_idx == usize::MAX { "取最大".to_string() } else { format!("模板「{}」", a.sim_widgets[ref_idx].name) };
            a.log(format!("已对齐 {} 个控件（等高/等宽基准: {basis}）", sel.len()));
            refresh_sim(&a);
        });
    }
    // 仿真面板：按下时记录的几何快照换算为画布绝对坐标，避免局部坐标反复累加。
    {
        let app = app.clone();
        sim_panel_window.on_sim_drag(move |i, x, y| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                let canvas_w = a.sim_canvas_w;
                let canvas_h = a.sim_canvas_h;
                let before = (a.sim_widgets[idx].x, a.sim_widgets[idx].y);
                a.sim_widgets[idx].x = x as f64;
                a.sim_widgets[idx].y = y as f64;
                constrain_sim_widget(&mut a.sim_widgets[idx], canvas_w, canvas_h);
                let delta = (
                    a.sim_widgets[idx].x - before.0,
                    a.sim_widgets[idx].y - before.1,
                );
                if delta == (0.0, 0.0) {
                    return;
                }
                let selected: Vec<usize> = a
                    .sim_multi
                    .iter()
                    .filter_map(|selected| usize::try_from(*selected).ok())
                    .filter(|selected| *selected != idx && *selected < a.sim_widgets.len())
                    .collect();
                for selected in selected {
                    a.sim_widgets[selected].x += delta.0;
                    a.sim_widgets[selected].y += delta.1;
                    constrain_sim_widget(&mut a.sim_widgets[selected], canvas_w, canvas_h);
                }
                a.mark_sim_dirty();
                if a.sim_multi.len() > 1 {
                    refresh_sim(&a);
                } else {
                    sim_set_row(&a, idx);
                }
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
                let canvas_w = a.sim_canvas_w;
                let canvas_h = a.sim_canvas_h;
                let before = (a.sim_widgets[idx].w, a.sim_widgets[idx].h);
                a.sim_widgets[idx].w = nw as f64;
                a.sim_widgets[idx].h = nh as f64;
                constrain_sim_widget(&mut a.sim_widgets[idx], canvas_w, canvas_h);
                if before == (a.sim_widgets[idx].w, a.sim_widgets[idx].h) {
                    return;
                }
                a.mark_sim_dirty();
                sim_set_row(&a, idx); // 只更新被缩放的控件
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
                a.mark_sim_dirty();
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
            if a.sim_widgets.is_empty() {
                return;
            }
            let _ = configure_sim_generators(&a, false);
            a.sim_running = false;
            a.sim_widgets.clear();
            a.sim_tx_frames.clear();
            a.sim_sel = -1;
            a.sim_multi.clear();
            a.mark_sim_dirty();
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
            let starting = !a.sim_running;
            if starting && !a.license_allows("simulation") {
                return;
            }
            match configure_sim_generators(&a, starting) {
                Ok(count) if starting => {
                    a.log(format!(
                        "仿真调度器已启动：{count} 个发生器在 CAN 后台线程运行"
                    ));
                }
                Ok(_) => {}
                Err(error) => {
                    a.log(format!("仿真未启动：{error}"));
                    return;
                }
            }
            a.sim_running = starting;
            if starting {
                a.sim_sel = -1;
                a.sim_multi.clear();
                for wdg in a.sim_widgets.iter_mut() {
                    wdg.last_fire = None;
                    wdg.tick = 0;
                    wdg.binding_error_reported = false;
                    wdg.trace_history.clear();
                    wdg.trace_paused = false;
                    wdg.group_values.clear();
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
                if !running && win.get_runtime_fullscreen() {
                    win.set_runtime_fullscreen(false);
                }
            }
            // 进入运行模式：关闭属性窗
            if running && let Some(win) = ppw.upgrade() {
                    win.set_has_sel(false);
                    let _ = win.hide();
                }
        });
    }
    {
        let spw = sim_panel_window.as_weak();
        sim_panel_window.on_runtime_fullscreen_toggle(move || {
            let Some(win) = spw.upgrade() else { return };
            if !win.get_running() {
                return;
            }
            let fullscreen = !win.get_runtime_fullscreen();
            win.set_runtime_fullscreen(fullscreen);
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
            let frame_text = win.get_p_frame();
            let frame_id = match u32::from_str_radix(
                frame_text
                    .trim()
                    .trim_start_matches("0x")
                    .trim_start_matches("0X"),
                16,
            ) {
                Ok(value) => value,
                Err(_) => {
                    a.log(format!(
                        "仿真属性未应用：帧 ID「{}」不是有效十六进制数",
                        frame_text
                    ));
                    return;
                }
            };
            let frame_extended = win.get_p_extended();
            let frame_fd = win.get_p_fd();
            let frame_brs = win.get_p_brs() && frame_fd;
            let dlc_text = win.get_p_dlc();
            let frame_dlc = match dlc_text.trim().parse::<u8>() {
                Ok(value) => value,
                Err(_) => {
                    a.log(format!(
                        "仿真属性未应用：DLC「{}」不是 0..64 的整数",
                        dlc_text
                    ));
                    return;
                }
            };
            let (px, py) = (win.get_p_x(), win.get_p_y());
            let (pw, ph) = (win.get_p_w(), win.get_p_h());
            let align = win.get_p_align();
            let channel_text = win.get_p_chan();
            let chan = match channel_text.trim().parse::<u8>() {
                Ok(value) if value > 0 => value,
                _ => {
                    a.log(format!(
                        "仿真属性未应用：通道「{}」必须是大于 0 的整数",
                        channel_text
                    ));
                    return;
                }
            };
            let press = pf(win.get_p_pressval(), 1.0);
            let release = pf(win.get_p_releaseval(), 0.0);
            let dbc_path = win.get_p_dbc().trim().to_string();
            let primary_signal = win.get_p_signal().trim().to_string();
            if let Err(error) = sim_validate_binding_profile(
                &a,
                &dbc_path,
                frame_id,
                &primary_signal,
                sim::SimFrameProfile::new(
                    frame_extended,
                    frame_fd,
                    frame_brs,
                    frame_dlc,
                ),
            ) {
                a.log(format!(
                    "仿真属性未应用：CAN{} 0x{:X}：{}",
                    chan, frame_id, error
                ));
                return;
            }
            let valid_extra_signals = sim_frame_signal_choices(
                &a,
                &dbc_path,
                frame_id,
                &primary_signal,
                chan,
            )
            .0;
            let trace_signals: Vec<String> = win
                .get_p_trace_signals()
                .split([',', '，', ';', '；'])
                .map(str::trim)
                .filter(|signal| {
                    !signal.is_empty()
                        && valid_extra_signals
                            .iter()
                            .any(|valid| valid == *signal)
                })
                .take(3)
                .map(ToOwned::to_owned)
                .collect();
            let canvas_w = a.sim_canvas_w;
            let canvas_h = a.sim_canvas_h;
            let w = &mut a.sim_widgets[idx as usize];
            w.name = win.get_p_name().trim().to_string();
            w.channel = chan;
            w.dbc_path = dbc_path;
            w.frame_id = frame_id;
            w.frame_extended = frame_extended;
            w.frame_fd = frame_fd;
            w.frame_brs = frame_brs;
            w.frame_dlc = frame_dlc;
            w.frame_profile_explicit = true;
            w.signal = primary_signal;
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
            w.trace_signals = trace_signals;
            w.trace_window_secs = win
                .get_p_trace_window()
                .trim()
                .parse::<u64>()
                .unwrap_or(30)
                .clamp(5, 600);
            w.trace_auto_range = win.get_p_trace_auto();
            w.alarm_message = win.get_p_alarm_message().trim().to_string();
            w.image_path = win.get_p_image_path().trim().to_string();
            w.image_cache_path.clear();
            w.image_load_ok = false;
            w.trace_history.clear();
            w.group_values.clear();
            w.binding_error_reported = false;
            constrain_sim_widget(w, canvas_w, canvas_h);
            a.mark_sim_dirty();
            refresh_sim(&a);
            // 回填(尺寸/位置可能被 clamp)
            let wclone = a.sim_widgets[idx as usize].clone();
            sim_prepare_props(&win, &a, &wclone);
        });
    }
    // 仿真属性的 DBC 树：DBC 文件 -> 报文 -> 信号。搜索时自动展开匹配路径，
    // 双击信号一次完成文件、报文、帧属性和信号的原子绑定。
    let binding_tree_items = Rc::new(std::cell::RefCell::new(Vec::new()));
    let binding_tree_dbcs = Rc::new(std::cell::RefCell::new(
        std::collections::HashSet::<String>::new(),
    ));
    let binding_tree_messages = Rc::new(std::cell::RefCell::new(
        std::collections::HashSet::<(String, u32, bool)>::new(),
    ));
    let binding_tree_filter = Rc::new(std::cell::RefCell::new(String::new()));
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        let items = binding_tree_items.clone();
        let expanded_dbcs = binding_tree_dbcs.clone();
        let expanded_messages = binding_tree_messages.clone();
        let filter = binding_tree_filter.clone();
        sim_prop_window.on_binding_tree_opened(move || {
            let Some(window) = ppw.upgrade() else { return };
            let a = app.borrow();
            filter.borrow_mut().clear();
            window.set_binding_tree_filter("".into());
            window.set_binding_tree_cursor(-1);
            if let Some(widget) = a.sim_widgets.get(a.sim_sel.max(0) as usize)
                && !widget.dbc_path.is_empty()
            {
                expanded_dbcs.borrow_mut().insert(widget.dbc_path.clone());
                expanded_messages.borrow_mut().insert((
                    widget.dbc_path.clone(),
                    widget.frame_id,
                    widget.frame_extended,
                ));
            }
            refresh_sim_binding_tree(
                &window,
                &a,
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
            );
        });
    }
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        let items = binding_tree_items.clone();
        let expanded_dbcs = binding_tree_dbcs.clone();
        let expanded_messages = binding_tree_messages.clone();
        let filter = binding_tree_filter.clone();
        sim_prop_window.on_binding_tree_filter_changed(move |value| {
            let Some(window) = ppw.upgrade() else { return };
            *filter.borrow_mut() = value.to_string();
            window.set_binding_tree_cursor(-1);
            refresh_sim_binding_tree(
                &window,
                &app.borrow(),
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
            );
        });
    }
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        let items = binding_tree_items.clone();
        let expanded_dbcs = binding_tree_dbcs.clone();
        let expanded_messages = binding_tree_messages.clone();
        let filter = binding_tree_filter.clone();
        sim_prop_window.on_binding_tree_row_clicked(move |index| {
            let Some(window) = ppw.upgrade() else { return };
            let Some(item) = items.borrow().get(index.max(0) as usize).cloned() else {
                return;
            };
            match item {
                SimBindingTreeItem::Dbc(path) => {
                    let mut expanded = expanded_dbcs.borrow_mut();
                    if !expanded.remove(&path) {
                        expanded.insert(path);
                    }
                }
                SimBindingTreeItem::Message(path, id, extended) => {
                    let key = (path, id, extended);
                    let mut expanded = expanded_messages.borrow_mut();
                    if !expanded.remove(&key) {
                        expanded.insert(key);
                    }
                }
                SimBindingTreeItem::Raw | SimBindingTreeItem::Signal(..) => return,
            }
            refresh_sim_binding_tree(
                &window,
                &app.borrow(),
                &items,
                &expanded_dbcs,
                &expanded_messages,
                &filter,
            );
        });
    }
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        let items = binding_tree_items.clone();
        let expanded_dbcs = binding_tree_dbcs.clone();
        let expanded_messages = binding_tree_messages.clone();
        let filter = binding_tree_filter.clone();
        sim_prop_window.on_binding_tree_choose(move |index| {
            let Some(window) = ppw.upgrade() else { return };
            let Some(item) = items.borrow().get(index.max(0) as usize).cloned() else {
                return;
            };
            if matches!(item, SimBindingTreeItem::Dbc(_) | SimBindingTreeItem::Message(..)) {
                match item {
                    SimBindingTreeItem::Dbc(path) => {
                        let mut expanded = expanded_dbcs.borrow_mut();
                        if !expanded.remove(&path) {
                            expanded.insert(path);
                        }
                    }
                    SimBindingTreeItem::Message(path, id, extended) => {
                        let key = (path, id, extended);
                        let mut expanded = expanded_messages.borrow_mut();
                        if !expanded.remove(&key) {
                            expanded.insert(key);
                        }
                    }
                    _ => {}
                }
                refresh_sim_binding_tree(
                    &window,
                    &app.borrow(),
                    &items,
                    &expanded_dbcs,
                    &expanded_messages,
                    &filter,
                );
                return;
            }
            let mut a = app.borrow_mut();
            let selected = a.sim_sel;
            if selected < 0 || selected as usize >= a.sim_widgets.len() {
                return;
            }
            match item {
                SimBindingTreeItem::Raw => {
                    let widget = &mut a.sim_widgets[selected as usize];
                    widget.dbc_path.clear();
                    widget.signal.clear();
                    widget.trace_signals.clear();
                    widget.frame_extended = widget.frame_id > 0x7FF;
                    widget.frame_fd = false;
                    widget.frame_brs = false;
                    widget.frame_dlc = 8;
                    widget.frame_profile_explicit = true;
                    widget.binding_error_reported = false;
                }
                SimBindingTreeItem::Signal(path, id, extended, signal) => {
                    let range = sim_signal_range(&a, &path, id, &signal);
                    let profile = sim_binding_frame_profile(
                        &a.dbcs,
                        &a.dbc_paths,
                        &path,
                        id,
                        &signal,
                    )
                    .ok();
                    let widget = &mut a.sim_widgets[selected as usize];
                    widget.dbc_path = path;
                    widget.frame_id = id;
                    widget.frame_extended = extended;
                    widget.signal = signal;
                    widget.trace_signals.clear();
                    if let Some(profile) = profile {
                        widget.frame_extended = profile.extended;
                        widget.frame_fd = profile.fd;
                        widget.frame_brs = profile.brs;
                        widget.frame_dlc = profile.dlc;
                    }
                    widget.frame_profile_explicit = true;
                    widget.binding_error_reported = false;
                    if let Some((minimum, maximum)) = range {
                        widget.min = minimum;
                        widget.max = maximum;
                    }
                }
                SimBindingTreeItem::Dbc(_) | SimBindingTreeItem::Message(..) => unreachable!(),
            }
            a.mark_sim_dirty();
            let widget = a.sim_widgets[selected as usize].clone();
            a.log(format!(
                "仿真控件树状绑定: {} · 0x{:X}/{}",
                if widget.dbc_path.is_empty() {
                    "RAW".to_string()
                } else {
                    std::path::Path::new(&widget.dbc_path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&widget.dbc_path)
                        .to_string()
                },
                widget.frame_id,
                widget.signal
            ));
            refresh_sim(&a);
            sim_prepare_props(&window, &a, &widget);
            window.set_binding_tree_open(false);
        });
    }

    // 仿真面板：先选择一个明确的 DBC 文件，再从该文件选择信号。
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_dbc(move |idx| {
            let Some(win) = ppw.upgrade() else { return };
            let mut a = app.borrow_mut();
            let sel = a.sim_sel;
            if sel < 0 || sel as usize >= a.sim_widgets.len() {
                return;
            }
            let current_path = a.sim_widgets[sel as usize].dbc_path.clone();
            let (choices, _) = sim_dbc_choices(&a, &current_path);
            let Some(dbc_path) = choices.get(idx.max(0) as usize).cloned() else {
                return;
            };
            let widget = &mut a.sim_widgets[sel as usize];
            if widget.dbc_path == dbc_path {
                return;
            }
            widget.dbc_path = dbc_path;
            widget.signal.clear();
            widget.trace_signals.clear();
            if widget.dbc_path.is_empty() {
                widget.frame_extended = widget.frame_id > 0x7FF;
                widget.frame_fd = false;
                widget.frame_brs = false;
                widget.frame_dlc = 8;
                widget.frame_profile_explicit = true;
            }
            widget.binding_error_reported = false;
            a.mark_sim_dirty();
            let widget = a.sim_widgets[sel as usize].clone();
            a.log(if widget.dbc_path.is_empty() {
                format!("仿真控件「{}」切换为原始字节绑定", widget.name)
            } else {
                format!(
                    "仿真控件「{}」选择 DBC: {}",
                    widget.name,
                    std::path::Path::new(&widget.dbc_path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&widget.dbc_path)
                )
            });
            refresh_sim(&a);
            sim_prepare_props(&win, &a, &widget);
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_marquee(move |x, y, width, height| {
            let mut a = app.borrow_mut();
            let (left, top) = (x as f64, y as f64);
            let (right, bottom) = (left + width as f64, top + height as f64);
            let selected: Vec<i32> = if width >= 3.0 && height >= 3.0 {
                a.sim_widgets
                    .iter()
                    .enumerate()
                    .filter(|(_, widget)| {
                        widget.x < right
                            && widget.x + widget.w > left
                            && widget.y < bottom
                            && widget.y + widget.h > top
                    })
                    .map(|(index, _)| index as i32)
                    .collect()
            } else {
                Vec::new()
            };
            a.sim_multi.clear();
            a.sim_multi.extend(selected);
            a.sim_sel = a.sim_multi.iter().copied().next().unwrap_or(-1);
            refresh_sim(&a);
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
            let dbc_path = a.sim_widgets[sel as usize].dbc_path.clone();
            let (choices, _) = sim_signal_choices(&a, &dbc_path);
            if let Some((id, sig)) = choices.get(idx as usize).cloned() {
                // 自动带入该信号的 DBC 量程(若有)
                let range = sim_signal_range(&a, &dbc_path, id, &sig);
                let profile = sim_binding_frame_profile(
                    &a.dbcs,
                    &a.dbc_paths,
                    &dbc_path,
                    id,
                    &sig,
                )
                .ok();
                let w = &mut a.sim_widgets[sel as usize];
                w.frame_id = id;
                w.signal = sig;
                if let Some(profile) = profile {
                    w.frame_extended = profile.extended;
                    w.frame_fd = profile.fd;
                    w.frame_brs = profile.brs;
                    w.frame_dlc = profile.dlc;
                    w.frame_profile_explicit = true;
                }
                w.trace_signals.clear();
                w.binding_error_reported = false;
                if let Some((mn, mx)) = range {
                    w.min = mn;
                    w.max = mx;
                }
                a.mark_sim_dirty();
                let wclone = a.sim_widgets[sel as usize].clone();
                a.log(format!(
                    "仿真控件绑定信号: {} · 0x{:X}/{} [{}, {}]",
                    std::path::Path::new(&wclone.dbc_path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&wclone.dbc_path),
                    wclone.frame_id,
                    wclone.signal,
                    wclone.min,
                    wclone.max
                ));
                refresh_sim(&a);
                sim_prepare_props(&win, &a, &wclone);
            }
        });
    }
    // 选中控件属性(Rust 在 select 时填充, 应用时回读)
    {
        let app = app.clone();
        sim_panel_window.on_sim_button(move |i, pressed| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx < a.sim_widgets.len() {
                let (ch, dbc_path, id, sig, val, ext, fd, brs, dlc) = {
                    let wdg = &a.sim_widgets[idx];
                    (
                        wdg.channel,
                        wdg.dbc_path.clone(),
                        wdg.frame_id,
                        wdg.signal.clone(),
                        if pressed { wdg.press_val } else { wdg.release_val },
                        wdg.frame_extended,
                        wdg.frame_fd,
                        wdg.frame_brs,
                        wdg.frame_dlc,
                    )
                };
                let profile = sim::SimFrameProfile::new(ext, fd, brs, dlc);
                let error = sim_send(&mut a, ch, &dbc_path, id, &sig, val, profile).err();
                let should_report = error.is_some() && !a.sim_widgets[idx].binding_error_reported;
                a.sim_widgets[idx].binding_error_reported = error.is_some();
                if should_report {
                    let name = a.sim_widgets[idx].name.clone();
                    a.log(format!(
                        "仿真按钮「{name}」未发送: {}",
                        error.as_deref().unwrap_or_default()
                    ));
                }
            }
        });
    }
    // 多信号控件的附加序列只能从主绑定的同一 DBC/报文中选择。
    // UI 显示完整 CAN/DBC/ID/报文/信号关系，Rust 保存稳定的信号名。
    {
        let app = app.clone();
        let ppw = sim_prop_window.as_weak();
        sim_prop_window.on_prop_extra_signal(move |slot, choice_index| {
            let Some(win) = ppw.upgrade() else { return };
            if !(0..3).contains(&slot) {
                return;
            }
            let mut a = app.borrow_mut();
            let sel = a.sim_sel;
            if sel < 0 || sel as usize >= a.sim_widgets.len() {
                return;
            }
            let widget = &a.sim_widgets[sel as usize];
            let (choices, _) = sim_frame_signal_choices(
                &a,
                &widget.dbc_path,
                widget.frame_id,
                &widget.signal,
                widget.channel,
            );
            let Some(signal) = choices.get(choice_index.max(0) as usize).cloned() else {
                return;
            };
            let widget = &mut a.sim_widgets[sel as usize];
            widget.trace_signals.resize(3, String::new());
            // A signal can appear in only one series slot.
            if !signal.is_empty() {
                for (index, existing) in widget.trace_signals.iter_mut().enumerate() {
                    if index != slot as usize && *existing == signal {
                        existing.clear();
                    }
                }
            }
            widget.trace_signals[slot as usize] = signal;
            while widget.trace_signals.last().is_some_and(|signal| signal.is_empty()) {
                widget.trace_signals.pop();
            }
            widget.trace_history.clear();
            widget.group_values.clear();
            a.mark_sim_dirty();
            let widget = a.sim_widgets[sel as usize].clone();
            refresh_sim(&a);
            sim_prepare_props(&win, &a, &widget);
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_switch(move |i| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx >= a.sim_widgets.len() {
                return;
            }
            let next = !a.sim_widgets[idx].switch_on;
            let (ch, dbc_path, id, sig, val, ext, fd, brs, dlc) = {
                let wdg = &a.sim_widgets[idx];
                (
                    wdg.channel,
                    wdg.dbc_path.clone(),
                    wdg.frame_id,
                    wdg.signal.clone(),
                    if next { wdg.press_val } else { wdg.release_val },
                    wdg.frame_extended,
                    wdg.frame_fd,
                    wdg.frame_brs,
                    wdg.frame_dlc,
                )
            };
            let profile = sim::SimFrameProfile::new(ext, fd, brs, dlc);
            let error = sim_send(&mut a, ch, &dbc_path, id, &sig, val, profile).err();
            let should_report = error.is_some() && !a.sim_widgets[idx].binding_error_reported;
            a.sim_widgets[idx].binding_error_reported = error.is_some();
            if error.is_none() {
                a.sim_widgets[idx].switch_on = next;
                sim_set_row(&a, idx);
            } else if should_report {
                let name = a.sim_widgets[idx].name.clone();
                a.log(format!(
                    "仿真开关「{name}」未发送: {}",
                    error.as_deref().unwrap_or_default()
                ));
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
                let (ch, dbc_path, id, signal, ext, fd, brs, dlc) = (
                    a.sim_widgets[idx].channel,
                    a.sim_widgets[idx].dbc_path.clone(),
                    a.sim_widgets[idx].frame_id,
                    a.sim_widgets[idx].signal.clone(),
                    a.sim_widgets[idx].frame_extended,
                    a.sim_widgets[idx].frame_fd,
                    a.sim_widgets[idx].frame_brs,
                    a.sim_widgets[idx].frame_dlc,
                );
                let profile = sim::SimFrameProfile::new(ext, fd, brs, dlc);
                let error = sim_send(&mut a, ch, &dbc_path, id, &signal, val, profile).err();
                let should_report = error.is_some() && !a.sim_widgets[idx].binding_error_reported;
                a.sim_widgets[idx].binding_error_reported = error.is_some();
                if should_report {
                    let name = a.sim_widgets[idx].name.clone();
                    let kind = a.sim_widgets[idx].kind.label();
                    a.log(format!(
                        "仿真{kind}「{name}」未发送: {}",
                        error.as_deref().unwrap_or_default()
                    ));
                }
                sim_set_row(&a, idx);
            }
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_input(move |i, text| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx >= a.sim_widgets.len() {
                return;
            }
            let Ok(value) = text.trim().parse::<f64>() else {
                let name = a.sim_widgets[idx].name.clone();
                a.log(format!("仿真数值输入「{name}」格式无效: {text}"));
                return;
            };
            let (min, max) = (a.sim_widgets[idx].min, a.sim_widgets[idx].max);
            let value = value.clamp(min.min(max), min.max(max));
            let (ch, dbc_path, id, signal, ext, fd, brs, dlc) = (
                a.sim_widgets[idx].channel,
                a.sim_widgets[idx].dbc_path.clone(),
                a.sim_widgets[idx].frame_id,
                a.sim_widgets[idx].signal.clone(),
                a.sim_widgets[idx].frame_extended,
                a.sim_widgets[idx].frame_fd,
                a.sim_widgets[idx].frame_brs,
                a.sim_widgets[idx].frame_dlc,
            );
            let profile = sim::SimFrameProfile::new(ext, fd, brs, dlc);
            let error = sim_send(&mut a, ch, &dbc_path, id, &signal, value, profile).err();
            a.sim_widgets[idx].binding_error_reported = error.is_some();
            if let Some(error) = error {
                let name = a.sim_widgets[idx].name.clone();
                a.log(format!("仿真数值输入「{name}」未发送: {error}"));
            } else {
                a.sim_widgets[idx].slider_val = value;
                sim_set_row(&a, idx);
            }
        });
    }
    {
        let app = app.clone();
        sim_panel_window.on_sim_trend_action(move |i, action| {
            let mut a = app.borrow_mut();
            let idx = i as usize;
            if idx >= a.sim_widgets.len() || a.sim_widgets[idx].kind != SimKind::Trend {
                return;
            }
            match action {
                0 => {
                    a.sim_widgets[idx].trace_paused = !a.sim_widgets[idx].trace_paused;
                    let paused = a.sim_widgets[idx].trace_paused;
                    let name = a.sim_widgets[idx].name.clone();
                    a.log(format!(
                        "趋势图「{name}」{}",
                        if paused { "已暂停" } else { "继续采样" }
                    ));
                }
                1 => {
                    a.sim_widgets[idx].trace_history.clear();
                    let name = a.sim_widgets[idx].name.clone();
                    a.log(format!("趋势图「{name}」已清空"));
                }
                _ => return,
            }
            sim_set_row(&a, idx);
        });
    }
}
