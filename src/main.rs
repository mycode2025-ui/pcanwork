#![cfg_attr(windows, windows_subsystem = "windows")]

mod can {
    pub use pcanwork_core::can::*;
}
mod chart;
mod convert {
    pub use pcanwork_core::convert::*;
}
mod dbc {
    pub use pcanwork_core::dbc::*;
}
mod expr {
    pub use pcanwork_core::expr::*;
}
mod feature_hex;
mod ipc;
mod license;
mod msg_table;
mod ota {
    pub use pcanwork_core::ota::*;
}
#[path = "../shared/product_version.rs"]
mod product_version;
mod recording {
    pub use pcanwork_core::recording::*;
}
mod render;
mod settings;
mod sim;
mod tree;
mod tx;
mod update;
mod vary {
    pub use pcanwork_core::vary::*;
}
#[cfg(windows)]
mod windows_dpi;

// Callback wiring is split per-window into the wire_*.rs files below, pulled in via
// include!() so they share this module's imports and private items.
include!("wire_main.rs");
include!("wire_dialogs.rs");
include!("wire_chart.rs");
include!("wire_tx.rs");
include!("wire_ota.rs");
include!("wire_playback.rs");
include!("wire_sim.rs");
include!("wire_pyauto.rs");
use chart::{chart_full_range, refresh_chart};
use msg_table::build_msg_table;
use recording::Format as RecFmt;
use render::{build_signal_panel, build_stats, refresh_signal_picker};
use sim::{
    configure_sim_generators, constrain_sim_widget, refresh_sim, refresh_sim_context,
    sim_binding_frame_profile, sim_dbc_choices, sim_find_free_position, sim_frame_signal_choices,
    sim_migrate_dbc_bindings, sim_prepare_props, sim_send, sim_set_row, sim_signal_choices,
    sim_signal_range, sim_tick, sim_validate_binding_profile,
};
#[cfg(test)]
use sim::{sim_decode_value, sim_encode_value, sim_gen_value};
use tx::{
    build_tx_data_editor_rows, build_tx_dbc_page, collect_tx_data_editor,
    edit_tx_data_editor_byte, paste_tx_data_editor, populate_sig_panel, push_tx_list,
    selected_signal, set_signal_value, tx_list_sig, tx_task_from_form, ui_to_vary, update_tx_task,
};
use feature_hex::{
    build_feature_hex_rows, collect_feature_hex_rows, edit_feature_hex_byte,
    fill_feature_hex_rows,
};

use can::{
    CanFrame, Cmd, CommandSender, DeviceConfig, DynamicPeriodicConfig, Evt, OtaAck, OtaJob,
    OtaResponseId, OtaStep,
};
use crossbeam_channel::Sender as WorkerSender;
use dbc::{DbcDb, Decoded, MessageDef};
use serde::{Deserialize, Serialize};
use slint::{Color, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();
use pcanwork_ui_features::{
    CacheConfigWindow, ChannelConfigWindow, ConsoleHelpWindow, ConvertWindow, DbcDiagnosticRow,
    DbcDiagnosticsWindow, I18n as FeatureI18n, OtaWindow, PbFileRow, PlaybackWindow,
    ScriptRunnerWindow, SignalPickRow, SignalSelectWindow, SimPanelWindow, SimPropWindow, SimRow,
    Theme as FeatureTheme, TriggerWindow, TxByteCell as FeatureTxByteCell,
    TxByteRow as FeatureTxByteRow, UdsWindow, XcpWindow,
};

const TRACE_CAP: usize = 100_000;
pub(crate) const DISPLAY_CAP: usize = 1500;
const CHART_CAP: usize = 10_000;
const LOG_CAP: usize = 500;
const MAX_CAN_EVENTS_PER_TICK: usize = 2000;
const MAX_CAN_EVENT_TIME_PER_TICK: Duration = Duration::from_millis(8);
type ProjectLoadResult = Result<(Project, Vec<(String, DbcDb)>, Vec<String>, bool), String>;

enum WorkerEvent {
    Log(String),
    PlaybackParsed {
        replace: bool,
        files: Vec<(String, Vec<CanFrame>)>,
        errors: Vec<String>,
    },
    ConversionFinished {
        batch: bool,
        status: String,
        log: String,
    },
    DbcLoaded {
        path: String,
        result: Result<DbcDb, String>,
    },
    DbcReloaded {
        loaded: Vec<(String, DbcDb)>,
        errors: Vec<String>,
    },
    ProjectLoaded {
        path: std::path::PathBuf,
        result: Box<ProjectLoadResult>,
    },
    ProjectSaved {
        path: std::path::PathBuf,
        sim_revision: u64,
        result: Result<(), String>,
    },
    TxFilePrepared {
        path: String,
        repeat: u32,
        english: bool,
        result: Result<TxFilePayload, String>,
    },
    TxListLoaded(Result<Vec<TxTaskDto>, String>),
    HardwareScanned {
        pcan: Vec<can::PcanChannelInfo>,
        zcan: Vec<can::ZcanUsbChannelInfo>,
        elapsed_ms: u128,
    },
}

enum TxFilePayload {
    Ota(OtaJob),
    Frames(Vec<CanFrame>),
}

pub(crate) fn key_of(ch: u8, tx: bool, ext: bool, id: u32) -> u64 {
    ((ch as u64) << 40) | ((tx as u64) << 39) | ((ext as u64) << 38) | (id as u64)
}

pub(crate) fn show_child_window<C: slint::ComponentHandle + 'static>(component: &C) {
    if let Err(error) = component.show() {
        eprintln!("Failed to show child window: {error}");
        return;
    }
    activate_child_window(component.window());
    component.window().request_redraw();
    let weak = component.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(10), move || {
        if let Some(component) = weak.upgrade() {
            activate_child_window(component.window());
        }
    });
}

#[cfg(windows)]
fn activate_child_window(window: &slint::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    type Hwnd = isize;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn BringWindowToTop(h: Hwnd) -> i32;
        fn IsIconic(h: Hwnd) -> i32;
        fn SetActiveWindow(h: Hwnd) -> Hwnd;
        fn SetForegroundWindow(h: Hwnd) -> i32;
        fn ShowWindow(h: Hwnd, command: i32) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    let slint_handle = window.window_handle();
    let Ok(handle) = slint_handle.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(w) = handle.as_raw() {
        let hwnd = w.hwnd.get() as Hwnd;
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            BringWindowToTop(hwnd);
            SetActiveWindow(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
fn activate_child_window(_window: &slint::Window) {}

pub(crate) struct FrameRec {
    pub(crate) no: u64,
    pub(crate) key: u64,
    pub(crate) t: f64,
    pub(crate) ch: u8,
    pub(crate) tx: bool,
    pub(crate) id: u32,
    pub(crate) ext: bool,
    pub(crate) fd: bool,
    pub(crate) brs: bool,
    pub(crate) remote: bool,
    pub(crate) error: bool,
    pub(crate) data: Vec<u8>,
    pub(crate) delta: f64,
    pub(crate) count: u64,
    pub(crate) changed_mask: Vec<bool>,
    pub(crate) name: String,
}

pub(crate) struct LastInfo {
    pub(crate) t: f64,
    pub(crate) data: Vec<u8>,
    pub(crate) count: u64,
    pub(crate) min_cycle: f64,
    pub(crate) max_cycle: f64,
    pub(crate) sum_cycle: f64,
    ext: bool,
    fd: bool,
    brs: bool,
    remote: bool,
    pub(crate) byte_change_t: Vec<f64>,
}

#[derive(Clone)]
pub(crate) struct Series {
    pub(crate) id: u32,
    pub(crate) signal: String,
    pub(crate) name: String,
    pub(crate) color: Color,
    pub(crate) unit: String,
    pub(crate) samples: VecDeque<(f64, f64)>,
    pub(crate) cur: f64,
    pub(crate) visible: bool,

    pub(crate) expr: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExprVar {
    pub(crate) name: String,
    pub(crate) formula: String,
    #[serde(default)]
    pub(crate) unit: String,
}

const CONSOLE_LINE_CAP: usize = 5000;
const CONSOLE_PARTIAL_MAX: usize = 8192;

pub(crate) const CONSOLE_HELP: &str = include_str!("../docs/PRINTF_OVER_CAN.md");

#[derive(Default)]
pub(crate) struct ConsoleBuf {
    pub(crate) lines: VecDeque<String>,
    partial: Vec<u8>,
    revision: u64,
}

impl ConsoleBuf {
    pub(crate) fn feed(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        for &b in data {
            match b {
                0x00 | 0x0D => {}
                0x0A => self.flush_line(),
                _ => {
                    self.partial.push(b);
                    if self.partial.len() >= CONSOLE_PARTIAL_MAX {
                        self.flush_line();
                    }
                }
            }
        }
    }
    fn flush_line(&mut self) {
        let s = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        self.lines.push_back(s);
        while self.lines.len() > CONSOLE_LINE_CAP {
            self.lines.pop_front();
        }
    }
    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn rows(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lines.iter().cloned().collect();
        if !self.partial.is_empty() {
            v.push(String::from_utf8_lossy(&self.partial).into_owned());
        }
        v
    }

    pub(crate) fn export_text(&self) -> String {
        let mut s = self.lines.iter().cloned().collect::<Vec<_>>().join("\n");
        if !self.partial.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&String::from_utf8_lossy(&self.partial));
        }
        s
    }
}

#[derive(Clone)]
pub(crate) enum DisplayItem {
    Message(u64),
    Signal { key: u64, signal: String },
}

#[derive(Clone)]
pub(crate) enum SignalPickItem {
    DbcRoot,
    MessagesRoot,
    Message(u32),
    Signal(u32, String),
    ExprVar(String),
}

#[derive(Clone)]
pub(crate) struct TxTask {
    pub(crate) name: String,
    pub(crate) ch: u8,
    pub(crate) id: u32,
    pub(crate) ext: bool,
    pub(crate) fd: bool,
    pub(crate) brs: bool,
    pub(crate) remote: bool, // RTR remote frame (classic CAN only)
    pub(crate) data: Vec<u8>,
    pub(crate) periodic: bool,
    pub(crate) period_ms: u64,
    pub(crate) repeat: i64,
    pub(crate) sent: u64,
    pub(crate) handle: u64,
    pub(crate) dbc_id: Option<u32>,
    pub(crate) sig_values: Vec<(String, f64)>,
    pub(crate) varies: Vec<SignalVary>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SignalVary {
    pub(crate) signal: String,
    pub(crate) mode: vary::VaryMode,
}

enum TrigCond {
    IdEquals(u32),
    ByteEquals { off: usize, val: u8 },
    ErrorFrame,
}

#[derive(Clone, Copy)]
enum TrigAction {
    Alarm,
    StartRecord,
    StopRecord,
    SendFrame,
}

struct Trigger {
    cond: TrigCond,
    action: TrigAction,
    last: Option<std::time::Instant>,

    send_ch: u8,
    send_id: u32,
    send_ext: bool,
    send_fd: bool,
    send_data: Vec<u8>,
}

impl Trigger {
    fn matches(&self, f: &CanFrame) -> bool {
        match &self.cond {
            TrigCond::IdEquals(id) => f.id == *id,
            TrigCond::ByteEquals { off, val } => f.data.get(*off).copied() == Some(*val),
            TrigCond::ErrorFrame => f.error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum SimKind {
    Indicator,
    Dial,
    Bar,
    Numeric,
    Label,
    Button,
    Slider,
    SignalGen,
    Switch,
    Trend,
    Level,
    Knob,
    Input,
    BarChart,
    StatusGroup,
    Alarm,
    Image,
}

impl SimKind {
    fn from_i32(v: i32) -> SimKind {
        match v {
            1 => SimKind::Dial,
            2 => SimKind::Bar,
            3 => SimKind::Numeric,
            4 => SimKind::Label,
            5 => SimKind::Button,
            6 => SimKind::Slider,
            7 => SimKind::SignalGen,
            8 => SimKind::Switch,
            9 => SimKind::Trend,
            10 => SimKind::Level,
            11 => SimKind::Knob,
            12 => SimKind::Input,
            13 => SimKind::BarChart,
            14 => SimKind::StatusGroup,
            15 => SimKind::Alarm,
            16 => SimKind::Image,
            _ => SimKind::Indicator,
        }
    }
    pub(crate) fn to_i32(self) -> i32 {
        match self {
            SimKind::Indicator => 0,
            SimKind::Dial => 1,
            SimKind::Bar => 2,
            SimKind::Numeric => 3,
            SimKind::Label => 4,
            SimKind::Button => 5,
            SimKind::Slider => 6,
            SimKind::SignalGen => 7,
            SimKind::Switch => 8,
            SimKind::Trend => 9,
            SimKind::Level => 10,
            SimKind::Knob => 11,
            SimKind::Input => 12,
            SimKind::BarChart => 13,
            SimKind::StatusGroup => 14,
            SimKind::Alarm => 15,
            SimKind::Image => 16,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SimKind::Indicator => "指示灯",
            SimKind::Dial => "仪表盘",
            SimKind::Bar => "进度条",
            SimKind::Numeric => "数值",
            SimKind::Label => "标签",
            SimKind::Button => "按钮",
            SimKind::Slider => "滑块",
            SimKind::SignalGen => "信号发生器",
            SimKind::Switch => "开关",
            SimKind::Trend => "实时趋势",
            SimKind::Level => "液位计",
            SimKind::Knob => "旋钮",
            SimKind::Input => "数值输入",
            SimKind::BarChart => "柱状图",
            SimKind::StatusGroup => "状态灯组",
            SimKind::Alarm => "报警卡片",
            SimKind::Image => "图片背景",
        }
    }
    fn label_i18n(self, en: bool) -> &'static str {
        if !en {
            return self.label();
        }
        match self {
            SimKind::Indicator => "Indicator",
            SimKind::Dial => "Dial",
            SimKind::Bar => "Bar",
            SimKind::Numeric => "Numeric",
            SimKind::Label => "Label",
            SimKind::Button => "Button",
            SimKind::Slider => "Slider",
            SimKind::SignalGen => "SignalGen",
            SimKind::Switch => "Switch",
            SimKind::Trend => "Trend",
            SimKind::Level => "Level",
            SimKind::Knob => "Knob",
            SimKind::Input => "Input",
            SimKind::BarChart => "BarChart",
            SimKind::StatusGroup => "StatusGroup",
            SimKind::Alarm => "Alarm",
            SimKind::Image => "Image",
        }
    }

    fn default_size(self) -> (f64, f64) {
        match self {
            SimKind::Indicator => (90.0, 90.0),
            SimKind::Dial => (150.0, 150.0),
            SimKind::Bar => (200.0, 56.0),
            SimKind::Numeric => (150.0, 70.0),
            SimKind::Label => (140.0, 40.0),
            SimKind::Button => (120.0, 56.0),
            SimKind::Slider => (220.0, 70.0),
            SimKind::SignalGen => (170.0, 70.0),
            SimKind::Switch => (132.0, 52.0),
            SimKind::Trend => (360.0, 210.0),
            SimKind::Level => (110.0, 190.0),
            SimKind::Knob => (140.0, 150.0),
            SimKind::Input => (190.0, 72.0),
            SimKind::BarChart => (320.0, 210.0),
            SimKind::StatusGroup => (280.0, 170.0),
            SimKind::Alarm => (270.0, 120.0),
            SimKind::Image => (420.0, 260.0),
        }
    }

    pub(crate) fn min_size(self) -> (f64, f64) {
        match self {
            SimKind::Indicator => (72.0, 72.0),
            SimKind::Dial => (112.0, 112.0),
            SimKind::Bar => (140.0, 52.0),
            SimKind::Numeric => (100.0, 56.0),
            SimKind::Label => (90.0, 36.0),
            SimKind::Button => (96.0, 44.0),
            SimKind::Slider => (150.0, 60.0),
            SimKind::SignalGen => (140.0, 58.0),
            SimKind::Switch => (112.0, 44.0),
            SimKind::Trend => (260.0, 160.0),
            SimKind::Level => (88.0, 140.0),
            SimKind::Knob => (112.0, 120.0),
            SimKind::Input => (92.0, 58.0),
            SimKind::BarChart => (240.0, 160.0),
            SimKind::StatusGroup => (220.0, 130.0),
            SimKind::Alarm => (220.0, 96.0),
            SimKind::Image => (180.0, 120.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) enum GenMode {
    Constant,
    Ramp,
    Sine,
}

fn default_align() -> i32 {
    1
}
fn default_chan() -> u8 {
    1
}
fn default_w() -> f64 {
    120.0
}
fn default_h() -> f64 {
    60.0
}
fn default_press() -> f64 {
    1.0
}

fn default_sim_dlc() -> u8 {
    8
}

fn default_trace_window_secs() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_alarm_message() -> String {
    "信号值超出允许范围".to_string()
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SimWidget {
    pub(crate) kind: SimKind,
    pub(crate) name: String,
    #[serde(default = "default_chan")]
    pub(crate) channel: u8,
    #[serde(default)]
    pub(crate) dbc_path: String,
    pub(crate) frame_id: u32,
    #[serde(default)]
    pub(crate) frame_extended: bool,
    #[serde(default)]
    pub(crate) frame_fd: bool,
    #[serde(default)]
    pub(crate) frame_brs: bool,
    #[serde(default = "default_sim_dlc")]
    pub(crate) frame_dlc: u8,
    /// False only for legacy project data created before explicit frame profiles.
    #[serde(default)]
    pub(crate) frame_profile_explicit: bool,
    pub(crate) signal: String,
    pub(crate) threshold: f64,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) gen_mode: GenMode,
    pub(crate) gen_step: f64,
    pub(crate) period_ms: u64,

    #[serde(default)]
    pub(crate) x: f64,
    #[serde(default)]
    pub(crate) y: f64,
    #[serde(default = "default_w")]
    pub(crate) w: f64,
    #[serde(default = "default_h")]
    pub(crate) h: f64,
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) slider_val: f64,
    #[serde(default = "default_press")]
    pub(crate) press_val: f64,
    #[serde(default)]
    pub(crate) release_val: f64,
    #[serde(default = "default_align")]
    pub(crate) align: i32,
    #[serde(default)]
    pub(crate) trace_signals: Vec<String>,
    #[serde(default = "default_trace_window_secs")]
    pub(crate) trace_window_secs: u64,
    #[serde(default = "default_true")]
    pub(crate) trace_auto_range: bool,
    #[serde(default = "default_alarm_message")]
    pub(crate) alarm_message: String,
    #[serde(default)]
    pub(crate) image_path: String,

    #[serde(skip)]
    pub(crate) cur: f64,
    #[serde(skip)]
    pub(crate) tick: u64,
    #[serde(skip)]
    pub(crate) last_fire: Option<std::time::Instant>,
    #[serde(skip)]
    pub(crate) binding_error_reported: bool,
    #[serde(skip)]
    pub(crate) switch_on: bool,
    #[serde(skip)]
    pub(crate) trace_history: Vec<std::collections::VecDeque<f64>>,
    #[serde(skip)]
    pub(crate) trace_paused: bool,
    #[serde(skip)]
    pub(crate) group_values: Vec<Option<f64>>,
    #[serde(skip)]
    pub(crate) image_cache: slint::Image,
    #[serde(skip)]
    pub(crate) image_cache_path: String,
    #[serde(skip)]
    pub(crate) image_load_ok: bool,
}

#[derive(Serialize, Deserialize)]
struct TxTaskDto {
    name: String,
    ch: u8,
    id: u32,
    ext: bool,
    fd: bool,
    brs: bool,
    #[serde(default)]
    remote: bool,
    data: Vec<u8>,
    periodic: bool,
    period_ms: u64,
    #[serde(default = "default_repeat")]
    repeat: i64,
    dbc_id: Option<u32>,
    sig_values: Vec<(String, f64)>,
    #[serde(default)]
    varies: Vec<SignalVary>,
}

fn default_repeat() -> i64 {
    -1
}

impl TxTaskDto {
    fn from_task(t: &TxTask) -> Self {
        TxTaskDto {
            name: t.name.clone(),
            ch: t.ch,
            id: t.id,
            ext: t.ext,
            fd: t.fd,
            brs: t.brs,
            remote: t.remote,
            data: t.data.clone(),
            periodic: t.periodic,
            period_ms: t.period_ms,
            repeat: t.repeat,
            dbc_id: t.dbc_id,
            sig_values: t.sig_values.clone(),
            varies: t.varies.clone(),
        }
    }
    fn into_task(self, handle: u64) -> TxTask {
        TxTask {
            name: self.name,
            ch: self.ch,
            id: self.id,
            ext: self.ext,
            fd: self.fd,
            brs: self.brs,
            remote: self.remote,
            data: self.data,
            periodic: false,
            period_ms: self.period_ms.max(1),
            repeat: self.repeat,
            sent: 0,
            handle,
            dbc_id: self.dbc_id,
            sig_values: self.sig_values,
            varies: self.varies,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Project {
    #[serde(default)]
    name: String,
    #[serde(default)]
    settings: settings::Settings,
    #[serde(default)]
    txs: Vec<TxTaskDto>,
}

#[derive(Clone)]
struct ChannelEditSession {
    channels: Vec<DeviceConfig>,
    selected: i32,
    dirty: bool,
}

#[derive(Default)]
pub(crate) struct Filter {
    pub(crate) allow: Vec<(u32, u32)>,
    pub(crate) deny: Vec<u32>,
    pub(crate) name: Option<String>,
    pub(crate) name_exclude: bool,
    pub(crate) name_prefix: bool,
    pub(crate) name_suffix: bool,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) dir_filter: Option<bool>,
}

impl Filter {
    pub(crate) fn accept(&self, id: u32, name: &str, data: &[u8], tx: bool) -> bool {
        if let Some(d) = self.dir_filter
            && tx != d
        {
            return false;
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|(a, b)| id >= *a && id <= *b) {
            return false;
        }
        if self.deny.contains(&id) {
            return false;
        }
        if let Some(pat) = &self.name {
            let hay = name.to_ascii_lowercase();
            let pat = pat.to_ascii_lowercase();
            let m = if self.name_prefix {
                hay.starts_with(&pat)
            } else if self.name_suffix {
                hay.ends_with(&pat)
            } else {
                hay.contains(&pat)
            };
            if m == self.name_exclude {
                return false;
            }
        }
        if let Some(seq) = &self.data
            && !seq.is_empty()
            && !data.windows(seq.len()).any(|w| w == seq.as_slice())
        {
            return false;
        }
        true
    }
}

pub(crate) struct App {
    pub(crate) cmd: CommandSender,
    worker_tx: WorkerSender<WorkerEvent>,
    pub(crate) license_gate: Rc<license::RuntimeGate>,
    pub(crate) project_name: String,
    pub(crate) project_path: Option<std::path::PathBuf>,
    pub(crate) recent_project_paths: Vec<String>,
    recent_project_model: Rc<VecModel<RecentProjectRow>>,
    pub(crate) sim_dirty: bool,
    pub(crate) sim_revision: u64,
    pub(crate) dbcs: Vec<DbcDb>,
    pub(crate) mode_trace: bool,
    pub(crate) time_mode: i32,
    pub(crate) capture_wall_epoch: Option<f64>,
    cols_hidden: std::collections::HashSet<String>,
    pub(crate) sim_widgets: Vec<SimWidget>,
    pub(crate) sim_tx_frames: HashMap<sim::SimFrameCacheKey, Vec<u8>>,
    pub(crate) sim_sampler: sim::SimSampler,
    pub(crate) sim_sampler_signature: u64,
    pub(crate) sim_sampler_generation: u64,
    pub(crate) sim_sampler_keys: Vec<u64>,
    pub(crate) sim_sampler_reported_skips: (u64, u64),
    pub(crate) sim_model: Rc<VecModel<SimRow>>,
    pub(crate) sim_sel: i32,
    pub(crate) sim_multi: std::collections::HashSet<i32>,
    pub(crate) sim_running: bool,
    pub(crate) sim_canvas_w: f64,
    pub(crate) sim_canvas_h: f64,
    pub(crate) paused: bool,
    autoscroll: bool,
    recording: bool,
    pub(crate) connected: bool,
    pub(crate) connected_channels: std::collections::HashSet<u8>,
    shutdown_requested: bool,
    pub(crate) conn_name: String,
    pub(crate) running: bool,
    pub(crate) baud: String,
    device_cfg: DeviceConfig,
    pub(crate) channels: Vec<DeviceConfig>,
    pub(crate) pcan_devices: Vec<can::PcanChannelInfo>,
    pub(crate) zcan_devices: Vec<can::ZcanUsbChannelInfo>,
    pub(crate) last_hardware_scan: Option<std::time::Instant>,
    hardware_scan_in_progress: bool,
    hardware_scan_status: String,
    channel_edit: Option<ChannelEditSession>,
    channel_connect_pending: bool,
    channel_connect_expected: usize,
    channel_sel: i32,
    recorder: recording::Recorder,
    rec_fmt: RecFmt,
    rec_path: Option<std::path::PathBuf>,
    pub(crate) sig_log: Option<std::io::BufWriter<std::fs::File>>,
    pub(crate) sig_log_last_flush: Option<std::time::Instant>,
    trigger: Option<Trigger>,
    dbc_paths: Vec<String>,

    pub(crate) trace: VecDeque<FrameRec>,
    pub(crate) no_counter: u64,
    pub(crate) last: HashMap<u64, LastInfo>,
    pub(crate) last_dirty: bool,

    pub(crate) rx: u64,
    pub(crate) tx: u64,
    pub(crate) err: u64,
    capture_dropped_frames: u64,
    capture_dropped_events: u64,
    capture_hardware_overruns: u64,
    capture_hardware_errors: u64,
    capture_queue_depth: usize,
    capture_queue_capacity: usize,
    capture_queue_high_watermark: usize,
    command_rejected: u64,
    command_queue_depth: usize,
    command_queue_capacity: usize,
    command_queue_high_watermark: usize,
    timestamp_samples: u64,
    timestamp_latest_jitter_us: f64,
    timestamp_max_jitter_us: f64,
    timestamp_drift_ppm: f64,
    timestamp_monotonic_violations: u64,

    pub(crate) series: Vec<Series>,

    pub(crate) expr_vars: Vec<ExprVar>,
    pub(crate) sig_latest: HashMap<String, f64>,
    pub(crate) expr_decode_ids: HashSet<u32>,
    pub(crate) sig_cat: i32,
    pub(crate) signal_pick_expr_selected: Option<String>,

    pub(crate) console_enabled: bool,
    pub(crate) console_id: Option<u32>,
    pub(crate) console_ch: u8,
    pub(crate) console: ConsoleBuf,
    pub(crate) selected_key: Option<u64>,
    selected_index: i32,
    pub(crate) sig_panel: Vec<(u32, String)>,
    dbc_signal_choices: Vec<(u32, String)>,

    filter: Filter,
    pub(crate) txs: Vec<TxTask>,
    pub(crate) tx_sel: i32,
    pub(crate) tx_dbc_order: Vec<(u32, bool, String)>,
    pub(crate) tx_sig_cache: u64,
    pub(crate) tx_msgs_cache: u64,
    pub(crate) tx_list_cache: u64,
    pub(crate) tx_checked: HashSet<u64>,
    tx_speed: f64,
    chan_names_cache: u64,
    pub(crate) next_handle: u64,
    logs: VecDeque<String>,
    pub(crate) sort_col: i32,
    pub(crate) sort_desc: bool,
    pub(crate) display_items: Vec<DisplayItem>,
    pub(crate) expanded_keys: HashSet<u64>,
    pub(crate) expanded_signal_cache: HashMap<(u64, String), Decoded>,
    pub(crate) msg_model: Rc<VecModel<MsgRow>>,
    pub(crate) chart_model: Rc<VecModel<ChartSeries>>,
    pub(crate) chart_xlabel_model: Rc<VecModel<SharedString>>,
    log_model: Rc<VecModel<SharedString>>,
    console_model: Rc<VecModel<SharedString>>,
    dbc_signal_model: Rc<VecModel<SharedString>>,
    chan_stat_model: Rc<VecModel<ChanStat>>,
    id_stat_model: Rc<VecModel<IdStat>>,
    sig_model: Rc<VecModel<SigRow>>,
    dbc_signal_cache: u64,
    console_cache: u64,
    pub(crate) sig_panel_cache: u64,
    trace_cap: usize,
    chart_cap: usize,
    pub(crate) tree_collapsed: HashSet<String>,
    pub(crate) tree_row_keys: Vec<String>,
    pub(crate) tree_dbc_index: Vec<i32>,
    pub(crate) signal_pick_items: Vec<SignalPickItem>,
    pub(crate) signal_pick_cache: u64,
    pub(crate) signal_pick_selected: Option<(u32, String)>,
    pub(crate) signal_pick_msg_expanded: HashSet<u32>,
    pub(crate) signal_pick_root_open: bool,
    pub(crate) signal_pick_messages_open: bool,
    pub(crate) signal_pick_filter: String,
    pub(crate) chart_paused: bool,
    pub(crate) chart_normalize: bool,
    pub(crate) chart_cursor: bool,
    pub(crate) chart_dual: bool,
    pub(crate) chart_time_mode: i32,
    pub(crate) chart_time_source: i32,
    pub(crate) chart_view: Option<(f64, f64)>,
    pub(crate) chart_pause_view: Option<(f64, f64)>,
    pub(crate) chart_frozen_series: Option<Vec<Series>>,
    chart_highlight: Option<(String, std::time::Instant)>,
    pub(crate) tree_curve_sig: Vec<Option<String>>,
    pub(crate) last_tree_sig: u64,
    pub(crate) lang_en: bool,

    pub(crate) python_interpreter: String,
    pub(crate) last_script_path: String,
    pub(crate) py_child: Option<std::process::Child>,
    pub(crate) py_out_rx: Option<crossbeam_channel::Receiver<String>>,
    pub(crate) py_output_dropped: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub(crate) py_output_dropped_seen: u64,
    pub(crate) py_started: Option<std::time::Instant>,
    pub(crate) py_stop_flag: bool,
    pub(crate) run_status: String,
    pub(crate) py_output: String,
    pub(crate) py_dirty: bool,
    pub(crate) py_timeout_secs: u64,
    pub(crate) ipc_snapshot: std::sync::Arc<std::sync::Mutex<ipc::Snapshot>>,
    pub(crate) ipc_subs: std::sync::Arc<ipc::SubRegistry>,
    pub(crate) ipc_handle_map: HashMap<(u64, u64), u64>,
    pub(crate) dbc_snap: std::sync::Arc<ipc::DbcSnapshot>,
    pb_raw: Vec<CanFrame>,
    pb_files: Vec<(String, Vec<CanFrame>)>,
    pb_pos: usize,
    pb_total: usize,
    pb_playing: bool,
    pub(crate) last_msg_sig: u64,

    pub(crate) fps: f64,
    pub(crate) bus_load: f64,
    win_start: std::time::Instant,
    win_frames: u64,
    win_bits: u64,
    pub(crate) chan_stats: std::collections::BTreeMap<u8, ChanCounters>,
}

#[derive(Default, Clone)]
pub(crate) struct ChanCounters {
    pub(crate) rx: u64,
    pub(crate) tx: u64,
    pub(crate) err: u64,
    pub(crate) bus_load: f64,
    pub(crate) fps: f64,
    win_bits: u64,
    win_frames: u64,
}

fn frame_bits(f: &CanFrame) -> u64 {
    let overhead = if f.fd {
        if f.ext { 60 } else { 40 }
    } else if f.ext {
        67
    } else {
        47
    };
    overhead + 8 * f.data.len() as u64
}

fn baud_bps(s: &str) -> f64 {
    let t = s.trim().to_ascii_uppercase();
    if let Some(v) = t.strip_suffix('M') {
        v.trim().parse::<f64>().unwrap_or(0.5) * 1e6
    } else if let Some(v) = t.strip_suffix('K') {
        v.trim().parse::<f64>().unwrap_or(500.0) * 1e3
    } else {
        t.parse::<f64>().unwrap_or(500_000.0)
    }
}

impl App {
    pub(crate) fn license_allows(&mut self, feature: &str) -> bool {
        if self.license_gate.allows(feature) {
            return true;
        }
        self.log(format!("授权校验失败，功能已阻止: {feature}"));
        false
    }

    pub(crate) fn mark_sim_dirty(&mut self) {
        self.sim_revision = self.sim_revision.wrapping_add(1);
        self.sim_dirty = true;
    }

    fn log(&mut self, msg: impl Into<String>) {
        let message = msg.into();
        self.logs.push_back(message.clone());
        self.log_model.push(message.into());
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
            self.log_model.remove(0);
        }
    }

    pub(crate) fn dbc_decode(&self, id: u32, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let r = d.decode(id, data);
            if !r.is_empty() {
                return r;
            }
        }
        Vec::new()
    }

    pub(crate) fn dbc_decode_frame(&self, id: u32, ext: bool, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let result = d.decode_ext(id, ext, data);
            if !result.is_empty() {
                return result;
            }
        }
        Vec::new()
    }

    pub(crate) fn dbc_decode_all_frame(&self, id: u32, ext: bool, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let result = d.decode_all_ext(id, ext, data);
            if !result.is_empty() {
                return result;
            }
        }
        Vec::new()
    }

    pub(crate) fn dbc_message_name_frame(&self, id: u32, ext: bool) -> Option<&str> {
        self.dbcs.iter().find_map(|d| d.message_name_ext(id, ext))
    }

    pub(crate) fn dbc_message_frame(&self, id: u32, ext: bool) -> Option<&MessageDef> {
        self.dbcs.iter().find_map(|d| d.message_ext(id, ext))
    }

    pub(crate) fn dbc_encode_frame(
        &self,
        id: u32,
        ext: bool,
        vals: &std::collections::HashMap<String, f64>,
    ) -> Option<Vec<u8>> {
        self.dbcs.iter().find_map(|d| d.encode_ext(id, ext, vals))
    }

    pub(crate) fn dbc_loaded(&self) -> bool {
        !self.dbcs.is_empty()
    }

    pub(crate) fn dbc_has_signal(&self, name: &str) -> bool {
        self.dbcs.iter().any(|d| {
            d.messages()
                .any(|m| m.signals.iter().any(|s| s.name == name))
        })
    }

    fn trig_start_record(&mut self) {
        let path = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("trigger_record.csv")))
            .unwrap_or_else(|| std::path::PathBuf::from("trigger_record.csv"));
        match self.recorder.start(path.clone(), RecFmt::Csv) {
            Ok(()) => {
                self.rec_fmt = RecFmt::Csv;
                self.rec_path = Some(path.clone());
                self.recording = true;
                self.log(format!("⚠ 触发开始记录: {}", path.display()));
            }
            Err(error) => self.log(format!("触发开始记录失败: {error}")),
        }
    }

    fn trig_stop_record(&mut self) {
        self.recording = false;
        if let Err(error) = self.recorder.stop() {
            self.log(format!("触发停止记录失败: {error}"));
        }
        self.log("⚠ 触发停止记录".to_string());
    }

    fn ingest(&mut self, f: CanFrame, playback_frame: bool) {
        let new_time_source = if playback_frame { 1 } else { 0 };
        if self.chart_time_source != new_time_source {
            self.chart_time_mode = if playback_frame { 0 } else { 1 };
        }
        self.chart_time_source = new_time_source;

        if !playback_frame && self.capture_wall_epoch.is_none() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            self.capture_wall_epoch = Some(now - f.t);
        }

        if self.console_enabled
            && !f.tx
            && self.console_id.is_none_or(|id| id == f.id)
            && (self.console_ch == 0 || self.console_ch == f.ch)
        {
            self.console.feed(&f.data);
        }

        let fire = if let Some(tr) = self.trigger.as_mut() {
            if tr.matches(&f) {
                let now = std::time::Instant::now();
                let ok = tr
                    .last
                    .is_none_or(|t| now.duration_since(t).as_millis() > 300);
                if ok {
                    tr.last = Some(now);
                    Some(tr.action)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(act) = fire {
            match act {
                TrigAction::Alarm => {
                    self.log(format!("⚠ 触发命中: ID 0x{:X}", f.id));
                }
                TrigAction::StartRecord => {
                    if !self.recording {
                        self.trig_start_record();
                    }
                }
                TrigAction::StopRecord => {
                    if self.recording {
                        self.trig_stop_record();
                    }
                }
                TrigAction::SendFrame => {
                    if let Some(tr) = self.trigger.as_ref() {
                        let resp = CanFrame {
                            t: 0.0,
                            ch: tr.send_ch,
                            tx: true,
                            id: tr.send_id,
                            ext: tr.send_ext,
                            fd: tr.send_fd,
                            brs: false,
                            remote: false,
                            error: false,
                            data: tr.send_data.clone(),
                        };
                        let sid = tr.send_id;
                        let _ = self.cmd.send(Cmd::SendOnce(resp));
                        self.log(format!("⚠ 触发发送报文: 0x{sid:X}"));
                    }
                }
            }
        }
        let key = key_of(f.ch, f.tx, f.ext, f.id);
        let li = self.last.entry(key).or_insert(LastInfo {
            t: f.t,
            data: f.data.clone(),
            count: 0,
            min_cycle: f64::MAX,
            max_cycle: 0.0,
            sum_cycle: 0.0,
            ext: f.ext,
            fd: f.fd,
            brs: f.brs,
            remote: f.remote,
            byte_change_t: vec![f.t; f.data.len()],
        });
        let delta = (f.t - li.t).max(0.0);

        let mut changed_mask = vec![false; f.data.len()];
        if li.count > 0 {
            for (i, b) in f.data.iter().enumerate() {
                let prev = li.data.get(i).copied();
                if prev != Some(*b) {
                    changed_mask[i] = true;
                }
            }
            if delta < li.min_cycle {
                li.min_cycle = delta;
            }
            if delta > li.max_cycle {
                li.max_cycle = delta;
            }
            li.sum_cycle += delta;
        }
        if li.byte_change_t.len() != f.data.len() {
            li.byte_change_t = vec![f.t; f.data.len()];
        }
        for (i, &c) in changed_mask.iter().enumerate() {
            if c {
                li.byte_change_t[i] = f.t;
            }
        }
        li.t = f.t;
        li.data = f.data.clone();
        li.ext = f.ext;
        li.fd = f.fd;
        li.brs = f.brs;
        li.remote = f.remote;
        li.count += 1;
        let count = li.count;
        self.last_dirty = true;

        let bits = frame_bits(&f);
        if f.tx {
            self.tx += 1;
        } else {
            self.rx += 1;
        }
        if f.error {
            self.err += 1;
        }

        self.win_frames += 1;
        self.win_bits += bits;

        let cs = self.chan_stats.entry(f.ch).or_default();
        if f.tx {
            cs.tx += 1;
        } else {
            cs.rx += 1;
        }
        if f.error {
            cs.err += 1;
        }
        cs.win_frames += 1;
        cs.win_bits += bits;

        let name = self
            .dbc_message_name_frame(f.id, f.ext)
            .unwrap_or("")
            .to_string();

        if self.recording
            && let Err(error) = self.recorder.push(f.clone())
        {
            self.recording = false;
            let _ = self.recorder.stop();
            self.log(format!("记录已停止: {error}"));
        }

        let mut log_lines: Vec<String> = Vec::new();

        let need_dbc_series = self.series.iter().any(|s| s.expr.is_none() && s.id == f.id);
        let need_expr = !self.expr_vars.is_empty() && self.expr_decode_ids.contains(&f.id);
        if self.dbc_loaded() && (need_dbc_series || need_expr) {
            let decoded = self.dbc_decode_frame(f.id, f.ext, &f.data);
            let logging = self.sig_log.is_some();

            for s in self
                .series
                .iter_mut()
                .filter(|s| s.expr.is_none() && s.id == f.id)
            {
                if let Some(dec) = decoded.iter().find(|x| x.name == s.signal) {
                    s.cur = dec.physical;
                    s.samples.push_back((f.t, dec.physical));
                    while s.samples.len() > self.chart_cap {
                        s.samples.pop_front();
                    }
                    if logging {
                        log_lines.push(format!(
                            "{:.6},{},{},{}",
                            f.t, s.signal, dec.physical, s.unit
                        ));
                    }
                }
            }

            if need_expr {
                for dec in &decoded {
                    self.sig_latest.insert(dec.name.clone(), dec.physical);
                }
                let cap = self.chart_cap;
                let t = f.t;

                let evals: Vec<(usize, f64)> = self
                    .series
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| {
                        s.expr
                            .as_ref()
                            .and_then(|fm| expr::eval(fm, &self.sig_latest).ok().map(|v| (i, v)))
                    })
                    .collect();
                for (i, v) in evals {
                    let s = &mut self.series[i];
                    s.cur = v;
                    s.samples.push_back((t, v));
                    while s.samples.len() > cap {
                        s.samples.pop_front();
                    }
                    if logging {
                        log_lines.push(format!("{:.6},{},{},{}", t, s.signal, v, s.unit));
                    }
                }
            }
        }
        let mut signal_log_flushed = false;
        let signal_log_error = if let Some(w) = self.sig_log.as_mut() {
            let write_error = log_lines
                .into_iter()
                .find_map(|line| writeln!(w, "{line}").err());
            if write_error.is_none()
                && self
                    .sig_log_last_flush
                    .map(|last| last.elapsed() >= std::time::Duration::from_secs(1))
                    .unwrap_or(true)
            {
                let flush_error = w.flush().err();
                signal_log_flushed = flush_error.is_none();
                flush_error
            } else {
                write_error
            }
        } else {
            None
        };
        if signal_log_flushed {
            self.sig_log_last_flush = Some(std::time::Instant::now());
        }
        if let Some(error) = signal_log_error {
            self.sig_log = None;
            self.sig_log_last_flush = None;
            self.log(format!(
                "Signal recording stopped after file write failure: {error}"
            ));
        }

        self.no_counter += 1;
        self.trace.push_back(FrameRec {
            no: self.no_counter,
            key,
            t: f.t,
            ch: f.ch,
            tx: f.tx,
            id: f.id,
            ext: f.ext,
            fd: f.fd,
            brs: f.brs,
            remote: f.remote,
            error: f.error,
            data: f.data,
            delta,
            count,
            changed_mask,
            name,
        });
        while self.trace.len() > self.trace_cap {
            self.trace.pop_front();
        }
    }
}

pub(crate) fn id_str(id: u32, ext: bool) -> String {
    if ext {
        format!("0x{id:08X}")
    } else {
        format!("0x{id:03X}")
    }
}

const COL_DEFAULTS: &[(&str, f32)] = &[
    ("no", 52.0),
    ("time", 96.0),
    ("cycle", 72.0),
    ("ch", 56.0),
    ("dir", 44.0),
    ("id", 88.0),
    ("name", 150.0),
    ("kind", 48.0),
    ("fd", 40.0),
    ("brs", 40.0),
    ("dlc", 44.0),
    ("len", 44.0),
    ("data", 230.0),
    ("count", 72.0),
    ("comment", 140.0),
];

fn apply_col_widths(ui: &AppWindow, hidden: &std::collections::HashSet<String>) {
    let cw = ui.global::<ColW>();
    // 时间差已从用户可见列中移除，强制为 0 以兼容旧工程配置。
    cw.set_delta(0.0);
    // The tree expander is a fixed, non-hideable control column.
    let mut total = 28.0_f32;
    for (k, def) in COL_DEFAULTS {
        let w = if hidden.contains(*k) { 0.0 } else { *def };
        total += w;
        match *k {
            "no" => cw.set_no(w),
            "time" => cw.set_time(w),
            "ch" => cw.set_ch(w),
            "dir" => cw.set_dir(w),
            "id" => cw.set_id(w),
            "name" => cw.set_name(w),
            "kind" => cw.set_kind(w),
            "fd" => cw.set_fd(w),
            "brs" => cw.set_brs(w),
            "dlc" => cw.set_dlc(w),
            "len" => cw.set_len(w),
            "data" => cw.set_data(w),
            "cycle" => cw.set_cycle(w),
            "count" => cw.set_count(w),
            "comment" => cw.set_comment(w),
            _ => {}
        }
    }
    cw.set_total(total);
}

pub(crate) fn fmt_wall(unix_secs: f64, with_date: bool) -> String {
    let secs = unix_secs.floor() as i64;
    let nanos = ((unix_secs - secs as f64) * 1e9) as u32;
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(utc) => {
            let dt = utc.with_timezone(&chrono::Local);
            if with_date {
                dt.format("%m-%d %H:%M:%S%.3f").to_string()
            } else {
                dt.format("%H:%M:%S%.3f").to_string()
            }
        }
        None => format!("{unix_secs:.6}"),
    }
}

const PALETTE: [(u8, u8, u8); 8] = [
    (37, 99, 235),
    (220, 38, 38),
    (22, 163, 74),
    (217, 119, 6),
    (147, 51, 234),
    (8, 145, 178),
    (190, 24, 93),
    (101, 116, 139),
];

fn start_update_check(weak: slint::Weak<AppWindow>) {
    let Some(ui) = weak.upgrade() else { return };
    if ui.get_update_state() == 1 {
        return;
    }
    ui.set_update_state(1);
    ui.set_update_error(SharedString::default());
    drop(ui);

    std::thread::spawn(move || {
        let result = update::check(product_version::current());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match result {
                Ok(update::CheckResult::Available(info)) => {
                    ui.set_update_version(info.version.into());
                    ui.set_update_notes(info.notes.into());
                    ui.set_update_gitee_url(info.gitee_download.into());
                    ui.set_update_github_url(info.github_download.into());
                    ui.set_update_state(3);
                    ui.set_update_toast_open(true);
                }
                Ok(update::CheckResult::Current { latest }) => {
                    ui.set_update_version(latest.into());
                    ui.set_update_state(2);
                }
                Err(error) => {
                    ui.set_update_error(error.into());
                    ui.set_update_state(4);
                }
            }
        });
    });
}

#[derive(Clone, Default)]
struct ChildWindowStore(Rc<std::cell::RefCell<Option<Rc<ChildWindows>>>>);

struct ChildWindows {
    chart: ChartWindow,
    signal: SignalSelectWindow,
    tx: TxWindow,
    uds: UdsWindow,
    xcp: XcpWindow,
    channel: ChannelConfigWindow,
    playback: PlaybackWindow,
    convert: ConvertWindow,
    cache: CacheConfigWindow,
    trigger: TriggerWindow,
    sim_panel: SimPanelWindow,
    sim_prop: SimPropWindow,
    console_help: ConsoleHelpWindow,
    script_runner: ScriptRunnerWindow,
    dbc_diagnostics: DbcDiagnosticsWindow,
    dbc_diagnostics_model: Rc<VecModel<DbcDiagnosticRow>>,
}

impl ChildWindows {
    fn set_dark(&self, dark: bool) {
        self.chart.global::<Theme>().set_dark(dark);
        self.tx.global::<Theme>().set_dark(dark);
        macro_rules! apply_feature {
            ($($window:expr),+ $(,)?) => { $( $window.global::<FeatureTheme>().set_dark(dark); )+ };
        }
        apply_feature!(
            self.signal,
            self.uds,
            self.xcp,
            self.channel,
            self.playback,
            self.convert,
            self.cache,
            self.trigger,
            self.sim_panel,
            self.sim_prop,
            self.console_help,
            self.script_runner,
            self.dbc_diagnostics,
        );
    }

    fn set_big(&self, big: bool) {
        self.chart.global::<Theme>().set_big(big);
        self.tx.global::<Theme>().set_big(big);
        macro_rules! apply_feature {
            ($($window:expr),+ $(,)?) => { $( $window.global::<FeatureTheme>().set_big(big); )+ };
        }
        apply_feature!(
            self.signal,
            self.uds,
            self.xcp,
            self.channel,
            self.playback,
            self.convert,
            self.cache,
            self.trigger,
            self.sim_panel,
            self.sim_prop,
            self.console_help,
            self.script_runner,
            self.dbc_diagnostics,
        );
    }

    fn set_language(&self, english: bool) {
        self.chart.global::<I18n>().set_en(english);
        self.tx.global::<I18n>().set_en(english);
        macro_rules! apply_feature {
            ($($window:expr),+ $(,)?) => { $( $window.global::<FeatureI18n>().set_en(english); )+ };
        }
        apply_feature!(
            self.signal,
            self.uds,
            self.xcp,
            self.channel,
            self.playback,
            self.convert,
            self.cache,
            self.trigger,
            self.sim_panel,
            self.sim_prop,
            self.console_help,
            self.script_runner,
            self.dbc_diagnostics,
        );
    }
}

#[derive(Clone, Copy)]
enum ChildWindowKind {
    Chart,
    Tx,
    Uds,
    Xcp,
    Channel,
    Playback,
    Convert,
    Cache,
    Trigger,
    SimPanel,
    ConsoleHelp,
    ScriptRunner,
    DbcDiagnostics,
}

impl ChildWindowStore {
    fn get(&self) -> Option<Rc<ChildWindows>> {
        self.0.borrow().clone()
    }

    fn ensure(
        &self,
        app: Rc<std::cell::RefCell<App>>,
        ui: &AppWindow,
        ipc_port: u16,
        ipc_token: String,
    ) -> Result<Rc<ChildWindows>, slint::PlatformError> {
        if let Some(windows) = self.get() {
            return Ok(windows);
        }

        let windows = Rc::new(ChildWindows {
            chart: ChartWindow::new()?,
            signal: SignalSelectWindow::new()?,
            tx: TxWindow::new()?,
            uds: UdsWindow::new()?,
            xcp: XcpWindow::new()?,
            channel: ChannelConfigWindow::new()?,
            playback: PlaybackWindow::new()?,
            convert: ConvertWindow::new()?,
            cache: CacheConfigWindow::new()?,
            trigger: TriggerWindow::new()?,
            sim_panel: SimPanelWindow::new()?,
            sim_prop: SimPropWindow::new()?,
            console_help: ConsoleHelpWindow::new()?,
            script_runner: ScriptRunnerWindow::new()?,
            dbc_diagnostics: DbcDiagnosticsWindow::new()?,
            dbc_diagnostics_model: Rc::new(VecModel::default()),
        });

        windows.console_help.set_help_text(CONSOLE_HELP.into());
        windows
            .dbc_diagnostics
            .set_rows(ModelRc::from(windows.dbc_diagnostics_model.clone()));
        windows
            .chart
            .set_series(ModelRc::from(app.borrow().chart_model.clone()));
        windows
            .chart
            .set_chart_xlabels(ModelRc::from(app.borrow().chart_xlabel_model.clone()));
        windows
            .sim_panel
            .set_sim_widgets(ModelRc::from(app.borrow().sim_model.clone()));

        let dark = ui.global::<Theme>().get_dark();
        let big = ui.global::<Theme>().get_big();
        let english = ui.global::<I18n>().get_en();
        macro_rules! sync_main_globals {
            ($window:expr) => {{
                $window.global::<Theme>().set_dark(dark);
                $window.global::<Theme>().set_big(big);
                $window.global::<I18n>().set_en(english);
            }};
        }
        macro_rules! sync_feature_globals {
            ($window:expr) => {{
                $window.global::<FeatureTheme>().set_dark(dark);
                $window.global::<FeatureTheme>().set_big(big);
                $window.global::<FeatureI18n>().set_en(english);
            }};
        }
        sync_main_globals!(windows.chart);
        sync_main_globals!(windows.tx);
        sync_feature_globals!(windows.signal);
        sync_feature_globals!(windows.uds);
        sync_feature_globals!(windows.xcp);
        sync_feature_globals!(windows.channel);
        sync_feature_globals!(windows.playback);
        sync_feature_globals!(windows.convert);
        sync_feature_globals!(windows.cache);
        sync_feature_globals!(windows.trigger);
        sync_feature_globals!(windows.sim_panel);
        sync_feature_globals!(windows.sim_prop);
        sync_feature_globals!(windows.console_help);
        sync_feature_globals!(windows.script_runner);
        sync_feature_globals!(windows.dbc_diagnostics);

        wire_main_children(app.clone(), &windows);
        wire_dialogs(
            app.clone(),
            ui,
            &windows.chart,
            &windows.signal,
            &windows.tx,
            &windows.channel,
            &windows.playback,
            &windows.convert,
            &windows.cache,
            &windows.trigger,
            &windows.sim_panel,
            &windows.sim_prop,
        );
        wire_chart(
            app.clone(),
            ui,
            &windows.chart,
            &windows.signal,
            &windows.tx,
            &windows.channel,
            &windows.playback,
            &windows.convert,
            &windows.cache,
            &windows.trigger,
            &windows.sim_panel,
            &windows.sim_prop,
        );
        wire_tx(app.clone(), ui, &windows.tx);
        wire_ota_windows(app.clone(), ui, &windows.uds, &windows.xcp);
        wire_playback(
            app.clone(),
            ui,
            &windows.chart,
            &windows.signal,
            &windows.tx,
            &windows.channel,
            &windows.playback,
            &windows.convert,
            &windows.cache,
            &windows.trigger,
            &windows.sim_panel,
            &windows.sim_prop,
        );
        wire_sim(
            app.clone(),
            ui,
            &windows.chart,
            &windows.signal,
            &windows.tx,
            &windows.channel,
            &windows.playback,
            &windows.convert,
            &windows.cache,
            &windows.trigger,
            &windows.sim_panel,
            &windows.sim_prop,
        );
        wire_pyauto(app, ui, &windows.script_runner, ipc_port, ipc_token);

        *self.0.borrow_mut() = Some(windows.clone());
        Ok(windows)
    }

    fn ensure_and_show(
        &self,
        kind: ChildWindowKind,
        app: Rc<std::cell::RefCell<App>>,
        ui: &AppWindow,
        ipc_port: u16,
        ipc_token: String,
    ) {
        let windows = match self.ensure(app.clone(), ui, ipc_port, ipc_token) {
            Ok(windows) => windows,
            Err(error) => {
                app.borrow_mut().log(format!("创建功能窗口失败: {error}"));
                return;
            }
        };
        match kind {
            ChildWindowKind::Chart => show_child_window(&windows.chart),
            ChildWindowKind::Tx => show_child_window(&windows.tx),
            ChildWindowKind::Uds => show_child_window(&windows.uds),
            ChildWindowKind::Xcp => show_child_window(&windows.xcp),
            ChildWindowKind::Channel => {
                let mut a = app.borrow_mut();
                a.channel_edit = Some(ChannelEditSession {
                    channels: a.channels.clone(),
                    selected: a.channel_sel,
                    dirty: false,
                });
                a.channel_connect_pending = false;
                a.channel_connect_expected = 0;
                if a.last_hardware_scan.is_none() {
                    scan_attached_hardware(&mut a);
                }
                let selected = channel_selected(&a)
                    .clamp(0, channel_configs(&a).len() as i32 - 1)
                    .max(0);
                if let Some(session) = a.channel_edit.as_mut() {
                    session.selected = selected;
                }
                windows.channel.set_chan_sel(selected);
                windows.channel.set_validation_message("".into());
                windows.channel.set_validation_is_error(false);
                refresh_channel_window_lists(&windows.channel, &a);
                if let Some(channel) = channel_configs(&a).get(selected as usize) {
                    set_chan_form(&windows.channel, channel, &a);
                }
                drop(a);
                show_child_window(&windows.channel);
            }
            ChildWindowKind::Playback => show_child_window(&windows.playback),
            ChildWindowKind::Convert => show_child_window(&windows.convert),
            ChildWindowKind::Cache => {
                let a = app.borrow();
                windows.cache.set_trace_cap(a.trace_cap.to_string().into());
                windows.cache.set_chart_cap(a.chart_cap.to_string().into());
                drop(a);
                show_child_window(&windows.cache);
            }
            ChildWindowKind::Trigger => {
                windows.trigger.set_armed(app.borrow().trigger.is_some());
                show_child_window(&windows.trigger);
            }
            ChildWindowKind::SimPanel => {
                windows.sim_panel.set_running(app.borrow().sim_running);
                let size = windows.sim_panel.window().size();
                if size.width < 920 || size.height < 600 {
                    windows
                        .sim_panel
                        .window()
                        .set_size(slint::LogicalSize::new(1180.0, 760.0));
                }
                show_child_window(&windows.sim_panel);
            }
            ChildWindowKind::ConsoleHelp => show_child_window(&windows.console_help),
            ChildWindowKind::ScriptRunner => {
                prepare_and_show_script_runner(&app, &windows.script_runner)
            }
            ChildWindowKind::DbcDiagnostics => {
                refresh_dbc_diagnostics(
                    &app.borrow(),
                    &windows.dbc_diagnostics,
                    &windows.dbc_diagnostics_model,
                );
                show_child_window(&windows.dbc_diagnostics);
            }
        }
    }
}

fn wire_lazy_window_openers(
    app: Rc<std::cell::RefCell<App>>,
    ui: &AppWindow,
    store: ChildWindowStore,
    ipc_port: u16,
    ipc_token: String,
) {
    macro_rules! lazy_opener {
        ($registrar:ident, $kind:expr) => {{
            let app = app.clone();
            let uiw = ui.as_weak();
            let store = store.clone();
            let token = ipc_token.clone();
            ui.$registrar(move || {
                if let Some(ui) = uiw.upgrade() {
                    store.ensure_and_show($kind, app.clone(), &ui, ipc_port, token.clone());
                }
            });
        }};
    }
    lazy_opener!(on_open_chart_window, ChildWindowKind::Chart);
    lazy_opener!(on_open_tx_window, ChildWindowKind::Tx);
    lazy_opener!(on_open_uds_window, ChildWindowKind::Uds);
    lazy_opener!(on_open_xcp_window, ChildWindowKind::Xcp);
    lazy_opener!(on_open_channel_config, ChildWindowKind::Channel);
    lazy_opener!(on_open_playback_window, ChildWindowKind::Playback);
    lazy_opener!(on_open_convert_window, ChildWindowKind::Convert);
    lazy_opener!(on_open_cache_config, ChildWindowKind::Cache);
    lazy_opener!(on_open_trigger_window, ChildWindowKind::Trigger);
    lazy_opener!(on_open_sim_panel_window, ChildWindowKind::SimPanel);
    lazy_opener!(on_console_help, ChildWindowKind::ConsoleHelp);
    lazy_opener!(on_open_script_runner, ChildWindowKind::ScriptRunner);
    lazy_opener!(on_open_dbc_diagnostics, ChildWindowKind::DbcDiagnostics);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    windows_dpi::force_system_dpi_awareness();

    if let Err(error) = license::verify_self_integrity("pcanwork", product_version::current()) {
        rfd::MessageDialog::new()
            .set_title("PcanWork")
            .set_description(format!("程序完整性验证失败，软件无法启动。\n\nApplication integrity verification failed.\n\n{error}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
        return Ok(());
    }

    select_renderer();

    let ui = AppWindow::new()?;
    ui.set_app_version(format!("v{}", product_version::current()).into());
    ui.on_open_website(|| {
        let _ = open_external_url("https://www.hexbyte.cn");
    });
    {
        let weak = ui.as_weak();
        ui.on_check_update(move || start_update_check(weak.clone()));
    }
    {
        let weak = ui.as_weak();
        ui.on_open_update_gitee(move || {
            if let Some(window) = weak.upgrade() {
                let url = window.get_update_gitee_url();
                if !url.is_empty() {
                    let _ = open_external_url(url.as_str());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_open_update_github(move || {
            if let Some(window) = weak.upgrade() {
                let url = window.get_update_github_url();
                if !url.is_empty() {
                    let _ = open_external_url(url.as_str());
                }
            }
        });
    }
    start_update_check(ui.as_weak());
    ui.set_license_machine_code(license::machine_code().into());
    let trial_duration = license::runtime_trial_duration();
    let license_gate = Rc::new(license::RuntimeGate::new("pcanwork", trial_duration));
    let initially_licensed = license_gate.has_signed_license();
    ui.set_license_unlocked(initially_licensed);
    if let Ok(payload) = license::verify_installed("pcanwork", "*") {
        ui.set_license_info(format!("{} · .pcanlic", payload.license_id).into());
        ui.set_license_validity_zh(license::license_validity(&payload, false).into());
        ui.set_license_validity_en(license::license_validity(&payload, true).into());
    }
    ui.set_license_remaining(
        if initially_licensed {
            "已授权".to_string()
        } else {
            license::format_remaining(trial_duration.as_secs())
        }
        .into(),
    );
    ui.set_license_seconds(trial_duration.as_secs() as i32);
    {
        let weak = ui.as_weak();
        let gate = license_gate.clone();
        ui.on_license_import(move || {
            let Some(window) = weak.upgrade() else { return };
            let Some(path) = rfd::FileDialog::new()
                .add_filter("PcanWork License", &["pcanlic"])
                .pick_file()
            else {
                return;
            };
            match license::install_license(&path, gate.product()) {
                Ok(payload) => {
                    window.set_license_unlocked(true);
                    window.set_license_open(false);
                    window.set_license_remaining(
                        if window.global::<I18n>().get_en() {
                            "Licensed"
                        } else {
                            "已授权"
                        }
                        .into(),
                    );
                    window.set_license_info(format!("{} · .pcanlic", payload.license_id).into());
                    window
                        .set_license_validity_zh(license::license_validity(&payload, false).into());
                    window
                        .set_license_validity_en(license::license_validity(&payload, true).into());
                    window.set_license_error(
                        if window.global::<I18n>().get_en() {
                            "Signed license installed."
                        } else {
                            "签名授权文件已安装。"
                        }
                        .into(),
                    );
                }
                Err(error) => window.set_license_error(
                    if window.global::<I18n>().get_en() {
                        format!("License rejected: {error}")
                    } else {
                        format!("授权文件无效：{error}")
                    }
                    .into(),
                ),
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_license_copy_machine_code(move || {
            let Some(window) = weak.upgrade() else { return };
            match arboard::Clipboard::new().and_then(|mut clipboard| {
                clipboard.set_text(window.get_license_machine_code().to_string())
            }) {
                Ok(()) => window.set_license_error(
                    if window.global::<I18n>().get_en() {
                        "Machine code copied."
                    } else {
                        "机器码已复制。"
                    }
                    .into(),
                ),
                Err(error) => window.set_license_error(
                    if window.global::<I18n>().get_en() {
                        format!("Copy failed: {error}")
                    } else {
                        format!("复制失败：{error}")
                    }
                    .into(),
                ),
            }
        });
    }
    let _license_timer = {
        let weak = ui.as_weak();
        let gate = license_gate.clone();
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(window) = weak.upgrade() else { return };
            if gate.has_signed_license() {
                if !window.get_license_unlocked() {
                    window.set_license_unlocked(true);
                    window.set_license_remaining(
                        if window.global::<I18n>().get_en() {
                            "Licensed"
                        } else {
                            "已授权"
                        }
                        .into(),
                    );
                }
                return;
            }
            let remaining = gate.remaining_seconds();
            window.set_license_seconds(remaining.min(i32::MAX as u64) as i32);
            window.set_license_remaining(license::format_remaining(remaining).into());
            if remaining == 0 {
                let _ = window.window().hide();
                let _ = slint::quit_event_loop();
            }
        });
        timer
    };
    let (cmd_tx, evt_rx) = can::spawn();
    let (worker_tx, worker_rx) = crossbeam_channel::bounded::<WorkerEvent>(256);

    let dbc_snap0 = std::sync::Arc::new(ipc::DbcSnapshot::empty());
    let ipc_snapshot =
        std::sync::Arc::new(std::sync::Mutex::new(ipc::Snapshot::new(dbc_snap0.clone())));
    let (ipc_port, ipc_token, ipc_req_rx, ipc_subs) = ipc::spawn_ipc_server(ipc_snapshot.clone());

    let ipc_info_error = std::env::var("PCANWORK_IPC_INFO_FILE")
        .ok()
        .and_then(|info_path| {
            std::fs::write(&info_path, format!("{ipc_port}\n{ipc_token}\n"))
                .err()
                .map(|error| format!("写入 IPC 信息文件失败 {info_path}: {error}"))
        });

    let app = Rc::new(std::cell::RefCell::new(App {
        cmd: cmd_tx.clone(),
        worker_tx,
        license_gate: license_gate.clone(),
        project_name: String::new(),
        project_path: None,
        recent_project_paths: Vec::new(),
        recent_project_model: Rc::new(VecModel::default()),
        sim_dirty: false,
        sim_revision: 0,
        dbcs: Vec::new(),
        mode_trace: true,
        time_mode: 0,
        capture_wall_epoch: None,
        cols_hidden: std::collections::HashSet::new(),
        sim_widgets: Vec::new(),
        sim_tx_frames: HashMap::new(),
        sim_sampler: sim::SimSampler::spawn(),
        sim_sampler_signature: 0,
        sim_sampler_generation: 0,
        sim_sampler_keys: Vec::new(),
        sim_sampler_reported_skips: (0, 0),
        sim_model: Rc::new(VecModel::default()),
        sim_sel: -1,
        sim_multi: std::collections::HashSet::new(),
        sim_running: false,
        sim_canvas_w: 0.0,
        sim_canvas_h: 0.0,
        paused: false,
        autoscroll: true,
        recording: false,
        connected: false,
        connected_channels: std::collections::HashSet::new(),
        shutdown_requested: false,
        conn_name: String::new(),
        running: false,
        baud: "500K".into(),
        device_cfg: DeviceConfig {
            sw_channel: 1,
            is_fd: false,
            device_type: "Virtual".into(),
            hardware_label: String::new(),
            hardware_id: String::new(),
            device_index: 0,
            channel_index: 0,
            baud: "500K".into(),
            data_baud: "2M".into(),
            custom_bitrate: String::new(),
            termination: false,
            listen_only: false,
            fd_non_iso: false,
            net_server: true,
            ip: String::new(),
            port: String::new(),
        },
        channels: vec![DeviceConfig {
            sw_channel: 1,
            is_fd: false,
            device_type: "Virtual".into(),
            hardware_label: String::new(),
            hardware_id: String::new(),
            device_index: 0,
            channel_index: 0,
            baud: "500K".into(),
            data_baud: "2M".into(),
            custom_bitrate: String::new(),
            termination: false,
            listen_only: false,
            fd_non_iso: false,
            net_server: true,
            ip: String::new(),
            port: String::new(),
        }],
        pcan_devices: Vec::new(),
        zcan_devices: Vec::new(),
        last_hardware_scan: None,
        hardware_scan_in_progress: false,
        hardware_scan_status: String::new(),
        channel_edit: None,
        channel_connect_pending: false,
        channel_connect_expected: 0,
        channel_sel: 0,
        recorder: recording::Recorder::spawn(),
        rec_fmt: RecFmt::Csv,
        rec_path: None,
        sig_log: None,
        sig_log_last_flush: None,
        trigger: None,
        dbc_paths: Vec::new(),
        trace: VecDeque::new(),
        no_counter: 0,
        last: HashMap::new(),
        last_dirty: true,
        rx: 0,
        tx: 0,
        err: 0,
        capture_dropped_frames: 0,
        capture_dropped_events: 0,
        capture_hardware_overruns: 0,
        capture_hardware_errors: 0,
        capture_queue_depth: 0,
        capture_queue_capacity: 0,
        capture_queue_high_watermark: 0,
        command_rejected: 0,
        command_queue_depth: 0,
        command_queue_capacity: 0,
        command_queue_high_watermark: 0,
        timestamp_samples: 0,
        timestamp_latest_jitter_us: 0.0,
        timestamp_max_jitter_us: 0.0,
        timestamp_drift_ppm: 0.0,
        timestamp_monotonic_violations: 0,
        series: Vec::new(),
        expr_vars: Vec::new(),
        sig_latest: HashMap::new(),
        expr_decode_ids: HashSet::new(),
        sig_cat: 0,
        signal_pick_expr_selected: None,
        console_enabled: false,
        console_id: None,
        console_ch: 0,
        console: ConsoleBuf::default(),
        selected_key: None,
        selected_index: -1,
        sig_panel: Vec::new(),
        dbc_signal_choices: Vec::new(),
        filter: Filter::default(),
        txs: Vec::new(),
        tx_sel: -1,
        tx_dbc_order: Vec::new(),
        tx_sig_cache: u64::MAX,
        tx_msgs_cache: u64::MAX,
        tx_list_cache: u64::MAX,
        tx_checked: HashSet::new(),
        tx_speed: 1.0,
        chan_names_cache: u64::MAX,
        next_handle: 1,
        logs: VecDeque::new(),
        sort_col: -1,
        sort_desc: false,
        display_items: Vec::new(),
        expanded_keys: HashSet::new(),
        expanded_signal_cache: HashMap::new(),
        msg_model: Rc::new(VecModel::from(Vec::<MsgRow>::new())),
        chart_model: Rc::new(VecModel::from(Vec::<ChartSeries>::new())),
        chart_xlabel_model: Rc::new(VecModel::default()),
        log_model: Rc::new(VecModel::default()),
        console_model: Rc::new(VecModel::default()),
        dbc_signal_model: Rc::new(VecModel::default()),
        chan_stat_model: Rc::new(VecModel::default()),
        id_stat_model: Rc::new(VecModel::default()),
        sig_model: Rc::new(VecModel::default()),
        dbc_signal_cache: u64::MAX,
        console_cache: u64::MAX,
        sig_panel_cache: u64::MAX,
        trace_cap: TRACE_CAP,
        chart_cap: CHART_CAP,
        tree_collapsed: HashSet::new(),
        tree_row_keys: Vec::new(),
        tree_dbc_index: Vec::new(),
        signal_pick_items: Vec::new(),
        signal_pick_cache: u64::MAX,
        signal_pick_selected: None,
        signal_pick_msg_expanded: HashSet::new(),
        signal_pick_root_open: true,
        signal_pick_messages_open: true,
        signal_pick_filter: String::new(),
        chart_paused: false,
        chart_normalize: false,
        chart_cursor: false,
        chart_dual: false,
        chart_time_mode: 0,
        chart_time_source: 0,
        chart_view: None,
        chart_pause_view: None,
        chart_frozen_series: None,
        chart_highlight: None,
        tree_curve_sig: Vec::new(),
        last_tree_sig: u64::MAX,
        lang_en: false,
        python_interpreter: String::new(),
        last_script_path: String::new(),
        py_child: None,
        py_out_rx: None,
        py_output_dropped: None,
        py_output_dropped_seen: 0,
        py_started: None,
        py_stop_flag: false,
        run_status: String::new(),
        py_output: String::new(),
        py_dirty: false,
        py_timeout_secs: 120,
        ipc_snapshot: ipc_snapshot.clone(),
        ipc_subs: ipc_subs.clone(),
        ipc_handle_map: HashMap::new(),
        dbc_snap: dbc_snap0.clone(),
        pb_raw: Vec::new(),
        pb_files: Vec::new(),
        last_msg_sig: u64::MAX,
        fps: 0.0,
        bus_load: 0.0,
        win_start: std::time::Instant::now(),
        win_frames: 0,
        win_bits: 0,
        chan_stats: std::collections::BTreeMap::new(),
        pb_pos: 0,
        pb_total: 0,
        pb_playing: false,
    }));
    if let Some(error) = ipc_info_error {
        app.borrow_mut().log(error);
    }

    ui.set_msgs(ModelRc::from(app.borrow().msg_model.clone()));
    ui.set_logs(ModelRc::from(app.borrow().log_model.clone()));
    ui.set_console_lines(ModelRc::from(app.borrow().console_model.clone()));
    ui.set_dbc_signals(ModelRc::from(app.borrow().dbc_signal_model.clone()));
    ui.set_chan_stats(ModelRc::from(app.borrow().chan_stat_model.clone()));
    ui.set_id_stats(ModelRc::from(app.borrow().id_stat_model.clone()));
    ui.set_sigs(ModelRc::from(app.borrow().sig_model.clone()));
    ui.set_recent_projects(ModelRc::from(app.borrow().recent_project_model.clone()));

    ui.set_series(ModelRc::from(app.borrow().chart_model.clone()));
    let child_windows = ChildWindowStore::default();

    // Main-window callbacks are ready immediately. Secondary windows and their
    // callbacks are constructed together on the first request for a child tool.
    wire_main(app.clone(), &ui, child_windows.clone());
    wire_external_tools(app.clone(), &ui);
    wire_lazy_window_openers(
        app.clone(),
        &ui,
        child_windows.clone(),
        ipc_port,
        ipc_token.clone(),
    );

    {
        let mut a = app.borrow_mut();

        rebuild_dbc_snap(&mut a);

        if let Some(s) = settings::load() {
            a.recent_project_paths = s.recent_project_paths.clone();
            refresh_recent_projects(&a);
            apply_settings(&mut a, &ui, &s);
            sim_migrate_dbc_bindings(&mut a);

            ui.global::<Theme>().set_dark(s.dark);
            ui.global::<Theme>().set_big(s.big);
            ui.global::<I18n>().set_en(s.lang_en);
            a.lang_en = s.lang_en;
            a.log("已恢复上次配置".to_string());
        }

        if let Some(path) = std::env::args_os().skip(1).find_map(|arg| {
            let p = std::path::PathBuf::from(arg);
            let is_project = p
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| {
                    x.eq_ignore_ascii_case("pcprj")
                        || x.eq_ignore_ascii_case("zcp")
                        || x.eq_ignore_ascii_case("json")
                })
                .unwrap_or(false);
            if is_project { Some(p) } else { None }
        }) {
            match std::fs::read_to_string(&path)
                .map_err(|e| format!("Read project failed: {e}"))
                .and_then(|txt| {
                    serde_json::from_str::<Project>(&txt)
                        .map_err(|e| format!("Parse project failed: {e}"))
                }) {
                Ok(proj) => {
                    a.project_name = if proj.name.trim().is_empty() {
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("CAN_Test_Project")
                            .to_string()
                    } else {
                        proj.name.clone()
                    };
                    a.project_path = Some(path.clone());
                    touch_recent_project(&mut a, &path);
                    refresh_recent_projects(&a);
                    persist_settings(&mut a, &ui);
                    ui.set_project_open(true);
                    a.sim_dirty = false;
                    a.sim_revision = 0;
                    let _ = configure_sim_generators(&a, false);
                    a.sim_running = false;
                    apply_settings(&mut a, &ui, &proj.settings);
                    sim_migrate_dbc_bindings(&mut a);
                    a.txs.clear();
                    for dto in proj.txs {
                        let h = a.next_handle;
                        a.next_handle += 1;
                        a.txs.push(dto.into_task(h));
                    }
                    a.last_tree_sig = u64::MAX;
                    a.log(format!("Opened project: {}", path.display()));
                }
                Err(e) => a.log(e),
            }
        }
    }

    let timer = Timer::default();
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let child_windows = child_windows.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            let windows = child_windows.get();
            {
                let mut a = app.borrow_mut();
                while let Some(event) = a.recorder.try_event() {
                    match event {
                        recording::Event::Started { path, format } => {
                            a.recording = true;
                            a.rec_fmt = format;
                            a.rec_path = Some(path.clone());
                            a.log(format!("开始记录({}): {}", format.name(), path.display()));
                        }
                        recording::Event::Stopped {
                            path,
                            format,
                            frames,
                        } => {
                            a.recording = false;
                            a.log(format!(
                                "已保存 {}: {}（{} 帧）",
                                format.name(),
                                path.display(),
                                frames
                            ));
                        }
                        recording::Event::Failed(error) => {
                            a.recording = false;
                            a.log(format!("记录失败并已停止: {error}"));
                        }
                    }
                }
                while let Ok(event) = worker_rx.try_recv() {
                    match event {
                        WorkerEvent::Log(message) => a.log(message),
                        WorkerEvent::PlaybackParsed {
                            replace,
                            files,
                            errors,
                        } => {
                            if replace {
                                a.pb_files.clear();
                            }
                            let frame_count: usize =
                                files.iter().map(|(_, frames)| frames.len()).sum();
                            a.pb_files.extend(files);
                            for error in errors {
                                a.log(error);
                            }
                            let file_count = a.pb_files.len();
                            a.log(format!(
                                "已载入 {file_count} 个回放文件，本次 {frame_count} 帧"
                            ));
                            if let Some(windows) = windows.as_ref() {
                                pb_apply_files(&mut a, &windows.playback);
                            }
                        }
                        WorkerEvent::ConversionFinished { batch, status, log } => {
                            if let Some(windows) = windows.as_ref() {
                                if batch {
                                    windows.convert.set_status2(status.into());
                                } else {
                                    windows.convert.set_status1(status.into());
                                }
                            }
                            a.log(log);
                        }
                        WorkerEvent::DbcLoaded { path, result } => match result {
                            Ok(db) => {
                                let count = db.messages().count();
                                a.log(format!("已加载 DBC: {} ({count} 条报文)", db.file_name));
                                a.dbcs.push(db);
                                a.dbc_paths.push(path);
                                a.expanded_signal_cache.clear();
                                rebuild_dbc_snap(&mut a);
                            }
                            Err(error) => a.log(format!("加载 DBC 失败: {error}")),
                        },
                        WorkerEvent::DbcReloaded { loaded, errors } => {
                            a.dbcs.clear();
                            a.dbc_paths.clear();
                            a.expanded_signal_cache.clear();
                            for (path, db) in loaded {
                                a.dbc_paths.push(path);
                                a.dbcs.push(db);
                            }
                            for error in errors {
                                a.log(error);
                            }
                            rebuild_dbc_snap(&mut a);
                            let count = a.dbcs.len();
                            a.log(format!("已重新加载 {count} 个 DBC"));
                        }
                        WorkerEvent::ProjectLoaded { path, result } => match *result {
                            Ok((mut project, loaded_dbcs, errors, replace_dbcs)) => {
                                let Some(main_window) = uiw.upgrade() else {
                                    continue;
                                };
                                let dark = project.settings.dark;
                                let big = project.settings.big;
                                let english = project.settings.lang_en;
                                a.lang_en = english;
                                project.settings.dbc_path = None;
                                project.settings.dbc_paths.clear();
                                let tasks = a.txs.clone();
                                for task in &tasks {
                                    stop_task_periodic(&a, task);
                                }
                                a.project_name = if project.name.trim().is_empty() {
                                    path.file_stem()
                                        .and_then(|name| name.to_str())
                                        .unwrap_or("CAN_Test_Project")
                                        .to_string()
                                } else {
                                    project.name
                                };
                                a.project_path = Some(path.clone());
                                touch_recent_project(&mut a, &path);
                                refresh_recent_projects(&a);
                                persist_settings(&mut a, &main_window);
                                main_window.set_project_open(true);
                                a.sim_dirty = false;
                                a.sim_revision = 0;
                                let _ = configure_sim_generators(&a, false);
                                a.sim_running = false;
                                a.sim_sel = -1;
                                a.sim_multi.clear();
                                apply_settings(&mut a, &main_window, &project.settings);
                                if replace_dbcs {
                                    a.dbcs.clear();
                                    a.dbc_paths.clear();
                                    a.expanded_signal_cache.clear();
                                    for (dbc_path, database) in loaded_dbcs {
                                        a.dbc_paths.push(dbc_path);
                                        a.dbcs.push(database);
                                    }
                                    rebuild_dbc_snap(&mut a);
                                }
                                sim_migrate_dbc_bindings(&mut a);
                                a.txs.clear();
                                let count = project.txs.len();
                                for dto in project.txs {
                                    let handle = a.next_handle;
                                    a.next_handle += 1;
                                    a.txs.push(dto.into_task(handle));
                                }
                                for error in errors {
                                    a.log(error);
                                }
                                a.last_tree_sig = u64::MAX;
                                a.log(format!(
                                    "已打开工程: {}（发送任务 {count} 条，默认停发）",
                                    path.display()
                                ));

                                main_window.global::<Theme>().set_dark(dark);
                                main_window.global::<Theme>().set_big(big);
                                main_window.global::<I18n>().set_en(english);
                                if let Some(windows) = windows.as_ref() {
                                    windows.set_dark(dark);
                                    windows.set_big(big);
                                    windows.set_language(english);
                                }
                            }
                            Err(error) => a.log(error),
                        },
                        WorkerEvent::ProjectSaved {
                            path,
                            sim_revision,
                            result,
                        } => match result {
                            Ok(()) => {
                                a.project_path = Some(path.clone());
                                touch_recent_project(&mut a, &path);
                                refresh_recent_projects(&a);
                                if let Some(main_window) = uiw.upgrade() {
                                    persist_settings(&mut a, &main_window);
                                }
                                if a.sim_revision == sim_revision {
                                    a.sim_dirty = false;
                                }
                                a.log(format!("已保存工程: {}", path.display()));
                            }
                            Err(error) => a.log(error),
                        },
                        WorkerEvent::TxFilePrepared {
                            path,
                            repeat,
                            english,
                            result,
                        } => {
                            if let Some(windows) = windows.as_ref() {
                                let window = &windows.tx;
                                match result {
                                    Ok(TxFilePayload::Ota(job)) => {
                                        let total = job.steps.len();
                                        if !a.license_allows("firmware-update") {
                                            window.set_tx_file_status(if english { "License required" } else { "需要有效授权" }.into());
                                            continue;
                                        }
                                        if a.cmd.send(Cmd::OtaRun(job)).is_err() {
                                            window.set_tx_file_status(
                                                if english {
                                                    "CAN backend has stopped"
                                                } else {
                                                    "CAN 后台线程已退出"
                                                }
                                                .into(),
                                            );
                                        } else {
                                            window.set_tx_file_progress(0.0);
                                            window.set_tx_file_status(
                                                if english {
                                                    format!("OTA started ({total} steps)")
                                                } else {
                                                    format!("OTA 已启动（{total} 步）")
                                                }
                                                .into(),
                                            );
                                        }
                                    }
                                    Ok(TxFilePayload::Frames(frames)) => {
                                        let frame_count = frames.len();
                                        let total = frame_count as u64 * repeat as u64;
                                        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
                                        let enqueue_result = match a.cmd.send(Cmd::SendBatch {
                                            frames,
                                            repeat,
                                            ack: Some(ack_tx),
                                        }) {
                                            Ok(()) => ack_rx
                                                .recv_timeout(std::time::Duration::from_millis(500))
                                                .map_err(|_| "CAN 后台未在 500ms 内确认发送任务".to_string())
                                                .and_then(|result| result),
                                            Err(_) => Err("CAN 后台线程已退出或命令队列已满".to_string()),
                                        };
                                        if let Err(error) = enqueue_result {
                                            window.set_tx_file_status(
                                                if english {
                                                    format!("Batch rejected: {error}")
                                                } else {
                                                    format!("批量发送未提交：{error}")
                                                }
                                                .into(),
                                            );
                                        } else {
                                            window.set_tx_file_progress(1.0);
                                            window.set_tx_file_status(
                                                if english {
                                                    format!(
                                                        "Queued {total} frames ({frame_count} x {repeat})"
                                                    )
                                                } else {
                                                    format!(
                                                        "已提交 {total} 帧（{frame_count} 帧 x {repeat}）"
                                                    )
                                                }
                                                .into(),
                                            );
                                            a.log(format!(
                                                "File send queued: {path}, total {total} frames"
                                            ));
                                        }
                                    }
                                    Err(error) => window.set_tx_file_status(error.into()),
                                }
                            }
                        }
                        WorkerEvent::TxListLoaded(result) => match result {
                            Ok(dtos) => {
                                let tasks = a.txs.clone();
                                for task in &tasks {
                                    stop_task_periodic(&a, task);
                                }
                                a.txs.clear();
                                let count = dtos.len();
                                for dto in dtos {
                                    let handle = a.next_handle;
                                    a.next_handle += 1;
                                    a.txs.push(dto.into_task(handle));
                                }
                                a.log(format!("已加载发送列表 {count} 条（默认停发）"));
                            }
                            Err(error) => a.log(format!("加载发送列表失败: {error}")),
                        },
                        WorkerEvent::HardwareScanned { pcan, zcan, elapsed_ms } => {
                            a.pcan_devices = pcan;
                            a.zcan_devices = zcan;
                            a.hardware_scan_in_progress = false;
                            a.hardware_scan_status = if a.pcan_devices.is_empty()
                                && a.zcan_devices.is_empty()
                            {
                                if a.lang_en {
                                    format!("No hardware found · {elapsed_ms} ms")
                                } else {
                                    format!("未发现硬件 · {elapsed_ms} ms")
                                }
                            } else if a.lang_en {
                                format!(
                                    "{} channel(s) detected · {elapsed_ms} ms",
                                    a.pcan_devices.len() + a.zcan_devices.len()
                                )
                            } else {
                                format!(
                                    "已发现 {} 个物理通道 · {elapsed_ms} ms",
                                    a.pcan_devices.len() + a.zcan_devices.len()
                                )
                            };
                            reconcile_stable_hardware(&mut a);
                            if let Some(windows) = windows.as_ref() {
                                refresh_channel_window_lists(&windows.channel, &a);
                                let selected = channel_selected(&a);
                                if let Some(channel) = channel_configs(&a).get(selected as usize) {
                                    set_chan_form(&windows.channel, channel, &a);
                                }
                            }
                        }
                    }
                }
                for _ in 0..64 {
                    let Ok(ureq) = ipc_req_rx.try_recv() else {
                        break;
                    };
                    handle_ipc(&mut a, ureq);
                }
                reap_child(&mut a);
                drain_py_output(&mut a);
                publish_snapshot(&mut a);

                if a.py_dirty {
                    if a.py_child.is_none() {
                        let path = run_log_path();
                        if let Err(error) = std::fs::write(&path, &a.py_output) {
                            a.log(format!(
                                "保存测试运行日志失败 {}: {error}",
                                path.display()
                            ));
                            if !a.run_status.starts_with("FAIL") {
                                a.run_status = "FAIL: 运行日志保存失败".into();
                            }
                        }
                    }
                    if let Some(windows) = windows.as_ref() {
                        let w = &windows.script_runner;
                        w.set_output(a.py_output.clone().into());
                        w.set_running(a.py_child.is_some());
                        let rs = a.run_status.clone();
                        w.set_result(if rs.starts_with("PASS") {
                            1
                        } else if rs.starts_with("FAIL") {
                            -1
                        } else {
                            0
                        });
                        w.set_status_text(rs.into());
                    }
                    a.py_dirty = false;
                }
            }
            let ui = match uiw.upgrade() {
                Some(u) => u,
                None => return,
            };
            let mut a = app.borrow_mut();

            let event_deadline = std::time::Instant::now() + MAX_CAN_EVENT_TIME_PER_TICK;
            for _ in 0..MAX_CAN_EVENTS_PER_TICK {
                if std::time::Instant::now() >= event_deadline {
                    break;
                }
                let Ok(evt) = evt_rx.try_recv() else {
                    break;
                };
                match evt {
                    Evt::Frame(f) => {
                        ipc_fanout(&a, &f);
                        if let Some(windows) = windows.as_ref() {
                            uds_observe_frame(&windows.uds, &f);
                            xcp_observe_frame(&windows.xcp, &f);
                        }
                        a.ingest(f, false);
                    }
                    Evt::Frames(frames) => {
                        for f in frames {
                            ipc_fanout(&a, &f);
                            if let Some(windows) = windows.as_ref() {
                                uds_observe_frame(&windows.uds, &f);
                                xcp_observe_frame(&windows.xcp, &f);
                            }
                            a.ingest(f, false);
                        }
                    }
                    Evt::PlaybackFrame(f) => {
                        ipc_fanout(&a, &f);
                        if let Some(windows) = windows.as_ref() {
                            uds_observe_frame(&windows.uds, &f);
                            xcp_observe_frame(&windows.xcp, &f);
                        }
                        a.ingest(f, true);
                    }
                    Evt::Log(s) => a.log(s),
                    Evt::Connected { channels, name, error } => {
                        let attempted_from_config = a.channel_connect_pending;
                        let expected = a.channel_connect_expected;
                        a.connected = !channels.is_empty();
                        a.connected_channels = channels.into_iter().collect();
                        if a.connected && !name.is_empty() {
                            a.conn_name = name.clone();
                            a.log(format!("后端: {name}"));
                        } else if !a.connected {
                            a.conn_name.clear();
                        }
                        if attempted_from_config {
                            a.channel_connect_pending = false;
                            a.channel_connect_expected = 0;
                            if let Some(windows) = windows.as_ref() {
                                windows.channel.set_connecting(false);
                                let success = error.is_none()
                                    && expected > 0
                                    && a.connected_channels.len() == expected;
                                if success {
                                    windows.channel.set_validation_is_error(false);
                                    windows.channel.set_validation_message(
                                        if a.lang_en {
                                            "All channels connected"
                                        } else {
                                            "全部通道连接成功"
                                        }
                                        .into(),
                                    );
                                    a.channel_edit = None;
                                    let _ = windows.channel.hide();
                                } else {
                                    windows.channel.set_validation_is_error(true);
                                    windows.channel.set_validation_message(
                                        error
                                            .unwrap_or_else(|| {
                                                if a.lang_en {
                                                    "Channel connection failed".into()
                                                } else {
                                                    "通道连接失败，请检查设备状态和参数".into()
                                                }
                                            })
                                            .into(),
                                    );
                                }
                            }
                        }
                    }
                    Evt::Running(r) => a.running = r,
                    Evt::Playback(pos, total, playing) => {
                        a.pb_pos = pos;
                        a.pb_total = total;
                        a.pb_playing = playing;
                    }
                    Evt::PeriodicDone(handle) => {
                        if let Some(t) = a.txs.iter_mut().find(|t| t.handle == handle) {
                            t.periodic = false;
                            a.tx_list_cache = u64::MAX;
                        }
                    }
                    Evt::DynamicUpdate {
                        handle,
                        data,
                        signal_values,
                        sent,
                    } => {
                        if let Some(t) = a.txs.iter_mut().find(|t| t.handle == handle) {
                            t.data = data;
                            t.sig_values = signal_values;
                            t.sent = sent;
                            a.tx_list_cache = u64::MAX;
                        }
                    }
                    Evt::CaptureHealth {
                        dropped_frames,
                        dropped_events,
                        hardware_overruns,
                        hardware_errors,
                        queue_depth,
                        queue_capacity,
                        queue_high_watermark,
                        command_rejected,
                        command_queue_depth,
                        command_queue_capacity,
                        command_queue_high_watermark,
                        timestamp_samples,
                        timestamp_latest_jitter_us,
                        timestamp_max_jitter_us,
                        timestamp_drift_ppm,
                        timestamp_monotonic_violations,
                    } => {
                        if dropped_frames > a.capture_dropped_frames {
                            let newly_dropped = dropped_frames - a.capture_dropped_frames;
                            a.log(format!(
                                "严重: CAN UI 队列已丢失 {} 帧（累计 {}），请降低显示负载或停止测量",
                                newly_dropped,
                                dropped_frames
                            ));
                        }
                        if command_rejected > a.command_rejected {
                            let newly_rejected = command_rejected - a.command_rejected;
                            a.log(format!(
                                "严重: CAN 命令队列拒绝了 {} 个操作（累计 {}），操作未执行",
                                newly_rejected, command_rejected
                            ));
                        }
                        a.capture_dropped_frames = dropped_frames;
                        a.capture_dropped_events = dropped_events;
                        a.capture_hardware_overruns = hardware_overruns;
                        a.capture_hardware_errors = hardware_errors;
                        a.capture_queue_depth = queue_depth;
                        a.capture_queue_capacity = queue_capacity;
                        a.capture_queue_high_watermark = queue_high_watermark;
                        a.command_rejected = command_rejected;
                        a.command_queue_depth = command_queue_depth;
                        a.command_queue_capacity = command_queue_capacity;
                        a.command_queue_high_watermark = command_queue_high_watermark;
                        a.timestamp_samples = timestamp_samples;
                        a.timestamp_latest_jitter_us = timestamp_latest_jitter_us;
                        a.timestamp_max_jitter_us = timestamp_max_jitter_us;
                        a.timestamp_drift_ppm = timestamp_drift_ppm;
                        a.timestamp_monotonic_violations = timestamp_monotonic_violations;
                    }
                    Evt::ShutdownFinished => {
                        if let Err(error) = slint::quit_event_loop() {
                            eprintln!("Failed to quit Slint event loop: {error}");
                        }
                    }
                    Evt::OtaProgress(done, total, text) => {
                        let progress = if total == 0 {
                            0.0
                        } else {
                            done as f32 / total as f32
                        };
                        if let Some(windows) = windows.as_ref() {
                            windows.tx.set_tx_file_progress(progress.clamp(0.0, 1.0));
                            windows.tx.set_tx_file_status(text.clone().into());
                            windows.uds.set_ota_status(text.clone().into());
                            windows.xcp.set_ota_status(text.clone().into());
                        }
                        a.log(text);
                    }
                }
            }

            let dt = a.win_start.elapsed().as_secs_f64();
            if dt >= 1.0 {
                a.fps = a.win_frames as f64 / dt;
                let default_bps = baud_bps(&a.device_cfg.baud);

                let bps_of: std::collections::HashMap<u8, f64> = a
                    .channels
                    .iter()
                    .map(|c| (c.sw_channel, baud_bps(&c.baud)))
                    .collect();
                let mut max_load = 0.0_f64;
                for (ch, cs) in a.chan_stats.iter_mut() {
                    cs.fps = cs.win_frames as f64 / dt;
                    let bps = bps_of.get(ch).copied().unwrap_or(default_bps);
                    cs.bus_load = if bps > 0.0 {
                        (cs.win_bits as f64 / dt / bps * 100.0).min(100.0)
                    } else {
                        0.0
                    };
                    if cs.bus_load > max_load {
                        max_load = cs.bus_load;
                    }
                    cs.win_frames = 0;
                    cs.win_bits = 0;
                }
                a.bus_load = max_load;
                a.win_frames = 0;
                a.win_bits = 0;
                a.win_start = std::time::Instant::now();
            }

            sim_tick(&mut a);
            refresh_sim(&a);
            if let Some(windows) = windows.as_ref() {
                refresh_sim_context(&windows.sim_panel, &a);
            }

            refresh_ui(&mut a, &ui, windows.as_deref());
        });
    }

    let pb_timer = Timer::default();
    {
        let app = app.clone();
        let child_windows = child_windows.clone();
        pb_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(windows) = child_windows.get() else {
                return;
            };
            let w = &windows.playback;
            let a = app.borrow();
            let en = a.lang_en;
            w.set_pos(a.pb_pos.to_string().into());
            w.set_total(a.pb_total.to_string().into());
            w.set_playing(a.pb_playing);
            w.set_status(
                if a.pb_total == 0 {
                    if en {
                        "No file loaded"
                    } else {
                        "未载入文件"
                    }
                } else if a.pb_playing {
                    if en { "Playing" } else { "回放中" }
                } else if a.pb_pos >= a.pb_total {
                    if en { "Done" } else { "回放完成" }
                } else {
                    if en {
                        "Ready / Paused"
                    } else {
                        "就绪/已暂停"
                    }
                }
                .into(),
            );
        });
    }

    #[cfg(windows)]
    let _titlebar_timer = {
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            apply_brand_titlebar,
        );
        apply_brand_titlebar();
        t
    };

    #[cfg(debug_assertions)]
    if std::env::var_os("PCANWORK_DEBUG_OPEN_SIM").is_some() {
        let ui = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
            if let Some(ui) = ui.upgrade() {
                ui.invoke_open_sim_panel_window();
            }
        });
    }

    #[cfg(debug_assertions)]
    if std::env::var_os("PCANWORK_DEBUG_TEST_SIM_LIBRARY").is_some() {
        let ui = ui.as_weak();
        let app = app.clone();
        let child_windows = child_windows.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            let Some(ui) = ui.upgrade() else { return };
            ui.invoke_open_sim_panel_window();
            let app = app.clone();
            let child_windows = child_windows.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(450), move || {
                let Some(windows) = child_windows.get() else {
                    panic!("simulation child windows were not created");
                };
                let panel = &windows.sim_panel;
                panel.invoke_signal_library_refresh();
                panel.invoke_signal_library_row_clicked(0, false);
                let rows = panel.get_signal_library_rows();
                let message_index = (0..rows.row_count())
                    .find(|index| {
                        rows.row_data(*index)
                            .is_some_and(|row| row.kind == "message")
                    })
                    .expect("DBC signal library did not expose a message")
                    as i32;
                panel.invoke_signal_library_row_clicked(message_index, false);
                let rows = panel.get_signal_library_rows();
                let signal_index = (0..rows.row_count())
                    .find(|index| {
                        rows.row_data(*index)
                            .is_some_and(|row| row.kind == "signal")
                    })
                    .expect("DBC signal library did not expose a signal")
                    as i32;
                panel.invoke_signal_library_activate(signal_index);
                {
                    let app = app.borrow();
                    assert!(
                        app.sim_widgets
                            .iter()
                            .any(|widget| !widget.signal.is_empty()),
                        "DBC signal library activation did not bind or create a control"
                    );
                }
                let before_drop = app.borrow().sim_widgets.len();
                panel.invoke_signal_library_drop(signal_index, 900.0, 240.0);
                assert_eq!(
                    app.borrow().sim_widgets.len(),
                    before_drop + 1,
                    "dropping a DBC signal on blank canvas did not create a control"
                );
                let (target_x, target_y, before_rebind) = {
                    let app = app.borrow();
                    let widget = &app.sim_widgets[0];
                    (
                        (widget.x + widget.w / 2.0) as f32,
                        (widget.y + widget.h / 2.0) as f32,
                        app.sim_widgets.len(),
                    )
                };
                panel.invoke_signal_library_drop(signal_index, target_x, target_y);
                assert_eq!(
                    app.borrow().sim_widgets.len(),
                    before_rebind,
                    "dropping on an existing control unexpectedly created another control"
                );
                panel.invoke_signal_library_create(-1, SimKind::Indicator.to_i32());
                assert!(
                    app.borrow().sim_widgets.len() > before_rebind,
                    "batch create from marked signals did not create a control"
                );
            });
        });
    }

    ui.run()?;
    Ok(())
}

fn open_external_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/C", "start", "", url])
            .spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening URLs is not supported on this platform",
    ))
}

#[cfg(windows)]
fn apply_brand_titlebar() {
    use std::os::raw::c_void;
    type Hwnd = isize;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: extern "system" fn(Hwnd, isize) -> i32, l: isize) -> i32;
        fn GetWindowThreadProcessId(h: Hwnd, pid: *mut u32) -> u32;
        fn IsWindowVisible(h: Hwnd) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(h: Hwnd, attr: u32, val: *const c_void, sz: u32) -> i32;
    }
    extern "system" fn cb(h: Hwnd, _l: isize) -> i32 {
        const DWMWA_CAPTION_COLOR: u32 = 35;
        const DWMWA_TEXT_COLOR: u32 = 36;
        let caption: u32 = 0x006f_4011; // COLORREF 0x00BBGGRR, #11406f
        let text: u32 = 0x00ff_ffff;
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            if pid == GetCurrentProcessId() && IsWindowVisible(h) != 0 {
                DwmSetWindowAttribute(
                    h,
                    DWMWA_CAPTION_COLOR,
                    &caption as *const _ as *const c_void,
                    4,
                );
                DwmSetWindowAttribute(h, DWMWA_TEXT_COLOR, &text as *const _ as *const c_void, 4);
            }
        }
        1
    }
    unsafe {
        EnumWindows(cb, 0);
    }
}

#[cfg(windows)]
pub(crate) fn set_window_topmost(window: &slint::Window, on: bool) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    type Hwnd = isize;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowPos(h: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }
    const HWND_TOPMOST: Hwnd = -1;
    const HWND_NOTOPMOST: Hwnd = -2;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOACTIVATE: u32 = 0x0010;
    let slint_handle = window.window_handle();
    let Ok(handle) = slint_handle.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(w) = handle.as_raw() {
        let hwnd: Hwnd = w.hwnd.get() as Hwnd;
        unsafe {
            SetWindowPos(
                hwnd,
                if on { HWND_TOPMOST } else { HWND_NOTOPMOST },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn set_window_topmost(_window: &slint::Window, _on: bool) {}

fn select_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let mode = std::env::var("PCANWORK_RENDERER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| settings::load().map(|s| s.renderer))
        .unwrap_or_else(|| "auto".to_string());
    let use_software = match mode.to_ascii_lowercase().as_str() {
        "cpu" | "software" => true,
        "gpu" | "femtovg" => false,
        _ => detect_virtual_display(), // "auto"
    };
    let backend = if use_software {
        "winit-software"
    } else {
        "winit-femtovg"
    };
    unsafe {
        std::env::set_var("SLINT_BACKEND", backend);
    }
}

pub(crate) fn restart_current_process() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    if let Ok(dir) = std::env::current_dir() {
        cmd.current_dir(dir);
    }
    cmd.spawn()?;
    std::process::exit(0);
}

#[cfg(windows)]
fn detect_virtual_display() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
        fn CloseHandle(h: isize) -> i32;
    }
    const SM_REMOTESESSION: i32 = 0x1000;
    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const INVALID_HANDLE_VALUE: isize = -1;

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
        return true;
    }

    const REMOTE_EXE: [&str; 10] = [
        "todesk",
        "sunlogin",
        "oray",
        "aweray",
        "awesun",
        "rustdesk",
        "anydesk",
        "teamviewer",
        "splashtop",
        "vncserver",
    ];
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut found = false;
    let mut entry = ProcessEntry32W {
        dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
        cnt_usage: 0,
        th32_process_id: 0,
        th32_default_heap_id: 0,
        th32_module_id: 0,
        cnt_threads: 0,
        th32_parent_process_id: 0,
        pc_pri_class_base: 0,
        dw_flags: 0,
        sz_exe_file: [0; 260],
    };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok != 0 {
        let len = entry
            .sz_exe_file
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.sz_exe_file.len());
        let name = String::from_utf16_lossy(&entry.sz_exe_file[..len]).to_ascii_lowercase();
        if REMOTE_EXE.iter().any(|k| name.contains(k)) {
            found = true;
            break;
        }
        ok = unsafe { Process32NextW(snap, &mut entry) };
    }
    unsafe { CloseHandle(snap) };
    found
}

#[cfg(not(windows))]
fn detect_virtual_display() -> bool {
    false
}

fn pb_apply_files(a: &mut App, w: &PlaybackWindow) {
    let concat = w.get_merge_concat();
    let mut out: Vec<CanFrame> = Vec::new();
    if concat {
        let mut cursor = 0.0_f64;
        for (_, fr) in &a.pb_files {
            if fr.is_empty() {
                continue;
            }
            let fmin = fr.iter().map(|f| f.t).fold(f64::INFINITY, f64::min);
            let fmax = fr.iter().map(|f| f.t).fold(f64::NEG_INFINITY, f64::max);
            let shift = cursor - fmin;
            for f in fr {
                let mut g = f.clone();
                g.t += shift;
                out.push(g);
            }
            cursor += (fmax - fmin) + 0.001;
        }
    } else {
        for (_, fr) in &a.pb_files {
            out.extend(fr.iter().cloned());
        }
    }
    out.sort_by(|x, y| x.t.partial_cmp(&y.t).unwrap_or(std::cmp::Ordering::Equal));
    a.pb_raw = out;

    let en = a.lang_en;
    let total = a.pb_raw.len();
    let names: Vec<String> = a.pb_files.iter().map(|(n, _)| n.clone()).collect();
    let fname = match names.len() {
        0 => {
            if en {
                "(no file selected)".to_string()
            } else {
                "(未选择文件)".to_string()
            }
        }
        1 => {
            if en {
                format!("{} ({total} frames)", names[0])
            } else {
                format!("{} ({total} 帧)", names[0])
            }
        }
        n => {
            if en {
                format!("{n} files: {} ({total} frames)", names.join(", "))
            } else {
                format!("{n} 个文件: {} ({total} 帧)", names.join(", "))
            }
        }
    };
    w.set_file_name(fname.into());

    let mut chans: Vec<u8> = a.pb_raw.iter().map(|f| f.ch).collect();
    chans.sort_unstable();
    chans.dedup();
    let ctxt = if chans.is_empty() {
        "-".to_string()
    } else {
        chans
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    w.set_src_channels(ctxt.into());

    let rows: Vec<PbFileRow> = a
        .pb_files
        .iter()
        .map(|(n, fr)| PbFileRow {
            name: n.clone().into(),
            count: if en {
                format!("{} frames", fr.len())
            } else {
                format!("{} 帧", fr.len())
            }
            .into(),
        })
        .collect();
    w.set_pb_files(ModelRc::from(Rc::new(VecModel::from(rows))));

    pb_build_and_load(a, w);
}

fn pb_build_and_load(a: &App, w: &PlaybackWindow) {
    let lo = parse_hex_u32(&w.get_id_lo()).unwrap_or(0);
    let hi = parse_hex_u32(&w.get_id_hi()).unwrap_or(u32::MAX);
    let ss = w
        .get_seg_start()
        .to_string()
        .trim()
        .parse::<f64>()
        .unwrap_or(f64::MIN);
    let se = w
        .get_seg_end()
        .to_string()
        .trim()
        .parse::<f64>()
        .unwrap_or(f64::MAX);
    let map = parse_channel_map(&w.get_channel_map());
    let frames: Vec<CanFrame> = a
        .pb_raw
        .iter()
        .filter(|f| f.id >= lo && f.id <= hi && f.t >= ss && f.t <= se)
        .filter_map(|f| {
            let dst = map.get(&f.ch).copied().unwrap_or(f.ch);
            if dst == 0 {
                return None;
            }
            let mut g = f.clone();
            g.ch = dst;
            Some(g)
        })
        .collect();
    let _ = a.cmd.send(Cmd::PlaybackLoad(frames));
}

fn parse_channel_map(s: &slint::SharedString) -> std::collections::HashMap<u8, u8> {
    let mut m = std::collections::HashMap::new();
    for tok in s.as_str().split(',') {
        let tok = tok.trim();
        if let Some((a, b)) = tok.split_once(':')
            && let (Ok(src), Ok(dst)) = (a.trim().parse::<u8>(), b.trim().parse::<u8>())
        {
            m.insert(src, dst);
        }
    }
    m
}

fn parse_hex_u32(s: &slint::SharedString) -> Option<u32> {
    let t = s.to_string();
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    let t = t.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(t, 16).ok()
}

/// True if the task has at least one signal with a real (non-None) variation mode.
fn has_vary(t: &TxTask) -> bool {
    t.varies
        .iter()
        .any(|v| !matches!(v.mode, vary::VaryMode::None))
}

fn eff_period(period_ms: u64, speed: f64) -> u64 {
    if speed <= 0.0 {
        return period_ms.max(1);
    }
    ((period_ms as f64 / speed).round() as u64).max(1)
}

fn dynamic_periodic_config(a: &App, idx: usize) -> Option<DynamicPeriodicConfig> {
    let task = a.txs.get(idx)?;
    let dbc_id = task.dbc_id?;
    Some(DynamicPeriodicConfig {
        frame: tx_frame(task),
        dbcs: a.dbcs.clone(),
        dbc_id,
        signal_values: task.sig_values.clone(),
        varies: task
            .varies
            .iter()
            .filter(|v| !matches!(v.mode, vary::VaryMode::None))
            .map(|v| (v.signal.clone(), v.mode.clone()))
            .collect(),
        period_ms: eff_period(task.period_ms, a.tx_speed),
        repeat: task.repeat,
        start_sent: task.sent,
    })
}

fn stop_task_periodic(a: &App, task: &TxTask) {
    let _ = a.cmd.send(Cmd::SetPeriodic {
        handle: task.handle,
        frame: tx_frame(task),
        period_ms: 1,
        repeat: task.repeat,
        enable: false,
    });
    let _ = a.cmd.send(Cmd::SetDynamicPeriodic {
        handle: task.handle,
        config: None,
    });
}

fn configure_task_periodic(a: &mut App, idx: usize) {
    let Some(periodic_requested) = a.txs.get(idx).map(|task| task.periodic) else {
        return;
    };
    if periodic_requested && !a.license_allows("can-transmit") {
        if let Some(task) = a.txs.get_mut(idx) {
            task.periodic = false;
        }
        return;
    }
    let task = &a.txs[idx];
    let handle = task.handle;
    let periodic = task.periodic;
    let dynamic = periodic && has_vary(task);

    if dynamic {
        let config = dynamic_periodic_config(a, idx);
        let _ = a.cmd.send(Cmd::SetDynamicPeriodic { handle, config });
    } else {
        let task = &a.txs[idx];
        let _ = a.cmd.send(Cmd::SetDynamicPeriodic {
            handle,
            config: None,
        });
        let _ = a.cmd.send(Cmd::SetPeriodic {
            handle,
            frame: tx_frame(task),
            period_ms: eff_period(task.period_ms, a.tx_speed),
            repeat: task.repeat,
            enable: periodic,
        });
    }
}

fn toggle_task_periodic(a: &mut App, idx: usize) {
    if a.txs[idx].periodic {
        a.txs[idx].sent = 0;
    }
    configure_task_periodic(a, idx);
}

fn tx_frame(t: &TxTask) -> CanFrame {
    CanFrame {
        t: 0.0,
        ch: t.ch,
        tx: true,
        id: t.id,
        ext: t.ext,
        fd: t.fd,
        brs: t.brs,
        remote: t.remote,
        error: false,
        data: t.data.clone(),
    }
}

fn display_key(a: &App, row: i32) -> Option<u64> {
    match a.display_items.get(row as usize) {
        Some(DisplayItem::Message(k)) => Some(*k),
        Some(DisplayItem::Signal { key, .. }) => Some(*key),
        None => None,
    }
}

fn act_only_id(a: &mut App, k: u64) {
    let id = (k & 0xFFFF_FFFF) as u32;
    a.filter.allow = vec![(id, id)];
    a.filter.deny.clear();
    a.log(format!("只显示 ID 0x{id:X}"));
}
fn act_hide_id(a: &mut App, k: u64) {
    let id = (k & 0xFFFF_FFFF) as u32;
    a.filter.deny.push(id);
    a.log(format!("隐藏 ID 0x{id:X}"));
}
fn act_to_tx(a: &mut App, k: u64) {
    if let Some(li) = a.last.get(&k) {
        let id = (k & 0xFFFF_FFFF) as u32;
        let data = li.data.clone();
        let ext = li.ext;
        let ch = ((k >> 40) & 0xFF) as u8;
        let h = a.next_handle;
        a.next_handle += 1;
        let n = a.txs.len() + 1;
        a.txs.push(TxTask {
            name: format!("Tx_{n}"),
            ch,
            id,
            ext,
            fd: li.fd,
            brs: li.brs,
            remote: li.remote,
            data,
            periodic: false,
            period_ms: 100,
            repeat: -1,
            sent: 0,
            handle: h,
            dbc_id: None,
            sig_values: Vec::new(),
            varies: Vec::new(),
        });
        a.log(format!("已把 0x{id:X} 添加到发送窗口"));
    }
}
fn act_send_now(a: &mut App, k: u64) {
    if !a.license_allows("can-transmit") {
        return;
    }
    if let Some(li) = a.last.get(&k) {
        let id = (k & 0xFFFF_FFFF) as u32;
        let ch = ((k >> 40) & 0xFF) as u8;
        let f = CanFrame {
            t: 0.0,
            ch,
            tx: true,
            id,
            ext: li.ext,
            fd: li.fd,
            brs: li.brs,
            remote: li.remote,
            error: false,
            data: li.data.clone(),
        };
        let _ = a.cmd.send(Cmd::SendOnce(f));
        a.log(format!("立即发送 0x{id:X}"));
    }
}
fn act_add_all_signals(a: &mut App, k: u64) {
    let id = (k & 0xFFFF_FFFF) as u32;
    let ext = ((k >> 38) & 1) == 1;
    let data = a.last.get(&k).map(|li| li.data.clone()).unwrap_or_default();
    if !a.dbc_loaded() {
        a.log("未加载 DBC，无法添加信号".to_string());
        return;
    }
    let decoded = a.dbc_decode_frame(id, ext, &data);
    if decoded.is_empty() {
        a.log(format!("0x{id:X} 未匹配 DBC，无信号可加"));
        return;
    }
    let mut added = 0;
    for dec in decoded {
        if a.series.iter().any(|s| s.id == id && s.signal == dec.name) {
            continue;
        }
        let idx = a.series.len();
        let c = PALETTE[idx % PALETTE.len()];
        a.series.push(Series {
            id,
            signal: dec.name.clone(),
            name: dec.name.clone(),
            color: Color::from_rgb_u8(c.0, c.1, c.2),
            unit: dec.unit.clone(),
            samples: VecDeque::new(),
            cur: 0.0,
            visible: true,
            expr: None,
        });
        added += 1;
    }
    a.log(format!("0x{id:X} 已添加 {added} 个信号到曲线"));
}

fn display_signal(a: &App, row: i32) -> Option<(u64, String)> {
    match a.display_items.get(row as usize) {
        Some(DisplayItem::Signal { key, signal }) => Some((*key, signal.clone())),
        _ => None,
    }
}

fn add_signal_to_chart(a: &mut App, id: u32, signal: &str) -> String {
    if a.series.iter().any(|s| s.id == id && s.signal == signal) {
        return format!("信号 {signal} 已在曲线中");
    }
    let unit = a
        .dbc_decode(id, &[0u8; 8])
        .into_iter()
        .find(|x| x.name == signal)
        .map(|x| x.unit)
        .unwrap_or_default();
    let idx = a.series.len();
    let c = PALETTE[idx % PALETTE.len()];
    a.series.push(Series {
        id,
        signal: signal.to_string(),
        name: signal.to_string(),
        color: Color::from_rgb_u8(c.0, c.1, c.2),
        unit,
        samples: VecDeque::new(),
        cur: 0.0,
        visible: true,
        expr: None,
    });
    format!("已添加信号到曲线: {signal}")
}

fn add_expr_to_chart(a: &mut App, name: &str) -> String {
    let Some(ev) = a.expr_vars.iter().find(|e| e.name == name).cloned() else {
        return format!("找不到表达式: {name}");
    };
    if a.series.iter().any(|s| s.expr.is_some() && s.name == name) {
        return format!("表达式 {name} 已在曲线中");
    }
    let idx = a.series.len();
    let c = PALETTE[idx % PALETTE.len()];
    a.series.push(Series {
        id: 0,
        signal: ev.name.clone(),
        name: ev.name.clone(),
        color: Color::from_rgb_u8(c.0, c.1, c.2),
        unit: ev.unit.clone(),
        samples: VecDeque::new(),
        cur: 0.0,
        visible: true,
        expr: Some(ev.formula.clone()),
    });
    format!("已添加表达式到曲线: {name}")
}

pub(crate) fn recompute_expr_ids(a: &mut App) {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in &a.expr_vars {
        if let Ok(refs) = expr::refs(&ev.formula) {
            for r in refs {
                names.insert(r);
            }
        }
    }
    let mut ids: HashSet<u32> = HashSet::new();
    for d in &a.dbcs {
        for m in d.messages() {
            if m.signals.iter().any(|s| names.contains(&s.name)) {
                ids.insert(m.id);
            }
        }
    }
    a.expr_decode_ids = ids;
}

pub(crate) fn parse_tx_bytes(value: &str, max_len: usize) -> Vec<u8> {
    let compact = value.replace([',', ';'], " ");
    let parts: Vec<&str> = compact.split_whitespace().collect();
    let mut data = Vec::new();
    if parts.len() == 1 && parts.first().map(|p| p.len() > 2).unwrap_or(false) {
        let s = parts[0].trim_start_matches("0x").trim_start_matches("0X");
        for chunk in s.as_bytes().chunks(2) {
            if chunk.len() == 2
                && let Ok(hex) = std::str::from_utf8(chunk)
                && let Ok(b) = u8::from_str_radix(hex, 16)
            {
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
    data.truncate(max_len);
    data
}

fn ch_from_name(s: &str) -> u8 {
    s.trim()
        .to_ascii_lowercase()
        .trim_start_matches("can")
        .trim()
        .parse::<u8>()
        .unwrap_or(1)
        .max(1)
}

fn default_channel() -> DeviceConfig {
    DeviceConfig {
        sw_channel: 1,
        is_fd: false,
        device_type: "Virtual".into(),
        hardware_label: String::new(),
        hardware_id: String::new(),
        device_index: 0,
        channel_index: 0,
        baud: "500K".into(),
        data_baud: "2M".into(),
        custom_bitrate: String::new(),
        termination: false,
        listen_only: false,
        fd_non_iso: false,
        net_server: true,
        ip: "192.168.0.178".into(),
        port: "8000".into(),
    }
}

fn renumber_channel_slice(channels: &mut [DeviceConfig]) {
    for (i, c) in channels.iter_mut().enumerate() {
        c.sw_channel = (i + 1) as u8;
    }
}

fn channel_configs(a: &App) -> &[DeviceConfig] {
    a.channel_edit
        .as_ref()
        .map(|session| session.channels.as_slice())
        .unwrap_or(a.channels.as_slice())
}

fn channel_selected(a: &App) -> i32 {
    a.channel_edit
        .as_ref()
        .map(|session| session.selected)
        .unwrap_or(a.channel_sel)
}

fn ensure_channel_edit_session(a: &mut App) {
    if a.channel_edit.is_none() {
        a.channel_edit = Some(ChannelEditSession {
            channels: a.channels.clone(),
            selected: a.channel_sel,
            dirty: false,
        });
    }
}

fn set_chan_form(w: &ChannelConfigWindow, c: &DeviceConfig, a: &App) {
    let device_upper = c.device_type.trim().to_ascii_uppercase();
    let detected_pcan = a.pcan_devices.iter().find(|hardware| {
        (!c.hardware_id.is_empty() && c.hardware_id == pcan_hardware_id(hardware))
            || (device_upper == "PCAN" && hardware.channel_index == c.channel_index)
    });
    let detected_zcan = a.zcan_devices.iter().find(|hardware| {
        (!c.hardware_id.is_empty() && c.hardware_id == zcan_hardware_id(hardware))
            || (hardware.device_type.eq_ignore_ascii_case(&c.device_type)
                && hardware.device_index == c.device_index
                && hardware.channel_index == c.channel_index)
    });
    let is_zlg_fd = device_upper.contains("USBCANFD");
    let is_network_fd = device_upper.contains("CANFDNET") || device_upper.contains("CANFDWIFI");
    let supports_fd = detected_pcan
        .map(|hardware| hardware.fd_capable)
        .or_else(|| detected_zcan.map(|hardware| hardware.fd_capable))
        .unwrap_or(is_zlg_fd || is_network_fd || c.is_fd);
    let supports_termination = is_zlg_fd;
    let supports_listen_only = device_upper != "PCAN"
        && (detected_zcan.is_some()
            || is_zlg_fd
            || device_upper.contains("USBCAN")
            || matches!(device_upper.as_str(), "GCAN" | "ZHCX" | "ZHCXCAN"));
    let supports_non_iso = supports_fd && device_upper != "PCAN";
    let identity = if !c.hardware_id.is_empty() {
        c.hardware_id.clone()
    } else if let Some(hardware) = detected_pcan {
        pcan_hardware_id(hardware)
    } else if let Some(hardware) = detected_zcan {
        zcan_hardware_id(hardware)
    } else {
        String::new()
    };
    let state = if detected_pcan.is_some() || detected_zcan.is_some() {
        if a.lang_en {
            "Detected and matched"
        } else {
            "已检测并匹配"
        }
    } else if identity.is_empty() {
        if a.lang_en {
            "Manual mapping; verify indices before connecting"
        } else {
            "手动映射，连接前请核对索引"
        }
    } else if a.lang_en {
        "Saved hardware is currently offline"
    } else {
        "已保存的硬件当前不在线"
    };
    let arbitration = if device_upper == "PCAN" && supports_fd {
        vec!["1M", "800K", "500K", "250K", "125K"]
    } else if device_upper == "PCAN" {
        vec!["1M", "500K", "250K", "125K"]
    } else if supports_fd {
        vec!["1M", "800K", "500K", "250K", "125K"]
    } else {
        vec![
            "1M", "800K", "500K", "250K", "125K", "100K", "50K", "20K", "10K", "5K",
        ]
    };
    let data_rates = vec!["8M", "5M", "4M", "2M", "1M", "800K", "500K", "250K", "125K"];
    w.set_is_fd(c.is_fd);
    w.set_device_type(c.device_type.clone().into());
    w.set_hardware_label(c.hardware_label.clone().into());
    w.set_device_index(c.device_index.to_string().into());
    w.set_channel_index((c.channel_index + 1).to_string().into());
    w.set_baud(c.baud.clone().into());
    w.set_data_baud(c.data_baud.clone().into());
    w.set_custom_bitrate(c.custom_bitrate.clone().into());
    w.set_termination(c.termination);
    w.set_listen_only(c.listen_only);
    w.set_fd_non_iso(c.fd_non_iso);
    w.set_manual_mode(c.hardware_id.is_empty());
    w.set_supports_fd(supports_fd);
    w.set_supports_termination(supports_termination);
    w.set_supports_listen_only(supports_listen_only);
    w.set_supports_non_iso(supports_non_iso);
    w.set_arb_baud_options(ModelRc::from(Rc::new(VecModel::from(
        arbitration
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    w.set_data_baud_options(ModelRc::from(Rc::new(VecModel::from(
        data_rates
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))));
    w.set_hardware_identity(identity.into());
    w.set_device_state(state.into());
    w.set_net_server(c.net_server);
    w.set_ip(c.ip.clone().into());
    w.set_port(c.port.clone().into());
}

fn chan_list_strings(a: &App) -> Vec<SharedString> {
    channel_configs(a)
        .iter()
        .map(|c| {
            let label = c.hardware_label.trim();
            let label = if label.is_empty() { "Unnamed" } else { label };
            let proto = if c.is_fd { "CAN FD" } else { "CAN" };
            format!("CAN{}  {}  {}", c.sw_channel, label, proto).into()
        })
        .collect()
}

fn chan_detail_strings(a: &App) -> Vec<SharedString> {
    channel_configs(a)
        .iter()
        .map(|c| {
            let dev = c.device_type.trim();
            let bus = if dev.eq_ignore_ascii_case("PCAN") {
                if let Some(hw) = a
                    .pcan_devices
                    .iter()
                    .find(|hw| hw.channel_index == c.channel_index)
                {
                    format!(
                        "{} {}: Device ID {:X}h",
                        hw.channel_name, hw.device_name, hw.device_id
                    )
                } else {
                    format!("PCAN_USBBUS{} not detected", c.channel_index + 1)
                }
            } else if dev.to_ascii_uppercase().contains("NET")
                || dev.to_ascii_uppercase().contains("WIFI")
            {
                format!("{}:{}", c.ip, c.port)
            } else {
                format!("dev{} CAN{}", c.device_index, c.channel_index + 1)
            };
            let pcan_cap = if dev.eq_ignore_ascii_case("PCAN") {
                a.pcan_devices
                    .iter()
                    .find(|hw| hw.channel_index == c.channel_index)
                    .map(|hw| hw.fd_capable)
            } else {
                None
            };
            let proto = if c.is_fd {
                let mut s = format!("CANFD {}/{}", c.baud, c.data_baud);
                if matches!(pcan_cap, Some(false)) {
                    s.push_str(" !");
                }
                s
            } else {
                format!("CAN {}", c.baud)
            };
            format!("{}  {}  {}", dev, bus, proto).into()
        })
        .collect()
}

struct HardwareDisplayRows {
    titles: Vec<SharedString>,
    details: Vec<SharedString>,
    added: Vec<bool>,
    groups: Vec<bool>,
    sources: Vec<i32>,
    enabled: Vec<bool>,
}

impl HardwareDisplayRows {
    fn push(
        &mut self,
        title: String,
        detail: String,
        added: bool,
        group: bool,
        source: i32,
        enabled: bool,
    ) {
        self.titles.push(title.into());
        self.details.push(detail.into());
        self.added.push(added);
        self.groups.push(group);
        self.sources.push(source);
        self.enabled.push(enabled);
    }
}

fn pcan_hardware_id(hw: &can::PcanChannelInfo) -> String {
    format!("PCAN:{:08X}:{}", hw.device_id, hw.channel_index)
}

fn zcan_hardware_id(hw: &can::ZcanUsbChannelInfo) -> String {
    let identity = if hw.serial_number.trim().is_empty() {
        format!("DEV{}", hw.device_index)
    } else {
        hw.serial_number.trim().to_ascii_uppercase()
    };
    format!(
        "{}:{}:{}",
        hw.device_type.trim().to_ascii_uppercase(),
        identity,
        hw.channel_index
    )
}

fn hardware_display_rows(
    devices: &[can::PcanChannelInfo],
    zcan_devices: &[can::ZcanUsbChannelInfo],
    channels: &[DeviceConfig],
    english: bool,
) -> HardwareDisplayRows {
    let mut result = HardwareDisplayRows {
        titles: Vec::new(),
        details: Vec::new(),
        added: Vec::new(),
        groups: Vec::new(),
        sources: Vec::new(),
        enabled: Vec::new(),
    };
    let mut pcan_groups =
        std::collections::BTreeMap::<(u32, String), Vec<(usize, &can::PcanChannelInfo)>>::new();
    for (index, hw) in devices.iter().enumerate() {
        pcan_groups
            .entry((hw.device_id, hw.device_name.clone()))
            .or_default()
            .push((index, hw));
    }
    for ((device_id, device_name), rows) in pcan_groups {
        result.push(
            format!("PEAK  {device_name}"),
            format!("Device ID {device_id:08X}h · {} channel(s)", rows.len()),
            false,
            true,
            -1,
            false,
        );
        for (source, hw) in rows {
            let stable_id = pcan_hardware_id(hw);
            let is_added = channels.iter().any(|channel| {
                (!channel.hardware_id.is_empty() && channel.hardware_id == stable_id)
                    || (channel.device_type.eq_ignore_ascii_case("PCAN")
                        && channel.channel_index == hw.channel_index)
            });
            let condition = match hw.channel_condition {
                1 => {
                    if english {
                        "available"
                    } else {
                        "可用"
                    }
                }
                2 | 4 => {
                    if english {
                        "in use"
                    } else {
                        "已占用"
                    }
                }
                _ => {
                    if english {
                        "unavailable"
                    } else {
                        "不可用"
                    }
                }
            };
            result.push(
                format!("↳ {}", hw.channel_name),
                format!(
                    "{} · {}",
                    if hw.fd_capable {
                        "CAN FD"
                    } else {
                        "Classical CAN"
                    },
                    condition
                ),
                is_added,
                false,
                source as i32,
                hw.channel_condition == 1 || is_added,
            );
        }
    }

    let offset = devices.len();
    let mut zcan_groups =
        std::collections::BTreeMap::<String, Vec<(usize, &can::ZcanUsbChannelInfo)>>::new();
    for (index, hw) in zcan_devices.iter().enumerate() {
        let serial = if hw.serial_number.trim().is_empty() {
            format!("dev{}", hw.device_index)
        } else {
            format!("SN {}", hw.serial_number.trim())
        };
        let key = format!("{}|{}|{}", hw.device_type, serial, hw.hardware_label);
        zcan_groups.entry(key).or_default().push((index, hw));
    }
    for (_key, rows) in zcan_groups {
        let first = rows[0].1;
        let vendor = match first.device_type.to_ascii_uppercase().as_str() {
            "GCAN" => "GCAN",
            "ZHCX" | "ZHCXCAN" => "ZHCX",
            _ => "ZLG",
        };
        let identity = if first.serial_number.trim().is_empty() {
            format!("dev{}", first.device_index)
        } else {
            format!("SN {}", first.serial_number.trim())
        };
        result.push(
            format!("{vendor}  {}", first.hardware_label),
            format!("{identity} · {} channel(s)", rows.len()),
            false,
            true,
            -1,
            false,
        );
        for (source, hw) in rows {
            let stable_id = zcan_hardware_id(hw);
            let is_added = channels.iter().any(|channel| {
                (!channel.hardware_id.is_empty() && channel.hardware_id == stable_id)
                    || (channel.device_type.eq_ignore_ascii_case(&hw.device_type)
                        && channel.device_index == hw.device_index
                        && channel.channel_index == hw.channel_index)
            });
            result.push(
                format!("↳ CAN{}", hw.channel_index + 1),
                format!(
                    "{} · dev{} ch{}",
                    if hw.fd_capable {
                        "CAN FD"
                    } else {
                        "Classical CAN"
                    },
                    hw.device_index,
                    hw.channel_index
                ),
                is_added,
                false,
                (offset + source) as i32,
                true,
            );
        }
    }

    if result.titles.is_empty() {
        result.titles.push(
            if english {
                "No CAN hardware detected"
            } else {
                "未发现 CAN 硬件"
            }
            .into(),
        );
        result.details.push(
            if english {
                "Check USB connection, driver installation, and device occupancy"
            } else {
                "请检查 USB、驱动安装以及设备是否被其他软件占用"
            }
            .into(),
        );
        result.added.push(false);
        result.groups.push(true);
        result.sources.push(-1);
        result.enabled.push(false);
    }
    result
}

fn refresh_and_reconcile_pcan(a: &mut App) {
    reconcile_stable_hardware(a);
    let configured = a
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.device_type.eq_ignore_ascii_case("PCAN"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if configured.len() != 1 || a.pcan_devices.len() != 1 {
        return;
    }
    let config_index = configured[0];
    let hardware = a.pcan_devices[0].clone();
    let configured_index = a.channels[config_index].channel_index;
    if configured_index == hardware.channel_index {
        return;
    }
    a.channels[config_index].channel_index = hardware.channel_index;
    if a.channels[config_index].hardware_label.trim().is_empty() {
        a.channels[config_index].hardware_label = hardware.device_name.clone();
    }
    a.log(format!(
        "PCAN 硬件通道已自动校正: PCAN_USBBUS{} → {}",
        configured_index + 1,
        hardware.channel_name
    ));
}

fn reconcile_stable_hardware(a: &mut App) {
    for channel in &mut a.channels {
        if channel.hardware_id.is_empty() {
            continue;
        }
        if let Some(hardware) = a
            .pcan_devices
            .iter()
            .find(|hardware| pcan_hardware_id(hardware) == channel.hardware_id)
        {
            channel.device_type = "PCAN".into();
            channel.device_index = 0;
            channel.channel_index = hardware.channel_index;
            if channel.hardware_label.trim().is_empty() {
                channel.hardware_label = hardware.device_name.clone();
            }
            continue;
        }
        if let Some(hardware) = a
            .zcan_devices
            .iter()
            .find(|hardware| zcan_hardware_id(hardware) == channel.hardware_id)
        {
            channel.device_type = hardware.device_type.clone();
            channel.device_index = hardware.device_index;
            channel.channel_index = hardware.channel_index;
            if channel.hardware_label.trim().is_empty() {
                channel.hardware_label = hardware.hardware_label.clone();
            }
        }
    }
    if let Some(session) = a.channel_edit.as_mut() {
        for channel in &mut session.channels {
            if channel.hardware_id.is_empty() {
                continue;
            }
            if let Some(hardware) = a
                .pcan_devices
                .iter()
                .find(|hardware| pcan_hardware_id(hardware) == channel.hardware_id)
            {
                channel.device_type = "PCAN".into();
                channel.device_index = 0;
                channel.channel_index = hardware.channel_index;
            } else if let Some(hardware) = a
                .zcan_devices
                .iter()
                .find(|hardware| zcan_hardware_id(hardware) == channel.hardware_id)
            {
                channel.device_type = hardware.device_type.clone();
                channel.device_index = hardware.device_index;
                channel.channel_index = hardware.channel_index;
            }
        }
    }
}

fn scan_attached_hardware(a: &mut App) -> bool {
    const MIN_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    let now = std::time::Instant::now();
    if a.last_hardware_scan
        .is_some_and(|last| now.saturating_duration_since(last) < MIN_SCAN_INTERVAL)
    {
        return false;
    }

    // Some vendor USB-CAN drivers are not re-entrant and can corrupt their
    // internal state when OpenDevice/CloseDevice is called repeatedly in a
    // tight loop. Mark the scan before entering the DLL so re-entrant UI
    // callbacks cannot start a second scan.
    a.last_hardware_scan = Some(now);
    if a.hardware_scan_in_progress {
        return false;
    }
    a.hardware_scan_in_progress = true;
    a.hardware_scan_status = if a.lang_en {
        "Scanning PEAK, ZLG, GCAN and ZHCX drivers...".into()
    } else {
        "正在扫描 PEAK、ZLG、GCAN 与 ZHCX 驱动...".into()
    };
    let worker = a.worker_tx.clone();
    let retained_zcan = a.connected.then(|| a.zcan_devices.clone());
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let pcan = can::pcan_attached_channels();
        let zcan = retained_zcan.unwrap_or_else(can::zcan_attached_channels);
        let _ = worker.send(WorkerEvent::HardwareScanned {
            pcan,
            zcan,
            elapsed_ms: started.elapsed().as_millis(),
        });
    });
    true
}

fn refresh_channel_window_lists(w: &ChannelConfigWindow, a: &App) {
    w.set_channels(ModelRc::from(Rc::new(VecModel::from(chan_list_strings(a)))));
    w.set_channel_details(ModelRc::from(Rc::new(VecModel::from(chan_detail_strings(
        a,
    )))));
    let hardware = hardware_display_rows(
        &a.pcan_devices,
        &a.zcan_devices,
        channel_configs(a),
        a.lang_en,
    );
    w.set_pcan_hardware(ModelRc::from(Rc::new(VecModel::from(hardware.titles))));
    w.set_pcan_hardware_details(ModelRc::from(Rc::new(VecModel::from(hardware.details))));
    w.set_pcan_hardware_added(ModelRc::from(Rc::new(VecModel::from(hardware.added))));
    w.set_pcan_hardware_group(ModelRc::from(Rc::new(VecModel::from(hardware.groups))));
    w.set_pcan_hardware_source(ModelRc::from(Rc::new(VecModel::from(hardware.sources))));
    w.set_pcan_hardware_enabled(ModelRc::from(Rc::new(VecModel::from(hardware.enabled))));
    w.set_scan_in_progress(a.hardware_scan_in_progress);
    w.set_scan_status(a.hardware_scan_status.clone().into());
    w.set_connecting(a.channel_connect_pending);
    w.set_config_dirty(a.channel_edit.as_ref().is_some_and(|session| session.dirty));
}

fn refresh_ui(a: &mut App, ui: &AppWindow, child_windows: Option<&ChildWindows>) {
    ui.set_connected(a.connected);
    ui.set_running(a.running);
    ui.set_recording(a.recording);
    ui.set_mode_trace(a.mode_trace);
    ui.set_paused(a.paused);
    ui.set_auto_scroll(a.autoscroll);
    ui.set_rx_count(a.rx.to_string().into());
    ui.set_tx_count(a.tx.to_string().into());
    ui.set_err_count(a.err.to_string().into());
    ui.set_capture_health(
        format!(
            "RX {}/{} H{} D{}  CMD {}/{} H{} R{}  REC {}/{} H{} D{}  HW O{} E{}  TS N{} J{:.0}/{:.0}us {:+.1}ppm M{}",
            a.capture_queue_depth,
            a.capture_queue_capacity,
            a.capture_queue_high_watermark,
            a.capture_dropped_frames,
            a.command_queue_depth,
            a.command_queue_capacity,
            a.command_queue_high_watermark,
            a.command_rejected,
            a.recorder.queue_depth(),
            a.recorder.queue_capacity(),
            a.recorder.queue_high_watermark(),
            a.recorder.dropped_frames(),
            a.capture_hardware_overruns,
            a.capture_hardware_errors,
            a.timestamp_samples,
            a.timestamp_latest_jitter_us,
            a.timestamp_max_jitter_us,
            a.timestamp_drift_ppm,
            a.timestamp_monotonic_violations,
        )
        .into(),
    );
    ui.set_capture_loss(
        a.capture_dropped_frames > 0
            || a.capture_dropped_events > 0
            || a.capture_hardware_overruns > 0
            || a.capture_hardware_errors > 0
            || a.command_rejected > 0
            || a.timestamp_monotonic_violations > 0
            || a.recorder.dropped_frames() > 0,
    );
    ui.set_fps(format!("{:.0}", a.fps).into());
    ui.set_bus_load(format!("{:.1}%", a.bus_load).into());
    ui.set_load_high(a.bus_load >= 70.0);

    let chan_load = if a.chan_stats.len() >= 2 {
        a.chan_stats
            .iter()
            .map(|(ch, cs)| format!("CAN{ch} {:.0}%", cs.bus_load))
            .collect::<Vec<_>>()
            .join("  ")
    } else {
        String::new()
    };
    ui.set_chan_load(chan_load.into());
    ui.set_baud(a.baud.clone().into());
    ui.set_total_count(a.trace.len().to_string().into());
    let sel_id_txt = a
        .selected_key
        .map(|k| {
            let id = (k & 0xFFFF_FFFF) as u32;
            let ext = ((k >> 38) & 1) == 1;
            let nm = a.dbc_message_name_frame(id, ext).unwrap_or("");
            if nm.is_empty() {
                format!("0x{id:X}")
            } else {
                format!("0x{id:X} {nm}")
            }
        })
        .unwrap_or_else(|| "无".into());
    ui.set_sel_id(sel_id_txt.into());

    ui.set_selected(a.selected_index);

    build_msg_table(a, ui);

    build_signal_panel(a, ui);

    let dbc_signal_sig = std::sync::Arc::as_ptr(&a.dbc_snap) as usize as u64;
    if dbc_signal_sig != a.dbc_signal_cache {
        a.dbc_signal_cache = dbc_signal_sig;
        a.dbc_signal_choices.clear();
        let mut dbc_signal_rows: Vec<SharedString> = Vec::new();
        let mut choices: Vec<(u32, String, String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();
        for d in &a.dbcs {
            for m in d.messages() {
                for s in &m.signals {
                    if seen.insert((m.id, s.name.clone())) {
                        choices.push((m.id, m.name.clone(), s.name.clone(), s.unit.clone()));
                    }
                }
            }
        }
        choices.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        for (id, msg_name, sig_name, unit) in choices {
            a.dbc_signal_choices.push((id, sig_name.clone()));
            let unit_suffix = if unit.is_empty() {
                String::new()
            } else {
                format!(" [{unit}]")
            };
            dbc_signal_rows.push(format!("0x{id:X} {msg_name} / {sig_name}{unit_suffix}").into());
        }
        if dbc_signal_rows.is_empty() {
            dbc_signal_rows.push("(无 DBC 信号)".into());
        }
        sync_vec_model(&a.dbc_signal_model, dbc_signal_rows);
    }
    if let Some(windows) = child_windows {
        refresh_signal_picker(a, &windows.signal);

        refresh_chart(a, ui, &windows.chart);

        {
            let sig = tx_list_sig(a);
            if sig != a.tx_list_cache {
                a.tx_list_cache = sig;
                push_tx_list(a, ui, &windows.tx);
            }
        }

        let chan_names: Vec<SharedString> = a
            .channels
            .iter()
            .map(|c| format!("CAN{}", c.sw_channel).into())
            .collect();
        {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            chan_names.hash(&mut h);
            let sig = h.finish();
            if sig != a.chan_names_cache {
                a.chan_names_cache = sig;
                windows
                    .tx
                    .set_channel_names(ModelRc::from(Rc::new(VecModel::from(chan_names))));
            }
        }

        build_tx_dbc_page(a, &windows.tx);
    }

    build_stats(a, ui);

    tree::build_tree(a, ui);

    if a.console_cache != a.console.revision {
        a.console_cache = a.console.revision;
        let console_rows = a.console.rows().into_iter().map(Into::into).collect();
        sync_vec_model(&a.console_model, console_rows);
    }
}

pub(crate) fn sync_vec_model<T: Clone + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for (index, row) in rows.into_iter().enumerate() {
        if index < model.row_count() {
            model.set_row_data(index, row);
        } else {
            model.push(row);
        }
    }
}

pub(crate) fn fmtf(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn publish_snapshot(a: &mut App) {
    let last_rebuild = if a.last_dirty {
        let mut last = HashMap::with_capacity(a.last.len());
        for (k, li) in a.last.iter() {
            last.insert(
                *k,
                ipc::LastSnap {
                    t: li.t,
                    count: li.count,
                    data: li.data.clone(),
                    ext: li.ext,
                },
            );
        }
        a.last_dirty = false;
        Some(last)
    } else {
        None
    };
    if let Ok(mut snap) = a.ipc_snapshot.lock() {
        snap.connected = a.connected;
        snap.running = a.running;
        snap.rx = a.rx;
        snap.tx = a.tx;
        snap.err = a.err;
        snap.no_counter = a.no_counter;
        snap.bus_load = a.bus_load;
        snap.fps = a.fps;
        snap.dropped_frames = a.capture_dropped_frames;
        snap.dropped_events = a.capture_dropped_events;
        snap.hardware_overruns = a.capture_hardware_overruns;
        snap.hardware_errors = a.capture_hardware_errors;
        snap.event_queue_depth = a.capture_queue_depth;
        snap.event_queue_capacity = a.capture_queue_capacity;
        snap.event_queue_high_watermark = a.capture_queue_high_watermark;
        snap.command_rejected = a.command_rejected;
        snap.command_queue_depth = a.command_queue_depth;
        snap.command_queue_capacity = a.command_queue_capacity;
        snap.command_queue_high_watermark = a.command_queue_high_watermark;
        snap.timestamp_samples = a.timestamp_samples;
        snap.timestamp_latest_jitter_us = a.timestamp_latest_jitter_us;
        snap.timestamp_max_jitter_us = a.timestamp_max_jitter_us;
        snap.timestamp_drift_ppm = a.timestamp_drift_ppm;
        snap.timestamp_monotonic_violations = a.timestamp_monotonic_violations;
        snap.last_log = a.logs.back().cloned().unwrap_or_default();
        snap.recent_logs = a.logs.iter().rev().take(100).cloned().collect();
        snap.recent_logs.reverse();
        snap.channels = if a.connected {
            let mut channels: Vec<u8> = a.connected_channels.iter().copied().collect();
            channels.sort_unstable();
            channels
                .into_iter()
                .map(|ch| {
                    let cs = a.chan_stats.get(&ch).cloned().unwrap_or_default();
                    ipc::ChanStatSnap {
                        ch,
                        rx: cs.rx,
                        tx: cs.tx,
                        err: cs.err,
                        bus_load: cs.bus_load,
                        fps: cs.fps,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        snap.console_enabled = a.console_enabled;
        if a.console_enabled {
            snap.console_text = a.console.export_text();
        }
        if let Some(last) = last_rebuild {
            snap.last = last;
        }
        snap.dbc = a.dbc_snap.clone();
    }
}

fn rebuild_dbc_snap(a: &mut App) {
    a.dbc_snap = std::sync::Arc::new(ipc::DbcSnapshot::from_dbcs(&a.dbcs));

    recompute_expr_ids(a);
}

fn stop_internal_periodic(a: &App, internal: u64) {
    let dummy = CanFrame {
        t: 0.0,
        ch: 1,
        tx: true,
        id: 0,
        ext: false,
        fd: false,
        brs: false,
        remote: false,
        error: false,
        data: Vec::new(),
    };
    let _ = a.cmd.send(Cmd::SetPeriodic {
        handle: internal,
        frame: dummy,
        period_ms: 1,
        repeat: -1,
        enable: false,
    });
}

fn handle_ipc(a: &mut App, ureq: ipc::UiReq) {
    use ipc::{IpcReq, IpcResp};
    let cid = ureq.client_id;
    let ok = || IpcResp::Ok(serde_json::json!({}));
    let license_denied = || IpcResp::Err {
        code: "LICENSE_REQUIRED".into(),
        msg: "试用已结束，需要有效的 .pcanlic 授权".into(),
    };

    let mut periodic_rollback: Option<u64> = None;
    let resp = match ureq.req {
        IpcReq::Invalid { code, msg } => IpcResp::Err { code, msg },
        IpcReq::SendOnce {
            ch,
            id,
            data,
            ext,
            fd,
            brs,
            remote,
        } => {
            if !a.license_allows("can-transmit") {
                license_denied()
            } else {
                match validate_ipc_tx_frame(ch, id, data, ext, fd, brs, remote) {
                    Ok(frame) => match a.cmd.send(Cmd::SendOnce(frame)) {
                        Ok(()) => ok(),
                        Err(_) => IpcResp::Err {
                            code: "BUSY".into(),
                            msg: "CAN 命令队列已满，本帧未提交，请稍后重试".into(),
                        },
                    },
                    Err(msg) => IpcResp::Err {
                        code: "BAD_FRAME".into(),
                        msg,
                    },
                }
            }
        }
        IpcReq::SendBatch { frames, repeat } => {
            if !a.license_allows("can-transmit") {
                license_denied()
            } else if frames.is_empty() {
                IpcResp::Err {
                    code: "BAD_ARG".into(),
                    msg: "frames 不能为空".into(),
                }
            } else if repeat == 0 {
                IpcResp::Err {
                    code: "BAD_ARG".into(),
                    msg: "repeat 必须大于 0".into(),
                }
            } else {
                let total = (frames.len() as u64).saturating_mul(repeat as u64);
                if total > 100_000 {
                    IpcResp::Err {
                        code: "BATCH_LIMIT".into(),
                        msg: format!("批量发送共 {total} 帧，超过 100000 帧安全上限"),
                    }
                } else {
                    let mut validated = Vec::with_capacity(frames.len());
                    let mut error = None;
                    for (index, frame) in frames.into_iter().enumerate() {
                        match validate_ipc_tx_frame(
                            frame.ch,
                            frame.id,
                            frame.data,
                            frame.ext,
                            frame.fd,
                            frame.brs,
                            frame.remote,
                        ) {
                            Ok(frame) => validated.push(frame),
                            Err(message) => {
                                error = Some(format!("frames[{index}]: {message}"));
                                break;
                            }
                        }
                    }
                    if let Some(msg) = error {
                        IpcResp::Err {
                            code: "BAD_FRAME".into(),
                            msg,
                        }
                    } else {
                        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
                        match a.cmd.send(Cmd::SendBatch {
                            frames: validated,
                            repeat,
                            ack: Some(ack_tx),
                        }) {
                            Ok(()) => match ack_rx
                                .recv_timeout(std::time::Duration::from_millis(500))
                            {
                                Ok(Ok(queued)) => {
                                    IpcResp::Ok(serde_json::json!({ "queued": queued }))
                                }
                                Ok(Err(msg)) => IpcResp::Err {
                                    code: "QUEUE_REJECTED".into(),
                                    msg,
                                },
                                Err(_) => IpcResp::Err {
                                    code: "TIMEOUT".into(),
                                    msg: "CAN 后台未在 500ms 内确认批量任务，任务状态未知".into(),
                                },
                            },
                            Err(_) => IpcResp::Err {
                                code: "BUSY".into(),
                                msg: "CAN 命令队列已满，批次未提交，请稍后重试".into(),
                            },
                        }
                    }
                }
            }
        }
        IpcReq::SetPeriodic {
            client_handle,
            ch,
            id,
            data,
            period_ms,
            repeat,
            ext,
            fd,
            brs,
            remote,
        } => {
            if !a.license_allows("can-transmit") {
                license_denied()
            } else {
                match validate_ipc_tx_frame(ch, id, data, ext, fd, brs, remote) {
                    Err(msg) => IpcResp::Err {
                        code: "BAD_FRAME".into(),
                        msg,
                    },
                    Ok(frame) => {
                        let internal = a.next_handle | (1u64 << 63);
                        a.next_handle += 1;
                        if let Some(old) = a.ipc_handle_map.insert((cid, client_handle), internal) {
                            stop_internal_periodic(a, old);
                        }
                        match a.cmd.send(Cmd::SetPeriodic {
                            handle: internal,
                            frame,
                            period_ms: period_ms.max(1),
                            repeat,
                            enable: true,
                        }) {
                            Ok(()) => {
                                periodic_rollback = Some(internal);
                                ok()
                            }
                            Err(_) => {
                                a.ipc_handle_map.remove(&(cid, client_handle));
                                IpcResp::Err {
                                    code: "BUSY".into(),
                                    msg: "CAN 命令队列已满，周期任务未提交，请稍后重试".into(),
                                }
                            }
                        }
                    }
                }
            }
        }
        IpcReq::StopPeriodic { client_handle } => {
            if let Some(internal) = a.ipc_handle_map.remove(&(cid, client_handle)) {
                stop_internal_periodic(a, internal);
            }
            ok()
        }
        IpcReq::Connect { channels } => {
            if !a.license_allows("can-connect") {
                license_denied()
            } else if channels.is_empty() {
                IpcResp::Err {
                    code: "BAD_ARG".into(),
                    msg: "channels 不能为空(至少给一个通道配置)".into(),
                }
            } else {
                a.channels = channels.clone();
                let _ = a.cmd.send(Cmd::ConnectChannels(channels));
                ok()
            }
        }
        IpcReq::ConnectConfigured => {
            if !a.license_allows("can-connect") {
                license_denied()
            } else {
                let _ = a.cmd.send(Cmd::ConnectChannels(a.channels.clone()));
                let expected_channels: Vec<u8> = a
                    .channels
                    .iter()
                    .map(|channel| channel.sw_channel.max(1))
                    .collect();
                IpcResp::Ok(serde_json::json!({
                    "channels": a.channels.len(),
                    "expected_channels": expected_channels,
                }))
            }
        }
        IpcReq::LoadDbc { path, loaded } => {
            if a.dbc_paths.iter().any(|x| x == &path) {
                IpcResp::Ok(serde_json::json!({ "loaded": false, "name": path, "note": "已加载" }))
            } else {
                match loaded {
                    Ok(db) => {
                        let name = db.file_name.clone();
                        a.dbcs.push(db);
                        a.dbc_paths.push(path.clone());
                        rebuild_dbc_snap(a);
                        a.log(format!("脚本加载 DBC: {name}"));
                        IpcResp::Ok(serde_json::json!({ "loaded": true, "name": name }))
                    }
                    Err(e) => IpcResp::Err {
                        code: "LOAD_FAIL".into(),
                        msg: e,
                    },
                }
            }
        }
        IpcReq::Disconnect => {
            let _ = a.cmd.send(Cmd::Disconnect);
            ok()
        }
        IpcReq::Start => {
            if !a.license_allows("can-capture") {
                license_denied()
            } else {
                let _ = a.cmd.send(Cmd::Start);
                ok()
            }
        }
        IpcReq::Stop => {
            let _ = a.cmd.send(Cmd::Stop);
            ok()
        }
        IpcReq::Log { msg } => {
            a.log(msg);
            ok()
        }
        IpcReq::RunResult { passed, summary } => {
            a.run_status = format!("{} {summary}", if passed { "PASS" } else { "FAIL" });
            let rs = a.run_status.clone();
            a.log(format!("[脚本] {rs}"));
            ok()
        }
        IpcReq::ConsoleSet {
            enabled,
            id,
            ch,
            clear,
        } => {
            if let Some(en) = enabled {
                a.console_enabled = en;
            }
            if let Some(idv) = id {
                a.console_id = if idv < 0 { None } else { Some(idv as u32) };
            }
            if let Some(c) = ch {
                a.console_ch = c;
            }
            if clear {
                a.console.clear();
            }
            ok()
        }
        IpcReq::ClientGone => {
            let internals: Vec<u64> = a
                .ipc_handle_map
                .iter()
                .filter(|((c, _), _)| *c == cid)
                .map(|(_, h)| *h)
                .collect();
            for internal in internals {
                stop_internal_periodic(a, internal);
            }
            a.ipc_handle_map.retain(|(c, _), _| *c != cid);
            ok()
        }
    };

    if ureq.reply.send(resp).is_err()
        && let Some(internal) = periodic_rollback
    {
        stop_internal_periodic(a, internal);
        a.ipc_handle_map.retain(|_, h| *h != internal);
    }
}

fn validate_ipc_tx_frame(
    ch: u8,
    id: u32,
    data: Vec<u8>,
    ext: bool,
    fd: bool,
    brs: bool,
    remote: bool,
) -> Result<CanFrame, String> {
    if ch == 0 {
        return Err("CAN 通道必须从 1 开始".into());
    }
    let max_id = if ext { 0x1FFF_FFFF } else { 0x7FF };
    if id > max_id {
        return Err(format!(
            "ID 0x{id:X} 超出{}帧范围 0x0..0x{max_id:X}",
            if ext { "扩展" } else { "标准" }
        ));
    }
    if brs && !fd {
        return Err("BRS 只能用于 CAN FD 帧".into());
    }
    if remote && fd {
        return Err("CAN FD 不支持远程帧".into());
    }
    if remote && !data.is_empty() {
        return Err("远程帧不能携带数据字节".into());
    }
    if !fd && data.len() > 8 {
        return Err(format!(
            "经典 CAN 最多 8 字节，当前为 {} 字节；需要显式设置 fd=True",
            data.len()
        ));
    }
    if fd && !matches!(data.len(), 0..=8 | 12 | 16 | 20 | 24 | 32 | 48 | 64) {
        return Err(format!(
            "CAN FD 数据长度 {} 无法直接映射 DLC；允许 0..8、12、16、20、24、32、48、64 字节",
            data.len()
        ));
    }
    Ok(CanFrame {
        t: 0.0,
        ch,
        tx: true,
        id,
        ext,
        fd,
        brs,
        remote,
        error: false,
        data,
    })
}

fn ipc_fanout(a: &App, f: &CanFrame) {
    let subs = a.ipc_subs.subs.lock().unwrap();
    if subs.is_empty() {
        return;
    }
    let line = ipc::frame_event_json(f);
    for s in subs.iter() {
        if !s.ids.is_empty() && !s.ids.contains(&f.id) {
            continue;
        }
        if s.out.try_send(line.clone()).is_err() {
            s.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn reap_child(a: &mut App) {
    let timed_out = a
        .py_started
        .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(a.py_timeout_secs));
    if (a.py_stop_flag || timed_out) && a.py_child.is_some() {
        if let Some(mut c) = a.py_child.take() {
            let _ = c.kill();
        }
        a.run_status = if timed_out {
            "FAIL: 超时".into()
        } else {
            "已停止".into()
        };
        a.py_started = None;
        a.py_stop_flag = false;
        a.py_dirty = true;
        return;
    }
    a.py_stop_flag = false;
    if let Some(c) = a.py_child.as_mut()
        && let Ok(Some(st)) = c.try_wait()
    {
        let success = st.success();
        a.py_child = None;
        a.py_started = None;

        if !(a.run_status.starts_with("PASS") || a.run_status.starts_with("FAIL")) {
            a.run_status = if success {
                "PASS".into()
            } else {
                "FAIL".into()
            };
        }
        a.py_dirty = true;
    }
}

fn run_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("pcanwork_last_run.log")
}

fn drain_py_output(a: &mut App) {
    let mut lines = Vec::new();
    if let Some(rx) = &a.py_out_rx {
        let started = std::time::Instant::now();
        for _ in 0..1024 {
            let Ok(line) = rx.try_recv() else {
                break;
            };
            lines.push(line);
            if started.elapsed() >= std::time::Duration::from_millis(8) {
                break;
            }
        }
    }
    let dropped = a
        .py_output_dropped
        .as_ref()
        .map(|counter| counter.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);
    if dropped > a.py_output_dropped_seen {
        let newly_dropped = dropped - a.py_output_dropped_seen;
        lines.push(format!(
            "[PcanWork] Python 输出队列已丢弃 {newly_dropped} 行（累计 {dropped}），测试结果日志不完整"
        ));
        a.py_output_dropped_seen = dropped;
        if !a.run_status.starts_with("FAIL") {
            a.run_status = "FAIL: Python 输出队列溢出".into();
        }
    }
    if lines.is_empty() {
        return;
    }
    for line in lines {
        a.py_output.push_str(&line);
        a.py_output.push('\n');
        a.log(line);
    }

    const CAP: usize = 200_000;
    if a.py_output.len() > CAP {
        let mut cut = a.py_output.len() - CAP;
        while cut < a.py_output.len() && !a.py_output.is_char_boundary(cut) {
            cut += 1;
        }
        a.py_output = a.py_output[cut..].to_string();
    }
    a.py_dirty = true;
}

fn gather_settings(a: &App, ui: &AppWindow) -> settings::Settings {
    let th = ui.global::<Theme>();
    settings::Settings {
        channels: a.channels.clone(),
        channel_sel: a.channel_sel,
        dark: th.get_dark(),
        big: th.get_big(),
        trace_cap: a.trace_cap,
        chart_cap: a.chart_cap,
        f_id: ui.get_f_id().to_string(),
        f_name: ui.get_f_name().to_string(),
        f_data: ui.get_f_data().to_string(),
        dir_filter: ui.get_dir_filter(),
        dbc_path: None,
        dbc_paths: a.dbc_paths.clone(),
        left_w: ui.get_left_w(),
        bottom_h: ui.get_bottom_h(),
        mode_trace: a.mode_trace,
        time_mode: a.time_mode,
        cols_hidden: {
            let mut v: Vec<&str> = a.cols_hidden.iter().map(|s| s.as_str()).collect();
            v.sort_unstable();
            v.join(",")
        },
        sim_widgets: serde_json::to_string(&a.sim_widgets).unwrap_or_default(),
        lang_en: ui.global::<I18n>().get_en(),
        python_interpreter_path: a.python_interpreter.clone(),
        last_script_path: a.last_script_path.clone(),
        expr_vars: a.expr_vars.clone(),
        console_enabled: a.console_enabled,
        console_id: a.console_id.map(|x| x as i64).unwrap_or(-1),
        console_ch: a.console_ch as i32,
        renderer: ui.get_renderer_mode().to_string(),
        recent_project_paths: a.recent_project_paths.clone(),
    }
}

fn persist_settings(a: &mut App, ui: &AppWindow) {
    let result = settings::save(&gather_settings(a, ui));
    if let Err(error) = result {
        a.log(format!("保存最近工程失败: {error}"));
    }
}

fn persist_project_if_open(a: &mut App, ui: &AppWindow) {
    persist_settings(a, ui);
    let Some(path) = a.project_path.clone() else {
        return;
    };
    let project = Project {
        name: a.project_name.clone(),
        settings: gather_settings(a, ui),
        txs: a.txs.iter().map(TxTaskDto::from_task).collect(),
    };
    let worker = a.worker_tx.clone();
    let sim_revision = a.sim_revision;
    std::thread::spawn(move || {
        let result = serde_json::to_string_pretty(&project)
            .map_err(|error| format!("序列化工程失败: {error}"))
            .and_then(|text| {
                std::fs::write(&path, text).map_err(|error| format!("保存工程失败: {error}"))
            });
        let _ = worker.send(WorkerEvent::ProjectSaved {
            path,
            sim_revision,
            result,
        });
    });
}

fn commit_channel_edit(a: &mut App) -> Result<usize, String> {
    ensure_channel_edit_session(a);
    let session = a.channel_edit.as_ref().expect("channel edit session");
    can::validate_channel_set(&session.channels)?;
    a.channels = session.channels.clone();
    a.channel_sel = session
        .selected
        .clamp(0, a.channels.len() as i32 - 1)
        .max(0);
    if let Some(first) = a.channels.first().cloned() {
        a.baud = first.baud.clone();
        a.device_cfg = first;
    }
    if let Some(session) = a.channel_edit.as_mut() {
        session.channels = a.channels.clone();
        session.selected = a.channel_sel;
        session.dirty = false;
    }
    Ok(a.channels.len())
}

fn touch_recent_project(a: &mut App, path: &std::path::Path) {
    let path = path.to_string_lossy().to_string();
    a.recent_project_paths
        .retain(|item| !item.eq_ignore_ascii_case(&path));
    a.recent_project_paths.insert(0, path);
    a.recent_project_paths.truncate(12);
}

fn refresh_recent_projects(a: &App) {
    let rows = a
        .recent_project_paths
        .iter()
        .map(|path| {
            let file = std::path::Path::new(path);
            let available = file.is_file();
            let name = file
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(path);
            let modified = std::fs::metadata(file)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(|time| {
                    chrono::DateTime::<chrono::Local>::from(time)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            RecentProjectRow {
                name: name.into(),
                path: path.as_str().into(),
                modified: modified.into(),
                available,
            }
        })
        .collect::<Vec<_>>();
    sync_vec_model(&a.recent_project_model, rows);
}

fn apply_settings(a: &mut App, ui: &AppWindow, s: &settings::Settings) {
    if !s.channels.is_empty() {
        a.channels = s.channels.clone();
        a.channel_sel = s.channel_sel.clamp(0, a.channels.len() as i32 - 1).max(0);
    }
    a.python_interpreter = s.python_interpreter_path.clone();
    a.last_script_path = s.last_script_path.clone();
    a.expr_vars = s.expr_vars.clone();
    let rmode = if s.renderer.is_empty() {
        "auto".to_string()
    } else {
        s.renderer.clone()
    };
    ui.set_renderer_mode(rmode.into());
    recompute_expr_ids(a);

    a.console_enabled = s.console_enabled;
    a.console_id = if s.console_id < 0 {
        None
    } else {
        Some(s.console_id as u32)
    };
    a.console_ch = s.console_ch.clamp(0, 255) as u8;
    ui.set_console_enabled(a.console_enabled);
    ui.set_console_id(
        a.console_id
            .map(|x| format!("0x{x:X}"))
            .unwrap_or_default()
            .into(),
    );
    ui.set_console_ch(a.console_ch as i32);
    a.mode_trace = s.mode_trace;
    a.time_mode = s.time_mode;
    ui.set_time_mode(s.time_mode);
    a.cols_hidden = s
        .cols_hidden
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    apply_col_widths(ui, &a.cols_hidden);
    a.sim_tx_frames.clear();
    if !s.sim_widgets.trim().is_empty() {
        a.sim_widgets = serde_json::from_str(&s.sim_widgets).unwrap_or_default();
    }
    if s.trace_cap >= 1000 {
        a.trace_cap = s.trace_cap;
    }
    if s.chart_cap >= 500 {
        a.chart_cap = s.chart_cap;
    }

    let effective: Vec<String> = if !s.dbc_paths.is_empty() {
        s.dbc_paths.clone()
    } else {
        s.dbc_path.clone().into_iter().collect()
    };
    if !effective.is_empty() {
        a.dbcs.clear();
        a.dbc_paths.clear();
        a.expanded_signal_cache.clear();
        for dp in effective {
            match DbcDb::load(&dp) {
                Ok(db) => {
                    a.log(format!("加载 DBC: {}", db.file_name));
                    a.dbcs.push(db);
                    a.dbc_paths.push(dp);
                }
                Err(e) => a.log(format!("加载 DBC 失败 {dp}: {e}")),
            }
        }
        rebuild_dbc_snap(a);
    }
    a.filter = parse_filter(&s.f_id, &s.f_name, &s.f_data);
    a.filter.dir_filter = dir_idx_to_opt(s.dir_filter);
    ui.set_mode_trace(s.mode_trace);
    ui.set_f_id(s.f_id.clone().into());
    ui.set_f_name(s.f_name.clone().into());
    ui.set_f_data(s.f_data.clone().into());
    ui.set_dir_filter(s.dir_filter);
    if s.left_w > 80.0 {
        ui.set_left_w(s.left_w);
    }
    if s.bottom_h > 60.0 {
        ui.set_bottom_h(s.bottom_h);
    }
    refresh_and_reconcile_pcan(a);
}

fn dir_idx_to_opt(idx: i32) -> Option<bool> {
    match idx {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

fn parse_filter(id_s: &str, name_s: &str, data_s: &str) -> Filter {
    let mut f = Filter::default();

    for tok in id_s.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if let Some(rest) = tok.strip_prefix('!') {
            if let Some(v) = parse_u32(rest) {
                f.deny.push(v);
            }
        } else if let Some((a, b)) = tok.split_once('-') {
            if let (Some(a), Some(b)) = (parse_u32(a.trim()), parse_u32(b.trim())) {
                f.allow.push((a.min(b), a.max(b)));
            }
        } else if let Some(v) = parse_u32(tok) {
            f.allow.push((v, v));
        }
    }

    let n = name_s.trim();
    if !n.is_empty() {
        if let Some(rest) = n.strip_prefix('!') {
            f.name = Some(rest.to_string());
            f.name_exclude = true;
        } else if let Some(rest) = n.strip_suffix('*') {
            f.name = Some(rest.to_string());
            f.name_prefix = true;
        } else if let Some(rest) = n.strip_prefix('*') {
            f.name = Some(rest.to_string());
            f.name_suffix = true;
        } else {
            f.name = Some(n.to_string());
        }
    }

    let d = data_s.trim();
    if !d.is_empty() {
        let bytes: Vec<u8> = d
            .split_whitespace()
            .filter_map(|x| u8::from_str_radix(x.trim_start_matches("0x"), 16).ok())
            .collect();
        if !bytes.is_empty() {
            f.data = Some(bytes);
        }
    }
    f
}

fn parse_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        u32::from_str_radix(s, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_frame_validation_blocks_ambiguous_or_invalid_frames() {
        assert!(validate_ipc_tx_frame(0, 0x100, vec![], false, false, false, false).is_err());
        assert!(validate_ipc_tx_frame(1, 0x800, vec![], false, false, false, false).is_err());
        assert!(validate_ipc_tx_frame(1, 0x100, vec![0; 9], false, false, false, false).is_err());
        assert!(validate_ipc_tx_frame(1, 0x100, vec![0; 10], false, true, false, false).is_err());
        assert!(validate_ipc_tx_frame(1, 0x100, vec![], false, false, true, false).is_err());
        assert!(validate_ipc_tx_frame(1, 0x100, vec![0], false, false, false, true).is_err());
        assert!(validate_ipc_tx_frame(1, 0x18FF50E5, vec![0; 64], true, true, true, false).is_ok());
    }

    #[test]
    fn console_reassembles_printf_lines() {
        let mut c = ConsoleBuf::default();

        c.feed(b"Hello, ");
        c.feed(b"world!\r\n");
        c.feed(&[b'A', b'B', b'C', 0x0A, 0x00, 0x00]);

        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.lines[0], "Hello, world!");
        assert_eq!(c.lines[1], "ABC");

        c.feed(b"partial");
        let rows = c.rows();
        assert_eq!(rows.last().unwrap(), "partial");
        assert_eq!(rows.len(), 3);

        assert_eq!(c.export_text(), "Hello, world!\nABC\npartial");

        let mut c2 = ConsoleBuf::default();
        c2.feed("温度=25℃\n".as_bytes());
        assert_eq!(c2.lines[0], "温度=25℃");

        let mut c3 = ConsoleBuf::default();
        let fd_line = b"FD log #07 temp=25C volt=72.1 current=12.3 state=OK\n";
        assert!(fd_line.len() > 8 && fd_line.len() <= 64);
        c3.feed(fd_line);
        assert_eq!(c3.lines.len(), 1);
        assert_eq!(
            c3.lines[0],
            "FD log #07 temp=25C volt=72.1 current=12.3 state=OK"
        );

        let mut c4 = ConsoleBuf::default();
        for chunk in b"hello FD vs classic same line\n".chunks(8) {
            c4.feed(chunk);
        }
        assert_eq!(c4.lines[0], "hello FD vs classic same line");
        // clear
        c.clear();
        assert!(c.rows().is_empty());
    }

    #[test]
    fn project_roundtrip() {
        let proj = Project {
            name: "UnitTest".into(),
            settings: settings::Settings {
                mode_trace: true,
                dark: true,
                big: false,
                trace_cap: 200_000,
                chart_cap: 4096,
                dir_filter: 2,
                f_id: "0x100-0x200".into(),
                sim_widgets: serde_json::to_string(&vec![mkwidget(SimKind::Dial, GenMode::Sine)])
                    .expect("serialize simulation layout"),
                ..Default::default()
            },
            txs: vec![TxTaskDto {
                name: "t1".into(),
                ch: 1,
                id: 0x123,
                ext: false,
                fd: true,
                brs: true,
                remote: false,
                data: vec![0x11, 0x22, 0x33],
                periodic: true,
                period_ms: 100,
                repeat: -1,
                dbc_id: None,
                sig_values: vec![("rpm".into(), 1500.0)],
                varies: Vec::new(),
            }],
        };
        let json = serde_json::to_string_pretty(&proj).expect("serialize");
        let back: Project = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.name, "UnitTest");
        assert_eq!(back.settings.trace_cap, 200_000);
        assert!(back.settings.dark);
        assert!(!back.settings.big);
        assert_eq!(back.settings.dir_filter, 2);
        assert_eq!(back.settings.f_id, "0x100-0x200");
        let sim_layout: Vec<SimWidget> =
            serde_json::from_str(&back.settings.sim_widgets).expect("simulation layout");
        assert_eq!(sim_layout.len(), 1);
        assert!(sim_layout[0].kind == SimKind::Dial);
        assert_eq!(sim_layout[0].frame_id, 0x100);
        assert_eq!(back.txs.len(), 1);
        assert_eq!(back.txs[0].id, 0x123);
        assert_eq!(back.txs[0].data, vec![0x11, 0x22, 0x33]);
        assert_eq!(back.txs[0].sig_values, vec![("rpm".to_string(), 1500.0)]);

        let task = back.txs.into_iter().next().unwrap().into_task(7);
        assert!(!task.periodic);
        assert_eq!(task.handle, 7);
        assert_eq!(task.id, 0x123);
    }

    #[test]
    fn project_partial_load() {
        let json = r#"{ "txs": [] }"#;
        let back: Project = serde_json::from_str(json).expect("partial deserialize");
        assert!(back.txs.is_empty());

        assert!(back.settings.big);
    }

    fn mkwidget(kind: SimKind, mode: GenMode) -> SimWidget {
        SimWidget {
            kind,
            name: "w".into(),
            channel: 1,
            dbc_path: String::new(),
            frame_id: 0x100,
            frame_extended: false,
            frame_fd: false,
            frame_brs: false,
            frame_dlc: 8,
            frame_profile_explicit: true,
            signal: String::new(),
            threshold: 10.0,
            min: 0.0,
            max: 100.0,
            gen_mode: mode,
            gen_step: 7.0,
            period_ms: 100,
            x: 10.0,
            y: 20.0,
            w: 120.0,
            h: 80.0,
            enabled: true,
            slider_val: 5.0,
            press_val: 1.0,
            release_val: 0.0,
            align: 1,
            trace_signals: Vec::new(),
            trace_window_secs: 30,
            trace_auto_range: true,
            alarm_message: "信号值超出允许范围".into(),
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

    #[test]
    fn sim_gen_waveforms() {
        let mut c = mkwidget(SimKind::SignalGen, GenMode::Constant);
        c.min = 42.0;
        for t in 0..50 {
            c.tick = t;
            assert!((sim_gen_value(&c) - 42.0).abs() < 1e-9);
        }
        let mut r = mkwidget(SimKind::SignalGen, GenMode::Ramp);
        let mut s = mkwidget(SimKind::SignalGen, GenMode::Sine);
        for t in 0..1000u64 {
            r.tick = t;
            s.tick = t;
            let rv = sim_gen_value(&r);
            let sv = sim_gen_value(&s);
            assert!(
                (-1e-6..=100.0 + 1e-6).contains(&rv),
                "ramp out of range: {rv}"
            );
            assert!(
                (-1e-6..=100.0 + 1e-6).contains(&sv),
                "sine out of range: {sv}"
            );
        }
    }

    #[test]
    fn sim_widget_roundtrip() {
        let mut w = mkwidget(SimKind::Dial, GenMode::Sine);
        w.channel = 2;
        w.dbc_path = r"D:\dbc\powertrain.dbc".into();
        w.frame_extended = true;
        w.frame_fd = true;
        w.frame_brs = true;
        w.frame_dlc = 16;
        w.trace_signals = vec!["Voltage".into(), "Current".into()];
        w.trace_window_secs = 45;
        w.trace_auto_range = false;
        w.alarm_message = "Over temperature".into();
        w.image_path = r"D:\images\device.png".into();
        w.trace_history = vec![std::collections::VecDeque::from([1.0, 2.0])];
        w.trace_paused = true;
        w.group_values = vec![Some(1.0)];
        let json = serde_json::to_string(&vec![w]).expect("serialize");
        let back: Vec<SimWidget> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].channel, 2);
        assert_eq!(back[0].frame_id, 0x100);
        assert_eq!(back[0].gen_step, 7.0);
        assert_eq!(back[0].x, 10.0);
        assert_eq!(back[0].w, 120.0);
        assert_eq!(back[0].slider_val, 5.0);
        assert!(back[0].enabled);
        assert_eq!(back[0].dbc_path, r"D:\dbc\powertrain.dbc");
        assert!(back[0].frame_extended);
        assert!(back[0].frame_fd);
        assert!(back[0].frame_brs);
        assert_eq!(back[0].frame_dlc, 16);
        assert!(back[0].frame_profile_explicit);
        assert_eq!(back[0].trace_signals, ["Voltage", "Current"]);
        assert_eq!(back[0].trace_window_secs, 45);
        assert!(!back[0].trace_auto_range);
        assert_eq!(back[0].alarm_message, "Over temperature");
        assert_eq!(back[0].image_path, r"D:\images\device.png");
        assert!(back[0].trace_history.is_empty());
        assert!(!back[0].trace_paused);
        assert!(back[0].group_values.is_empty());
        assert!(back[0].image_cache_path.is_empty());
        assert!(!back[0].image_load_ok);

        assert_eq!(back[0].tick, 0);
        assert!(back[0].last_fire.is_none());
        assert!(!back[0].binding_error_reported);
        assert!(!back[0].switch_on);

        let mut legacy: serde_json::Value = serde_json::from_str(&json).unwrap();
        legacy[0].as_object_mut().unwrap().remove("dbc_path");
        legacy[0].as_object_mut().unwrap().remove("frame_extended");
        legacy[0].as_object_mut().unwrap().remove("frame_fd");
        legacy[0].as_object_mut().unwrap().remove("frame_brs");
        legacy[0].as_object_mut().unwrap().remove("frame_dlc");
        legacy[0]
            .as_object_mut()
            .unwrap()
            .remove("frame_profile_explicit");
        legacy[0].as_object_mut().unwrap().remove("trace_signals");
        legacy[0]
            .as_object_mut()
            .unwrap()
            .remove("trace_window_secs");
        legacy[0]
            .as_object_mut()
            .unwrap()
            .remove("trace_auto_range");
        legacy[0].as_object_mut().unwrap().remove("alarm_message");
        legacy[0].as_object_mut().unwrap().remove("image_path");
        let legacy_back: Vec<SimWidget> = serde_json::from_value(legacy).unwrap();
        assert!(legacy_back[0].dbc_path.is_empty());
        assert!(!legacy_back[0].frame_extended);
        assert!(!legacy_back[0].frame_fd);
        assert!(!legacy_back[0].frame_brs);
        assert_eq!(legacy_back[0].frame_dlc, 8);
        assert!(!legacy_back[0].frame_profile_explicit);
        assert!(legacy_back[0].trace_signals.is_empty());
        assert_eq!(legacy_back[0].trace_window_secs, 30);
        assert!(legacy_back[0].trace_auto_range);
        assert_eq!(legacy_back[0].alarm_message, "信号值超出允许范围");
    }

    #[test]
    fn sim_kind_mapping() {
        for k in 0..17 {
            assert_eq!(SimKind::from_i32(k).to_i32(), k);
        }
    }

    #[test]
    fn sim_trend_path_is_decimated_and_bounded() {
        let history = (0..1_000)
            .map(|value| value as f64 / 10.0)
            .collect::<std::collections::VecDeque<_>>();
        let path = sim::sim_trace_path(&history, 0.0, 100.0);
        assert!(path.starts_with("M 0.00 100.00"));
        assert!(path.matches(" L ").count() <= 120);
        assert!(path.contains("100.00 0.10") || path.contains("100.00 0.00"));
    }

    #[test]
    fn sim_multisignal_rows_expose_values_states_and_auto_range() {
        let mut bars = mkwidget(SimKind::BarChart, GenMode::Constant);
        bars.signal = "SOC".into();
        bars.trace_signals = vec!["Voltage".into(), "Current".into(), "Temp".into()];
        bars.group_values = vec![Some(80.0), Some(60.0), Some(20.0), Some(110.0)];
        bars.threshold = 50.0;
        let row = sim::sim_make_row(&bars, false, false);
        assert_eq!(row.series_label_1.as_str(), "SOC");
        assert_eq!(row.series_label_4.as_str(), "Temp");
        assert_eq!(row.series_value_1.as_str(), "80.00");
        assert!(row.series_on_1);
        assert!(!row.series_on_3);
        assert_eq!(row.series_level_4, 1.0);

        let mut trend = mkwidget(SimKind::Trend, GenMode::Constant);
        trend.signal = "Speed".into();
        trend.trace_history = vec![std::collections::VecDeque::from([20.0, 40.0])];
        let row = sim::sim_make_row(&trend, false, false);
        assert!(row.range_label.starts_with("A "));
        assert!(!row.trace_path_1.is_empty());
    }

    #[test]
    fn sim_alarm_card_changes_state_outside_limits() {
        let mut alarm = mkwidget(SimKind::Alarm, GenMode::Constant);
        alarm.min = -10.0;
        alarm.max = 60.0;
        alarm.cur = 61.0;
        alarm.alarm_message = "Over temperature".into();
        let row = sim::sim_make_row(&alarm, false, false);
        assert!(row.alarm);
        assert_eq!(row.alarm_message.as_str(), "Over temperature");
        alarm.cur = 25.0;
        assert!(!sim::sim_make_row(&alarm, false, false).alarm);
    }

    #[test]
    fn sim_new_controls_are_placed_without_overlap() {
        let mut first = mkwidget(SimKind::Trend, GenMode::Constant);
        first.x = 24.0;
        first.y = 24.0;
        first.w = 360.0;
        first.h = 210.0;
        let (x, y) = sim_find_free_position(&[first], 110.0, 190.0, 1100.0, 620.0);
        assert!(x >= 396.0 || y >= 246.0);
    }

    #[test]
    fn sim_widget_is_constrained_to_canvas_and_kind_minimum() {
        let mut w = mkwidget(SimKind::Switch, GenMode::Constant);
        w.x = 990.0;
        w.y = -40.0;
        w.w = 12.0;
        w.h = 8.0;
        constrain_sim_widget(&mut w, 800.0, 500.0);
        assert_eq!((w.w, w.h), SimKind::Switch.min_size());
        assert_eq!(w.y, 0.0);
        assert!(w.x + w.w <= 800.0);
        assert!(w.y + w.h <= 500.0);

        let mut input = mkwidget(SimKind::Input, GenMode::Constant);
        input.w = 40.0;
        input.h = 30.0;
        constrain_sim_widget(&mut input, 800.0, 500.0);
        assert_eq!((input.w, input.h), (92.0, 58.0));
    }

    #[test]
    fn sim_binding_decodes_dbc_signal() {
        let txt = "VERSION \"\"\nBO_ 256 New_Message_1: 8 ECU\n SG_ New_Signal_1 : 7|8@0+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_sim_bind.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = dbc::DbcDb::load(&p.to_string_lossy()).unwrap();
        let path = p.to_string_lossy().to_string();
        let dbcs = vec![db];
        let paths = vec![path.clone()];
        let frame = [42u8, 0, 0, 0, 0, 0, 0, 0];

        let v = sim_decode_value(&dbcs, &paths, &path, 0x100, "New_Signal_1", &frame);
        assert_eq!(v, Ok(Some(42.0)), "DBC 信号绑定应解出 byte0=42");
        assert_eq!(
            sim_decode_value(&dbcs, &paths, "", 0x100, "New_Signal_1", &frame),
            Ok(Some(42.0)),
            "旧工程未保存 DBC 路径时，唯一匹配可以安全迁移"
        );

        let v0 = sim_decode_value(&dbcs, &paths, "", 0x100, "", &frame);
        assert_eq!(v0, Ok(Some(42.0)));

        assert!(sim_decode_value(&dbcs, &paths, &path, 0x100, "Nope", &frame).is_err());

        assert!(sim_decode_value(&dbcs, &paths, &path, 0x222, "New_Signal_1", &frame).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sim_signal_library_applies_complete_dbc_binding() {
        let txt = "VERSION \"\"\nBO_ 256 BatteryStatus: 8 ECU\n SG_ PackVoltage : 7|8@0+ (2,0) [0|510] \"V\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_sim_library_bind.dbc");
        std::fs::write(&p, txt).unwrap();
        let path = p.to_string_lossy().to_string();
        let dbcs = vec![dbc::DbcDb::load(&path).unwrap()];
        let paths = vec![path.clone()];
        let mut widget = make_sim_widget(SimKind::Numeric, false, 1, 40.0, 50.0);
        let item = SimBindingTreeItem::Signal(path.clone(), 0x100, false, "PackVoltage".into());

        apply_sim_signal_binding(&dbcs, &paths, &mut widget, &item).unwrap();

        assert_eq!(widget.dbc_path, path);
        assert_eq!(widget.frame_id, 0x100);
        assert!(!widget.frame_extended);
        assert!(!widget.frame_fd);
        assert!(!widget.frame_brs);
        assert_eq!(widget.frame_dlc, 8);
        assert_eq!(widget.signal, "PackVoltage");
        assert_eq!((widget.min, widget.max), (0.0, 510.0));
        assert!(widget.frame_profile_explicit);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sim_signal_library_factory_keeps_drop_geometry_and_kind_defaults() {
        let widget = make_sim_widget(SimKind::Dial, true, 3, 123.0, 234.0);
        assert_eq!(widget.kind, SimKind::Dial);
        assert_eq!(widget.name, "Dial3");
        assert_eq!((widget.x, widget.y), (123.0, 234.0));
        assert_eq!((widget.w, widget.h), SimKind::Dial.default_size());
        assert!(widget.dbc_path.is_empty());
        assert!(widget.signal.is_empty());

        let (x, y) =
            sim::sim_find_free_position_from(&[], widget.w, widget.h, 1100.0, 620.0, 350.0);
        assert!(x >= 350.0, "信号库创建的控件不能被左侧面板遮挡");
        assert!(y >= 0.0);
    }

    #[test]
    fn sim_binding_uses_dbc_extended_flag_for_low_numeric_id() {
        let txt = "VERSION \"\"\nBO_ 2147483904 ExtendedLowId: 1 ECU\n SG_ Value : 7|8@0+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_sim_extended_low_id.dbc");
        std::fs::write(&p, txt).unwrap();
        let path = p.to_string_lossy().to_string();
        let dbcs = vec![dbc::DbcDb::load(&path).unwrap()];
        let paths = vec![path.clone()];
        assert_eq!(
            sim_decode_value(&dbcs, &paths, &path, 0x100, "Value", &[77]),
            Ok(Some(77.0))
        );
        assert_eq!(
            sim_binding_frame_profile(&dbcs, &paths, &path, 0x100, "Value"),
            Ok(sim::SimFrameProfile::new(true, false, false, 1)),
            "低数值 ID 仍必须继承 DBC 的扩展帧属性"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sim_binding_derives_can_fd_and_canonical_dlc() {
        let txt = "VERSION \"\"\nBO_ 512 FdMessage: 10 ECU\n SG_ Value : 7|8@0+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_sim_fd_profile.dbc");
        std::fs::write(&p, txt).unwrap();
        let path = p.to_string_lossy().to_string();
        let dbcs = vec![dbc::DbcDb::load(&path).unwrap()];
        let paths = vec![path.clone()];
        assert_eq!(
            sim_binding_frame_profile(&dbcs, &paths, &path, 0x200, "Value"),
            Ok(sim::SimFrameProfile::new(false, true, false, 12)),
            "10 字节 DBC 报文必须映射为 CAN FD 的合法 12 字节 DLC"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sim_binding_requires_explicit_dbc_when_definitions_overlap() {
        let p1 = std::env::temp_dir().join("pcanwork_sim_bind_a.dbc");
        let p2 = std::env::temp_dir().join("pcanwork_sim_bind_b.dbc");
        let a = "VERSION \"\"\nBO_ 256 Shared_Message: 8 ECU\n SG_ Shared_Signal : 7|8@0+ (1,0) [0|255] \"\" Vector__XXX\n";
        let b = "VERSION \"\"\nBO_ 256 Shared_Message: 8 ECU\n SG_ Shared_Signal : 7|8@0+ (2,0) [0|510] \"\" Vector__XXX\n";
        std::fs::write(&p1, a).unwrap();
        std::fs::write(&p2, b).unwrap();
        let paths = vec![
            p1.to_string_lossy().to_string(),
            p2.to_string_lossy().to_string(),
        ];
        let dbcs = vec![
            dbc::DbcDb::load(&paths[0]).unwrap(),
            dbc::DbcDb::load(&paths[1]).unwrap(),
        ];
        let frame = [42u8, 0, 0, 0, 0, 0, 0, 0];

        let ambiguous = sim_decode_value(&dbcs, &paths, "", 0x100, "Shared_Signal", &frame);
        assert!(
            ambiguous
                .as_ref()
                .is_err_and(|error| error.contains("必须明确选择 DBC")),
            "重复定义不能再按加载顺序选择: {ambiguous:?}"
        );
        let ambiguous_tx = sim_encode_value(&dbcs, &paths, "", 0x100, "Shared_Signal", 42.0);
        assert!(
            ambiguous_tx
                .as_ref()
                .is_err_and(|error| error.contains("必须明确选择 DBC")),
            "重复定义不能进入发送编码: {ambiguous_tx:?}"
        );

        assert_eq!(
            sim_decode_value(&dbcs, &paths, &paths[0], 0x100, "Shared_Signal", &frame),
            Ok(Some(42.0))
        );
        assert_eq!(
            sim_decode_value(&dbcs, &paths, &paths[1], 0x100, "Shared_Signal", &frame),
            Ok(Some(84.0))
        );
        let encoded_a =
            sim_encode_value(&dbcs, &paths, &paths[0], 0x100, "Shared_Signal", 42.0).unwrap();
        let encoded_b =
            sim_encode_value(&dbcs, &paths, &paths[1], 0x100, "Shared_Signal", 42.0).unwrap();
        assert_eq!(encoded_a[0], 42);
        assert_eq!(encoded_b[0], 21);

        let _ = std::fs::remove_file(&p1);
        let _ = std::fs::remove_file(&p2);
    }
}
