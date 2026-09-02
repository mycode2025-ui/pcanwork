//! Simulation panel helpers: signal generation, per-tick stepping, row/model build,
//! property fill, and DBC signal choices. Extracted from main.rs.

use crate::can::{CanFrame, Cmd, SimGeneratorMode, SimPeriodicConfig, SimSignalGenerator};
use crate::dbc::DbcDb;
use crate::{
    App, GenMode, SimKind, SimPanelWindow, SimPropWindow, SimRow, SimWidget, fmtf, key_of,
};
use slint::{Model, ModelRc, VecModel};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct SimFrameProfile {
    pub(crate) extended: bool,
    pub(crate) fd: bool,
    pub(crate) brs: bool,
    pub(crate) dlc: u8,
}

impl SimFrameProfile {
    pub(crate) fn new(extended: bool, fd: bool, brs: bool, dlc: u8) -> Self {
        Self {
            extended,
            fd,
            brs,
            dlc,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct SimFrameCacheKey {
    channel: u8,
    dbc_path: String,
    id: u32,
    profile: SimFrameProfile,
}

impl SimFrameCacheKey {
    fn new(channel: u8, dbc_path: String, id: u32, profile: SimFrameProfile) -> Self {
        Self {
            channel: channel.max(1),
            dbc_path,
            id,
            profile,
        }
    }
}

#[derive(Clone)]
struct SimSampleBinding {
    widget_index: usize,
    key: u64,
    dbc_path: String,
    frame_id: u32,
    signals: Vec<String>,
}

struct SimSampleRow {
    widget_index: usize,
    values: Vec<Option<f64>>,
    error: Option<String>,
}

struct SimSampleResult {
    generation: u64,
    rows: Vec<SimSampleRow>,
}

enum SimSamplerCommand {
    Configure {
        generation: u64,
        bindings: Vec<SimSampleBinding>,
        dbcs: Vec<DbcDb>,
        dbc_paths: Vec<String>,
    },
    Sample {
        generation: u64,
        frames: std::collections::HashMap<u64, Vec<u8>>,
    },
}

pub(crate) struct SimSampler {
    commands: crossbeam_channel::Sender<SimSamplerCommand>,
    results: crossbeam_channel::Receiver<SimSampleResult>,
    skipped_requests: Arc<AtomicU64>,
    skipped_results: Arc<AtomicU64>,
}

impl SimSampler {
    pub(crate) fn spawn() -> Self {
        let (command_tx, command_rx) = crossbeam_channel::bounded(4);
        let (result_tx, result_rx) = crossbeam_channel::bounded(2);
        let skipped_requests = Arc::new(AtomicU64::new(0));
        let skipped_results = Arc::new(AtomicU64::new(0));
        let output_skips = skipped_results.clone();
        std::thread::Builder::new()
            .name("pcanwork-sim-sampler".into())
            .spawn(move || {
                let mut generation = 0u64;
                let mut bindings = Vec::new();
                let mut dbcs = Vec::new();
                let mut dbc_paths = Vec::new();
                while let Ok(command) = command_rx.recv() {
                    match command {
                        SimSamplerCommand::Configure {
                            generation: next_generation,
                            bindings: next_bindings,
                            dbcs: next_dbcs,
                            dbc_paths: next_paths,
                        } => {
                            generation = next_generation;
                            bindings = next_bindings;
                            dbcs = next_dbcs;
                            dbc_paths = next_paths;
                        }
                        SimSamplerCommand::Sample {
                            generation: sample_generation,
                            frames,
                        } => {
                            if sample_generation != generation {
                                continue;
                            }
                            let mut rows = Vec::with_capacity(bindings.len());
                            for binding in &bindings {
                                let Some(data) = frames.get(&binding.key) else {
                                    continue;
                                };
                                let mut values = Vec::with_capacity(binding.signals.len());
                                let mut error = None;
                                for signal in &binding.signals {
                                    match sim_decode_value(
                                        &dbcs,
                                        &dbc_paths,
                                        &binding.dbc_path,
                                        binding.frame_id,
                                        signal,
                                        data,
                                    ) {
                                        Ok(value) => values.push(value),
                                        Err(message) => {
                                            values.push(None);
                                            error.get_or_insert(message);
                                        }
                                    }
                                }
                                rows.push(SimSampleRow {
                                    widget_index: binding.widget_index,
                                    values,
                                    error,
                                });
                            }
                            if result_tx
                                .try_send(SimSampleResult { generation, rows })
                                .is_err()
                            {
                                output_skips.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
            .expect("simulation sampler thread");
        Self {
            commands: command_tx,
            results: result_rx,
            skipped_requests,
            skipped_results,
        }
    }

    fn configure(
        &self,
        generation: u64,
        bindings: Vec<SimSampleBinding>,
        dbcs: Vec<DbcDb>,
        dbc_paths: Vec<String>,
    ) -> bool {
        self.commands
            .try_send(SimSamplerCommand::Configure {
                generation,
                bindings,
                dbcs,
                dbc_paths,
            })
            .is_ok()
    }

    fn sample(&self, generation: u64, frames: std::collections::HashMap<u64, Vec<u8>>) {
        if self
            .commands
            .try_send(SimSamplerCommand::Sample { generation, frames })
            .is_err()
        {
            self.skipped_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn latest(&self) -> Option<SimSampleResult> {
        let mut latest = None;
        while let Ok(result) = self.results.try_recv() {
            latest = Some(result);
        }
        latest
    }

    fn skipped(&self) -> (u64, u64) {
        (
            self.skipped_requests.load(Ordering::Relaxed),
            self.skipped_results.load(Ordering::Relaxed),
        )
    }
}

pub(crate) fn dbc_path_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.replace('/', "\\")
            .eq_ignore_ascii_case(&right.replace('/', "\\"))
    } else {
        left == right
    }
}

fn dbc_short_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn dbc_contains_signal(db: &DbcDb, id: u32, signal: &str) -> bool {
    db.messages()
        .any(|message| message.id == id && message.signals.iter().any(|item| item.name == signal))
}

fn sim_binding_extended(db: &DbcDb, id: u32, signal: &str) -> Result<bool, String> {
    let mut matches = db.messages().filter(|message| {
        message.id == id && message.signals.iter().any(|item| item.name == signal)
    });
    let Some(message) = matches.next() else {
        return Err(format!("DBC 中不存在 0x{id:X}/{signal}"));
    };
    let extended = message.extended;
    if matches.any(|message| message.extended != extended) {
        return Err(format!(
            "DBC 同时定义标准帧和扩展帧 0x{id:X}/{signal}，绑定不明确"
        ));
    }
    Ok(extended)
}

fn sim_binding_dbc_index(
    dbcs: &[DbcDb],
    dbc_paths: &[String],
    dbc_path: &str,
    id: u32,
    signal: &str,
) -> Result<usize, String> {
    if signal.is_empty() {
        return Err("原始字节绑定不需要 DBC".to_string());
    }
    if !dbc_path.trim().is_empty() {
        let Some(index) = dbc_paths
            .iter()
            .position(|loaded| dbc_path_eq(loaded, dbc_path))
        else {
            return Err(format!("指定 DBC 未加载: {}", dbc_short_name(dbc_path)));
        };
        let Some(db) = dbcs.get(index) else {
            return Err("DBC 文件列表与解析数据库不同步".to_string());
        };
        if !dbc_contains_signal(db, id, signal) {
            return Err(format!(
                "{} 中不存在 0x{id:X}/{}",
                dbc_short_name(dbc_path),
                signal
            ));
        }
        return Ok(index);
    }

    let mut matches = dbcs
        .iter()
        .enumerate()
        .filter_map(|(index, db)| dbc_contains_signal(db, id, signal).then_some(index));
    let Some(index) = matches.next() else {
        return Err(format!("没有 DBC 定义 0x{id:X}/{signal}"));
    };
    if matches.next().is_some() {
        return Err(format!(
            "多个 DBC 同时定义 0x{id:X}/{signal}，必须明确选择 DBC 文件"
        ));
    }
    Ok(index)
}

fn canonical_can_fd_length(required: u64) -> Option<u8> {
    [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64]
        .into_iter()
        .find(|length| *length as u64 >= required)
}

fn validate_sim_frame_profile(
    id: u32,
    signal: &str,
    profile: SimFrameProfile,
    dbc_profile: Option<SimFrameProfile>,
) -> Result<(), String> {
    let SimFrameProfile {
        extended,
        fd,
        brs,
        dlc,
    } = profile;
    if id > if extended { 0x1FFF_FFFF } else { 0x7FF } {
        return Err(format!(
            "0x{id:X} 超出{}帧 ID 范围",
            if extended { "扩展" } else { "标准" }
        ));
    }
    if brs && !fd {
        return Err("BRS 只能用于 CAN FD 帧".into());
    }
    if !fd && dlc > 8 {
        return Err(format!("经典 CAN 的 DLC 不能超过 8（当前 {dlc}）"));
    }
    if fd && !matches!(dlc, 0..=8 | 12 | 16 | 20 | 24 | 32 | 48 | 64) {
        return Err(format!(
            "CAN FD DLC={dlc} 无效；允许 0..8、12、16、20、24、32、48、64"
        ));
    }
    if !signal.is_empty() {
        let dbc_profile = dbc_profile.ok_or_else(|| "DBC 帧属性不可用".to_string())?;
        if extended != dbc_profile.extended {
            return Err(format!(
                "DBC 定义为{}帧，但控件配置为{}帧",
                if dbc_profile.extended {
                    "扩展"
                } else {
                    "标准"
                },
                if extended { "扩展" } else { "标准" }
            ));
        }
        if dbc_profile.fd && !fd {
            return Err(format!(
                "DBC 报文需要 {} 字节，必须启用 CAN FD",
                dbc_profile.dlc
            ));
        }
        if dlc < dbc_profile.dlc {
            return Err(format!(
                "DBC 报文至少需要 {} 字节，但控件 DLC={dlc}",
                dbc_profile.dlc
            ));
        }
    }
    Ok(())
}

pub(crate) fn sim_validate_binding_profile(
    a: &App,
    dbc_path: &str,
    id: u32,
    signal: &str,
    profile: SimFrameProfile,
) -> Result<(), String> {
    let dbc_profile = if signal.is_empty() {
        None
    } else {
        Some(sim_binding_frame_profile(
            &a.dbcs,
            &a.dbc_paths,
            dbc_path,
            id,
            signal,
        )?)
    };
    validate_sim_frame_profile(id, signal, profile, dbc_profile)
}

pub(crate) fn sim_binding_frame_profile(
    dbcs: &[DbcDb],
    dbc_paths: &[String],
    dbc_path: &str,
    id: u32,
    signal: &str,
) -> Result<SimFrameProfile, String> {
    if signal.is_empty() {
        return Ok(SimFrameProfile::new(id > 0x7FF, false, false, 8));
    }
    let index = sim_binding_dbc_index(dbcs, dbc_paths, dbc_path, id, signal)?;
    let extended = sim_binding_extended(&dbcs[index], id, signal)?;
    let message = dbcs[index]
        .message_ext(id, extended)
        .ok_or_else(|| format!("DBC 中不存在 0x{id:X} ext={extended}"))?;
    let dlc = canonical_can_fd_length(message.size)
        .ok_or_else(|| format!("DBC 报文长度 {} 超过 CAN FD 64 字节上限", message.size))?;
    Ok(SimFrameProfile::new(extended, message.size > 8, false, dlc))
}

pub(crate) fn configure_sim_generators(a: &App, enable: bool) -> Result<usize, String> {
    if !enable {
        a.cmd
            .send(Cmd::SetSimulationPeriodics(Vec::new()))
            .map_err(|error| format!("停止仿真发生器失败: {error:?}"))?;
        return Ok(0);
    }

    struct Group {
        config: SimPeriodicConfig,
        signals: std::collections::HashSet<String>,
    }
    let mut groups: std::collections::HashMap<SimFrameCacheKey, Group> =
        std::collections::HashMap::new();
    for widget in a
        .sim_widgets
        .iter()
        .filter(|widget| widget.enabled && widget.kind == SimKind::SignalGen)
    {
        let profile = SimFrameProfile::new(
            widget.frame_extended,
            widget.frame_fd,
            widget.frame_brs,
            widget.frame_dlc,
        );
        sim_validate_binding_profile(
            a,
            &widget.dbc_path,
            widget.frame_id,
            &widget.signal,
            profile,
        )?;
        let (loaded_path, dbc) = if widget.signal.is_empty() {
            (String::new(), None)
        } else {
            let index = sim_binding_dbc_index(
                &a.dbcs,
                &a.dbc_paths,
                &widget.dbc_path,
                widget.frame_id,
                &widget.signal,
            )?;
            (a.dbc_paths[index].clone(), Some(a.dbcs[index].clone()))
        };
        let key = SimFrameCacheKey::new(
            widget.channel.max(1),
            loaded_path.clone(),
            widget.frame_id,
            profile,
        );
        let base = a
            .sim_tx_frames
            .get(&key)
            .cloned()
            .unwrap_or_else(|| vec![0; widget.frame_dlc as usize]);
        let group = groups.entry(key).or_insert_with(|| Group {
            config: SimPeriodicConfig {
                frame: CanFrame {
                    t: 0.0,
                    ch: widget.channel.max(1),
                    tx: true,
                    id: widget.frame_id,
                    ext: widget.frame_extended,
                    fd: widget.frame_fd,
                    brs: widget.frame_brs,
                    remote: false,
                    error: false,
                    data: base,
                },
                dbc,
                dbc_id: widget.frame_id,
                generators: Vec::new(),
            },
            signals: std::collections::HashSet::new(),
        });
        if !group.signals.insert(widget.signal.clone()) {
            return Err(format!(
                "CAN{} 0x{:X}/{} 被多个发生器重复绑定，无法确定唯一输出值",
                widget.channel,
                widget.frame_id,
                if widget.signal.is_empty() {
                    "byte0"
                } else {
                    &widget.signal
                }
            ));
        }
        let mode = match widget.gen_mode {
            GenMode::Constant => SimGeneratorMode::Constant { value: widget.min },
            GenMode::Ramp => SimGeneratorMode::Ramp {
                min: widget.min,
                max: widget.max,
                step: widget.gen_step,
            },
            GenMode::Sine => SimGeneratorMode::Sine {
                min: widget.min,
                max: widget.max,
            },
        };
        group.config.generators.push(SimSignalGenerator {
            signal: widget.signal.clone(),
            mode,
            period_ms: widget.period_ms.max(10),
        });
    }
    let count = groups
        .values()
        .map(|group| group.config.generators.len())
        .sum();
    a.cmd
        .send(Cmd::SetSimulationPeriodics(
            groups.into_values().map(|group| group.config).collect(),
        ))
        .map_err(|error| format!("启动仿真发生器失败: {error:?}"))?;
    Ok(count)
}

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

#[cfg(test)]
pub(crate) fn sim_encode_value(
    dbcs: &[DbcDb],
    dbc_paths: &[String],
    dbc_path: &str,
    id: u32,
    signal: &str,
    val: f64,
) -> Result<Vec<u8>, String> {
    if !signal.is_empty() {
        let index = sim_binding_dbc_index(dbcs, dbc_paths, dbc_path, id, signal)?;
        let mut m = std::collections::HashMap::new();
        m.insert(signal.to_string(), val);
        let extended = dbcs[index].message_ext(id, false).is_none();
        dbcs[index].encode_ext(id, extended, &m).ok_or_else(|| {
            format!(
                "{} 无法编码 0x{id:X}/{signal}",
                dbc_short_name(&dbc_paths[index])
            )
        })
    } else {
        let mut d = vec![0u8; 8];
        d[0] = val.clamp(0.0, 255.0) as u8;
        Ok(d)
    }
}

/// Build one frame and send it. Named signals are encoded only by the explicitly
/// selected DBC (or a unique legacy match); ambiguous bindings never fall back.
pub(crate) fn sim_send(
    a: &mut App,
    ch: u8,
    dbc_path: &str,
    id: u32,
    signal: &str,
    val: f64,
    profile: SimFrameProfile,
) -> Result<(), String> {
    sim_send_updates(a, ch, dbc_path, id, &[(signal, val)], profile)
}

/// Encode every changed signal of one CAN frame into a shared byte image and
/// submit exactly one frame. This prevents controls bound to the same message
/// from overwriting each other's bits when they become due in the same tick.
fn sim_send_updates(
    a: &mut App,
    ch: u8,
    dbc_path: &str,
    id: u32,
    updates: &[(&str, f64)],
    profile: SimFrameProfile,
) -> Result<(), String> {
    if updates.is_empty() {
        return Ok(());
    }
    let named = updates.iter().any(|(signal, _)| !signal.is_empty());
    if named && updates.iter().any(|(signal, _)| signal.is_empty()) {
        return Err("同一原子发送组不能混合 DBC 信号与原始字节绑定".into());
    }
    let first_signal = updates[0].0;
    sim_validate_binding_profile(a, dbc_path, id, first_signal, profile)?;
    let (data, ext, cache_key) = if !named {
        let mut data = vec![0u8; profile.dlc as usize];
        if let Some(first) = data.first_mut() {
            *first = updates
                .last()
                .map(|(_, value)| *value)
                .unwrap_or_default()
                .clamp(0.0, 255.0) as u8;
        }
        (data, profile.extended, None)
    } else {
        let index = sim_binding_dbc_index(&a.dbcs, &a.dbc_paths, dbc_path, id, first_signal)?;
        let loaded_path = a.dbc_paths[index].clone();
        let key = SimFrameCacheKey::new(ch, loaded_path, id, profile);
        let mut data = a.sim_tx_frames.get(&key).cloned().unwrap_or_default();
        for (signal, value) in updates {
            let update_index = sim_binding_dbc_index(&a.dbcs, &a.dbc_paths, dbc_path, id, signal)?;
            if update_index != index {
                return Err(format!(
                    "同一报文的信号来自不同 DBC：{} 与 {}",
                    dbc_short_name(&a.dbc_paths[index]),
                    dbc_short_name(&a.dbc_paths[update_index])
                ));
            }
            let dbc_profile =
                sim_binding_frame_profile(&a.dbcs, &a.dbc_paths, dbc_path, id, signal)?;
            validate_sim_frame_profile(id, signal, profile, Some(dbc_profile))?;
            data = a.dbcs[index].encode_signal_into_ext(
                id,
                profile.extended,
                &data,
                signal,
                *value,
            )?;
        }
        data.resize(profile.dlc as usize, 0);
        (data, profile.extended, Some(key))
    };
    let f = CanFrame {
        t: 0.0,
        ch: ch.max(1),
        tx: true,
        id,
        ext,
        fd: profile.fd,
        brs: profile.brs,
        remote: false,
        error: false,
        data: data.clone(),
    };
    a.cmd
        .send(Cmd::SendOnce(f))
        .map_err(|error| format!("CAN{ch} 发送队列不可用: {error:?}"))?;
    if let Some(key) = cache_key {
        a.sim_tx_frames.insert(key, data);
    }
    Ok(())
}

fn sim_sample_configuration(a: &App) -> (u64, Vec<SimSampleBinding>, Vec<u64>) {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (Arc::as_ptr(&a.dbc_snap) as usize).hash(&mut hasher);
    let mut bindings = Vec::new();
    let mut keys = Vec::new();
    for (index, widget) in a.sim_widgets.iter().enumerate() {
        if !matches!(
            widget.kind,
            SimKind::Indicator
                | SimKind::Dial
                | SimKind::Bar
                | SimKind::Numeric
                | SimKind::Trend
                | SimKind::Level
                | SimKind::BarChart
                | SimKind::StatusGroup
                | SimKind::Alarm
        ) {
            continue;
        }
        let signals = if matches!(
            widget.kind,
            SimKind::Trend | SimKind::BarChart | SimKind::StatusGroup
        ) {
            sim_trace_signal_names(widget)
        } else {
            vec![widget.signal.clone()]
        };
        index.hash(&mut hasher);
        widget.kind.to_i32().hash(&mut hasher);
        widget.channel.hash(&mut hasher);
        widget.dbc_path.hash(&mut hasher);
        widget.frame_id.hash(&mut hasher);
        widget.frame_extended.hash(&mut hasher);
        signals.hash(&mut hasher);
        let key = key_of(
            widget.channel.max(1),
            false,
            widget.frame_extended,
            widget.frame_id,
        );
        bindings.push(SimSampleBinding {
            widget_index: index,
            key,
            dbc_path: widget.dbc_path.clone(),
            frame_id: widget.frame_id,
            signals,
        });
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    (hasher.finish(), bindings, keys)
}

fn apply_sim_sample_result(a: &mut App, result: SimSampleResult) {
    if result.generation != a.sim_sampler_generation {
        return;
    }
    for row in result.rows {
        if row.widget_index >= a.sim_widgets.len() {
            continue;
        }
        let kind = a.sim_widgets[row.widget_index].kind;
        if kind == SimKind::Trend {
            if !a.sim_widgets[row.widget_index].trace_paused {
                let max_samples = (a.sim_widgets[row.widget_index]
                    .trace_window_secs
                    .clamp(5, 600)
                    * 10) as usize;
                while a.sim_widgets[row.widget_index].trace_history.len() < row.values.len() {
                    a.sim_widgets[row.widget_index]
                        .trace_history
                        .push(std::collections::VecDeque::new());
                }
                a.sim_widgets[row.widget_index]
                    .trace_history
                    .truncate(row.values.len());
                for (series, value) in row.values.iter().enumerate() {
                    if let Some(value) = value {
                        let history = &mut a.sim_widgets[row.widget_index].trace_history[series];
                        history.push_back(*value);
                        while history.len() > max_samples {
                            history.pop_front();
                        }
                        if series == 0 {
                            a.sim_widgets[row.widget_index].cur = *value;
                        }
                    }
                }
            }
        } else if matches!(kind, SimKind::BarChart | SimKind::StatusGroup) {
            a.sim_widgets[row.widget_index].group_values = row.values;
            if let Some(value) = a.sim_widgets[row.widget_index]
                .group_values
                .first()
                .copied()
                .flatten()
            {
                a.sim_widgets[row.widget_index].cur = value;
            }
        } else if let Some(value) = row.values.first().copied().flatten() {
            a.sim_widgets[row.widget_index].cur = value;
        }

        if let Some(error) = row.error {
            if !a.sim_widgets[row.widget_index].binding_error_reported {
                a.log(format!(
                    "仿真控件「{}」后台采样失败: {error}",
                    a.sim_widgets[row.widget_index].name
                ));
            }
            a.sim_widgets[row.widget_index].binding_error_reported = true;
        } else {
            a.sim_widgets[row.widget_index].binding_error_reported = false;
        }
    }
}

fn sim_sample_tick(a: &mut App) {
    let (signature, bindings, keys) = sim_sample_configuration(a);
    if signature != a.sim_sampler_signature {
        let generation = a.sim_sampler_generation.wrapping_add(1);
        if !a
            .sim_sampler
            .configure(generation, bindings, a.dbcs.clone(), a.dbc_paths.clone())
        {
            return;
        }
        a.sim_sampler_signature = signature;
        a.sim_sampler_generation = generation;
        a.sim_sampler_keys = keys;
    }

    if let Some(result) = a.sim_sampler.latest() {
        apply_sim_sample_result(a, result);
    }
    let frames = a
        .sim_sampler_keys
        .iter()
        .filter_map(|key| a.last.get(key).map(|last| (*key, last.data.clone())))
        .collect();
    a.sim_sampler.sample(a.sim_sampler_generation, frames);

    let skipped = a.sim_sampler.skipped();
    if skipped != a.sim_sampler_reported_skips {
        a.sim_sampler_reported_skips = skipped;
        a.log(format!(
            "仿真后台采样节流: 请求 {}，结果 {}（CAN 报文未丢失）",
            skipped.0, skipped.1
        ));
    }
}

/// Sim panel step (every ~100ms). DBC decoding and curve sampling run on the
/// dedicated sampler thread; this UI callback only applies completed values.
pub(crate) fn sim_tick(a: &mut App) {
    // 图片只在路径改变时解码一次；仿真模型以 10 Hz 刷新，绝不能每帧访问磁盘。
    let mut image_errors = Vec::new();
    for widget in &mut a.sim_widgets {
        if widget.kind == SimKind::Image && widget.image_cache_path != widget.image_path {
            widget.image_cache_path = widget.image_path.clone();
            match slint::Image::load_from_path(Path::new(&widget.image_path)) {
                Ok(image) if !widget.image_path.is_empty() => {
                    widget.image_cache = image;
                    widget.image_load_ok = true;
                }
                _ => {
                    widget.image_cache = slint::Image::default();
                    widget.image_load_ok = false;
                    if !widget.image_path.is_empty() {
                        image_errors.push(format!(
                            "图片控件「{}」无法加载: {}",
                            widget.name, widget.image_path
                        ));
                    }
                }
            }
        }
    }
    for error in image_errors {
        a.log(error);
    }
    sim_sample_tick(a);
}

/// Decode a widget value. A named signal must resolve to exactly one selected DBC.
pub(crate) fn sim_decode_value(
    dbcs: &[DbcDb],
    dbc_paths: &[String],
    dbc_path: &str,
    id: u32,
    signal: &str,
    data: &[u8],
) -> Result<Option<f64>, String> {
    if signal.is_empty() {
        return Ok(data.first().map(|byte| *byte as f64));
    }
    let index = sim_binding_dbc_index(dbcs, dbc_paths, dbc_path, id, signal)?;
    let extended = sim_binding_extended(&dbcs[index], id, signal)?;
    Ok(dbcs[index]
        .decode_ext(id, extended, data)
        .into_iter()
        .find(|decoded| decoded.name == signal)
        .map(|decoded| decoded.physical))
}

/// Build the display row for one widget.
pub(crate) fn sim_make_row(w: &SimWidget, selected: bool, primary: bool) -> SimRow {
    let source = if w.dbc_path.is_empty() {
        if w.signal.is_empty() {
            "RAW".to_string()
        } else {
            "DBC未指定".to_string()
        }
    } else {
        dbc_short_name(&w.dbc_path)
    };
    let info = if w.signal.is_empty() {
        format!("CAN{} · {} · 0x{:X}", w.channel, source, w.frame_id)
    } else {
        format!(
            "CAN{} · {} · 0x{:X}/{}",
            w.channel, source, w.frame_id, w.signal
        )
    };
    let span = (w.max - w.min).abs().max(1e-9);
    let level = match w.kind {
        SimKind::Dial | SimKind::Bar | SimKind::Level => {
            ((w.cur - w.min) / span).clamp(0.0, 1.0) as f32
        }
        SimKind::Slider | SimKind::Knob | SimKind::Input => {
            ((w.slider_val - w.min) / span).clamp(0.0, 1.0) as f32
        }
        _ => 0.0,
    };
    let status = match w.kind {
        SimKind::Numeric | SimKind::Dial | SimKind::Bar | SimKind::Level | SimKind::Alarm => {
            format!("{:.2}", w.cur)
        }
        SimKind::Slider | SimKind::Knob | SimKind::Input => format!("{:.2}", w.slider_val),
        SimKind::Trend => w
            .trace_history
            .first()
            .and_then(|history| history.back())
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "--".to_string()),
        SimKind::SignalGen => format!("{:.1}", sim_gen_value(w)),
        _ => String::new(),
    };
    let (trace_min, trace_max) = sim_trace_range(w);
    let paths = [0, 1, 2, 3].map(|index| {
        w.trace_history
            .get(index)
            .map(|history| sim_trace_path(history, trace_min, trace_max))
            .unwrap_or_default()
    });
    let trace_names = sim_trace_signal_names(w);
    let trace_labels = trace_names.join(" · ");
    let series_labels: [String; 4] = std::array::from_fn(|index| {
        trace_names
            .get(index)
            .map(|name| {
                if name.is_empty() {
                    "byte0".to_string()
                } else {
                    name.clone()
                }
            })
            .unwrap_or_default()
    });
    let series_values: [Option<f64>; 4] = std::array::from_fn(|index| {
        if w.kind == SimKind::Trend {
            w.trace_history
                .get(index)
                .and_then(|history| history.back())
                .copied()
        } else {
            w.group_values.get(index).copied().flatten()
        }
    });
    let series_text: [String; 4] = std::array::from_fn(|index| {
        series_values[index]
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "--".to_string())
    });
    let series_level: [f32; 4] = std::array::from_fn(|index| {
        series_values[index]
            .map(|value| ((value - w.min) / span).clamp(0.0, 1.0) as f32)
            .unwrap_or(0.0)
    });
    let series_on: [bool; 4] =
        std::array::from_fn(|index| series_values[index].is_some_and(|value| value > w.threshold));
    let alarm = w.kind == SimKind::Alarm && (w.cur < w.min.min(w.max) || w.cur > w.min.max(w.max));
    let image_source = w.image_cache.clone();
    let has_image = w.kind == SimKind::Image && w.image_load_ok;
    SimRow {
        kind: w.kind.to_i32(),
        name: w.name.clone().into(),
        info: info.into(),
        status: status.into(),
        x: w.x as f32,
        y: w.y as f32,
        w: w.w as f32,
        h: w.h as f32,
        on: (w.kind == SimKind::Indicator && w.cur > w.threshold)
            || (w.kind == SimKind::Switch && w.switch_on),
        level,
        selected,
        primary,
        align: w.align,
        trace_path_1: paths[0].clone().into(),
        trace_path_2: paths[1].clone().into(),
        trace_path_3: paths[2].clone().into(),
        trace_path_4: paths[3].clone().into(),
        trace_labels: trace_labels.into(),
        series_label_1: series_labels[0].clone().into(),
        series_label_2: series_labels[1].clone().into(),
        series_label_3: series_labels[2].clone().into(),
        series_label_4: series_labels[3].clone().into(),
        series_value_1: series_text[0].clone().into(),
        series_value_2: series_text[1].clone().into(),
        series_value_3: series_text[2].clone().into(),
        series_value_4: series_text[3].clone().into(),
        series_level_1: series_level[0],
        series_level_2: series_level[1],
        series_level_3: series_level[2],
        series_level_4: series_level[3],
        series_on_1: series_on[0],
        series_on_2: series_on[1],
        series_on_3: series_on[2],
        series_on_4: series_on[3],
        range_label: if w.trace_auto_range {
            format!("A {:.1}…{:.1}", trace_min, trace_max).into()
        } else {
            format!("F {:.1}…{:.1}", trace_min, trace_max).into()
        },
        paused: w.trace_paused,
        alarm,
        alarm_message: w.alarm_message.clone().into(),
        image_source,
        has_image,
    }
}

fn sim_trace_range(w: &SimWidget) -> (f64, f64) {
    if !w.trace_auto_range {
        return if w.min <= w.max {
            (w.min, w.max)
        } else {
            (w.max, w.min)
        };
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for value in w
        .trace_history
        .iter()
        .flatten()
        .filter(|value| value.is_finite())
    {
        lo = lo.min(*value);
        hi = hi.max(*value);
    }
    if !lo.is_finite() || !hi.is_finite() {
        return if w.min <= w.max {
            (w.min, w.max)
        } else {
            (w.max, w.min)
        };
    }
    if (hi - lo).abs() < 1e-9 {
        let pad = lo.abs().max(1.0) * 0.05;
        return (lo - pad, hi + pad);
    }
    let pad = (hi - lo) * 0.05;
    (lo - pad, hi + pad)
}

fn sim_trace_signal_names(w: &SimWidget) -> Vec<String> {
    let mut names = Vec::with_capacity(4);
    names.push(w.signal.trim().to_string());
    for signal in &w.trace_signals {
        let signal = signal.trim();
        if !signal.is_empty() && !names.iter().any(|existing| existing == signal) {
            names.push(signal.to_string());
            if names.len() == 4 {
                break;
            }
        }
    }
    if names.len() == 1 && names[0].is_empty() {
        return names;
    }
    names
}

pub(crate) fn sim_trace_path(
    history: &std::collections::VecDeque<f64>,
    min: f64,
    max: f64,
) -> String {
    use std::fmt::Write as _;

    if history.len() < 2 {
        return String::new();
    }
    let max_points = 120usize;
    let stride = history.len().div_ceil(max_points).max(1);
    let mut points: Vec<(usize, f64)> = history
        .iter()
        .enumerate()
        .step_by(stride)
        .map(|(index, value)| (index, *value))
        .collect();
    let last_index = history.len() - 1;
    if points.last().is_none_or(|(index, _)| *index != last_index) {
        points.push((last_index, history[last_index]));
    }
    let span = (max - min).abs().max(1e-9);
    let mut path = String::with_capacity(points.len() * 18);
    for (point, (index, value)) in points.iter().enumerate() {
        let x = *index as f64 * 100.0 / last_index as f64;
        let y = 100.0 - ((*value - min) / span).clamp(0.0, 1.0) * 100.0;
        let _ = write!(
            path,
            "{} {:.2} {:.2} ",
            if point == 0 { 'M' } else { 'L' },
            x,
            y
        );
    }
    path
}

pub(crate) fn constrain_sim_widget(w: &mut SimWidget, canvas_w: f64, canvas_h: f64) {
    let (min_w, min_h) = w.kind.min_size();
    if !w.w.is_finite() {
        w.w = min_w;
    }
    if !w.h.is_finite() {
        w.h = min_h;
    }
    if !w.x.is_finite() {
        w.x = 0.0;
    }
    if !w.y.is_finite() {
        w.y = 0.0;
    }

    if canvas_w > 0.0 && canvas_h > 0.0 {
        w.w = w.w.max(min_w).min(canvas_w);
        w.h = w.h.max(min_h).min(canvas_h);
        w.x = w.x.clamp(0.0, (canvas_w - w.w).max(0.0));
        w.y = w.y.clamp(0.0, (canvas_h - w.h).max(0.0));
    } else {
        w.w = w.w.max(min_w);
        w.h = w.h.max(min_h);
        w.x = w.x.max(0.0);
        w.y = w.y.max(0.0);
    }
}

pub(crate) fn sim_find_free_position(
    widgets: &[SimWidget],
    width: f64,
    height: f64,
    canvas_width: f64,
    canvas_height: f64,
) -> (f64, f64) {
    sim_find_free_position_from(widgets, width, height, canvas_width, canvas_height, 24.0)
}

pub(crate) fn sim_find_free_position_from(
    widgets: &[SimWidget],
    width: f64,
    height: f64,
    canvas_width: f64,
    canvas_height: f64,
    start_x: f64,
) -> (f64, f64) {
    let canvas_width = if canvas_width > 200.0 {
        canvas_width
    } else {
        1100.0
    };
    let canvas_height = if canvas_height > 150.0 {
        canvas_height
    } else {
        620.0
    };
    let margin = 12.0;
    let step = 24.0;
    let max_x = (canvas_width - width).max(0.0);
    let max_y = (canvas_height - height).max(0.0);
    let mut y = 24.0_f64.min(max_y);
    while y <= max_y {
        let mut x = start_x.max(0.0).min(max_x);
        while x <= max_x {
            let overlaps = widgets.iter().any(|widget| {
                x < widget.x + widget.w + margin
                    && x + width + margin > widget.x
                    && y < widget.y + widget.h + margin
                    && y + height + margin > widget.y
            });
            if !overlaps {
                return (x, y);
            }
            x += step;
        }
        y += step;
    }
    let offset = (widgets.len() % 8) as f64 * 18.0;
    ((start_x + offset).min(max_x), offset.min(max_y))
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

/// Keep the simulation workspace header/status synchronized with the main project.
pub(crate) fn refresh_sim_context(window: &SimPanelWindow, a: &App) {
    window.set_running(a.sim_running);
    window.set_project_name(a.project_name.clone().into());
    window.set_project_path(
        a.project_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_layout_dirty(a.sim_dirty);
    window.set_widget_count(a.sim_widgets.len() as i32);
    window.set_selected_count(a.sim_multi.len() as i32);
    window.set_dbc_count(a.dbcs.len() as i32);
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
pub(crate) fn sim_fill_props(win: &SimPropWindow, w: &SimWidget, a: &App) {
    win.set_has_sel(true);
    win.set_p_name(w.name.clone().into());
    win.set_p_dbc(w.dbc_path.clone().into());
    win.set_p_frame(format!("{:X}", w.frame_id).into());
    win.set_p_extended(w.frame_extended);
    win.set_p_fd(w.frame_fd);
    win.set_p_brs(w.frame_brs);
    win.set_p_dlc(w.frame_dlc.to_string().into());
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
    win.set_p_trace_signals(w.trace_signals.join(", ").into());
    win.set_p_trace_window(w.trace_window_secs.to_string().into());
    win.set_p_trace_auto(w.trace_auto_range);
    win.set_p_alarm_message(w.alarm_message.clone().into());
    win.set_p_image_path(w.image_path.clone().into());
    let binding_result = sim_validate_binding_profile(
        a,
        &w.dbc_path,
        w.frame_id,
        &w.signal,
        SimFrameProfile::new(w.frame_extended, w.frame_fd, w.frame_brs, w.frame_dlc),
    );
    win.set_p_binding_ok(binding_result.is_ok());
    win.set_p_binding_error(binding_result.err().unwrap_or_default().into());
    let message_name = a
        .dbc_paths
        .iter()
        .position(|path| dbc_path_eq(path, &w.dbc_path))
        .and_then(|index| a.dbcs.get(index))
        .and_then(|db| db.message_ext(w.frame_id, w.frame_extended))
        .map(|message| message.name.as_str())
        .unwrap_or("");
    let profile = format!(
        "{} · {}{} · DLC {}",
        if w.frame_extended { "EXT" } else { "STD" },
        if w.frame_fd { "CAN FD" } else { "CAN" },
        if w.frame_brs { "/BRS" } else { "" },
        w.frame_dlc
    );
    win.set_p_bind(if w.signal.is_empty() {
        format!(
            "CAN{} · {} · RAW · 0x{:X}/byte0",
            w.channel, profile, w.frame_id
        )
        .into()
    } else if w.dbc_path.is_empty() {
        format!(
            "CAN{} · {} · DBC未指定 · 0x{:X}/{}",
            w.channel, profile, w.frame_id, w.signal
        )
        .into()
    } else {
        if message_name.is_empty() {
            format!(
                "CAN{} · {} · {} · 0x{:X}/{}",
                w.channel,
                profile,
                dbc_short_name(&w.dbc_path),
                w.frame_id,
                w.signal
            )
            .into()
        } else {
            format!(
                "CAN{} · {} · {} · 0x{:X} {} / {}",
                w.channel,
                profile,
                dbc_short_name(&w.dbc_path),
                w.frame_id,
                message_name,
                w.signal
            )
            .into()
        }
    });
}

/// Build the DBC file selector. Index zero means raw/unbound; subsequent indices
/// are stable paths aligned with App.dbcs/App.dbc_paths.
pub(crate) fn sim_dbc_choices(
    a: &App,
    current_path: &str,
) -> (Vec<String>, Vec<slint::SharedString>) {
    let mut choices = vec![String::new()];
    let mut rows = vec![if a.lang_en {
        "No DBC (raw byte binding)".into()
    } else {
        "不选择 DBC（原始字节绑定）".into()
    }];
    for (index, path) in a.dbc_paths.iter().enumerate() {
        let parsed_name = a
            .dbcs
            .get(index)
            .map(|db| db.file_name.as_str())
            .unwrap_or_default();
        let short = if parsed_name.is_empty() {
            dbc_short_name(path)
        } else {
            parsed_name.to_string()
        };
        rows.push(format!("{short}  ·  {path}").into());
        choices.push(path.clone());
    }
    if !current_path.is_empty()
        && !choices
            .iter()
            .any(|loaded| dbc_path_eq(loaded, current_path))
    {
        rows.push(
            if a.lang_en {
                format!("Missing DBC  ·  {current_path}")
            } else {
                format!("DBC未加载  ·  {current_path}")
            }
            .into(),
        );
        choices.push(current_path.to_string());
    }
    (choices, rows)
}

/// Look up the [min, max] range in the selected DBC (only when max > min).
pub(crate) fn sim_signal_range(
    a: &App,
    dbc_path: &str,
    id: u32,
    signal: &str,
) -> Option<(f64, f64)> {
    sim_signal_range_in(&a.dbcs, &a.dbc_paths, dbc_path, id, signal)
}

pub(crate) fn sim_signal_range_in(
    dbcs: &[crate::dbc::DbcDb],
    dbc_paths: &[String],
    dbc_path: &str,
    id: u32,
    signal: &str,
) -> Option<(f64, f64)> {
    if signal.is_empty() {
        return None;
    }
    let index = sim_binding_dbc_index(dbcs, dbc_paths, dbc_path, id, signal).ok()?;
    let extended = sim_binding_extended(&dbcs[index], id, signal).ok()?;
    if let Some(message) = dbcs[index].message_ext(id, extended)
        && let Some(signal) = message.signals.iter().find(|item| item.name == signal)
        && let Some((min, max)) = signal.effective_physical_range()
        && max > min
    {
        return Some((min, max));
    }
    None
}

/// Build the signal selector from one explicit DBC. Signals from different files
/// are never merged or de-duplicated.
pub(crate) fn sim_signal_choices(
    a: &App,
    dbc_path: &str,
) -> (Vec<(u32, String)>, Vec<slint::SharedString>) {
    let mut choices: Vec<(u32, String)> = Vec::new();
    let mut rows: Vec<slint::SharedString> = Vec::new();
    let Some(index) = a
        .dbc_paths
        .iter()
        .position(|path| dbc_path_eq(path, dbc_path))
    else {
        return (choices, rows);
    };
    if let Some(db) = a.dbcs.get(index) {
        for message in db.messages() {
            for signal in &message.signals {
                let warning = if signal.fits_in_bytes(message.size) {
                    ""
                } else if a.lang_en {
                    "  ·  invalid bit layout"
                } else {
                    "  ·  位布局超出DLC"
                };
                rows.push(
                    format!(
                        "{}  ·  0x{:X} {}{}",
                        signal.name, message.id, message.name, warning
                    )
                    .into(),
                );
                choices.push((message.id, signal.name.clone()));
            }
        }
    }
    (choices, rows)
}

/// Build the additional-series selector for the widget's exact primary binding.
/// Index zero explicitly means "not selected". Every other row is constrained
/// to the same DBC file and frame; channel is displayed so the resulting CAN
/// relationship is unambiguous in the property window.
pub(crate) fn sim_frame_signal_choices(
    a: &App,
    dbc_path: &str,
    frame_id: u32,
    primary_signal: &str,
    channel: u8,
) -> (Vec<String>, Vec<slint::SharedString>) {
    let mut choices = vec![String::new()];
    let mut rows = vec![if a.lang_en {
        "Not selected".into()
    } else {
        "不选择".into()
    }];
    let Some(index) = a
        .dbc_paths
        .iter()
        .position(|path| dbc_path_eq(path, dbc_path))
    else {
        return (choices, rows);
    };
    let Some(message) = a.dbcs.get(index).and_then(|db| db.message(frame_id)) else {
        return (choices, rows);
    };
    let dbc_name = dbc_short_name(dbc_path);
    for signal in &message.signals {
        if signal.name == primary_signal {
            continue;
        }
        rows.push(
            format!(
                "{}  ·  CAN{} · 0x{:X} {} · {}",
                signal.name,
                channel.max(1),
                frame_id,
                message.name,
                dbc_name
            )
            .into(),
        );
        choices.push(signal.name.clone());
    }
    (choices, rows)
}

pub(crate) fn sim_prepare_props(win: &SimPropWindow, a: &App, w: &SimWidget) {
    let (dbc_choices, dbc_rows) = sim_dbc_choices(a, &w.dbc_path);
    win.set_dbc_files(ModelRc::from(Rc::new(VecModel::from(dbc_rows))));
    win.set_p_dbc_index(
        dbc_choices
            .iter()
            .position(|path| dbc_path_eq(path, &w.dbc_path))
            .map(|index| index as i32)
            .unwrap_or(0),
    );
    let (signal_choices, signal_rows) = sim_signal_choices(a, &w.dbc_path);
    win.set_dbc_signals(ModelRc::from(Rc::new(VecModel::from(signal_rows))));
    win.set_p_signal_index(
        signal_choices
            .iter()
            .position(|(id, signal)| *id == w.frame_id && *signal == w.signal)
            .map(|index| index as i32)
            .unwrap_or(-1),
    );
    let (frame_choices, frame_rows) =
        sim_frame_signal_choices(a, &w.dbc_path, w.frame_id, &w.signal, w.channel);
    win.set_frame_signals(ModelRc::from(Rc::new(VecModel::from(frame_rows))));
    let extra_index = |slot: usize| {
        w.trace_signals
            .get(slot)
            .and_then(|signal| frame_choices.iter().position(|choice| choice == signal))
            .map(|index| index as i32)
            .unwrap_or(0)
    };
    win.set_p_extra_signal_1(extra_index(0));
    win.set_p_extra_signal_2(extra_index(1));
    win.set_p_extra_signal_3(extra_index(2));
    sim_fill_props(win, w, a);
}

/// Upgrade legacy widgets that did not persist a DBC identity. A binding is
/// migrated only when exactly one loaded DBC defines it; duplicates remain
/// deliberately unresolved so load order can never choose silently.
pub(crate) fn sim_migrate_dbc_bindings(a: &mut App) -> (usize, usize) {
    let mut migrated = 0usize;
    let mut ambiguous = 0usize;
    for widget in &mut a.sim_widgets {
        if widget.signal.is_empty() || !widget.dbc_path.is_empty() {
            continue;
        }
        let matches: Vec<usize> = a
            .dbcs
            .iter()
            .enumerate()
            .filter_map(|(index, db)| {
                dbc_contains_signal(db, widget.frame_id, &widget.signal).then_some(index)
            })
            .collect();
        if matches.len() == 1 {
            if let Some(path) = a.dbc_paths.get(matches[0]) {
                widget.dbc_path = path.clone();
                migrated += 1;
            }
        } else if matches.len() > 1 {
            ambiguous += 1;
        }
    }
    if migrated > 0 {
        a.mark_sim_dirty();
        a.log(format!(
            "已为 {migrated} 个旧版仿真控件补全唯一 DBC 绑定，请保存工程"
        ));
    }
    if ambiguous > 0 {
        a.log(format!(
            "{ambiguous} 个旧版仿真控件存在重复 DBC 定义，已保持未绑定，请在属性窗口明确选择"
        ));
    }
    let mut profile_migrations = 0usize;
    for index in 0..a.sim_widgets.len() {
        if a.sim_widgets[index].frame_profile_explicit {
            continue;
        }
        let widget = &a.sim_widgets[index];
        let profile = sim_binding_frame_profile(
            &a.dbcs,
            &a.dbc_paths,
            &widget.dbc_path,
            widget.frame_id,
            &widget.signal,
        );
        if let Ok(profile) = profile {
            let widget = &mut a.sim_widgets[index];
            widget.frame_extended = profile.extended;
            widget.frame_fd = profile.fd;
            widget.frame_brs = profile.brs;
            widget.frame_dlc = profile.dlc;
            widget.frame_profile_explicit = true;
            profile_migrations += 1;
        }
    }
    if profile_migrations > 0 {
        a.mark_sim_dirty();
        a.log(format!(
            "已为 {profile_migrations} 个旧版仿真控件补全 Ext/FD/BRS/DLC，请保存工程"
        ));
    }
    (migrated, ambiguous)
}

#[cfg(test)]
mod sampler_tests {
    use super::*;

    #[test]
    fn background_sampler_decodes_multiple_signals_without_ui_state() {
        let text = "VERSION \"\"\nBO_ 256 SampleFrame: 2 ECU\n SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B : 8|8@1+ (2,0) [0|510] \"\" Vector__XXX\n";
        let path = std::env::temp_dir().join("pcanwork_background_sampler.dbc");
        std::fs::write(&path, text).unwrap();
        let path_text = path.to_string_lossy().to_string();
        let sampler = SimSampler::spawn();
        let key = crate::key_of(1, false, false, 0x100);
        assert!(sampler.configure(
            7,
            vec![SimSampleBinding {
                widget_index: 3,
                key,
                dbc_path: path_text.clone(),
                frame_id: 0x100,
                signals: vec!["A".into(), "B".into()],
            }],
            vec![DbcDb::load(&path_text).unwrap()],
            vec![path_text],
        ));
        sampler.sample(7, std::collections::HashMap::from([(key, vec![42, 21])]));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = loop {
            if let Some(result) = sampler.latest() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "sampler timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(result.generation, 7);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].widget_index, 3);
        assert_eq!(result.rows[0].values, [Some(42.0), Some(42.0)]);
        assert!(result.rows[0].error.is_none());
        let _ = std::fs::remove_file(path);
    }
}
