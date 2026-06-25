#![cfg_attr(windows, windows_subsystem = "windows")]

//! PcanWork — 参考 ZLG ZXDoc 风格的 CAN/CAN FD 报文分析工具（Slint + Rust）。
//!
//! 当前版本已打通的核心链路
//!   设备层(PCAN/虚拟回退) → 接收线程 → 100ms 批量刷新 → 报文表(Trace/Overwrite)
//!   → DBC 解码 → 信号解析面板 → 实时曲线；以及发送(单次/周期)、快速过滤、统计、日志。
mod blf;
mod can;
mod chart;
mod convert;
mod dbc;
mod expr;
mod ipc;
mod msg_table;
mod render;
mod settings;
mod sim;
mod tree;
mod tx;
mod vary;
#[cfg(windows)]
mod windows_dpi;

// Callback wiring is split per-window into the wire_*.rs files below, pulled in via
// include!() so they share this module's imports and private items.
include!("wire_main.rs");
include!("wire_dialogs.rs");
include!("wire_chart.rs");
include!("wire_tx.rs");
include!("wire_playback.rs");
include!("wire_sim.rs");
include!("wire_pyauto.rs");
use chart::{chart_full_range, refresh_chart};
use msg_table::build_msg_table;
use render::{build_signal_panel, build_stats, refresh_signal_picker};
use sim::{
    refresh_sim, sim_fill_props, sim_send, sim_set_row, sim_signal_choices, sim_signal_range,
    sim_tick,
};
#[cfg(test)]
use sim::{sim_decode_value, sim_gen_value};
use tx::{
    build_tx_dbc_page, populate_sig_panel, push_tx_list, selected_signal, set_signal_value,
    tx_list_sig, tx_task_from_form, ui_to_vary, update_tx_task,
};

use can::{CanFrame, Cmd, DeviceConfig, Evt};
use dbc::{DbcDb, Decoded, MessageDef};
use serde::{Deserialize, Serialize};
use slint::{Color, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Duration;

slint::include_modules!();

const TRACE_CAP: usize = 100_000; // 原始缓存上限
pub(crate) const DISPLAY_CAP: usize = 1500; // 单次刷新最多渲染行数（保证刷新轻量
const CHART_CAP: usize = 10_000; // 每信号曲线点数上
const LOG_CAP: usize = 500;

pub(crate) fn key_of(ch: u8, tx: bool, id: u32) -> u64 {
    ((ch as u64) << 40) | ((tx as u64) << 39) | (id as u64)
}

/// 单帧的显示记录（已算好衍生字段）
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
    pub(crate) sum_cycle: f64, // 周期累加(用于真均值 = sum_cycle/(count-1))
    ext: bool,
    pub(crate) byte_change_t: Vec<f64>, // 每字节最近一次变化的时间戳，用于 Overwrite 模式 500ms 高亮淡出
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
    /// Some(公式) = 表达式派生信号(用 sig_latest 实时求值采样); None = 普通 DBC 信号(id+signal)。
    pub(crate) expr: Option<String>,
}

/// 表达式派生信号定义(用户在"选择信号 → Expression Variables"里创建)。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExprVar {
    pub(crate) name: String,
    pub(crate) formula: String,
    #[serde(default)]
    pub(crate) unit: String,
}

const CONSOLE_LINE_CAP: usize = 5000; // 报文日志最多保留行数
const CONSOLE_PARTIAL_MAX: usize = 8192; // 单行未换行字节上限(超出强制断行,防膨胀)

/// 下位机(MCU)侧 printf-over-CAN 实现指南,显示在"报文日志 → 帮助"窗口里。
pub(crate) const CONSOLE_HELP: &str = r#"// ===== printf-over-CAN: 下位机(MCU)C 实现 =====
// 作用: 把固件 printf 的字符通过 CAN 数据域发出, 上位机 PcanWork 的
// [报文日志]面板按 ID 把字节重组成文本实时显示.
//
// 与上位机解码器的约定 (务必遵守) :
// 1) 数据域里放 ASCII 或 UTF-8 字符 (UTF-8 可发中文) .
// 2) 行尾用 '\n' (0x0A) 断行.
// 3) 不足整帧的字节用 0x00 填充 -- 上位机会自动跳过 0x00.
// 4) '\r' (0x0D) 上位机会忽略, 可发可不发.
// 5) CAN_LOG_ID 必须和[报文日志]面板里填的 ID 一致.
//
// 经典 CAN: 每帧最多 8 字节, 长行自动拆成多帧, 上位机按 ID 拼到 '\n' 为一行.
// CAN FD : 每帧最多 64 字节, 推荐一行装一帧, 发得更整, 更省带宽.

#include <stdint.h>
#include <stdio.h>

#define CAN_LOG_ID 0x7E0u // 经典 CAN 用的 ID (与上位机一致)
#define CAN_LOG_ID_FD 0x7E1u // CAN FD 用的 ID

// 你的 CAN 驱动: 发一帧. data=数据域, len=字节数(经典<=8, FD<=64),
// is_fd=1 用 CAN FD 发. 请按你的芯片 HAL 实现这个函数.
extern void board_can_send(uint32_t id, const uint8_t *data, uint8_t len, int is_fd);

// ---------- 方案 A: 经典 CAN (每帧 8 字节) ----------
static uint8_t s_buf[8];
static uint8_t s_len = 0;

static void can_log_flush(void)
{
 if (s_len == 0) return;
 while (s_len < 8) s_buf[s_len++] = 0x00; // 0x00 填充, 上位机跳过
 board_can_send(CAN_LOG_ID, s_buf, 8, 0);
 s_len = 0;
}

void can_log_putc(char c)
{
 s_buf[s_len++] = (uint8_t)c;
 if (c == '\n' || s_len == 8) can_log_flush(); // 换行或满 8 字节就发
}

// ---------- 方案 B: CAN FD (一帧最多 64 字节, 推荐) ----------
static uint8_t s_fd[64];
static uint8_t s_fdlen = 0;

// FD 的 DLC 只允许这些长度, >8 时要补齐到最近的合法值
static uint8_t fd_pad_len(uint8_t n)
{
 static const uint8_t L[] = {8,12,16,20,24,32,48,64};
 if (n <= 8) return n;
 for (unsigned i = 0; i < sizeof(L); i++) if (L[i] >= n) return L[i];
 return 64;
}

static void can_logfd_flush(void)
{
 if (s_fdlen == 0) return;
 uint8_t L = fd_pad_len(s_fdlen);
 while (s_fdlen < L) s_fd[s_fdlen++] = 0x00; // 补到合法 FD 长度
 board_can_send(CAN_LOG_ID_FD, s_fd, L, 1);
 s_fdlen = 0;
}

void can_logfd_putc(char c)
{
 s_fd[s_fdlen++] = (uint8_t)c;
 if (c == '\n' || s_fdlen == 64) can_logfd_flush();
}

// ---------- 把 printf 接到 CAN ----------
// GCC / newlib: 重定向 _write (printf 最终调它)
int _write(int fd, const char *buf, int n)
{
 (void)fd;
 for (int i = 0; i < n; i++) can_log_putc(buf[i]); // 经典; FD 改 can_logfd_putc
 return n;
}

// Keil MicroLIB / armcc: 改成重定向 fputc
// int fputc(int ch, FILE *f) { (void)f; can_log_putc((char)ch); return ch; }

// 之后正常用: printf("temp=%d volt=%d\n", t, v); 就会从 CAN 发出.
//
// 注意:
// * printf 较重, 别在中断里大量打印; 总线别打满 (每行限频) .
// * CAN 波特率要和上位机一致 (FD 还要数据域波特率一致) .
// * 经典 CAN 与 FD 不能在同一条总线混跑, 按你的总线选一种.
"#;

/// printf-over-CAN 控制台缓冲: 把 CAN 帧数据域里的 ASCII/UTF-8 字节重组成文本行。
/// 跳过 0x00(填充)和 0x0D(回车),遇 0x0A(换行)断行,每行用 UTF-8 lossy 解码
/// (兼容纯 ASCII 与下位机发的 UTF-8 中文)。
#[derive(Default)]
pub(crate) struct ConsoleBuf {
    pub(crate) lines: VecDeque<String>, // 已完成的行
    partial: Vec<u8>,                   // 当前未换行的行字节
}

impl ConsoleBuf {
    /// 喂入一帧数据域字节。
    pub(crate) fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match b {
                0x00 | 0x0D => {} // 填充 / 回车: 跳过
                0x0A => self.flush_line(),
                _ => {
                    self.partial.push(b);
                    if self.partial.len() >= CONSOLE_PARTIAL_MAX {
                        self.flush_line(); // 超长无换行: 强制断行
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
    }
    /// 显示用的行 = 已完成行 + 当前未完成行(非空时附在末尾)。
    pub(crate) fn rows(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lines.iter().cloned().collect();
        if !self.partial.is_empty() {
            v.push(String::from_utf8_lossy(&self.partial).into_owned());
        }
        v
    }
    /// 导出为纯文本(供保存到文件)。
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
    ExprVar(String), // 表达式派生信号(按名字)
}

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
    pub(crate) repeat: i64,                    // 发送次数：-1=无限循环；N>=1=发 N 次后自动停
    pub(crate) sent: u64,
    pub(crate) handle: u64,
    pub(crate) dbc_id: Option<u32>,           // 绑定的 DBC 报文（DBC 发送任务）
    pub(crate) sig_values: Vec<(String, f64)>, // 各信号当前物理值（编辑的实际值，作为变化基准）
    pub(crate) varies: Vec<SignalVary>,         // 每信号的实际值变化规则
}

/// 单个信号的实际值变化规则（信号变化发送）。未列出的信号视为「无变化」。
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SignalVary {
    pub(crate) signal: String,
    pub(crate) mode: vary::VaryMode,
}

/// 触发条件（命中后执行动作）
enum TrigCond {
    IdEquals(u32),
    ByteEquals { off: usize, val: u8 },
    ErrorFrame,
}

/// 触发动作
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
    last: Option<std::time::Instant>, // 节流，避免连续命中刷
// 与 SendFrame 动作使用：命中时发送的响应
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

// ======================= 仿真面板(画布式仪表盘) =======================

/// 仿真控件类型。RX 类读 a.last 实时显示; TX 类在运行模式下发帧
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) enum SimKind {
    Indicator, // 0 指示灯: 信号>阈值点亮
    Dial,      // 1 仪表盘: 指针式表盘, cur 映射到[min,max]
    Bar,       // 2 进度条: cur 映射到 [min,max]
    Numeric,   // 3 数值: 显示 cur
    Label,     // 4 文本标签: 显示 name
    Button,    // 5 按钮: 运行模式点击发帧(值=max)
    Slider,    // 6 滑块: 运行模式拖动设值并发帧
    SignalGen, // 7 信号发生器: 运行模式按周期发时变信号
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
        }
    }
    /// 新建时的默认尺寸(逻辑 px)
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
        }
    }
}

/// 信号发生器波形
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

/// 保存当前配置（失败静默）。
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SimWidget {
    pub(crate) kind: SimKind,
    pub(crate) name: String,
    #[serde(default = "default_chan")]
    pub(crate) channel: u8, // CAN 通道(1 起)
    pub(crate) frame_id: u32,
    pub(crate) signal: String, // DBC 信号名(空=用 byte0 原始字节)
    pub(crate) threshold: f64, // 指示灯阈
    pub(crate) min: f64,       // 量程下限 / 滑块下限
    pub(crate) max: f64,       // 量程上限 / 滑块上限
    pub(crate) gen_mode: GenMode,
    pub(crate) gen_step: f64,
    pub(crate) period_ms: u64,
    // 画布位置/尺寸(逻辑 px), 加 serde default 便于将来加字段时旧档容错
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
    pub(crate) slider_val: f64, // 滑块/按钮当前值(持久化)
    #[serde(default = "default_press")]
    pub(crate) press_val: f64, // 按钮按下发送
    #[serde(default)]
    pub(crate) release_val: f64, // 按钮释放发送
    #[serde(default = "default_align")]
    pub(crate) align: i32, // 文本对齐: 0左 1中 2右(标签/数值)
    // ---- 运行期(不序列化) ----
    #[serde(skip)]
    pub(crate) cur: f64, // RX 最新
    #[serde(skip)]
    pub(crate) tick: u64, // 发生器步进计
    #[serde(skip)]
    pub(crate) last_fire: Option<std::time::Instant>,
}

/// 发送任务的可序列化形态（用于发送列表保存/加载；handle/sent 重新生成）。
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
            periodic: false, // 加载后默认停发，避免一加载就开始周期发
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

/// 工程文件（.zcp）：把当前配置 + 发送列表打包为一个可保存/打开的工程。
#[derive(Serialize, Deserialize)]
struct Project {
    #[serde(default)]
    settings: settings::Settings,
    #[serde(default)]
    txs: Vec<TxTaskDto>,
}

#[derive(Default)]
pub(crate) struct Filter {
    pub(crate) allow: Vec<(u32, u32)>, // ID 允许范围
    pub(crate) deny: Vec<u32>,         // ID 排除
    pub(crate) name: Option<String>,
    pub(crate) name_exclude: bool,
    pub(crate) name_prefix: bool,
    pub(crate) name_suffix: bool,
    pub(crate) data: Option<Vec<u8>>,
    pub(crate) dir_filter: Option<bool>, // None=全部, Some(true)=仅Tx, Some(false)=仅Rx
}

impl Filter {
    pub(crate) fn accept(&self, id: u32, name: &str, data: &[u8], tx: bool) -> bool {
        if let Some(d) = self.dir_filter
            && tx != d {
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
            && !seq.is_empty() && !data.windows(seq.len()).any(|w| w == seq.as_slice()) {
                return false;
            }
        true
    }
}

pub(crate) struct App {
    pub(crate) cmd: Sender<Cmd>,
    pub(crate) dbcs: Vec<DbcDb>,
    pub(crate) mode_trace: bool,
    pub(crate) time_mode: i32,       // 时间列显示 0=相对(秒) 1=绝对(时分秒) 2=系统(含日期)
    pub(crate) capture_wall_epoch: Option<f64>, // 采集起点对应的墙钟 unix 秒(首帧时 now-f.t 推得)
    cols_hidden: std::collections::HashSet<String>, // 隐藏的报文表列(key)
    pub(crate) sim_widgets: Vec<SimWidget>,          // 仿真面板控件
    pub(crate) sim_model: Rc<VecModel<SimRow>>,      // 仿真面板常驻模型
    pub(crate) sim_sel: i32,                         // 主选中控件(-1=无), 驱动属性窗口
    pub(crate) sim_multi: std::collections::HashSet<i32>, // 多选集合(用于对齐)
    pub(crate) sim_running: bool,                    // 仿真运行(锁定)模式
    pub(crate) paused: bool,
    autoscroll: bool,
    recording: bool,
    pub(crate) connected: bool,
    pub(crate) conn_name: String, // 实际连接的后端设备名
    pub(crate) running: bool,
    pub(crate) baud: String,
    device_cfg: DeviceConfig,
    pub(crate) channels: Vec<DeviceConfig>, // 多通道配置列表
    channel_sel: i32,            // 通道管理器当前选中的通道
    rec_file: Option<std::io::BufWriter<std::fs::File>>,
    rec_fmt: RecFmt,
    rec_blf_buf: Vec<CanFrame>,
    rec_path: Option<std::path::PathBuf>,
    pub(crate) sig_log: Option<std::io::BufWriter<std::fs::File>>, // 信号记录器(流式 CSV)
    trigger: Option<Trigger>,                           // 触发规则(条件→动作)
    dbc_paths: Vec<String>,

    pub(crate) trace: VecDeque<FrameRec>,
    pub(crate) no_counter: u64,
    pub(crate) last: HashMap<u64, LastInfo>,
    pub(crate) last_dirty: bool, // last 变化标记：仅变化时才重建 IPC 快照(避免每拍深拷贝整表)

    pub(crate) rx: u64,
    pub(crate) tx: u64,
    pub(crate) err: u64,

    pub(crate) series: Vec<Series>,
    // ---- 表达式派生信号 ----
    pub(crate) expr_vars: Vec<ExprVar>,              // 用户定义的派生信号(名/公式/单位)
    pub(crate) sig_latest: HashMap<String, f64>,     // 信号名 -> 最新物理值(供表达式求值)
    pub(crate) expr_decode_ids: HashSet<u32>,        // 被任一表达式引用到的报文 id(只对这些帧解码更新 sig_latest)
    pub(crate) sig_cat: i32,                          // 选择信号对话框左侧分类: 0=CAN/DBC, 3=表达式
    pub(crate) signal_pick_expr_selected: Option<String>, // 表达式列表里选中的派生信号名
    // ---- CAN 报文日志 (printf-over-CAN 控制台) ----
    pub(crate) console_enabled: bool,                // 是否捕获
    pub(crate) console_id: Option<u32>,              // None=任意ID, Some=只收该ID
    pub(crate) console_ch: u8,                        // 0=任意通道, 否则只收该软件通道
    pub(crate) console: ConsoleBuf,                   // 文本重组缓冲
    pub(crate) selected_key: Option<u64>,
    selected_index: i32,
    pub(crate) sig_panel: Vec<(u32, String)>, // (msg id, signal name) 当前信号面板顺序，供“添加到曲线”定
dbc_signal_choices: Vec<(u32, String)>,

    filter: Filter,
    pub(crate) txs: Vec<TxTask>,
    pub(crate) tx_sel: i32,           // DBC 发送页选中的任务行
    pub(crate) tx_dbc_order: Vec<(u32, String)>, // DBC 报文选择列表的顺序（id, name
    pub(crate) tx_sig_cache: u64,     // 信号编辑器渲染签名，避免输入被每帧重建覆
    pub(crate) tx_msgs_cache: u64,    // DBC 报文选择列表渲染签名
    pub(crate) tx_list_cache: u64,    // 发送任务列表渲染签名，仅内容变化时重建
    pub(crate) tx_checked: HashSet<u64>, // 发送列表中被勾选的任务 handle（全选/上移/下移/删除用）
    tx_speed: f64,                    // 发送速度倍率：>1 更快(缩短间隔)，<1 更慢；默认 1.0
    chan_names_cache: u64,            // 通道名列表渲染签
    change_next: HashMap<u64, std::time::Instant>, // 信号变化任务的下次步进时间（按 handle）
    pub(crate) next_handle: u64,
    logs: VecDeque<String>,
    pub(crate) sort_col: i32,
    pub(crate) sort_desc: bool,
    pub(crate) display_items: Vec<DisplayItem>, // 当前显示顺序对应的行映射，含报文行和展开的信号行
    pub(crate) expanded_keys: HashSet<u64>,
    pub(crate) msg_model: Rc<VecModel<MsgRow>>, // 常驻报文模型，原地更新，避免每帧换模型丢点击
    pub(crate) chart_model: Rc<VecModel<ChartSeries>>, // 常驻曲线模型，原地更新，避免重建行元素打断游标拖
trace_cap: usize,                // 报文原始缓存上限（可配置
chart_cap: usize,                // 每信号曲线点数上限（可配置）
    pub(crate) tree_collapsed: HashSet<String>,
    pub(crate) tree_row_keys: Vec<String>,
    pub(crate) signal_pick_items: Vec<SignalPickItem>,
    pub(crate) signal_pick_selected: Option<(u32, String)>,
    pub(crate) signal_pick_msg_expanded: HashSet<u32>,
    pub(crate) signal_pick_root_open: bool,
    pub(crate) signal_pick_messages_open: bool,
    pub(crate) signal_pick_filter: String,
    pub(crate) chart_paused: bool,
    pub(crate) chart_normalize: bool,
    pub(crate) chart_cursor: bool,
    pub(crate) chart_dual: bool, // 双游
pub(crate) chart_view: Option<(f64, f64)>, // 时间轴可视窗口（None=自动全程
pub(crate) chart_pause_view: Option<(f64, f64)>, // 暂停时冻结的时间窗（None=未暂停或未快照）
    pub(crate) chart_frozen_series: Option<Vec<Series>>, // 暂停时的曲线数据快照（定格绘制，游标在其上测量）
    chart_highlight: Option<(String, std::time::Instant)>, // 高亮的曲线信号（双击工程树触发）
    pub(crate) tree_curve_sig: Vec<Option<String>>, // 每个树行对应的曲线信号名（仅曲线叶子有）
    pub(crate) last_tree_sig: u64,                  // 工程树渲染签名，避免每帧重建导致悬浮抖动
    pub(crate) lang_en: bool,                       // 界面语言镜像(true=英文)，供 Rust 端构建的文案(树/状态等)翻译
    // ---- Python 自动化测试（IPC 服务端 + 脚本运行器）----
    pub(crate) python_interpreter: String,          // 选定的 python.exe 路径（持久化）
    pub(crate) last_script_path: String,            // 上次运行脚本路径（持久化）
    pub(crate) py_child: Option<std::process::Child>, // 正在运行的脚本子进程（仅 UI 线程持有）
    pub(crate) py_out_rx: Option<std::sync::mpsc::Receiver<String>>, // 子进程 stdout/stderr 行
    pub(crate) py_started: Option<std::time::Instant>, // 本次运行起点（用于超时）
    pub(crate) py_stop_flag: bool,                  // 请求停止（tick 的 reap_child 执行 kill）
    pub(crate) run_status: String,                  // 运行状态文案（Running/PASS/FAIL...）
    pub(crate) py_output: String,                   // 脚本 stdout/stderr 汇总缓冲（回写运行器面板 + 落日志文件）
    pub(crate) py_dirty: bool,                      // 运行器窗口需刷新（输出/状态有变）
    pub(crate) py_timeout_secs: u64,                // 本次运行的看门狗超时（单脚本 120s；套件用大值兜底）
    pub(crate) ipc_snapshot: std::sync::Arc<std::sync::Mutex<ipc::Snapshot>>, // 只读快照（tick 每 100ms 重发布）
    pub(crate) ipc_subs: std::sync::Arc<ipc::SubRegistry>, // 客户端订阅注册表 + 单运行闸门
    pub(crate) ipc_handle_map: HashMap<(u64, u64), u64>, // (client_id, 客户端 handle) -> 内部周期 handle
    pub(crate) dbc_snap: std::sync::Arc<ipc::DbcSnapshot>, // 不可变 DBC 快照（DBC 变化时重建）
    pb_raw: Vec<CanFrame>, // 回放合并后的原始帧（未过滤）
    pb_files: Vec<(String, Vec<CanFrame>)>, // 各回放文件（名称, 帧）——用于多文件合并/切换合并方式时重组
    pb_pos: usize,
    pb_total: usize,
    pb_playing: bool,
    pub(crate) last_msg_sig: u64, // 报文表上次渲染的输入签名，用于跳过无变化的整表重
// 实时统计
    pub(crate) fps: f64,
    pub(crate) bus_load: f64, // 全局显示用：取最忙通道的负载（多卡时各总线独立，单一全局值无意义）
    win_start: std::time::Instant,
    win_frames: u64,
    win_bits: u64,
    pub(crate) chan_stats: std::collections::BTreeMap<u8, ChanCounters>, // 按软件通道号(CAN1=1..)分桶的统计
}

/// 单通道实时统计：rx/tx/err 累计 + 当前统计窗口的位数/帧数 + 算出的 bus_load/fps。
/// 每条物理总线一份——同一帧在某通道上要么本卡发(tx)要么收(rx)，不会在同一通道里重复计，
/// 故 per-channel 负载天然没有"同线两卡 tx+rx 翻倍"的问题。
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

/// 估算一帧在总线上占用的位数（含帧间隔，忽略位填充）
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

/// 把波特率串（如 "500K"/"1M"/"250000"）解析为 bit/s。
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

/// 录制文件格式
#[derive(Clone, Copy, PartialEq)]
enum RecFmt {
    Csv,
    Asc,
    Blf,
}

impl App {
    fn log(&mut self, msg: impl Into<String>) {
        self.logs.push_back(msg.into());
        while self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
    }

    // ---- 多 DBC 融合查询：按加载顺序遍历, 第一个命中的 DBC 生效 ----
    /// 解码: 返回首个能解出信号的 DBC 结果
pub(crate) fn dbc_decode(&self, id: u32, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let r = d.decode(id, data);
            if !r.is_empty() {
                return r;
            }
        }
        Vec::new()
    }
    /// 编码: 首个成功。
pub(crate) fn dbc_message_name(&self, id: u32) -> Option<&str> {
        self.dbcs.iter().find_map(|d| d.message_name(id))
    }
    /// 报文定义: 首个匹配
pub(crate) fn dbc_message(&self, id: u32) -> Option<&MessageDef> {
        self.dbcs.iter().find_map(|d| d.message(id))
    }
    /// 编码: 首个成功
pub(crate) fn dbc_encode(&self, id: u32, vals: &std::collections::HashMap<String, f64>) -> Option<Vec<u8>> {
        self.dbcs.iter().find_map(|d| d.encode(id, vals))
    }
    /// 是否加载了任何 DBC。
pub(crate) fn dbc_loaded(&self) -> bool {
        !self.dbcs.is_empty()
    }

    /// 已加载的 DBC 里是否存在名为 name 的信号(供表达式引用名校验)。
    pub(crate) fn dbc_has_signal(&self, name: &str) -> bool {
        self.dbcs
            .iter()
            .any(|d| d.messages().any(|m| m.signals.iter().any(|s| s.name == name)))
    }

    /// 触发动作：开始记录到 exe 同目录的 trigger_record.csv（无对话框）
fn trig_start_record(&mut self) {
        let path = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("trigger_record.csv")))
            .unwrap_or_else(|| std::path::PathBuf::from("trigger_record.csv"));
        match std::fs::File::create(&path) {
            Ok(file) => {
                let mut w = std::io::BufWriter::new(file);
                let _ = writeln!(w, "Time,Ch,Dir,ID,Len,Data");
                self.rec_fmt = RecFmt::Csv;
                self.rec_path = Some(path.clone());
                self.rec_file = Some(w);
                self.recording = true;
                self.log(format!("⚠ 触发开始记录: {}", path.display()));
            }
            Err(e) => self.log(format!("触发开始记录失败: {e}")),
        }
    }

    /// 触发动作：停止记录（与手动停止一致）
fn trig_stop_record(&mut self) {
        self.recording = false;
        self.rec_file = None;
        if self.rec_fmt == RecFmt::Blf
            && let Some(p) = self.rec_path.clone() {
                let buf = std::mem::take(&mut self.rec_blf_buf);
                let _ = blf::write(&p.to_string_lossy(), &buf);
            }
        self.log("⚠ 触发停止记录".to_string());
    }

    fn ingest(&mut self, f: CanFrame) {
        // 首帧记录墙钟起点(epoch = now - f.t), 供绝对/系统时间显示
        if self.capture_wall_epoch.is_none() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            self.capture_wall_epoch = Some(now - f.t);
        }
        // CAN 报文日志(printf-over-CAN): 把匹配帧(仅 RX)的数据域字节重组成文本
        if self.console_enabled
            && !f.tx
            && self.console_id.is_none_or(|id| id == f.id)
            && (self.console_ch == 0 || self.console_ch == f.ch)
        {
            self.console.feed(&f.data);
        }
        // 触发条件求值（节流 300ms），命中执行动作
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
        let key = key_of(f.ch, f.tx, f.id);
        let li = self.last.entry(key).or_insert(LastInfo {
            t: f.t,
            data: f.data.clone(),
            count: 0,
            min_cycle: f64::MAX,
            max_cycle: 0.0,
            sum_cycle: 0.0,
            ext: f.ext,
            byte_change_t: vec![f.t; f.data.len()],
        });
        let delta = (f.t - li.t).max(0.0);
        // 逐字节变
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
        // 实时统计窗口累计（全局）
        self.win_frames += 1;
        self.win_bits += bits;
        // 按通道分桶累计
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

        let name = self.dbc_message_name(f.id).unwrap_or("").to_string();

        // 记录文件（CSV / Vector ASC / BLF
if self.recording && self.rec_fmt == RecFmt::Blf {
            self.rec_blf_buf.push(f.clone());
        }
        if let Some(w) = self.rec_file.as_mut() {
            match self.rec_fmt {
                RecFmt::Blf => {}
                RecFmt::Csv => {
                    let _ = writeln!(
                        w,
                        "{:.6},CAN{},{},0x{:X},{},{}",
                        f.t,
                        f.ch,
                        if f.tx { "Tx" } else { "Rx" },
                        f.id,
                        f.data.len(),
                        f.data_hex()
                    );
                }
                RecFmt::Asc => {
                    // <t> <ch> <id>[x] Rx|Tx d <dlc> <bytes>
                    let idtxt = if f.ext {
                        format!("{:X}x", f.id)
                    } else {
                        format!("{:X}", f.id)
                    };
                    let _ = writeln!(
                        w,
                        "{:.6} {} {:<16}{}   d {} {}",
                        f.t,
                        f.ch,
                        idtxt,
                        if f.tx { "Tx" } else { "Rx" },
                        f.data.len(),
                        f.data_hex()
                    );
                }
            }
        }

        // 曲线采样
        let mut log_lines: Vec<String> = Vec::new();
        // 需要解码本帧吗? ① 有匹配此 id 的普通 DBC 系列, 或 ② 本 id 被某表达式引用
        let need_dbc_series =
            self.series.iter().any(|s| s.expr.is_none() && s.id == f.id);
        let need_expr =
            !self.expr_vars.is_empty() && self.expr_decode_ids.contains(&f.id);
        if self.dbc_loaded() && (need_dbc_series || need_expr) {
            let decoded = self.dbc_decode(f.id, &f.data);
            let logging = self.sig_log.is_some();
            // ① 普通 DBC 系列: 按 id 采样
            for s in self.series.iter_mut().filter(|s| s.expr.is_none() && s.id == f.id) {
                if let Some(dec) = decoded.iter().find(|x| x.name == s.signal) {
                    s.cur = dec.physical;
                    s.samples.push_back((f.t, dec.physical));
                    while s.samples.len() > self.chart_cap {
                        s.samples.pop_front();
                    }
                    if logging {
                        // 信号记录器：流式写盘，不受 chart_cap 限制
                        log_lines.push(format!("{:.6},{},{},{}", f.t, s.signal, dec.physical, s.unit));
                    }
                }
            }
            // ② 表达式系列: 更新最新信号值, 再对每个表达式系列实时求值采样
            if need_expr {
                for dec in &decoded {
                    self.sig_latest.insert(dec.name.clone(), dec.physical);
                }
                let cap = self.chart_cap;
                let t = f.t;
                // 先算(不可变借用 sig_latest), 再写回(可变借用 series), 避开借用冲突
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
        if !log_lines.is_empty()
            && let Some(w) = self.sig_log.as_mut() {
                for l in log_lines {
                    let _ = writeln!(w, "{l}");
                }
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

/// 报文表各列默认宽度(逻辑 px)。隐藏时改写 ColW.<key>=0。
const COL_DEFAULTS: &[(&str, f32)] = &[
    ("no", 52.0),
    ("time", 96.0),
    ("delta", 80.0),
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
    ("cycle", 72.0),
    ("count", 72.0),
    ("comment", 140.0),
];

/// 按隐藏集合改写 ColW 全局各列宽度(隐藏列=0)并重算 total。
fn apply_col_widths(ui: &AppWindow, hidden: &std::collections::HashSet<String>) {
    let cw = ui.global::<ColW>();
    let mut total = 0.0_f32;
    for (k, def) in COL_DEFAULTS {
        let w = if hidden.contains(*k) { 0.0 } else { *def };
        total += w;
        match *k {
            "no" => cw.set_no(w),
            "time" => cw.set_time(w),
            "delta" => cw.set_delta(w),
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

/// 发送任务的可序列化形态（用于发送列表保存/加载；handle/sent 重新生成）。
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

/// 信号发生器当前输出值（基于 tick 计数的无状态波形）。
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    windows_dpi::force_system_dpi_awareness();

    let ui = AppWindow::new()?;
    let chart_window = ChartWindow::new()?;
    let signal_window = SignalSelectWindow::new()?;
    let tx_window = TxWindow::new()?;
    let channel_window = ChannelConfigWindow::new()?;
    let playback_window = PlaybackWindow::new()?;
    let convert_window = ConvertWindow::new()?;
    let cache_window = CacheConfigWindow::new()?;
    let trigger_window = TriggerWindow::new()?;
    let sim_panel_window = SimPanelWindow::new()?;
    let sim_prop_window = SimPropWindow::new()?;
    let console_help_window = ConsoleHelpWindow::new()?;
    console_help_window.set_help_text(CONSOLE_HELP.into());
    let script_runner_window = ScriptRunnerWindow::new()?;
    let (cmd_tx, evt_rx) = can::spawn();

    // Python 自动化 IPC 服务端：绑定 127.0.0.1:0，App 构造前创建以便把 snapshot/registry 移入。
    let dbc_snap0 = std::sync::Arc::new(ipc::DbcSnapshot::empty());
    let ipc_snapshot = std::sync::Arc::new(std::sync::Mutex::new(ipc::Snapshot::new(dbc_snap0.clone())));
    let (ipc_port, ipc_token, ipc_req_rx, ipc_subs) = ipc::spawn_ipc_server(ipc_snapshot.clone());
    // 可选：把 IPC 端点写入文件，供 CI / 外部 REPL / pytest 附加（脚本运行器内的脚本走环境变量，无需此项）。
    if let Ok(info_path) = std::env::var("PCANWORK_IPC_INFO_FILE") {
        let _ = std::fs::write(&info_path, format!("{ipc_port}\n{ipc_token}\n"));
    }

    let app = Rc::new(std::cell::RefCell::new(App {
        cmd: cmd_tx.clone(),
        dbcs: Vec::new(),
        mode_trace: true,
        time_mode: 0,
        capture_wall_epoch: None,
        cols_hidden: std::collections::HashSet::new(),
        sim_widgets: Vec::new(),
        sim_model: Rc::new(VecModel::default()),
        sim_sel: -1,
        sim_multi: std::collections::HashSet::new(),
        sim_running: false,
        paused: false,
        autoscroll: true,
        recording: false,
        connected: false,
        conn_name: String::new(),
        running: false,
        baud: "500K".into(),
        device_cfg: DeviceConfig {
            sw_channel: 1,
            is_fd: false,
            device_type: "Virtual".into(),
            device_index: 0,
            channel_index: 0,
            baud: "500K".into(),
            data_baud: "2M".into(),
            termination: false,
            net_server: true,
            ip: String::new(),
            port: String::new(),
        },
        channels: vec![DeviceConfig {
            sw_channel: 1,
            is_fd: false,
            device_type: "Virtual".into(),
            device_index: 0,
            channel_index: 0,
            baud: "500K".into(),
            data_baud: "2M".into(),
            termination: false,
            net_server: true,
            ip: String::new(),
            port: String::new(),
        }],
        channel_sel: 0,
        rec_file: None,
        rec_fmt: RecFmt::Csv,
        rec_blf_buf: Vec::new(),
        rec_path: None,
        sig_log: None,
        trigger: None,
        dbc_paths: Vec::new(),
        trace: VecDeque::new(),
        no_counter: 0,
        last: HashMap::new(),
        last_dirty: true,
        rx: 0,
        tx: 0,
        err: 0,
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
        change_next: HashMap::new(),
        next_handle: 1,
        logs: VecDeque::new(),
        sort_col: -1,
        sort_desc: false,
        display_items: Vec::new(),
        expanded_keys: HashSet::new(),
        msg_model: Rc::new(VecModel::from(Vec::<MsgRow>::new())),
        chart_model: Rc::new(VecModel::from(Vec::<ChartSeries>::new())),
        trace_cap: TRACE_CAP,
        chart_cap: CHART_CAP,
        tree_collapsed: HashSet::new(),
        tree_row_keys: Vec::new(),
        signal_pick_items: Vec::new(),
        signal_pick_selected: None,
        signal_pick_msg_expanded: HashSet::new(),
        signal_pick_root_open: true,
        signal_pick_messages_open: true,
        signal_pick_filter: String::new(),
        chart_paused: false,
        chart_normalize: false,
        chart_cursor: false,
        chart_dual: false,
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
    app.borrow_mut()
        .log("PcanWork 启动。点击“连接设备”自动尝试 PCAN，无卡则回退虚拟总线。");
    // 报文表绑定常驻模型（只设一次，之后原地更新
ui.set_msgs(ModelRc::from(app.borrow().msg_model.clone()));
    // 曲线绑定常驻模型（原地更新，避免重建行元素打断游标拖动）
    ui.set_series(ModelRc::from(app.borrow().chart_model.clone()));
    chart_window.set_series(ModelRc::from(app.borrow().chart_model.clone()));
    // 仿真面板绑定常驻模型
    sim_panel_window.set_sim_widgets(ModelRc::from(app.borrow().sim_model.clone()));

    // ---- callback wiring (grouped by window; see wire_*.rs) ----
    wire_main(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window, &console_help_window);
    wire_dialogs(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window);
    wire_chart(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window);
    wire_tx(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window);
    wire_playback(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window);
    wire_sim(app.clone(), &ui, &chart_window, &signal_window, &tx_window, &channel_window, &playback_window, &convert_window, &cache_window, &trigger_window, &sim_panel_window, &sim_prop_window);
    wire_pyauto(app.clone(), &ui, &script_runner_window, ipc_port, ipc_token.clone());

    // ---------------- 启动即自动连接 + 接收 + 加载示例 DBC ----------------
    {
        let mut a = app.borrow_mut();
        // 自动加载 DBC：先找 sample.dbc(cwd→exe 目录)，找不到再取目录里第一个 *.dbc
        let first_dbc_in = |dir: &std::path::Path| -> Option<std::path::PathBuf> {
            let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .ok()?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension()
                        .map(|x| x.eq_ignore_ascii_case("dbc"))
                        .unwrap_or(false)
                })
                .collect();
            found.sort();
            found.into_iter().next()
        };
        let mut dbc_path = std::path::PathBuf::from("sample.dbc");
        if !dbc_path.exists() {
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.to_path_buf()));
            if let Some(p) = exe_dir.as_ref().map(|d| d.join("sample.dbc")).filter(|p| p.exists()) {
                dbc_path = p;
            } else if let Some(p) = first_dbc_in(std::path::Path::new(".")) {
                dbc_path = p;
            } else if let Some(p) = exe_dir.as_ref().and_then(|d| first_dbc_in(d)) {
                dbc_path = p;
            }
        }
        if dbc_path.exists() {
            let p = dbc_path.to_string_lossy().to_string();
            match DbcDb::load(&p) {
                Ok(db) => {
                    a.log(format!("自动加载示例 DBC: {}", db.file_name));
                    a.dbcs.push(db);
                    a.dbc_paths.push(p);
                }
                Err(e) => a.log(format!("自动加载 sample.dbc 失败: {e}")),
            }
        }
        rebuild_dbc_snap(&mut a);
        // 启动后不自动连接/接收，等用户点「连接」「启动」再开始（避免一打开就跑模拟数据
a.log("就绪。点击「连接」选择设备，再点「启动」开始接收。".to_string());

        // ---- 恢复上次配置（通道/主题/缓存/过滤/布局/DBC）----
        if let Some(s) = settings::load() {
            apply_settings(&mut a, &ui, &s);
            // 主题应用到所有窗
for t in [
                ui.global::<Theme>(),
                chart_window.global::<Theme>(),
                tx_window.global::<Theme>(),
                signal_window.global::<Theme>(),
                channel_window.global::<Theme>(),
                playback_window.global::<Theme>(),
                convert_window.global::<Theme>(),
                cache_window.global::<Theme>(),
                trigger_window.global::<Theme>(),
                sim_panel_window.global::<Theme>(),
                sim_prop_window.global::<Theme>(),
                script_runner_window.global::<Theme>(),
                console_help_window.global::<Theme>(),
            ] {
                t.set_dark(s.dark);
                t.set_big(s.big);
            }
            // 恢复界面语言
            for g in [
                ui.global::<I18n>(),
                chart_window.global::<I18n>(),
                tx_window.global::<I18n>(),
                signal_window.global::<I18n>(),
                channel_window.global::<I18n>(),
                playback_window.global::<I18n>(),
                convert_window.global::<I18n>(),
                cache_window.global::<I18n>(),
                trigger_window.global::<I18n>(),
                sim_panel_window.global::<I18n>(),
                sim_prop_window.global::<I18n>(),
                script_runner_window.global::<I18n>(),
                console_help_window.global::<I18n>(),
            ] {
                g.set_en(s.lang_en);
            }
            a.lang_en = s.lang_en; // 镜像到 Rust 端，供工程树等翻译
            a.log("已恢复上次配置".to_string());
        }
    }

    // ---------------- 100ms 批量刷新 ----------------
    let timer = Timer::default();
    {
        let app = app.clone();
        let uiw = ui.as_weak();
        let chartw = chart_window.as_weak();
        let signalw = signal_window.as_weak();
        let txw = tx_window.as_weak();
        let rw = script_runner_window.as_weak();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            // Python 自动化 IPC：在任何窗口 upgrade（关闭子窗口会 return）之前先处理，
            // 保证脚本调用不被"关闭了曲线/信号/发送窗口"卡死。仅触碰 a + 通道，不碰窗口。
            {
                let mut a = app.borrow_mut();
                while let Ok(ureq) = ipc_req_rx.try_recv() {
                    handle_ipc(&mut a, ureq);
                }
                reap_child(&mut a);
                drain_py_output(&mut a);
                publish_snapshot(&mut a);
                // 把脚本输出/状态回写到运行器窗口（与主窗口解耦：运行器关了就跳过）。
                if a.py_dirty {
                    // 日志文件只在脚本结束后写一次(运行中靠面板看实时输出，避免每拍全量重写)。
                    if a.py_child.is_none() {
                        let _ = std::fs::write(run_log_path(), &a.py_output);
                    }
                    if let Some(w) = rw.upgrade() {
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
            let chart_window = match chartw.upgrade() {
                Some(w) => w,
                None => return,
            };
            let signal_window = match signalw.upgrade() {
                Some(w) => w,
                None => return,
            };
            let tx_window = match txw.upgrade() {
                Some(w) => w,
                None => return,
            };
            let mut a = app.borrow_mut();

            // 1) 收事
while let Ok(evt) = evt_rx.try_recv() {
                match evt {
                    Evt::Frame(f) => {
                        ipc_fanout(&a, &f);
                        a.ingest(f);
                    }
                    Evt::Log(s) => a.log(s),
                    Evt::Connected(c, name) => {
                        a.connected = c;
                        if c && !name.is_empty() {
                            a.conn_name = name.clone();
                            a.log(format!("后端: {name}"));
                        } else if !c {
                            a.conn_name.clear();
                        }
                    }
                    Evt::Running(r) => a.running = r,
                    Evt::Playback(pos, total, playing) => {
                        a.pb_pos = pos;
                        a.pb_total = total;
                        a.pb_playing = playing;
                    }
                    Evt::PeriodicDone(handle) => {
                        // 后端周期任务发够次数自动停止：同步 UI 状态
                        if let Some(t) = a.txs.iter_mut().find(|t| t.handle == handle) {
                            t.periodic = false;
                            a.tx_list_cache = u64::MAX; // 强制刷新列表
                        }
                    }
                }
            }

            // 每约 1 秒计算一次 FPS / 总线负载（全局 + 按通道）
            let dt = a.win_start.elapsed().as_secs_f64();
            if dt >= 1.0 {
                a.fps = a.win_frames as f64 / dt;
                let default_bps = baud_bps(&a.device_cfg.baud);
                // 各通道按各自配置的波特率算负载（CAN1/CAN2 可能是不同总线、不同速率）
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
                a.bus_load = max_load; // 全局显示=最忙通道
                a.win_frames = 0;
                a.win_bits = 0;
                a.win_start = std::time::Instant::now();
            }

            // 仿真面板步进（TX 发帧 / RX 读值）+ 刷新显示
            // (信号变化 vary 任务改由下方独立 5ms 高频定时器驱动, 不再受 100ms 刷新限速)
            sim_tick(&mut a);
            refresh_sim(&a);

            refresh_ui(&mut a, &ui, &chart_window, &signal_window, &tx_window);
        });
    }

    // 信号变化(vary)任务的高频驱动：独立 5ms 定时器，不受 100ms UI 刷新限速。
    // 配合 step_signal_changes 的追帧逻辑，10ms/20ms/50ms 等周期能按设定速率发送。
    let change_timer = Timer::default();
    {
        let app = app.clone();
        change_timer.start(TimerMode::Repeated, Duration::from_millis(5), move || {
            let mut a = app.borrow_mut();
            step_signal_changes(&mut a);
        });
    }

    // 独立刷新定时器（不动 codex 的主刷新
let pb_timer = Timer::default();
    {
        let app = app.clone();
        let pw = playback_window.as_weak();
        pb_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(w) = pw.upgrade() else { return };
            let a = app.borrow();
            let en = a.lang_en;
            w.set_pos(a.pb_pos.to_string().into());
            w.set_total(a.pb_total.to_string().into());
            w.set_playing(a.pb_playing);
            w.set_status(
                if a.pb_total == 0 {
                    if en { "No file loaded" } else { "未载入文件" }
                } else if a.pb_playing {
                    if en { "Playing" } else { "回放中" }
                } else if a.pb_pos >= a.pb_total {
                    if en { "Done" } else { "回放完成" }
                } else {
                    if en { "Ready / Paused" } else { "就绪/已暂停" }
                }
                .into(),
            );
        });
    }

    // 标题栏锁品牌深蓝(与 modbus/serial 统一), 低频定时器兼顾后开的子窗口
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

    ui.run()?;
    Ok(())
}

/// 把本进程所有可见顶层窗口的标题栏锁成品牌深蓝 #11406f + 白字(Win11),
/// 不再跟随系统强调色, 三个工具外观统一。
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
                DwmSetWindowAttribute(h, DWMWA_CAPTION_COLOR, &caption as *const _ as *const c_void, 4);
                DwmSetWindowAttribute(h, DWMWA_TEXT_COLOR, &text as *const _ as *const c_void, 4);
            }
        }
        1
    }
    unsafe {
        EnumWindows(cb, 0);
    }
}

/// 把 a.pb_files 按当前合并方式重组进 a.pb_raw，更新文件名/通道显示，再构建下发。
/// concat=false 按各自时间戳交错；concat=true 顺序拼接（后文件接到前文件末尾）。
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
            cursor += (fmax - fmin) + 0.001; // 文件之间留 1ms 间隙
        }
    } else {
        for (_, fr) in &a.pb_files {
            out.extend(fr.iter().cloned());
        }
    }
    out.sort_by(|x, y| x.t.partial_cmp(&y.t).unwrap_or(std::cmp::Ordering::Equal));
    a.pb_raw = out;

    // 文件名显示：单文件显名，多文件显个数+名字
    let en = a.lang_en;
    let total = a.pb_raw.len();
    let names: Vec<String> = a.pb_files.iter().map(|(n, _)| n.clone()).collect();
    let fname = match names.len() {
        0 => if en { "(no file selected)".to_string() } else { "(未选择文件)".to_string() },
        1 => if en { format!("{} ({total} frames)", names[0]) } else { format!("{} ({total} 帧)", names[0]) },
        n => if en { format!("{n} files: {} ({total} frames)", names.join(", ")) } else { format!("{n} 个文件: {} ({total} 帧)", names.join(", ")) },
    };
    w.set_file_name(fname.into());

    // 通道并集
    let mut chans: Vec<u8> = a.pb_raw.iter().map(|f| f.ch).collect();
    chans.sort_unstable();
    chans.dedup();
    let ctxt = if chans.is_empty() {
        "-".to_string()
    } else {
        chans.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
    };
    w.set_src_channels(ctxt.into());

    // 文件列表模型（可展开列表用）
    let rows: Vec<PbFileRow> = a
        .pb_files
        .iter()
        .map(|(n, fr)| PbFileRow {
            name: n.clone().into(),
            count: if en { format!("{} frames", fr.len()) } else { format!("{} 帧", fr.len()) }.into(),
        })
        .collect();
    w.set_pb_files(ModelRc::from(Rc::new(VecModel::from(rows))));

    pb_build_and_load(a, w);
}

/// 按当前过滤/时间段设置，从原始帧构建回放序列并下发（通道统一映射到 CAN1）。
fn pb_build_and_load(a: &App, w: &PlaybackWindow) {
    let lo = parse_hex_u32(&w.get_id_lo()).unwrap_or(0);
    let hi = parse_hex_u32(&w.get_id_hi()).unwrap_or(u32::MAX);
    let ss = w.get_seg_start().to_string().trim().parse::<f64>().unwrap_or(f64::MIN);
    let se = w.get_seg_end().to_string().trim().parse::<f64>().unwrap_or(f64::MAX);
    let map = parse_channel_map(&w.get_channel_map());
    let frames: Vec<CanFrame> = a
        .pb_raw
        .iter()
        .filter(|f| f.id >= lo && f.id <= hi && f.t >= ss && f.t <= se)
        .filter_map(|f| {
            // 通道映射：源→目标；目标 0 = 忽略；未列出则保持原通道
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

/// 解析通道映射串 "src:dst,src:dst"。
fn parse_channel_map(s: &slint::SharedString) -> std::collections::HashMap<u8, u8> {
    let mut m = std::collections::HashMap::new();
    for tok in s.as_str().split(',') {
        let tok = tok.trim();
        if let Some((a, b)) = tok.split_once(':')
            && let (Ok(src), Ok(dst)) = (a.trim().parse::<u8>(), b.trim().parse::<u8>()) {
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

/// Cheap deterministic pseudo-random fraction in [0,1) from a seed (splitmix64-style).
fn rand01(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / ((1u64 << 53) as f64)
}

/// Signal-variation send: driven by the app each 100ms tick. For every due task with a
/// variation rule, recompute each varying signal's value at the current send index n,
/// re-encode the DBC frame, send it, and schedule the next step.
fn step_signal_changes(a: &mut App) {
    let now = std::time::Instant::now();
    let idxs: Vec<usize> = (0..a.txs.len())
        .filter(|&i| a.txs[i].periodic && has_vary(&a.txs[i]))
        .collect();
    for i in idxs {
        let handle = a.txs[i].handle;
        // 追帧：自"应发时刻"起欠几帧补几帧。高频定时器驱动 + 这里按调度时刻
        // (prev+period, 不是 now+period)推进，使平均速率严格等于设定周期、不漂移、
        // 也不再被 UI 刷新间隔限速。单次 tick 上限 64 帧，防 app 卡顿后积压暴发刷爆总线。
        let mut guard = 0u32;
        while a.change_next.get(&handle).is_some_and(|x| now >= *x) {
            if guard >= 64 {
                // 积压过多：重新对齐到当前，丢弃积压
                let period = eff_period(a.txs[i].period_ms, a.tx_speed);
                a.change_next
                    .insert(handle, now + std::time::Duration::from_millis(period));
                break;
            }
            guard += 1;

            let id = a.txs[i].id;
            let n = a.txs[i].sent;
            // 重算每个带变化规则(非 None)的信号，按发送序号 n
            let updates: Vec<(String, f64)> = {
                let t = &a.txs[i];
                t.varies
                    .iter()
                    .filter(|v| !matches!(v.mode, vary::VaryMode::None))
                    .map(|v| {
                        let base = t
                            .sig_values
                            .iter()
                            .find(|(s, _)| s == &v.signal)
                            .map(|(_, x)| *x)
                            .unwrap_or(0.0);
                        let mut seed = handle ^ n.wrapping_mul(0x0100_0001);
                        for b in v.signal.bytes() {
                            seed = seed.wrapping_mul(31).wrapping_add(b as u64);
                        }
                        (v.signal.clone(), vary::eval(&v.mode, n, base, rand01(seed)))
                    })
                    .collect()
            };
            {
                let t = &mut a.txs[i];
                for (sig, val) in updates {
                    if let Some(e) = t.sig_values.iter_mut().find(|(s, _)| s == &sig) {
                        e.1 = val;
                    } else {
                        t.sig_values.push((sig, val));
                    }
                }
            }
            let map: std::collections::HashMap<String, f64> =
                a.txs[i].sig_values.iter().cloned().collect();
            if let Some(data) = a.dbc_encode(id, &map) {
                a.txs[i].data = data;
                let f = tx_frame(&a.txs[i]);
                let _ = a.cmd.send(Cmd::SendOnce(f));
            }
            a.txs[i].sent += 1; // n advances each send

            // 发送次数限制：repeat>=1 时发够即自动停止（-1=无限循环）
            let rep = a.txs[i].repeat;
            if rep >= 1 && a.txs[i].sent >= rep as u64 {
                a.txs[i].periodic = false;
                a.change_next.remove(&handle);
                a.tx_list_cache = u64::MAX; // 强制刷新列表状态
                break;
            }
            // 下次应发 = 本次应发时刻 + period（从调度时刻推进，不漂移）
            let period = eff_period(a.txs[i].period_ms, a.tx_speed);
            let prev = a.change_next.get(&handle).copied().unwrap_or(now);
            a.change_next
                .insert(handle, prev + std::time::Duration::from_millis(period));
        }
    }
}

/// 按发送速度倍率换算实际间隔：speed>1 缩短间隔(更快)，speed<1 拉长(更慢)。
fn eff_period(period_ms: u64, speed: f64) -> u64 {
    if speed <= 0.0 {
        return period_ms.max(1);
    }
    ((period_ms as f64 / speed).round() as u64).max(1)
}

/// 切换任务的周期/启动状态后调用：信号变化任务由 app 驱动，其它走控制线程周期。
fn toggle_task_periodic(a: &mut App, idx: usize) {
    let is_change = has_vary(&a.txs[idx]);
    let on = a.txs[idx].periodic;
    let handle = a.txs[idx].handle;
    if on {
        a.txs[idx].sent = 0; // 每次启动从头计数（同时让信号变化从 n=0 起算）
    }
    if is_change {
        if on {
            a.change_next.insert(handle, std::time::Instant::now());
        } else {
            a.change_next.remove(&handle);
        }
    } else {
        let speed = a.tx_speed;
        let t = &a.txs[idx];
        let _ = a.cmd.send(Cmd::SetPeriodic {
            handle: t.handle,
            frame: tx_frame(t),
            period_ms: eff_period(t.period_ms, speed),
            repeat: t.repeat,
            enable: t.periodic,
        });
    }
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

// ---- 报文行动作：右键菜单(ctx_*) 与 选中条(sel_*) 共用，仅"取键方式"不同 ----
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
        let h = a.next_handle;
        a.next_handle += 1;
        let n = a.txs.len() + 1;
        a.txs.push(TxTask {
            name: format!("Tx_{n}"),
            ch: 1,
            id,
            ext,
            fd: false,
            brs: false,
            remote: false,
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
    if let Some(li) = a.last.get(&k) {
        let id = (k & 0xFFFF_FFFF) as u32;
        let f = CanFrame {
            t: 0.0,
            ch: 1,
            tx: true,
            id,
            ext: li.ext,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data: li.data.clone(),
        };
        let _ = a.cmd.send(Cmd::SendOnce(f));
        a.log(format!("立即发送 0x{id:X}"));
    }
}
fn act_add_all_signals(a: &mut App, k: u64) {
    let id = (k & 0xFFFF_FFFF) as u32;
    let data = a.last.get(&k).map(|li| li.data.clone()).unwrap_or_default();
    if !a.dbc_loaded() {
        a.log("未加载 DBC，无法添加信号".to_string());
        return;
    }
    let decoded = a.dbc_decode(id, &data);
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

/// 把一个表达式派生信号加到曲线。重名直接提示。
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

/// 重算"被任一表达式引用到的报文 id"集合(表达式或 DBC 变化时调用)。
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
    data.truncate(max_len);
    data
}

/// 从通道名（如 "CAN1"）解析软件通道号。
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
        device_index: 0,
        channel_index: 0,
        baud: "500K".into(),
        data_baud: "2M".into(),
        termination: false,
        net_server: true,
        ip: "192.168.0.178".into(),
        port: "8000".into(),
    }
}

/// 重排软件通道号为 1..N
fn renumber_channels(a: &mut App) {
    for (i, c) in a.channels.iter_mut().enumerate() {
        c.sw_channel = (i + 1) as u8;
    }
}

/// 把通道配置写入通道配置窗口的表单字段
fn set_chan_form(w: &ChannelConfigWindow, c: &DeviceConfig) {
    w.set_is_fd(c.is_fd);
    w.set_device_type(c.device_type.clone().into());
    w.set_device_index(c.device_index.to_string().into());
    w.set_channel_index(c.channel_index.to_string().into());
    w.set_baud(c.baud.clone().into());
    w.set_data_baud(c.data_baud.clone().into());
    w.set_termination(c.termination);
    w.set_net_server(c.net_server);
    w.set_ip(c.ip.clone().into());
    w.set_port(c.port.clone().into());
}

/// 通道列表显示字符串
fn chan_list_strings(a: &App) -> Vec<SharedString> {
    a.channels
        .iter()
        .map(|c| {
            format!(
                "CAN{}  {}  {}  {}",
                c.sw_channel,
                c.device_type,
                if c.is_fd { "CANFD" } else { "CAN" },
                c.baud
            )
            .into()
        })
        .collect()
}

/// 每帧刷新各窗口属性与模型：状态栏、报文表、信号面板、曲线、发送列表、统计、工程树、日志。
fn refresh_ui(
    a: &mut App,
    ui: &AppWindow,
    chart_window: &ChartWindow,
    signal_window: &SignalSelectWindow,
    tx_window: &TxWindow,
) {
    // ----- 发送表 -----
    ui.set_connected(a.connected);
    ui.set_running(a.running);
    ui.set_recording(a.recording);
    ui.set_mode_trace(a.mode_trace);
    ui.set_paused(a.paused);
    ui.set_auto_scroll(a.autoscroll);
    ui.set_rx_count(a.rx.to_string().into());
    ui.set_tx_count(a.tx.to_string().into());
    ui.set_err_count(a.err.to_string().into());
    ui.set_fps(format!("{:.0}", a.fps).into());
    ui.set_bus_load(format!("{:.1}%", a.bus_load).into());
    ui.set_load_high(a.bus_load >= 70.0);
    // 多卡时给出每通道负载明细（单通道时留空，避免与全局 Load 重复）
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
            let nm = a.dbc_message_name(id).unwrap_or("");
            if nm.is_empty() {
                format!("0x{id:X}")
            } else {
                format!("0x{id:X} {nm}")
            }
        })
        .unwrap_or_else(|| "无".into());
    ui.set_sel_id(sel_id_txt.into());

    // 选中高亮（廉价属性，每帧都更新，无需重建模型
ui.set_selected(a.selected_index);

    // ----- 报文表（暂停时不重建；无变化时跳过整表重建） -----
    build_msg_table(a, ui);

    // ----- 信号解析面板 -----
    build_signal_panel(a, ui);

    // ----- DBC 信号候选（曲线信号列表处直接添加） -----
    a.dbc_signal_choices.clear();
    let mut dbc_signal_rows: Vec<SharedString> = Vec::new();
    {
        // 合并全部 DBC 的信号(按 id+信号名去重, 先加载者优先)
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
    }
    if dbc_signal_rows.is_empty() {
        dbc_signal_rows.push("(无 DBC 信号)".into());
    }
    let dbc_signal_model = ModelRc::from(Rc::new(VecModel::from(dbc_signal_rows)));
    ui.set_dbc_signals(dbc_signal_model);
    refresh_signal_picker(a, signal_window);

    // ----- 曲线 -----
    refresh_chart(a, ui, chart_window);

    // ----- 发送表 -----
    // 仅在内容变化时重建列表（避免每 100ms 重建模型导致 ListView 委托被反复销毁重建，
    // 造成点击「周期」等按钮明显的卡顿/延迟）。
    {
        let sig = tx_list_sig(a);
        if sig != a.tx_list_cache {
            a.tx_list_cache = sig;
            push_tx_list(a, ui, tx_window);
        }
    }
    // 通道名（CAN1..CANn）—— 所有页面统一来源
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
            tx_window.set_channel_names(ModelRc::from(Rc::new(VecModel::from(chan_names))));
        }
    }

    // ----- DBC 发送页：报文选择列表 + 信号编辑器 -----
    build_tx_dbc_page(a, tx_window);

    // ----- 统计 -----
    build_stats(a, ui);

    // ----- 发送表 -----
    tree::build_tree(a, ui);

    // ----- 日志 -----
    let logs: Vec<SharedString> = a.logs.iter().map(|s| s.clone().into()).collect();
    ui.set_logs(ModelRc::from(Rc::new(VecModel::from(logs))));

    // ----- CAN 报文日志(printf-over-CAN 控制台) -----
    let console_rows: Vec<SharedString> = a.console.rows().into_iter().map(|s| s.into()).collect();
    ui.set_console_lines(ModelRc::from(Rc::new(VecModel::from(console_rows))));
}



pub(crate) fn fmtf(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ---------------- Python 自动化 IPC：UI 线程侧（需 App 私有项，故在 main.rs）----------------

/// 把当前 App 状态发布到只读快照（tick 每 100ms 调一次），供 IPC 只读操作即时读取。
/// last 表只在变化时(last_dirty)才整表深拷贝，避免每拍 O(n) 克隆。
fn publish_snapshot(a: &mut App) {
    let last_rebuild = if a.last_dirty {
        let mut last = HashMap::with_capacity(a.last.len());
        for (k, li) in a.last.iter() {
            last.insert(*k, ipc::LastSnap { t: li.t, count: li.count, data: li.data.clone(), ext: li.ext });
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
        snap.channels = a
            .chan_stats
            .iter()
            .map(|(ch, cs)| ipc::ChanStatSnap {
                ch: *ch,
                rx: cs.rx,
                tx: cs.tx,
                err: cs.err,
                bus_load: cs.bus_load,
                fps: cs.fps,
            })
            .collect();
        snap.console_enabled = a.console_enabled;
        if a.console_enabled {
            // 仅启用时重建(避免空跑时每拍 join 大字符串); 停用后保留最后捕获文本供读取
            snap.console_text = a.console.export_text();
        }
        if let Some(last) = last_rebuild {
            snap.last = last;
        }
        snap.dbc = a.dbc_snap.clone();
    }
}

/// DBC 集变化后重建不可变 DBC 快照（供 IPC decode/encode 在 handler 线程无 App 访问）。
fn rebuild_dbc_snap(a: &mut App) {
    a.dbc_snap = std::sync::Arc::new(ipc::DbcSnapshot::from_dbcs(&a.dbcs));
    // DBC 变了, 表达式引用的报文 id 映射可能变, 重算
    recompute_expr_ids(a);
}

/// 停掉一个内部周期 handle（发 enable=false）。
fn stop_internal_periodic(a: &App, internal: u64) {
    let dummy = CanFrame { t: 0.0, ch: 1, tx: true, id: 0, ext: false, fd: false, brs: false, remote: false, error: false, data: Vec::new() };
    let _ = a.cmd.send(Cmd::SetPeriodic { handle: internal, frame: dummy, period_ms: 1, repeat: -1, enable: false });
}

/// 处理一条来自 IPC handler 线程的状态变更请求；每条恰好回一条 IpcResp；绝不碰窗口、不再借用 a。
fn handle_ipc(a: &mut App, ureq: ipc::UiReq) {
    use ipc::{IpcReq, IpcResp};
    let cid = ureq.client_id;
    let ok = || IpcResp::Ok(serde_json::json!({}));
    // 若回复发送失败(客户端已超时离开)，需回滚的已提交副作用：刚建的内部周期 handle。
    let mut periodic_rollback: Option<u64> = None;
    let resp = match ureq.req {
        IpcReq::SendOnce { ch, id, data, ext, fd, brs, remote } => {
            let f = CanFrame { t: 0.0, ch: ch.max(1), tx: true, id, ext, fd, brs, remote, error: false, data };
            let _ = a.cmd.send(Cmd::SendOnce(f));
            ok()
        }
        IpcReq::SetPeriodic { client_handle, ch, id, data, period_ms, repeat, ext, fd, brs, remote } => {
            // 内部 handle 用高位置 1 的不相交空间，绝不与 UI 周期任务相撞。
            let internal = a.next_handle | (1u64 << 63);
            a.next_handle += 1;
            periodic_rollback = Some(internal);
            // 同一 client_handle 重新 set_periodic 时，先停掉旧的内部周期任务，
            // 否则它变成孤儿、一直刷总线且不可停（仅 ClientGone 能清）。
            if let Some(old) = a.ipc_handle_map.insert((cid, client_handle), internal) {
                stop_internal_periodic(a, old);
            }
            let frame = CanFrame { t: 0.0, ch: ch.max(1), tx: true, id, ext, fd, brs, remote, error: false, data };
            let _ = a.cmd.send(Cmd::SetPeriodic { handle: internal, frame, period_ms: period_ms.max(1), repeat, enable: true });
            ok()
        }
        IpcReq::StopPeriodic { client_handle } => {
            if let Some(internal) = a.ipc_handle_map.remove(&(cid, client_handle)) {
                stop_internal_periodic(a, internal);
            }
            ok()
        }
        IpcReq::Connect { channels } => {
            // 空通道列表直接报错, 避免脚本误把主界面已配置的通道清空
            if channels.is_empty() {
                IpcResp::Err {
                    code: "BAD_ARG".into(),
                    msg: "channels 不能为空(至少给一个通道配置)".into(),
                }
            } else {
                // 同步到 App 通道列表：让 per-channel 负载用对的波特率、GUI 通道名也反映实连
                a.channels = channels.clone();
                let _ = a.cmd.send(Cmd::ConnectChannels(channels));
                ok()
            }
        }
        IpcReq::ConnectVirtual => {
            let _ = a.cmd.send(Cmd::ConnectVirtual);
            ok()
        }
        IpcReq::ConnectConfigured => {
            // 连接主界面"设备"对话框里已配置的全部通道（支持多卡）。
            let _ = a.cmd.send(Cmd::ConnectChannels(a.channels.clone()));
            IpcResp::Ok(serde_json::json!({ "channels": a.channels.len() }))
        }
        IpcReq::LoadDbc { path, loaded } => {
            // 解析已在 handler 线程完成(loaded)；此处仅去重+推入+重建快照(快，不阻塞 tick)。
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
                    Err(e) => IpcResp::Err { code: "LOAD_FAIL".into(), msg: e },
                }
            }
        }
        IpcReq::Disconnect => {
            let _ = a.cmd.send(Cmd::Disconnect);
            ok()
        }
        IpcReq::Start => {
            let _ = a.cmd.send(Cmd::Start);
            ok()
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
        IpcReq::ConsoleSet { enabled, id, ch, clear } => {
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
            // 停掉该客户端所有周期任务并清理映射（防止脚本崩溃后心跳继续刷总线）。
            let internals: Vec<u64> = a.ipc_handle_map.iter().filter(|((c, _), _)| *c == cid).map(|(_, h)| *h).collect();
            for internal in internals {
                stop_internal_periodic(a, internal);
            }
            a.ipc_handle_map.retain(|(c, _), _| *c != cid);
            ok()
        }
    };
    // 回复发送失败 = 客户端已在 8s 内超时离开：回滚刚建的周期任务，避免孤儿帧继续刷总线。
    if ureq.reply.send(resp).is_err()
        && let Some(internal) = periodic_rollback
    {
        stop_internal_periodic(a, internal);
        a.ipc_handle_map.retain(|_, h| *h != internal);
    }
}

/// 把一帧分发到匹配的 IPC 订阅者（在 tick 里，仅 try_send，满则丢最新并计数）。
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

/// 子进程生命周期（仅在 UI 线程调用）：停止标志/超时则 kill；否则 try_wait 收尾设状态。
fn reap_child(a: &mut App) {
    let timed_out = a.py_started.is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(a.py_timeout_secs));
    if (a.py_stop_flag || timed_out) && a.py_child.is_some() {
        if let Some(mut c) = a.py_child.take() {
            let _ = c.kill();
        }
        a.run_status = if timed_out { "FAIL: 超时".into() } else { "已停止".into() };
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
        // 脚本若已 run_result 设过 PASS/FAIL 则保留，否则用退出码。
        if !(a.run_status.starts_with("PASS") || a.run_status.starts_with("FAIL")) {
            a.run_status = if success { "PASS".into() } else { "FAIL".into() };
        }
        a.py_dirty = true;
    }
}

/// 把子进程输出行喂进日志面板（在 tick 里）。
/// 脚本运行完整日志的落盘路径（临时目录，覆盖写最近一次运行）。
fn run_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("pcanwork_last_run.log")
}

fn drain_py_output(a: &mut App) {
    let mut lines = Vec::new();
    if let Some(rx) = &a.py_out_rx {
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return;
    }
    for line in lines {
        a.py_output.push_str(&line);
        a.py_output.push('\n');
        a.log(line); // 同时进主窗口"运行日志"，便于运行器窗口关闭时仍有记录
    }
    // 限长：保留尾部 ~200KB（按字符边界裁剪，避免 UTF-8 截断 panic）
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

// ---------------- 过滤解析 ----------------

/// 收集当前可持久化配置（退出时保存）
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
        dbc_path: None, // 旧字段不再写
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
    }
}

/// 把配置应用到 App 与主窗口属性（主题由调用方按需分发到各窗口）
fn apply_settings(a: &mut App, ui: &AppWindow, s: &settings::Settings) {
    if !s.channels.is_empty() {
        a.channels = s.channels.clone();
        a.channel_sel = s.channel_sel.clamp(0, a.channels.len() as i32 - 1).max(0);
    }
    a.python_interpreter = s.python_interpreter_path.clone();
    a.last_script_path = s.last_script_path.clone();
    a.expr_vars = s.expr_vars.clone();
    recompute_expr_ids(a); // DBC 此时可能已加载; 若 DBC 之后才加载, 加载处会再次重算
    // CAN 报文日志配置 + 一次性同步到界面输入框(之后以界面为准, 不每帧覆盖)
    a.console_enabled = s.console_enabled;
    a.console_id = if s.console_id < 0 { None } else { Some(s.console_id as u32) };
    a.console_ch = s.console_ch.clamp(0, 255) as u8;
    ui.set_console_enabled(a.console_enabled);
    ui.set_console_id(a.console_id.map(|x| format!("0x{x:X}")).unwrap_or_default().into());
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
    if !s.sim_widgets.trim().is_empty() {
        a.sim_widgets = serde_json::from_str(&s.sim_widgets).unwrap_or_default();
    }
    if s.trace_cap >= 1000 {
        a.trace_cap = s.trace_cap;
    }
    if s.chart_cap >= 500 {
        a.chart_cap = s.chart_cap;
    }
    // DBC 路径：优先用多 DBC 列表 dbc_paths，否则回退旧版单一 dbc_path（替换当前列表）
    let effective: Vec<String> = if !s.dbc_paths.is_empty() {
        s.dbc_paths.clone()
    } else {
        s.dbc_path.clone().into_iter().collect()
    };
    if !effective.is_empty() {
        a.dbcs.clear();
        a.dbc_paths.clear();
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
}

/// 方向过滤下拉索引 → Option<bool>（0=全部 None, 1=Rx Some(false), 2=Tx Some(true)）。
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
    fn console_reassembles_printf_lines() {
        let mut c = ConsoleBuf::default();
        // 分两帧到达的一行 + 一个完整行(含 0x00 填充与 \r\n)
        c.feed(b"Hello, ");
        c.feed(b"world!\r\n");
        c.feed(&[b'A', b'B', b'C', 0x0A, 0x00, 0x00]); // ABC + 换行 + 填充
        // 已完成行
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.lines[0], "Hello, world!");
        assert_eq!(c.lines[1], "ABC");
        // 未完成行(无换行)出现在 rows 末尾
        c.feed(b"partial");
        let rows = c.rows();
        assert_eq!(rows.last().unwrap(), "partial");
        assert_eq!(rows.len(), 3);
        // 导出文本
        assert_eq!(c.export_text(), "Hello, world!\nABC\npartial");
        // UTF-8 中文(下位机发 UTF-8)
        let mut c2 = ConsoleBuf::default();
        c2.feed("温度=25℃\n".as_bytes());
        assert_eq!(c2.lines[0], "温度=25℃");
        // CAN FD: 单帧可达 64 字节, 解码器吃任意长度字节数组 -> 经典/FD 同一条路径
        let mut c3 = ConsoleBuf::default();
        let fd_line = b"FD log #07 temp=25C volt=72.1 current=12.3 state=OK\n"; // 51 字节, >8
        assert!(fd_line.len() > 8 && fd_line.len() <= 64);
        c3.feed(fd_line);
        assert_eq!(c3.lines.len(), 1);
        assert_eq!(c3.lines[0], "FD log #07 temp=25C volt=72.1 current=12.3 state=OK");
        // 经典分帧(每 8 字节)拼成同一行
        let mut c4 = ConsoleBuf::default();
        for chunk in b"hello FD vs classic same line\n".chunks(8) {
            c4.feed(chunk);
        }
        assert_eq!(c4.lines[0], "hello FD vs classic same line");
        // clear
        c.clear();
        assert!(c.rows().is_empty());
    }

    /// 工程文件 (.zcp) 的序列化往返：配置 + 发送列表 round-trip 后字段不丢失，
/// 且加载后周期任务强制停发（into_task 的安全约定）
#[test]
    fn project_roundtrip() {
        let proj = Project {
            settings: settings::Settings {
                mode_trace: true,
                dark: true,
                big: false,
                trace_cap: 200_000,
                chart_cap: 4096,
                dir_filter: 2,
                f_id: "0x100-0x200".into(),
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

        assert_eq!(back.settings.trace_cap, 200_000);
        assert!(back.settings.dark);
        assert!(!back.settings.big);
        assert_eq!(back.settings.dir_filter, 2);
        assert_eq!(back.settings.f_id, "0x100-0x200");
        assert_eq!(back.txs.len(), 1);
        assert_eq!(back.txs[0].id, 0x123);
        assert_eq!(back.txs[0].data, vec![0x11, 0x22, 0x33]);
        assert_eq!(back.txs[0].sig_values, vec![("rpm".to_string(), 1500.0)]);

        // 加载后默认停发（避免一打开工程就周期发送）
        let task = back.txs.into_iter().next().unwrap().into_task(7);
        assert!(!task.periodic);
        assert_eq!(task.handle, 7);
        assert_eq!(task.id, 0x123);
    }

    /// 缺字段的旧工程文件应能用默认值容错加载（serde default）
#[test]
    fn project_partial_load() {
        let json = r#"{ "txs": [] }"#;
        let back: Project = serde_json::from_str(json).expect("partial deserialize");
        assert!(back.txs.is_empty());
        // settings 缺省：big 默认 true（default_true
assert!(back.settings.big);
    }

    fn mkwidget(kind: SimKind, mode: GenMode) -> SimWidget {
        SimWidget {
            kind,
            name: "w".into(),
            channel: 1,
            frame_id: 0x100,
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
            cur: 0.0,
            tick: 0,
            last_fire: None,
        }
    }

    /// 信号发生器波形：恒定取 min；斜坡/正弦始终落在 [min,max]。
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
            assert!(rv >= -1e-6 && rv <= 100.0 + 1e-6, "ramp out of range: {rv}");
            assert!(sv >= -1e-6 && sv <= 100.0 + 1e-6, "sine out of range: {sv}");
        }
    }

    /// 仿真控件 JSON 往返：配置(含画布位置/尺寸/绑定信号)序列化后保留, 运行期字段(skip)复位为默认值。
    #[test]
    fn sim_widget_roundtrip() {
        let w = mkwidget(SimKind::Dial, GenMode::Sine);
        let json = serde_json::to_string(&vec![w]).expect("serialize");
        let back: Vec<SimWidget> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].frame_id, 0x100);
        assert_eq!(back[0].gen_step, 7.0);
        assert_eq!(back[0].x, 10.0);
        assert_eq!(back[0].w, 120.0);
        assert_eq!(back[0].slider_val, 5.0);
        assert!(back[0].enabled);
        // 运行期字段未持久化, 反序列化后为默认
        assert_eq!(back[0].tick, 0);
        assert!(back[0].last_fire.is_none());
    }

    /// SimKind 整数映射往返
#[test]
    fn sim_kind_mapping() {
        for k in 0..8 {
            assert_eq!(SimKind::from_i32(k).to_i32(), k);
        }
    }

    /// 控件↔DBC 信号绑定取值: 走真实 DBC 解码路径(同 aaaaa.dbc 的 New_Message_1/New_Signal_1 布局)。
#[test]
    fn sim_binding_decodes_dbc_signal() {
        // 0x100 New_Message_1, New_Signal_1 @ bit7 Motorola 8位 = byte0
        let txt = "VERSION \"\"\nBO_ 256 New_Message_1: 8 ECU\n SG_ New_Signal_1 : 7|8@0+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_sim_bind.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = dbc::DbcDb::load(&p.to_string_lossy()).unwrap();
        let dbcs = vec![db];
        let frame = [42u8, 0, 0, 0, 0, 0, 0, 0];

        // 绑定 DBC 信号名 → 解出 New_Signal_1 = 42
        let v = sim_decode_value(&dbcs, 0x100, "New_Signal_1", &frame);
        assert_eq!(v, Some(42.0), "DBC 信号绑定应解出 byte0=42");

        // 信号名为空 → byte0 兜底路径同样 = 42
        let v0 = sim_decode_value(&dbcs, 0x100, "", &frame);
        assert_eq!(v0, Some(42.0));

        // 不存在的信号 → None
        assert_eq!(sim_decode_value(&dbcs, 0x100, "Nope", &frame), None);
        // 未知的 ID + 指定信号 → None
        assert_eq!(sim_decode_value(&dbcs, 0x222, "New_Signal_1", &frame), None);
        let _ = std::fs::remove_file(&p);
    }
}
