//! Async backend: a Tokio runtime on a dedicated thread driving the Modbus
//! master (poll) and slave (simulator) engines, bridged to the Slint UI through
//! a weak handle and a command channel.
//!
//! The master side is MDI-style: any number of independent poll windows, each
//! with its own connection, engine, display settings and cached state. The flat
//! UI properties mirror the *active* window; switching windows restores the
//! target window's config and last-known state from its cache.

use crate::format::*;
use crate::protocol::*;

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::future::{ready, Ready};
use std::io::Write as _;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use tokio_modbus::client::Context;
use tokio_modbus::prelude::*;
use tokio_modbus::server::Service;
use tokio_serial::SerialStream;

const RESPONSE_TIMEOUT_MS: u64 = 1000;
const SCAN_TIMEOUT_MS: u64 = 400;
const COMMAND_QUEUE_CAPACITY: usize = 512;
const TRAFFIC_QUEUE_CAPACITY: usize = 1024;
const ENGINE_CONTROL_QUEUE_CAPACITY: usize = 256;
const CHART_LEN: usize = 600;
const CHART_SERIES_PER_PAGE: usize = 12;
const CHART_COLORS: [u32; 12] = [
    0xFF61AFEF, 0xFF98C379, 0xFFE06C75, 0xFFE5C07B, 0xFFC678DD, 0xFF56B6C2, 0xFFD19A66, 0xFFABB2BF,
    0xFFE06CB4, 0xFF7FB069, 0xFF5C9DFF, 0xFFF0A030,
];

// ===========================================================================
// Public API
// ===========================================================================

/// Snapshot of the flat master UI config, stored per window so it can be
/// restored when the window is re-selected.
#[derive(Clone, Default)]
pub struct UiCfg {
    pub transport: i32,
    pub host: String,
    pub port: i32,
    pub serial: String,
    pub baud_index: i32,
    pub databits_index: i32,
    pub parity: i32,
    pub stopbits: i32,
    pub slave_id: i32,
    pub function: i32,
    pub address: i32,
    pub quantity: i32,
    pub scanrate: i32,
    pub format: i32,
    pub scl_enabled: bool,
    pub scl_x1: String,
    pub scl_y1: String,
    pub scl_x2: String,
    pub scl_y2: String,
    pub scl_decimals: i32,
    pub col_normal: i32,
    pub col_op1: i32,
    pub col_v1: String,
    pub col_c1: i32,
    pub col_op2: i32,
    pub col_v2: String,
    pub col_c2: i32,
    pub vn_enabled: bool,
    pub vn_text: String,
}

pub struct MasterCfg {
    pub transport: Transport,
    pub slave_id: u8,
    pub area: Area,
    pub address: u16,
    pub quantity: u16,
    pub scan_ms: u64,
    pub format: RegFormat,
    pub scaling: Scaling,
    pub colors: ColorRules,
    pub value_names: ValueNames,
    /// false when the selected function is a WRITE code — the engine connects but
    /// does not poll (the main grid is then a write definition, not read display).
    pub poll: bool,
    /// Some → wrap the TCP connection in TLS (Modbus/TCP Security).
    pub tls: Option<crate::tls::TlsClientCfg>,
    /// 单次请求响应超时(ms)。0 → 用默认 1000ms。
    pub timeout_ms: u64,
    /// 通信错误(超时/断开)后是否自动重连。
    pub reconnect: bool,
    /// 自动重连间隔(ms)。
    pub reconnect_ms: u64,
}

pub struct SlaveCfg {
    pub transport: Transport,
    pub unit_id: u8,
    pub ignore_unit_id: bool,
    pub area: Area,
    pub address: u16,
    pub quantity: u16,
    pub format: RegFormat,
    /// Language selected when the server is started; used for asynchronous status text.
    pub english: bool,
    /// Some → accept TLS connections (Modbus/TCP Security).
    pub tls: Option<crate::tls::TlsServerCfg>,
}

/// Register-value simulation for the slave: animate a block of registers/coils.
#[derive(Clone)]
pub struct SimCfg {
    pub mode: SimMode,
    pub area: Area,
    pub address: u16,
    pub quantity: u16,
    pub step: u16,
    pub min: i64,
    pub max: i64,
    pub interval_ms: u64,
    pub target: i32, // -1 = whole block; else the single absolute address to animate
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SimMode {
    Off,
    Increment,
    Decrement,
    Random,
    Toggle,
}

impl SimMode {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => SimMode::Increment,
            2 => SimMode::Decrement,
            3 => SimMode::Random,
            4 => SimMode::Toggle,
            _ => SimMode::Off,
        }
    }
}

#[derive(Clone, Copy)]
pub enum WriteFunc {
    SingleCoil,
    SingleReg,
    MultiCoils,
    MultiRegs,
}

impl WriteFunc {
    pub fn from_index(i: i32) -> Self {
        match i {
            0 => WriteFunc::SingleCoil,
            2 => WriteFunc::MultiCoils,
            3 => WriteFunc::MultiRegs,
            _ => WriteFunc::SingleReg,
        }
    }
}

/// A user-defined derived channel: a named formula evaluated over the polled
/// register window (r0 = first register).
#[derive(Clone)]
pub struct DerivedCh {
    pub name: String,
    pub formula: String,
}

pub struct WriteReq {
    pub func: WriteFunc,
    pub address: u16,
    pub text: String,
    /// If set, `text` is a single number encoded across registers (FC16) using
    /// this format's width and byte order (32/64-bit int/float).
    pub encode: Option<RegFormat>,
}

/// One entry in the client Write List: a holding register, a value, and whether
/// it auto-increments each cycle during periodic auto-write.
#[derive(Clone)]
pub struct WriteItem {
    pub address: u16,
    pub value: u16,
    pub inc: bool,
}

pub struct ScanCfg {
    pub transport: Transport,
    pub slave_id: u8,
    pub area: Area,
    pub start: u16,
    pub count: u16,
}

pub struct SlaveScanCfg {
    pub transport: Transport,
    pub area: Area,
    pub address: u16,
    pub from_id: u8,
    pub to_id: u8,
}

#[derive(Clone)]
pub struct LogCfg {
    pub path: String,
    pub each_read: bool,
    pub period_s: u32,
    pub delimiter: char,
    pub on_change: bool,
    pub timestamp: bool,
}

pub enum Cmd {
    NewWindow(UiCfg),
    SelectWindow {
        id: u32,
        current: UiCfg,
    },
    CloseWindow {
        id: u32,
    },
    SetFloat {
        id: u32,
        float: Option<slint::Weak<crate::PollFloat>>,
    },
    MasterConnect(MasterCfg),
    MasterDisconnect,
    MasterWrite(WriteReq),
    MasterWriteOnce {
        func: WriteFunc,
        items: Vec<WriteItem>,
    },
    MasterAutoWrite {
        func: WriteFunc,
        items: Vec<WriteItem>,
        interval_ms: u64,
    },
    MasterAutoWriteStop,
    MasterMaskWrite {
        address: u16,
        and_mask: u16,
        or_mask: u16,
    },
    MasterReadWrite {
        read_addr: u16,
        read_qty: u16,
        write_addr: u16,
        write_values: Vec<u16>,
    },
    MasterFormat(RegFormat),
    MasterName {
        address: u16,
        name: String,
    },
    MasterScaling(Scaling),
    MasterColors(ColorRules),
    MasterValueNames(ValueNames),
    MasterCellFormat {
        address: u16,
        format: Option<RegFormat>,
    },
    MasterDerived(Vec<DerivedCh>),
    MasterChartExport(String),
    MasterChartAxis {
        addr: u16,
        right: bool,
    },
    MasterChartPage(i32),
    MasterChartFocus(u16),
    MasterReadDef {
        area: Area,
        address: u16,
        quantity: u16,
        scan_ms: u64,
        poll: bool,
    },
    MasterStartLog(LogCfg),
    MasterStopLog,
    ScanAddress(ScanCfg),
    ScanSlave(SlaveScanCfg),
    ScanStop,
    SlaveStart(SlaveCfg),
    SlaveStop,
    SlaveEdit {
        address: u16,
        text: String,
    },
    SlaveEditAt {
        area: Area,
        address: u16,
        text: String,
    },
    SlaveName {
        address: u16,
        name: String,
    },
    SlaveCellFormat {
        address: u16,
        format: Option<RegFormat>,
    },
    /// 导出全部数据到 CSV。server=true: 服务端扫 4 表。
    /// client: active_csv = UI 端活动窗口的当前行(保证名字/功能码正确, 因为未连接窗口
    /// 的预览数据只在 UI), 其余窗口从后端 cache 取(已连接/已轮询的)。
    ExportCsv {
        path: String,
        server: bool,
        active_id: u32,
        active_csv: String,
    },
    SlaveView {
        area: Area,
        address: u16,
        quantity: u16,
        format: RegFormat,
    },
    SlaveScaling(Scaling),
    SlaveColors(ColorRules),
    SlaveValueNames(ValueNames),
    SlaveSimStart(SimCfg),
    SlaveSimStop,
    SlaveAutoInc {
        address: u16,
    },
}

#[derive(Clone)]
pub struct CommandSender {
    tx: mpsc::Sender<Cmd>,
    weak: slint::Weak<crate::AppWindow>,
    rejected: Arc<AtomicU64>,
    high_watermark: Arc<AtomicUsize>,
    last_report_ms: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug)]
pub struct CommandRejected;

impl CommandSender {
    pub fn send(&self, command: Cmd) -> Result<(), CommandRejected> {
        let result = self.tx.try_send(command);
        let rejected = result.is_err();
        if rejected {
            self.rejected.fetch_add(1, Ordering::Relaxed);
        } else {
            self.high_watermark
                .fetch_max(self.queue_depth(), Ordering::Relaxed);
        }
        self.report_health(rejected);
        result.map_err(|_| CommandRejected)
    }

    fn queue_depth(&self) -> usize {
        self.tx.max_capacity().saturating_sub(self.tx.capacity())
    }

    fn report_health(&self, force: bool) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let previous = self.last_report_ms.load(Ordering::Relaxed);
        if !force && now_ms.saturating_sub(previous) < 500 {
            return;
        }
        if self
            .last_report_ms
            .compare_exchange(previous, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let depth = self.queue_depth();
        let high = self.high_watermark.load(Ordering::Relaxed);
        let rejected = self.rejected.load(Ordering::Relaxed);
        let _ = self.weak.upgrade_in_event_loop(move |app| {
            app.set_runtime_health(
                format!("CMD {depth}/{COMMAND_QUEUE_CAPACITY} H{high} R{rejected}").into(),
            );
            app.set_runtime_loss(rejected > 0);
        });
    }
}

pub fn start_backend(weak: slint::Weak<crate::AppWindow>) -> CommandSender {
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let sender = CommandSender {
        tx,
        weak: weak.clone(),
        rejected: Arc::new(AtomicU64::new(0)),
        high_watermark: Arc::new(AtomicUsize::new(0)),
        last_report_ms: Arc::new(AtomicU64::new(0)),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        rt.block_on(controller(rx, UiSink { weak }));
    });
    sender.report_health(true);
    sender
}

// ===========================================================================
// Controller — owns the poll windows, slave and scan lifecycles.
// ===========================================================================

struct Win {
    id: u32,
    title: String,
    cfg: UiCfg,
    connected: bool,
    engine: Option<MasterHandle>,
    cache: Arc<Mutex<WinCache>>,
    float: Arc<Mutex<Option<slint::Weak<crate::PollFloat>>>>,
}

/// UI 功能码索引(0-7)→ 数据表(与 main.rs::func_area 一致)。
fn func_area_be(function: i32) -> Area {
    match function {
        0 | 4 | 6 => Area::Coils,
        1 => Area::DiscreteInputs,
        3 => Area::InputRegisters,
        _ => Area::HoldingRegisters,
    }
}

/// CSV 字段转义(同 main.rs::csv_escape)。
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn save_cfg(windows: &mut [Win], id: u32, cfg: UiCfg) {
    if let Some(w) = windows.iter_mut().find(|w| w.id == id) {
        w.cfg = cfg;
    }
}

fn push_windows(windows: &[Win], ui: &UiSink) {
    let tabs: Vec<crate::WinTab> = windows
        .iter()
        .map(|w| crate::WinTab {
            id: w.id as i32,
            title: w.title.clone().into(),
            connected: w.connected,
        })
        .collect();
    ui.set_windows(tabs);
}

fn show_window(windows: &[Win], id: u32, ui: &UiSink) {
    if let Some(w) = windows.iter().find(|w| w.id == id) {
        ui.set_active_win(id);
        ui.push_config(&w.cfg);
        ui.push_cache(&w.cache.lock().unwrap());
    }
}

async fn controller(mut rx: mpsc::Receiver<Cmd>, ui: UiSink) {
    let active = Arc::new(AtomicU32::new(1));
    let mut windows: Vec<Win> = vec![Win {
        id: 1,
        title: "Poll 1".into(),
        cfg: UiCfg::default(),
        connected: false,
        engine: None,
        cache: Arc::new(Mutex::new(WinCache::default())),
        float: Arc::new(Mutex::new(None)),
    }];
    let mut next_id = 2u32;
    let mut slave: Option<SlaveHandle> = None;
    let mut scan: Option<JoinHandle<()>> = None;

    push_windows(&windows, &ui);

    macro_rules! to_active {
        ($msg:expr) => {{
            let aid = active.load(Ordering::Relaxed);
            if let Some(w) = windows.iter().find(|w| w.id == aid) {
                if let Some(e) = &w.engine {
                    e.send($msg);
                }
            }
        }};
    }

    while let Some(cmd) = rx.recv().await {
        // 处理命令前，把各窗口 tab 的连接态与异步 cache 对账：连接失败/掉线会异步写 cache，
        // 此处同步到 tab 并重推，避免标签停留在过时的"已连接"(成功连接仍由乐观置位即时变绿)。
        {
            let mut changed = false;
            for w in windows.iter_mut() {
                let c = w.cache.lock().unwrap().connected;
                if c != w.connected {
                    w.connected = c;
                    changed = true;
                }
            }
            if changed {
                push_windows(&windows, &ui);
            }
        }
        match cmd {
            Cmd::NewWindow(current) => {
                save_cfg(
                    &mut windows,
                    active.load(Ordering::Relaxed),
                    current.clone(),
                );
                let id = next_id;
                next_id += 1;
                windows.push(Win {
                    id,
                    title: format!("Poll {id}"),
                    cfg: current,
                    connected: false,
                    engine: None,
                    cache: Arc::new(Mutex::new(WinCache::default())),
                    float: Arc::new(Mutex::new(None)),
                });
                active.store(id, Ordering::Relaxed);
                push_windows(&windows, &ui);
                show_window(&windows, id, &ui);
            }
            Cmd::SelectWindow { id, current } => {
                save_cfg(&mut windows, active.load(Ordering::Relaxed), current);
                active.store(id, Ordering::Relaxed);
                show_window(&windows, id, &ui);
                push_windows(&windows, &ui);
            }
            Cmd::SetFloat { id, float } => {
                if let Some(w) = windows.iter().find(|w| w.id == id) {
                    *w.float.lock().unwrap() = float.clone();
                    if let Some(fw) = float {
                        push_cache_to_float(&fw, &w.title, &w.cache.lock().unwrap());
                    }
                }
            }
            Cmd::CloseWindow { id } => {
                if windows.len() > 1 {
                    if let Some(pos) = windows.iter().position(|w| w.id == id) {
                        if let Some(e) = windows[pos].engine.take() {
                            e.send(MasterMsg::Stop);
                        }
                        windows.remove(pos);
                        if active.load(Ordering::Relaxed) == id {
                            let nid = windows[0].id;
                            active.store(nid, Ordering::Relaxed);
                            show_window(&windows, nid, &ui);
                        }
                        push_windows(&windows, &ui);
                    }
                }
            }
            Cmd::MasterConnect(cfg) => {
                let aid = active.load(Ordering::Relaxed);
                if let Some(w) = windows.iter_mut().find(|w| w.id == aid) {
                    if let Some(e) = w.engine.take() {
                        e.send(MasterMsg::Stop);
                    }
                    let sink =
                        ui.window_sink(aid, active.clone(), w.cache.clone(), w.float.clone());
                    w.engine = Some(spawn_master(cfg, sink));
                    w.connected = true;
                }
                push_windows(&windows, &ui);
            }
            Cmd::MasterDisconnect => {
                let aid = active.load(Ordering::Relaxed);
                if let Some(w) = windows.iter_mut().find(|w| w.id == aid) {
                    if let Some(e) = w.engine.take() {
                        e.send(MasterMsg::Stop);
                    }
                    w.connected = false;
                    {
                        let mut c = w.cache.lock().unwrap();
                        c.connected = false;
                        c.status = "Disconnected".into();
                    }
                    ui.master_status("Disconnected", false);
                }
                push_windows(&windows, &ui);
            }
            Cmd::MasterWrite(req) => to_active!(MasterMsg::Write(req)),
            Cmd::MasterWriteOnce { func, items } => {
                to_active!(MasterMsg::WriteOnce { func, items })
            }
            Cmd::MasterAutoWrite {
                func,
                items,
                interval_ms,
            } => to_active!(MasterMsg::AutoWrite {
                func,
                items,
                interval_ms
            }),
            Cmd::MasterAutoWriteStop => to_active!(MasterMsg::AutoWriteStop),
            Cmd::MasterMaskWrite {
                address,
                and_mask,
                or_mask,
            } => to_active!(MasterMsg::MaskWrite {
                address,
                and_mask,
                or_mask
            }),
            Cmd::MasterReadWrite {
                read_addr,
                read_qty,
                write_addr,
                write_values,
            } => to_active!(MasterMsg::ReadWrite {
                read_addr,
                read_qty,
                write_addr,
                write_values
            }),
            Cmd::MasterFormat(f) => to_active!(MasterMsg::SetFormat(f)),
            Cmd::MasterName { address, name } => to_active!(MasterMsg::SetName { address, name }),
            Cmd::MasterScaling(s) => to_active!(MasterMsg::SetScaling(s)),
            Cmd::MasterColors(c) => to_active!(MasterMsg::SetColors(c)),
            Cmd::MasterValueNames(v) => to_active!(MasterMsg::SetValueNames(v)),
            Cmd::MasterCellFormat { address, format } => {
                to_active!(MasterMsg::SetCellFormat { address, format })
            }
            Cmd::MasterDerived(d) => to_active!(MasterMsg::SetDerived(d)),
            Cmd::MasterChartExport(p) => to_active!(MasterMsg::ExportChart(p)),
            Cmd::MasterChartAxis { addr, right } => {
                to_active!(MasterMsg::SetChartAxis { addr, right })
            }
            Cmd::MasterChartPage(direction) => {
                to_active!(MasterMsg::SetChartPage(direction))
            }
            Cmd::MasterChartFocus(addr) => {
                to_active!(MasterMsg::FocusChartAddress(addr))
            }
            Cmd::MasterReadDef {
                area,
                address,
                quantity,
                scan_ms,
                poll,
            } => {
                to_active!(MasterMsg::SetReadDef {
                    area,
                    address,
                    quantity,
                    scan_ms,
                    poll
                })
            }
            Cmd::MasterStartLog(c) => to_active!(MasterMsg::StartLog(c)),
            Cmd::MasterStopLog => to_active!(MasterMsg::StopLog),

            Cmd::ScanAddress(cfg) => {
                if let Some(s) = scan.take() {
                    s.abort();
                }
                scan = Some(spawn_scan_address(cfg, ui.clone()));
            }
            Cmd::ScanSlave(cfg) => {
                if let Some(s) = scan.take() {
                    s.abort();
                }
                scan = Some(spawn_scan_slave(cfg, ui.clone()));
            }
            Cmd::ScanStop => {
                if let Some(s) = scan.take() {
                    s.abort();
                }
                ui.scan_status("Stopped", false);
            }
            Cmd::SlaveStart(cfg) => {
                if let Some(s) = slave.take() {
                    s.stop();
                }
                slave = Some(spawn_slave(cfg, ui.clone()));
            }
            Cmd::SlaveStop => {
                if let Some(s) = slave.take() {
                    s.stop();
                }
                ui.slave_status("Stopped", false);
            }
            Cmd::SlaveEdit { address, text } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::Edit { address, text });
                }
            }
            Cmd::SlaveEditAt {
                area,
                address,
                text,
            } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::EditAt {
                        area,
                        address,
                        text,
                    });
                }
            }
            Cmd::SlaveName { address, name } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetName { address, name });
                }
            }
            Cmd::SlaveCellFormat { address, format } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetCellFormat { address, format });
                }
            }
            Cmd::ExportCsv {
                path,
                server,
                active_id,
                active_csv,
            } => {
                if server {
                    // 服务端: 交给 updater(它有 names + store 访问)扫 4 表导出
                    if let Some(s) = &slave {
                        s.send(SlaveMsg::ExportCsv(path));
                    }
                } else {
                    // 客户端: 活动窗口用 UI 传来的当前行(名字/功能码正确), 其余窗口从 cache 取。
                    let mut out = String::from("Import,Function,Address,Name,Value\n");
                    out.push_str(&active_csv);
                    for w in windows.iter().filter(|w| w.id != active_id) {
                        let area = func_area_be(w.cfg.function);
                        let c = w.cache.lock().unwrap();
                        for (i, r) in c.rows.iter().enumerate() {
                            let name = c
                                .names
                                .get(i)
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| r.name.to_string());
                            out.push_str(&format!(
                                "yes,{},{},{},{}\n",
                                area.csv_name(),
                                r.address,
                                csv_field(&name),
                                csv_field(&r.value)
                            ));
                        }
                    }
                    let result_ui = ui.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = std::fs::write(&path, out);
                        result_ui.file_result(false, "CSV 导出", path, result);
                    });
                }
            }
            Cmd::SlaveView {
                area,
                address,
                quantity,
                format,
            } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetView {
                        area,
                        address,
                        quantity,
                        format,
                    });
                }
            }
            Cmd::SlaveScaling(v) => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetScaling(v));
                }
            }
            Cmd::SlaveColors(v) => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetColors(v));
                }
            }
            Cmd::SlaveValueNames(v) => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SetValueNames(v));
                }
            }
            Cmd::SlaveSimStart(v) => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SimStart(v));
                }
            }
            Cmd::SlaveSimStop => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::SimStop);
                }
            }
            Cmd::SlaveAutoInc { address } => {
                if let Some(s) = &slave {
                    s.send(SlaveMsg::ToggleAutoInc { address });
                }
            }
        }
    }
}

// ===========================================================================
// Raw-byte tap — captures the exact ADU bytes for the traffic monitor.
// ===========================================================================

#[derive(Debug, Default)]
struct TrafficHealth {
    dropped_chunks: AtomicU64,
    dropped_bytes: AtomicU64,
    high_watermark: AtomicUsize,
}

#[derive(Clone, Debug)]
struct TrafficTx {
    tx: mpsc::Sender<(bool, Vec<u8>)>,
    health: Arc<TrafficHealth>,
}

impl TrafficTx {
    fn send(&self, item: (bool, Vec<u8>)) -> Result<(), ()> {
        let bytes = item.1.len() as u64;
        match self.tx.try_send(item) {
            Ok(()) => {
                self.health.high_watermark.fetch_max(
                    self.tx.max_capacity().saturating_sub(self.tx.capacity()),
                    Ordering::Relaxed,
                );
                Ok(())
            }
            Err(_) => {
                self.health.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                self.health
                    .dropped_bytes
                    .fetch_add(bytes, Ordering::Relaxed);
                Err(())
            }
        }
    }
}

struct TrafficRx {
    rx: mpsc::Receiver<(bool, Vec<u8>)>,
    health: Arc<TrafficHealth>,
}

fn traffic_channel() -> (TrafficTx, TrafficRx) {
    let (tx, rx) = mpsc::channel(TRAFFIC_QUEUE_CAPACITY);
    let health = Arc::new(TrafficHealth::default());
    (
        TrafficTx {
            tx,
            health: health.clone(),
        },
        TrafficRx { rx, health },
    )
}

#[derive(Debug)]
struct Tap<T> {
    inner: T,
    tx: TrafficTx,
}

impl<T: AsyncRead + Unpin> AsyncRead for Tap<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let pre = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &r {
            let post = buf.filled().len();
            if post > pre {
                let _ = this.tx.send((false, buf.filled()[pre..post].to_vec()));
            }
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tap<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = &r {
            if *n > 0 {
                let _ = this.tx.send((true, data[..*n].to_vec()));
            }
        }
        r
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// 把一个已 connect 的 UDP socket 包装成 AsyncRead+AsyncWrite 流，
/// 使 tokio-modbus 的 MBAP 帧编解码(tcp::attach_slave)能跑在 UDP 之上 = Modbus/UDP。
/// 语义: 一次 poll_write = 发一个请求数据报; 一次 poll_read = 收一个响应数据报。
/// Modbus 的请求/响应天然一来一回、每个 ADU 一个数据报，与 MBAP 长度字段一致。
#[derive(Debug)]
struct UdpStream {
    sock: tokio::net::UdpSocket,
}

impl AsyncRead for UdpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().sock.poll_recv(cx, buf)
    }
}

impl AsyncWrite for UdpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().sock.poll_send(cx, data)
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// 绑定本地任意端口并 connect 到目标，返回可做 MBAP 帧的 UDP 流。
async fn udp_connect(host: &str, port: u16) -> anyhow::Result<UdpStream> {
    let addr = resolve(host, port)?;
    let sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await?;
    sock.connect(addr).await?;
    Ok(UdpStream { sock })
}

async fn connect_tapped(
    t: &Transport,
    slave_id: u8,
    tls: Option<&crate::tls::TlsClientCfg>,
) -> anyhow::Result<(Context, TrafficRx, Option<String>)> {
    let (tap_tx, tap_rx) = traffic_channel();
    let mut tls_desc = None;
    let ctx = match t {
        Transport::Tcp { host, port } => {
            let addr = resolve(host, *port)?;
            let stream = tokio::net::TcpStream::connect(addr).await?;
            match tls {
                Some(tcfg) => {
                    let connector = crate::tls::client_connector(tcfg)?;
                    let name = crate::tls::server_name(tcfg, host)?;
                    let tls_stream = connector.connect(name, stream).await?;
                    tls_desc = Some(crate::tls::describe(tls_stream.get_ref().1));
                    tcp::attach_slave(
                        Tap {
                            inner: tls_stream,
                            tx: tap_tx,
                        },
                        Slave(slave_id),
                    )
                }
                None => tcp::attach_slave(
                    Tap {
                        inner: stream,
                        tx: tap_tx,
                    },
                    Slave(slave_id),
                ),
            }
        }
        Transport::Udp { host, port } => {
            // Modbus/UDP 用 MBAP 帧(同 TCP)，故仍走 tcp::attach_slave，只是底层是 UDP 流。
            let udp = udp_connect(host, *port).await?;
            tcp::attach_slave(
                Tap {
                    inner: udp,
                    tx: tap_tx,
                },
                Slave(slave_id),
            )
        }
        Transport::RtuOverTcp { host, port } => {
            // RTU 帧(CRC)走 TCP：用 rtu::attach_slave 做 RTU 编解码，底层是 TCP 流。
            let addr = resolve(host, *port)?;
            let stream = tokio::net::TcpStream::connect(addr).await?;
            rtu::attach_slave(
                Tap {
                    inner: stream,
                    tx: tap_tx,
                },
                Slave(slave_id),
            )
        }
        Transport::RtuOverUdp { host, port } => {
            let udp = udp_connect(host, *port).await?;
            rtu::attach_slave(
                Tap {
                    inner: udp,
                    tx: tap_tx,
                },
                Slave(slave_id),
            )
        }
        Transport::Rtu {
            path,
            baud,
            data_bits,
            parity,
            stop_bits,
        } => {
            let serial = SerialStream::open(&serial_builder(
                path, *baud, *data_bits, *parity, *stop_bits,
            ))?;
            rtu::attach_slave(
                Tap {
                    inner: serial,
                    tx: tap_tx,
                },
                Slave(slave_id),
            )
        }
    };
    Ok((ctx, tap_rx, tls_desc))
}

async fn connect_plain(t: &Transport, slave_id: u8) -> anyhow::Result<Context> {
    match t {
        Transport::Tcp { host, port } => {
            let addr = resolve(host, *port)?;
            Ok(tcp::connect_slave(addr, Slave(slave_id)).await?)
        }
        Transport::Udp { host, port } => {
            let udp = udp_connect(host, *port).await?;
            Ok(tcp::attach_slave(udp, Slave(slave_id)))
        }
        Transport::RtuOverTcp { host, port } => {
            let addr = resolve(host, *port)?;
            let stream = tokio::net::TcpStream::connect(addr).await?;
            Ok(rtu::attach_slave(stream, Slave(slave_id)))
        }
        Transport::RtuOverUdp { host, port } => {
            let udp = udp_connect(host, *port).await?;
            Ok(rtu::attach_slave(udp, Slave(slave_id)))
        }
        Transport::Rtu {
            path,
            baud,
            data_bits,
            parity,
            stop_bits,
        } => {
            let serial = SerialStream::open(&serial_builder(
                path, *baud, *data_bits, *parity, *stop_bits,
            ))?;
            Ok(rtu::attach_slave(serial, Slave(slave_id)))
        }
    }
}

fn resolve(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve {host}:{port}"))
}

fn fc_name(fc: u8) -> &'static str {
    match fc {
        1 => "Read Coils",
        2 => "Read Discrete Inputs",
        3 => "Read Holding Regs",
        4 => "Read Input Regs",
        5 => "Write Single Coil",
        6 => "Write Single Reg",
        7 => "Read Exception Status",
        8 => "Diagnostics",
        11 => "Get Comm Event Counter",
        15 => "Write Multiple Coils",
        16 => "Write Multiple Regs",
        17 => "Report Server ID",
        22 => "Mask Write Reg",
        23 => "Read/Write Multiple Regs",
        43 => "Encapsulated (MEI)",
        _ => "?",
    }
}
fn modbus_exc_name(e: u8) -> &'static str {
    match e {
        1 => "Illegal Function",
        2 => "Illegal Data Address",
        3 => "Illegal Data Value",
        4 => "Server Device Failure",
        5 => "Acknowledge",
        6 => "Server Busy",
        8 => "Memory Parity Error",
        10 => "Gateway Path Unavailable",
        11 => "Gateway Target Failed to Respond",
        _ => "?",
    }
}
fn u16be(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([
        b.get(i).copied().unwrap_or(0),
        b.get(i + 1).copied().unwrap_or(0),
    ])
}

/// 解析 Modbus ADU → 一行人类可读摘要(TID/Unit/Function/地址/数量/字节数/异常码)。
/// bytes = 完整 ADU(TCP 含 MBAP 头, RTU 含 unit+CRC)。无法解析返回 None。
fn parse_modbus_adu(is_tx: bool, bytes: &[u8], is_tcp: bool) -> Option<String> {
    let (tid, unit, pdu): (Option<u16>, u8, &[u8]) = if is_tcp {
        if bytes.len() < 8 {
            return None;
        }
        let body_len = u16be(bytes, 4) as usize;
        if u16be(bytes, 2) != 0 || !(2..=254).contains(&body_len) || bytes.len() != 6 + body_len {
            return None;
        }
        (Some(u16be(bytes, 0)), bytes[6], &bytes[7..])
    } else {
        if bytes.len() < 4 {
            return None;
        } // unit + fc + ... + CRC(2)
        (None, bytes[0], &bytes[1..bytes.len() - 2])
    };
    if pdu.is_empty() {
        return None;
    }
    let fc = pdu[0];
    let mut s = String::new();
    if let Some(t) = tid {
        s += &format!("TID={t} ");
    }
    s += &format!("U={unit} ");
    if fc & 0x80 != 0 {
        let e = pdu.get(1).copied().unwrap_or(0);
        s += &format!(
            "FC{:02} ⚠ Exception {:02X} ({})",
            fc & 0x7F,
            e,
            modbus_exc_name(e)
        );
        return Some(s);
    }
    s += &format!("FC{fc:02} {}", fc_name(fc));
    let p = &pdu[1..]; // fc 之后的参数
    match fc {
        1..=4 => {
            if is_tx {
                s += &format!(" addr={} qty={}", u16be(p, 0), u16be(p, 2));
            } else {
                s += &format!(" bytes={}", p.first().copied().unwrap_or(0));
            }
        }
        5 | 6 => {
            s += &format!(" addr={} val=0x{:04X}", u16be(p, 0), u16be(p, 2));
        }
        15 | 16 => {
            s += &format!(" addr={} qty={}", u16be(p, 0), u16be(p, 2));
        }
        22 => {
            s += &format!(
                " addr={} and=0x{:04X} or=0x{:04X}",
                u16be(p, 0),
                u16be(p, 2),
                u16be(p, 4)
            );
        }
        23 => {
            if is_tx {
                s += &format!(
                    " rd_addr={} rd_qty={} wr_addr={} wr_qty={}",
                    u16be(p, 0),
                    u16be(p, 2),
                    u16be(p, 4),
                    u16be(p, 6)
                );
            } else {
                s += &format!(" bytes={}", p.first().copied().unwrap_or(0));
            }
        }
        _ => {}
    }
    Some(s)
}

const MAX_MBAP_ADU: usize = 260;

#[derive(Default)]
struct MbapReassembler {
    bytes: Vec<u8>,
}

impl MbapReassembler {
    fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.bytes.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.bytes.len() < 7 {
                break;
            }
            let protocol = u16::from_be_bytes([self.bytes[2], self.bytes[3]]);
            let body_len = u16::from_be_bytes([self.bytes[4], self.bytes[5]]) as usize;
            let total = 6usize.saturating_add(body_len);
            if protocol != 0 || !(2..=254).contains(&body_len) || total > MAX_MBAP_ADU {
                self.bytes.remove(0);
                continue;
            }
            if self.bytes.len() < total {
                break;
            }
            frames.push(self.bytes.drain(..total).collect());
        }
        frames
    }
}

fn spawn_traffic_forwarder(mut rx: TrafficRx, sink: WindowSink, is_tcp: bool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf: Vec<crate::LogLine> = Vec::new();
        let mut tx_frames = MbapReassembler::default();
        let mut rx_frames = MbapReassembler::default();
        let mut health_tick = tokio::time::interval(Duration::from_secs(1));
        let mut flush_tick = tokio::time::interval(Duration::from_millis(50));
        let mut reported_drops = 0;
        let mut dirty = false;
        loop {
            tokio::select! {
                _ = flush_tick.tick(), if dirty => {
                    sink.traffic(buf.clone());
                    dirty = false;
                }
                _ = health_tick.tick() => {
                    let dropped = rx.health.dropped_chunks.load(Ordering::Relaxed);
                    if dropped > reported_drops {
                        let bytes = rx.health.dropped_bytes.load(Ordering::Relaxed);
                        let high = rx.health.high_watermark.load(Ordering::Relaxed);
                        buf.push(crate::LogLine {
                            time: now_hms().into(),
                            dir: "ERR".into(),
                            text: format!(
                                "Traffic monitor queue overflow: dropped {dropped} chunks / {bytes} bytes, H{high}/{TRAFFIC_QUEUE_CAPACITY}"
                            ).into(),
                        });
                        if buf.len() > 500 {
                            let excess = buf.len() - 500;
                            buf.drain(0..excess);
                        }
                        dirty = true;
                        reported_drops = dropped;
                    }
                }
                item = rx.rx.recv() => {
                    let Some((is_tx, chunk)) = item else { break };
                    let frames = if is_tcp {
                        if is_tx {
                            tx_frames.push(&chunk)
                        } else {
                            rx_frames.push(&chunk)
                        }
                    } else {
                        vec![chunk]
                    };
                    for bytes in frames {
                        let hex = bytes
                            .iter()
                            .map(|b| format!("{b:02X}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        let text = match parse_modbus_adu(is_tx, &bytes, is_tcp) {
                            Some(p) => format!("{p}   |   {hex}"),
                            None => hex,
                        };
                        buf.push(crate::LogLine {
                            time: now_hms().into(),
                            dir: if is_tx { "Tx" } else { "Rx" }.into(),
                            text: text.into(),
                        });
                        if buf.len() > 500 {
                            let excess = buf.len() - 500;
                            buf.drain(0..excess);
                        }
                        dirty = true;
                    }
                }
            }
        }
        if dirty {
            sink.traffic(buf);
        }
    })
}

// ===========================================================================
// Master (poll) engine
// ===========================================================================

enum MasterMsg {
    Write(WriteReq),
    WriteOnce {
        func: WriteFunc,
        items: Vec<WriteItem>,
    },
    AutoWrite {
        func: WriteFunc,
        items: Vec<WriteItem>,
        interval_ms: u64,
    },
    AutoWriteStop,
    MaskWrite {
        address: u16,
        and_mask: u16,
        or_mask: u16,
    },
    ReadWrite {
        read_addr: u16,
        read_qty: u16,
        write_addr: u16,
        write_values: Vec<u16>,
    },
    SetFormat(RegFormat),
    SetName {
        address: u16,
        name: String,
    },
    SetScaling(Scaling),
    SetColors(ColorRules),
    SetValueNames(ValueNames),
    SetCellFormat {
        address: u16,
        format: Option<RegFormat>,
    },
    SetDerived(Vec<DerivedCh>),
    ExportChart(String),
    SetChartAxis {
        addr: u16,
        right: bool,
    },
    SetChartPage(i32),
    FocusChartAddress(u16),
    SetReadDef {
        area: Area,
        address: u16,
        quantity: u16,
        scan_ms: u64,
        poll: bool,
    },
    StartLog(LogCfg),
    StopLog,
    Stop,
}

struct MasterHandle {
    ctrl: mpsc::Sender<MasterMsg>,
    sink: WindowSink,
}

impl MasterHandle {
    fn send(&self, m: MasterMsg) {
        if self.ctrl.try_send(m).is_err() {
            self.sink
                .message("Master control queue full: command rejected");
        }
    }
}

enum PollData {
    Regs(Vec<u16>),
    Bits(Vec<bool>),
}

enum PollErr {
    Exception(String),
    Io(String),
    Invalid(String),
}

struct ChartState {
    addrs: Vec<u16>,
    data: Vec<VecDeque<f64>>,
    right: std::collections::HashSet<u16>, // addrs assigned to the right Y axis
    page_start: usize,
    total: usize,
}

impl ChartState {
    fn new() -> Self {
        ChartState {
            addrs: Vec::new(),
            data: Vec::new(),
            right: std::collections::HashSet::new(),
            page_start: 0,
            total: 0,
        }
    }
    fn reset(&mut self) {
        self.addrs.clear();
        self.data.clear();
        self.page_start = 0;
        self.total = 0;
    }
    fn move_page(&mut self, direction: i32) -> bool {
        let max_start = self
            .total
            .saturating_sub(1)
            .div_euclid(CHART_SERIES_PER_PAGE)
            * CHART_SERIES_PER_PAGE;
        let next = if direction < 0 {
            self.page_start.saturating_sub(CHART_SERIES_PER_PAGE)
        } else if direction > 0 {
            self.page_start
                .saturating_add(CHART_SERIES_PER_PAGE)
                .min(max_start)
        } else {
            self.page_start
        };
        if next == self.page_start {
            return false;
        }
        self.page_start = next;
        true
    }
    fn focus_address(&mut self, rows: &[DisplayRow], address: u16) -> bool {
        let Some(index) = rows
            .iter()
            .filter(|row| row.num.is_some())
            .position(|row| row.address as u16 == address)
        else {
            return false;
        };
        let next = index.div_euclid(CHART_SERIES_PER_PAGE) * CHART_SERIES_PER_PAGE;
        if next == self.page_start {
            return false;
        }
        self.page_start = next;
        true
    }
}

/// Result of building the overlay (combined) chart series + the two axis ranges.
struct SeriesBundle {
    series: Vec<crate::ChartSeries>,
    left_min: String,
    left_max: String,
    right_min: String,
    right_max: String,
    has_right: bool,
}

/// Min/max over all data series assigned to the given axis (`right`).
fn axis_range(ch: &ChartState, right: bool) -> Option<(f64, f64)> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for (si, d) in ch.data.iter().enumerate() {
        let addr = ch.addrs.get(si).copied().unwrap_or(u16::MAX);
        if ch.right.contains(&addr) != right {
            continue;
        }
        for &v in d {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
    }
    if lo.is_finite() {
        Some((lo, hi))
    } else {
        None
    }
}

/// Build the overlay series, normalising each to its axis range.
fn build_series(ch: &ChartState) -> SeriesBundle {
    let lr = axis_range(ch, false);
    let rr = axis_range(ch, true);
    let mut series = Vec::new();
    for (si, d) in ch.data.iter().enumerate() {
        if d.len() < 2 {
            continue;
        }
        let addr = ch.addrs.get(si).copied().unwrap_or(0);
        let is_right = ch.right.contains(&addr);
        let (lo, hi) = if is_right { rr } else { lr }.unwrap_or((0.0, 1.0));
        let span = if (hi - lo).abs() < 1e-9 { 1.0 } else { hi - lo };
        let n = d.len();
        let mut cmd = String::with_capacity(n * 14);
        for (i, &v) in d.iter().enumerate() {
            let norm = ((v - lo) / span).clamp(0.0, 1.0);
            let x = i as f64 / (n - 1) as f64 * 1000.0;
            let y = (1.0 - norm) * 300.0;
            if i == 0 {
                cmd.push_str(&format!("M {x:.1} {y:.1}"));
            } else {
                cmd.push_str(&format!(" L {x:.1} {y:.1}"));
            }
        }
        series.push(crate::ChartSeries {
            name: format!("@{addr}").into(),
            addr: addr as i32,
            color: argb_to_color(CHART_COLORS[si % 12]),
            axis: if is_right { 1 } else { 0 },
            commands: cmd.into(),
        });
    }
    let fmt = |o: Option<(f64, f64)>| match o {
        Some((a, b)) => (format!("{a:.2}"), format!("{b:.2}")),
        None => ("—".to_string(), "—".to_string()),
    };
    let (left_min, left_max) = fmt(lr);
    let (right_min, right_max) = fmt(rr);
    SeriesBundle {
        series,
        left_min,
        left_max,
        right_min,
        right_max,
        has_right: rr.is_some(),
    }
}

struct Logger {
    file: std::io::BufWriter<std::fs::File>, // 缓冲写：批量落盘，减少异步工作线程上的阻塞 syscall
    cfg: LogCfg,
    last_write: Option<Instant>,
    last_flush: Option<Instant>,
    last_vals: Option<Vec<f64>>,
    header_written: bool,
}

impl Logger {
    fn maybe_log(&mut self, rows: &[DisplayRow]) -> std::io::Result<()> {
        let now = Instant::now();
        let due = self.cfg.each_read
            || self
                .last_write
                .is_none_or(|t| now.duration_since(t).as_secs() >= self.cfg.period_s.max(1) as u64);
        if !due {
            return Ok(());
        }
        let nums: Vec<f64> = rows.iter().filter_map(|r| r.num).collect();
        if self.cfg.on_change && self.last_vals.as_ref() == Some(&nums) {
            return Ok(());
        }
        let d = self.cfg.delimiter.to_string();
        if !self.header_written {
            let mut h = String::new();
            if self.cfg.timestamp {
                h.push_str("Timestamp");
                h.push_str(&d);
            }
            let addrs: Vec<String> = rows
                .iter()
                .filter(|r| r.num.is_some())
                .map(|r| format!("@{}", r.address))
                .collect();
            h.push_str(&addrs.join(&d));
            writeln!(self.file, "{h}")?;
            self.header_written = true;
        }
        let mut line = String::new();
        if self.cfg.timestamp {
            line.push_str(&now_timestamp());
            line.push_str(&d);
        }
        let vals: Vec<String> = nums.iter().map(|n| format!("{n}")).collect();
        line.push_str(&vals.join(&d));
        writeln!(self.file, "{line}")?;
        // 限频 flush(~1s)：避免每轮阻塞 syscall；崩溃最多丢最近 ~1s 行，停止/Drop 时也会 flush。
        if self
            .last_flush
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
        {
            self.file.flush()?;
            self.last_flush = Some(now);
        }
        self.last_write = Some(now);
        self.last_vals = Some(nums);
        Ok(())
    }
}

fn spawn_master(cfg: MasterCfg, sink: WindowSink) -> MasterHandle {
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<MasterMsg>(ENGINE_CONTROL_QUEUE_CAPACITY);
    let handle_sink = sink.clone();
    tokio::spawn(async move {
        let desc = cfg.transport.describe();
        if cfg.poll {
            if let Err(error) = validate_read_definition(cfg.area, cfg.address, cfg.quantity) {
                sink.status(format!("Invalid read definition: {error}"), false);
                return;
            }
        }
        sink.status(format!("Connecting — {desc} …"), false);

        let (mut ctx, traffic_rx, tls_desc) =
            match connect_tapped(&cfg.transport, cfg.slave_id, cfg.tls.as_ref()).await {
                Ok(c) => c,
                Err(e) => {
                    sink.status(format!("Connect failed: {e}"), false);
                    let mut lb = Vec::new();
                    sink.log(push_log(&mut lb, "ERR", e.to_string()));
                    return;
                }
            };
        let timeout_ms = if cfg.timeout_ms == 0 {
            RESPONSE_TIMEOUT_MS
        } else {
            cfg.timeout_ms
        };
        let reconnect = cfg.reconnect;
        let reconnect_ms = cfg.reconnect_ms.max(200);
        // MBAP 帧(TCP/UDP 同) vs RTU CRC: UDP 也是 MBAP，按 TCP 口径解析
        let is_tcp = matches!(cfg.transport, Transport::Tcp { .. } | Transport::Udp { .. });
        // 流量转发器: tokio JoinHandle drop 即 detach, 任务照跑; 旧连接断开时其通道关闭会自然结束
        drop(spawn_traffic_forwarder(traffic_rx, sink.clone(), is_tcp));
        let conn_desc = match &tls_desc {
            Some(td) => format!("{desc} · {td}"),
            None => desc.clone(),
        };
        if let Some(td) = &tls_desc {
            let mut lb = Vec::new();
            sink.log(push_log(&mut lb, "TX", format!("TLS established — {td}")));
        }
        sink.status(format!("Connected — {conn_desc}"), true);

        let mut tx = 0u64;
        let mut rx = 0u64;
        let mut err = 0u64;
        let mut consec_fail = 0u32; // 连续通信失败计数(用于报警/重连)
        let mut fmt = cfg.format;
        let mut scaling = cfg.scaling;
        let mut colors = cfg.colors;
        let mut value_names = cfg.value_names;
        let mut area = cfg.area;
        let mut address = cfg.address;
        let mut quantity = cfg.quantity;
        let mut scan_ms = cfg.scan_ms;
        let mut poll = cfg.poll;
        let mut last: Option<PollData> = None;
        let mut names: HashMap<u16, String> = HashMap::new();
        let mut cell_formats: HashMap<u16, RegFormat> = HashMap::new();
        let mut derived: Vec<DerivedCh> = Vec::new();
        let mut logbuf: Vec<crate::LogLine> = Vec::new();
        let mut chart = ChartState::new();
        let mut logger: Option<Logger> = None;
        // Periodic auto-write (client write list): items + whether it's running.
        let mut auto_write: Vec<WriteItem> = Vec::new();
        let mut aw_func = WriteFunc::MultiRegs;
        let mut aw_on = false;
        let mut aw_tick = tokio::time::interval(Duration::from_secs(3600));

        let mut tick = tokio::time::interval(Duration::from_millis(scan_ms.max(20)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = aw_tick.tick(), if aw_on && !auto_write.is_empty() => {
                    tx += 1;
                    match write_items(&mut ctx, aw_func, &auto_write, timeout_ms).await {
                        Ok(n) => {
                            rx += 1;
                            sink.log(push_log(&mut logbuf, "TX", format!("Auto-write {n} registers")));
                            // advance auto-incrementing entries for the next cycle
                            for it in auto_write.iter_mut() {
                                if it.inc { it.value = it.value.wrapping_add(1); }
                            }
                        }
                        Err(e) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("Auto-write failed: {e}"))); }
                    }
                    sink.counts(tx, rx, err);
                }
                _ = tick.tick(), if poll => {
                    tx += 1;
                    match poll_once(&mut ctx, area, address, quantity, timeout_ms).await {
                        Ok(data) => {
                            rx += 1;
                            consec_fail = 0; // 成功一次就清零连续失败
                            let drows = render_master_rows(&data, address, fmt, &cell_formats, &scaling);
                            let (rr, nn) = master_grid(drows.clone(), &poll_values(&data), &names, &colors, &value_names, &derived, area);
                            sink.rows(rr, nn);
                            update_chart(&mut chart, &drows);
                            sink.chart(&chart);
                            let log_error = logger
                                .as_mut()
                                .and_then(|active| active.maybe_log(&drows).err());
                            if let Some(error) = log_error {
                                logger = None;
                                sink.log(push_log(
                                    &mut logbuf,
                                    "ERR",
                                    format!("Logging stopped after file write failure: {error}"),
                                ));
                            }
                            sink.log(push_log(&mut logbuf, "RX", describe_poll(area, &data)));
                            sink.status(format!("Polling — {conn_desc}"), true);
                            last = Some(data);
                        }
                        Err(PollErr::Exception(s)) => {
                            // Modbus 异常是从机的合法应答, 不是连接问题, 不计连续失败/不重连
                            err += 1;
                            sink.log(push_log(&mut logbuf, "ERR", format!("Modbus exception: {s}")));
                        }
                        Err(PollErr::Io(s)) => {
                            err += 1;
                            consec_fail += 1;
                            sink.log(push_log(&mut logbuf, "ERR", format!("Comm error: {s}")));
                            // 通信错误(超时/传输断开)：标记未连接，不再误显示"已连接"。
                            sink.status(format!("Error — {s} (连续失败 {consec_fail})"), false);
                            if consec_fail == 5 {
                                sink.log(push_log(&mut logbuf, "ERR", "⚠ 连续 5 次通信失败".into()));
                            }
                            if reconnect {
                                sink.log(push_log(&mut logbuf, "TX", format!("将在 {reconnect_ms} ms 后自动重连…")));
                                tokio::time::sleep(Duration::from_millis(reconnect_ms)).await;
                                match connect_tapped(&cfg.transport, cfg.slave_id, cfg.tls.as_ref()).await {
                                    Ok((nctx, ntraffic, _ntls)) => {
                                        ctx = nctx;
                                        drop(spawn_traffic_forwarder(ntraffic, sink.clone(), is_tcp));
                                        consec_fail = 0;
                                        sink.status(format!("Reconnected — {conn_desc}"), true);
                                        sink.log(push_log(&mut logbuf, "TX", "已重连".into()));
                                    }
                                    Err(e) => {
                                        sink.log(push_log(&mut logbuf, "ERR", format!("重连失败: {e}")));
                                    }
                                }
                            }
                        }
                        Err(PollErr::Invalid(s)) => {
                            err += 1;
                            poll = false;
                            sink.log(push_log(&mut logbuf, "ERR", format!("Invalid read definition: {s}")));
                            sink.status(format!("Invalid read definition: {s}"), true);
                        }
                    }
                    sink.counts(tx, rx, err);
                }
                msg = ctrl_rx.recv() => {
                    match msg {
                        Some(MasterMsg::Stop) | None => break,
                        Some(MasterMsg::SetFormat(f)) => {
                            fmt = f;
                            chart.reset();
                            rerender(&last, address, fmt, &scaling, &names, &colors, &value_names, &cell_formats, &derived, area, &mut chart, &sink);
                        }
                        Some(MasterMsg::SetScaling(s)) => {
                            scaling = s;
                            chart.reset();
                            rerender(&last, address, fmt, &scaling, &names, &colors, &value_names, &cell_formats, &derived, area, &mut chart, &sink);
                        }
                        Some(MasterMsg::SetColors(c)) => {
                            colors = c;
                            if let Some(d) = &last {
                                let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                let (rr, nn) = master_grid(drows, &poll_values(d), &names, &colors, &value_names, &derived, area);
                                sink.rows(rr, nn);
                            }
                        }
                        Some(MasterMsg::SetValueNames(v)) => {
                            value_names = v;
                            if let Some(d) = &last {
                                let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                let (rr, nn) = master_grid(drows, &poll_values(d), &names, &colors, &value_names, &derived, area);
                                sink.rows(rr, nn);
                            }
                        }
                        Some(MasterMsg::SetDerived(list)) => {
                            derived = list;
                            if let Some(d) = &last {
                                let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                let (rr, nn) = master_grid(drows, &poll_values(d), &names, &colors, &value_names, &derived, area);
                                sink.rows(rr, nn);
                            }
                        }
                        Some(MasterMsg::ExportChart(path)) => {
                            let mut s = String::from("Sample");
                            for a in &chart.addrs { s.push_str(&format!(",@{a}")); }
                            s.push('\n');
                            let len = chart.data.iter().map(|d| d.len()).max().unwrap_or(0);
                            for row in 0..len {
                                s.push_str(&format!("{row}"));
                                for d in &chart.data {
                                    match d.get(row) {
                                        Some(v) => s.push_str(&format!(",{v}")),
                                        None => s.push(','),
                                    }
                                }
                                s.push('\n');
                            }
                            // 一次性导出(CSV 可能较大)移到阻塞线程池，避免卡住本引擎轮询。
                            let result_sink = sink.clone();
                            tokio::task::spawn_blocking(move || {
                                let result = std::fs::write(&path, s);
                                result_sink.file_result("图表导出", path, result);
                            });
                        }
                        Some(MasterMsg::SetChartAxis { addr: a, right }) => {
                            if right { chart.right.insert(a); } else { chart.right.remove(&a); }
                            sink.chart(&chart);
                        }
                        Some(MasterMsg::SetChartPage(direction)) => {
                            if chart.move_page(direction) {
                                if let Some(d) = &last {
                                    let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                    update_chart(&mut chart, &drows);
                                }
                                sink.chart(&chart);
                            }
                        }
                        Some(MasterMsg::FocusChartAddress(addr)) => {
                            if let Some(d) = &last {
                                let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                if chart.focus_address(&drows, addr) {
                                    update_chart(&mut chart, &drows);
                                    sink.chart(&chart);
                                }
                            }
                        }
                        Some(MasterMsg::SetName { address: a, name }) => {
                            if name.trim().is_empty() { names.remove(&a); } else { names.insert(a, name); }
                            if let Some(d) = &last {
                                let drows = render_master_rows(d, address, fmt, &cell_formats, &scaling);
                                let (rr, nn) = master_grid(drows, &poll_values(d), &names, &colors, &value_names, &derived, area);
                                sink.rows(rr, nn);
                            }
                        }
                        Some(MasterMsg::SetCellFormat { address: a, format }) => {
                            match format {
                                Some(f) => { cell_formats.insert(a, f); }
                                None => { cell_formats.remove(&a); }
                            }
                            chart.reset(); // value spans may change → chart series change
                            rerender(&last, address, fmt, &scaling, &names, &colors, &value_names, &cell_formats, &derived, area, &mut chart, &sink);
                        }
                        Some(MasterMsg::SetReadDef { area: na, address: nad, quantity: nq, scan_ms: ns, poll: np }) => {
                            match validate_read_definition(na, nad, nq) {
                                Ok(()) => {
                                    area = na;
                                    address = nad;
                                    quantity = nq;
                                    poll = np;
                                    if ns != scan_ms {
                                        scan_ms = ns;
                                        tick = tokio::time::interval(Duration::from_millis(scan_ms.max(20)));
                                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                    }
                                    chart.reset();
                                    last = None;
                                }
                                Err(error) => {
                                    sink.log(push_log(&mut logbuf, "ERR", format!("Invalid read definition: {error}")));
                                    sink.status(format!("Invalid read definition: {error}"), true);
                                }
                            }
                        }
                        Some(MasterMsg::StartLog(c)) => {
                            match std::fs::File::create(&c.path) {
                                Ok(file) => {
                                    sink.log(push_log(&mut logbuf, "TX", format!("Logging → {}", c.path)));
                                    logger = Some(Logger { file: std::io::BufWriter::new(file), cfg: c, last_write: None, last_flush: None, last_vals: None, header_written: false });
                                }
                                Err(e) => { sink.log(push_log(&mut logbuf, "ERR", format!("Log open failed: {e}"))); }
                            }
                        }
                        Some(MasterMsg::StopLog) => {
                            if logger.take().is_some() {
                                sink.log(push_log(&mut logbuf, "TX", "Logging stopped".into()));
                            }
                        }
                        Some(MasterMsg::Write(req)) => {
                            tx += 1;
                            match do_write(&mut ctx, &req, timeout_ms).await {
                                Ok(d) => { rx += 1; sink.log(push_log(&mut logbuf, "TX", d)); }
                                Err(e) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("Write failed: {e}"))); }
                            }
                            sink.counts(tx, rx, err);
                        }
                        Some(MasterMsg::WriteOnce { func, items }) => {
                            tx += 1;
                            match write_items(&mut ctx, func, &items, timeout_ms).await {
                                Ok(n) => { rx += 1; sink.log(push_log(&mut logbuf, "TX", format!("Wrote {n} registers"))); }
                                Err(e) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("Write list failed: {e}"))); }
                            }
                            sink.counts(tx, rx, err);
                        }
                        Some(MasterMsg::AutoWrite { func, items, interval_ms }) => {
                            auto_write = items;
                            aw_func = func;
                            aw_on = true;
                            aw_tick = tokio::time::interval(Duration::from_millis(interval_ms.max(50)));
                            aw_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            sink.log(push_log(&mut logbuf, "TX", format!("Auto-write started ({} items, {interval_ms} ms)", auto_write.len())));
                        }
                        Some(MasterMsg::AutoWriteStop) => {
                            aw_on = false;
                            sink.log(push_log(&mut logbuf, "TX", "Auto-write stopped".into()));
                        }
                        Some(MasterMsg::MaskWrite { address, and_mask, or_mask }) => {
                            tx += 1;
                            let dur = Duration::from_millis(timeout_ms.max(20));
                            let r = flatten_unit(tokio::time::timeout(dur, ctx.masked_write_register(address, and_mask, or_mask)).await);
                            match r {
                                Ok(()) => { rx += 1; sink.log(push_log(&mut logbuf, "TX", format!("FC22 Mask write @{address}: AND=0x{and_mask:04X} OR=0x{or_mask:04X}"))); }
                                Err(e) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("FC22 Mask write failed: {e}"))); }
                            }
                            sink.counts(tx, rx, err);
                        }
                        Some(MasterMsg::ReadWrite { read_addr, read_qty, write_addr, write_values }) => {
                            let validation = validate_span(read_addr, read_qty as usize, 125, "FC23 read")
                                .and_then(|()| validate_span(write_addr, write_values.len(), 121, "FC23 write"));
                            if let Err(error) = validation {
                                err += 1;
                                sink.log(push_log(&mut logbuf, "ERR", format!("FC23 validation failed: {error}")));
                                sink.status(format!("FC23 validation failed: {error}"), true);
                                sink.counts(tx, rx, err);
                                continue;
                            }
                            tx += 1;
                            let dur = Duration::from_millis(timeout_ms.max(20));
                            match tokio::time::timeout(dur, ctx.read_write_multiple_registers(read_addr, read_qty, write_addr, &write_values)).await {
                                Ok(Ok(Ok(regs))) => {
                                    rx += 1;
                                    let shown = regs.iter().map(|r| format!("0x{r:04X}")).collect::<Vec<_>>().join(" ");
                                    sink.log(push_log(&mut logbuf, "RX", format!("FC23 wrote {} @{write_addr}, read {read_qty}@{read_addr}: {shown}", write_values.len())));
                                }
                                Ok(Ok(Err(e))) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("FC23 exception: {e:?}"))); }
                                Ok(Err(e)) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", format!("FC23 failed: {e}"))); }
                                Err(_) => { err += 1; sink.log(push_log(&mut logbuf, "ERR", "FC23 timeout".into())); }
                            }
                            sink.counts(tx, rx, err);
                        }
                    }
                }
            }
        }

        let _ = ctx.disconnect().await;
        sink.status(format!("Disconnected — {desc}"), false);
    });

    MasterHandle {
        ctrl: ctrl_tx,
        sink: handle_sink,
    }
}

#[allow(clippy::too_many_arguments)]
fn rerender(
    last: &Option<PollData>,
    start: u16,
    fmt: RegFormat,
    scaling: &Scaling,
    names: &HashMap<u16, String>,
    colors: &ColorRules,
    value_names: &ValueNames,
    overrides: &HashMap<u16, RegFormat>,
    derived: &[DerivedCh],
    area: Area,
    chart: &mut ChartState,
    sink: &WindowSink,
) {
    if let Some(d) = last {
        let drows = render_master_rows(d, start, fmt, overrides, scaling);
        let (rr, nn) = master_grid(
            drows.clone(),
            &poll_values(d),
            names,
            colors,
            value_names,
            derived,
            area,
        );
        sink.rows(rr, nn);
        update_chart(chart, &drows);
        sink.chart(chart);
    }
}

fn render_master_rows(
    data: &PollData,
    start: u16,
    fmt: RegFormat,
    overrides: &HashMap<u16, RegFormat>,
    scaling: &Scaling,
) -> Vec<DisplayRow> {
    match data {
        PollData::Regs(v) => render_registers(start, v, fmt, overrides, scaling),
        PollData::Bits(v) => render_bits(start, v),
    }
}

fn validate_span(address: u16, quantity: usize, maximum: usize, label: &str) -> Result<(), String> {
    if quantity == 0 || quantity > maximum {
        return Err(format!(
            "{label} quantity must be 1..{maximum}, got {quantity}"
        ));
    }
    if address as usize + quantity > 65_536 {
        return Err(format!(
            "{label} address range exceeds 65535: {address} + {quantity}"
        ));
    }
    Ok(())
}

fn validate_read_definition(area: Area, address: u16, quantity: u16) -> Result<(), String> {
    let maximum = match area {
        Area::Coils | Area::DiscreteInputs => 2000,
        Area::HoldingRegisters | Area::InputRegisters => 125,
    };
    validate_span(address, quantity as usize, maximum, "read")
}

async fn poll_once(
    ctx: &mut Context,
    area: Area,
    addr: u16,
    qty: u16,
    timeout_ms: u64,
) -> Result<PollData, PollErr> {
    validate_read_definition(area, addr, qty).map_err(PollErr::Invalid)?;
    let dur = Duration::from_millis(timeout_ms.max(20));
    match area {
        Area::Coils => match tokio::time::timeout(dur, ctx.read_coils(addr, qty)).await {
            Ok(r) => flat_bits(r),
            Err(_) => Err(PollErr::Io("Timeout".into())),
        },
        Area::DiscreteInputs => {
            match tokio::time::timeout(dur, ctx.read_discrete_inputs(addr, qty)).await {
                Ok(r) => flat_bits(r),
                Err(_) => Err(PollErr::Io("Timeout".into())),
            }
        }
        Area::HoldingRegisters => {
            match tokio::time::timeout(dur, ctx.read_holding_registers(addr, qty)).await {
                Ok(r) => flat_regs(r),
                Err(_) => Err(PollErr::Io("Timeout".into())),
            }
        }
        Area::InputRegisters => {
            match tokio::time::timeout(dur, ctx.read_input_registers(addr, qty)).await {
                Ok(r) => flat_regs(r),
                Err(_) => Err(PollErr::Io("Timeout".into())),
            }
        }
    }
}

fn flat_regs(r: tokio_modbus::Result<Vec<u16>>) -> Result<PollData, PollErr> {
    match r {
        Ok(Ok(v)) => Ok(PollData::Regs(v)),
        Ok(Err(e)) => Err(PollErr::Exception(format!("{e:?}"))),
        Err(e) => Err(PollErr::Io(e.to_string())),
    }
}

fn flat_bits(r: tokio_modbus::Result<Vec<bool>>) -> Result<PollData, PollErr> {
    match r {
        Ok(Ok(v)) => Ok(PollData::Bits(v)),
        Ok(Err(e)) => Err(PollErr::Exception(format!("{e:?}"))),
        Err(e) => Err(PollErr::Io(e.to_string())),
    }
}

async fn do_write(ctx: &mut Context, req: &WriteReq, timeout_ms: u64) -> Result<String, String> {
    let dur = Duration::from_millis(timeout_ms.max(20));
    // Typed write: encode one number into registers and write via FC16.
    if let Some(fmt) = req.encode {
        // 精确整数解析 + 范围校验(不经 f64)：避免 u64 高半区被拒、>2^53 精度丢失、超范围静默饱和写错值。
        let words = encode_typed(&req.text, fmt)?;
        validate_span(req.address, words.len(), 123, "FC16 write")?;
        flatten_unit(
            tokio::time::timeout(dur, ctx.write_multiple_registers(req.address, &words)).await,
        )?;
        return Ok(format!(
            "Write {} regs @{} = {}",
            words.len(),
            req.address,
            req.text.trim()
        ));
    }
    match req.func {
        WriteFunc::SingleCoil => {
            let v = parse_bit(&req.text)
                .ok_or_else(|| "invalid coil value (use 0/1/on/off)".to_string())?;
            flatten_unit(tokio::time::timeout(dur, ctx.write_single_coil(req.address, v)).await)?;
            Ok(format!("Write Single Coil @{} = {}", req.address, v as u8))
        }
        WriteFunc::SingleReg => {
            let v = parse_word(&req.text).ok_or_else(|| "invalid register value".to_string())?;
            flatten_unit(
                tokio::time::timeout(dur, ctx.write_single_register(req.address, v)).await,
            )?;
            Ok(format!("Write Single Register @{} = {}", req.address, v))
        }
        WriteFunc::MultiCoils => {
            let vs = parse_bit_list(&req.text)
                .ok_or_else(|| "invalid coil list (e.g. 1,0,1)".to_string())?;
            validate_span(req.address, vs.len(), 1968, "FC15 write")?;
            flatten_unit(
                tokio::time::timeout(dur, ctx.write_multiple_coils(req.address, &vs)).await,
            )?;
            Ok(format!("Write {} Coils @{}", vs.len(), req.address))
        }
        WriteFunc::MultiRegs => {
            let vs = parse_word_list(&req.text)
                .ok_or_else(|| "invalid register list (e.g. 10,20,30)".to_string())?;
            validate_span(req.address, vs.len(), 123, "FC16 write")?;
            flatten_unit(
                tokio::time::timeout(dur, ctx.write_multiple_registers(req.address, &vs)).await,
            )?;
            Ok(format!("Write {} Registers @{}", vs.len(), req.address))
        }
    }
}

/// Write the list using the chosen function code. Single-* write each item in its
/// own request; Multi-* write the contiguous block in one request (base = first addr).
fn validate_write_items(func: WriteFunc, items: &[WriteItem]) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    if matches!(func, WriteFunc::MultiRegs | WriteFunc::MultiCoils) {
        if !items
            .windows(2)
            .all(|pair| pair[0].address.checked_add(1) == Some(pair[1].address))
        {
            return Err("multiple-write addresses are not contiguous".into());
        }
        let maximum = if matches!(func, WriteFunc::MultiCoils) {
            1968
        } else {
            123
        };
        validate_span(items[0].address, items.len(), maximum, "multiple write")?;
    }
    Ok(())
}

async fn write_items(
    ctx: &mut Context,
    func: WriteFunc,
    items: &[WriteItem],
    timeout_ms: u64,
) -> Result<usize, String> {
    let dur = Duration::from_millis(timeout_ms.max(20));
    if items.is_empty() {
        return Ok(0);
    }
    validate_write_items(func, items)?;
    let base = items[0].address;
    match func {
        WriteFunc::SingleReg => {
            for it in items {
                flatten_unit(
                    tokio::time::timeout(dur, ctx.write_single_register(it.address, it.value))
                        .await,
                )?;
            }
        }
        WriteFunc::MultiRegs => {
            let words: Vec<u16> = items.iter().map(|i| i.value).collect();
            flatten_unit(
                tokio::time::timeout(dur, ctx.write_multiple_registers(base, &words)).await,
            )?;
        }
        WriteFunc::SingleCoil => {
            for it in items {
                flatten_unit(
                    tokio::time::timeout(dur, ctx.write_single_coil(it.address, it.value != 0))
                        .await,
                )?;
            }
        }
        WriteFunc::MultiCoils => {
            let bits: Vec<bool> = items.iter().map(|i| i.value != 0).collect();
            flatten_unit(tokio::time::timeout(dur, ctx.write_multiple_coils(base, &bits)).await)?;
        }
    }
    Ok(items.len())
}

fn flatten_unit(
    r: Result<tokio_modbus::Result<()>, tokio::time::error::Elapsed>,
) -> Result<(), String> {
    match r {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(format!("{e:?}")),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("Timeout".into()),
    }
}

fn describe_poll(area: Area, data: &PollData) -> String {
    let n = match data {
        PollData::Regs(v) => v.len(),
        PollData::Bits(v) => v.len(),
    };
    format!("Read {} ×{n}", area.label())
}

// ----- chart -----

fn update_chart(ch: &mut ChartState, rows: &[DisplayRow]) {
    let all_points: Vec<(u16, f64)> = rows
        .iter()
        .filter_map(|r| r.num.map(|n| (r.address as u16, n)))
        .collect();
    ch.total = all_points.len();
    let max_start =
        ch.total.saturating_sub(1).div_euclid(CHART_SERIES_PER_PAGE) * CHART_SERIES_PER_PAGE;
    ch.page_start = ch.page_start.min(max_start);
    let page_end = ch
        .page_start
        .saturating_add(CHART_SERIES_PER_PAGE)
        .min(all_points.len());
    let pts = &all_points[ch.page_start..page_end];
    let addrs: Vec<u16> = pts.iter().map(|p| p.0).collect();
    if ch.addrs != addrs {
        ch.addrs = addrs;
        ch.data = ch
            .addrs
            .iter()
            .map(|_| VecDeque::with_capacity(CHART_LEN))
            .collect();
    }
    for (i, (_, n)) in pts.iter().enumerate() {
        let d = &mut ch.data[i];
        d.push_back(*n);
        while d.len() > CHART_LEN {
            d.pop_front();
        }
    }
}

/// Build one mini chart per register, each auto-scaled to its own Y range.
fn build_charts(ch: &ChartState) -> (Vec<crate::MiniChart>, bool) {
    let mut out = Vec::with_capacity(ch.data.len());
    for (si, d) in ch.data.iter().enumerate() {
        let n = d.len();
        if n < 2 {
            continue;
        }
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for &v in d {
            if v < min {
                min = v;
            }
            if v > max {
                max = v;
            }
        }
        if !min.is_finite() {
            min = 0.0;
            max = 1.0;
        }
        if (max - min).abs() < 1e-9 {
            min -= 1.0;
            max += 1.0;
        }
        let span = max - min;
        let mut cmd = String::with_capacity(n * 14);
        for (i, &v) in d.iter().enumerate() {
            let x = i as f64 / (n - 1) as f64 * 1000.0;
            let y = 300.0 - (v - min) / span * 300.0;
            if i == 0 {
                cmd.push_str(&format!("M {x:.1} {y:.1}"));
            } else {
                cmd.push_str(&format!(" L {x:.1} {y:.1}"));
            }
        }
        out.push(crate::MiniChart {
            name: format!("@{}", ch.addrs.get(si).copied().unwrap_or(0)).into(),
            linecolor: argb_to_color(CHART_COLORS[si % 12]),
            commands: cmd.into(),
            ymin: format!("{min:.2}").into(),
            ymax: format!("{max:.2}").into(),
        });
    }
    let has = !out.is_empty();
    (out, has)
}

// ===========================================================================
// Scan engine
// ===========================================================================

enum ScanErr {
    Exception(String),
    Timeout,
    Io(String),
}

async fn probe(ctx: &mut Context, area: Area, addr: u16) -> Result<String, ScanErr> {
    let dur = Duration::from_millis(SCAN_TIMEOUT_MS);
    match area {
        Area::Coils => match tokio::time::timeout(dur, ctx.read_coils(addr, 1)).await {
            Ok(Ok(Ok(v))) => Ok(format!(
                "coil = {}",
                v.first().map(|b| *b as u8).unwrap_or(0)
            )),
            Ok(Ok(Err(e))) => Err(ScanErr::Exception(format!("{e:?}"))),
            Ok(Err(e)) => Err(ScanErr::Io(e.to_string())),
            Err(_) => Err(ScanErr::Timeout),
        },
        Area::DiscreteInputs => {
            match tokio::time::timeout(dur, ctx.read_discrete_inputs(addr, 1)).await {
                Ok(Ok(Ok(v))) => Ok(format!(
                    "input = {}",
                    v.first().map(|b| *b as u8).unwrap_or(0)
                )),
                Ok(Ok(Err(e))) => Err(ScanErr::Exception(format!("{e:?}"))),
                Ok(Err(e)) => Err(ScanErr::Io(e.to_string())),
                Err(_) => Err(ScanErr::Timeout),
            }
        }
        Area::HoldingRegisters => {
            match tokio::time::timeout(dur, ctx.read_holding_registers(addr, 1)).await {
                Ok(Ok(Ok(v))) => Ok(format!("0x{0:04X} ({0})", v.first().copied().unwrap_or(0))),
                Ok(Ok(Err(e))) => Err(ScanErr::Exception(format!("{e:?}"))),
                Ok(Err(e)) => Err(ScanErr::Io(e.to_string())),
                Err(_) => Err(ScanErr::Timeout),
            }
        }
        Area::InputRegisters => {
            match tokio::time::timeout(dur, ctx.read_input_registers(addr, 1)).await {
                Ok(Ok(Ok(v))) => Ok(format!("0x{0:04X} ({0})", v.first().copied().unwrap_or(0))),
                Ok(Ok(Err(e))) => Err(ScanErr::Exception(format!("{e:?}"))),
                Ok(Err(e)) => Err(ScanErr::Io(e.to_string())),
                Err(_) => Err(ScanErr::Timeout),
            }
        }
    }
}

fn scan_row(addr_or_id: String, res: &Result<String, ScanErr>) -> crate::LogLine {
    let (dir, text) = match res {
        Ok(s) => ("OK", format!("{addr_or_id}: {s}")),
        Err(ScanErr::Exception(e)) => ("ERR", format!("{addr_or_id}: exception {e}")),
        Err(ScanErr::Timeout) => ("…", format!("{addr_or_id}: no response")),
        Err(ScanErr::Io(e)) => ("ERR", format!("{addr_or_id}: {e}")),
    };
    crate::LogLine {
        time: now_hms().into(),
        dir: dir.into(),
        text: text.into(),
    }
}

fn spawn_scan_address(cfg: ScanCfg, ui: UiSink) -> JoinHandle<()> {
    tokio::spawn(async move {
        ui.scan_rows(Vec::new());
        ui.scan_status("Connecting for address scan …", true);
        let mut ctx = match connect_plain(&cfg.transport, cfg.slave_id).await {
            Ok(c) => c,
            Err(e) => {
                ui.scan_status(format!("Connect failed: {e}"), false);
                return;
            }
        };
        let mut rows: Vec<crate::LogLine> = Vec::new();
        let mut found = 0u32;
        for off in 0..cfg.count {
            // 不跨 16 位地址空间回绕重扫低地址：start+off 超过 0xFFFF 即停。
            let addr32 = cfg.start as u32 + off as u32;
            if addr32 > 0xFFFF {
                break;
            }
            let addr = addr32 as u16;
            let res = probe(&mut ctx, cfg.area, addr).await;
            if res.is_ok() {
                found += 1;
            }
            rows.push(scan_row(format!("@{addr}"), &res));
            ui.scan_rows(rows.clone());
            ui.scan_status(
                format!(
                    "Address scan {}/{} — {} responded",
                    off + 1,
                    cfg.count,
                    found
                ),
                true,
            );
        }
        let _ = ctx.disconnect().await;
        ui.scan_status(
            format!("Address scan done — {} of {} responded", found, cfg.count),
            false,
        );
    })
}

fn spawn_scan_slave(cfg: SlaveScanCfg, ui: UiSink) -> JoinHandle<()> {
    tokio::spawn(async move {
        ui.scan_rows(Vec::new());
        ui.scan_status("Connecting for slave scan …", true);
        let mut ctx = match connect_plain(&cfg.transport, cfg.from_id).await {
            Ok(c) => c,
            Err(e) => {
                ui.scan_status(format!("Connect failed: {e}"), false);
                return;
            }
        };
        let mut rows: Vec<crate::LogLine> = Vec::new();
        let mut found = 0u32;
        let (lo, hi) = (cfg.from_id.min(cfg.to_id), cfg.from_id.max(cfg.to_id));
        for id in lo..=hi {
            ctx.set_slave(Slave(id));
            let res = probe(&mut ctx, cfg.area, cfg.address).await;
            if res.is_ok() {
                found += 1;
            }
            rows.push(scan_row(format!("ID {id}"), &res));
            ui.scan_rows(rows.clone());
            ui.scan_status(format!("Slave scan {lo}…{hi} — {found} responded"), true);
        }
        let _ = ctx.disconnect().await;
        ui.scan_status(
            format!("Slave scan done — {found} slave(s) responded"),
            false,
        );
    })
}

// ===========================================================================
// Slave (simulator) engine
// ===========================================================================

enum SlaveEvent {
    Log { dir: &'static str, text: String },
    Changed,
}

enum SlaveMsg {
    Edit {
        address: u16,
        text: String,
    },
    EditAt {
        area: Area,
        address: u16,
        text: String,
    },
    SetName {
        address: u16,
        name: String,
    },
    SetCellFormat {
        address: u16,
        format: Option<RegFormat>,
    },
    ExportCsv(String),
    SetView {
        area: Area,
        address: u16,
        quantity: u16,
        format: RegFormat,
    },
    SetScaling(Scaling),
    SetColors(ColorRules),
    SetValueNames(ValueNames),
    SimStart(SimCfg),
    SimStop,
    ToggleAutoInc {
        address: u16,
    },
}

struct SlaveShared {
    store: Mutex<DataStore>,
    requests: AtomicU64,
    events: mpsc::Sender<SlaveEvent>,
    event_drops: AtomicU64,
    event_high_watermark: AtomicUsize,
}

impl SlaveShared {
    fn send_event(&self, event: SlaveEvent) {
        match self.events.try_send(event) {
            Ok(()) => {
                self.event_high_watermark.fetch_max(
                    self.events
                        .max_capacity()
                        .saturating_sub(self.events.capacity()),
                    Ordering::Relaxed,
                );
            }
            Err(_) => {
                self.event_drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone)]
struct SlaveService {
    shared: Arc<SlaveShared>,
    unit_id: u8,
    tcp: bool,
    ignore_unit_id: bool,
}

impl Service for SlaveService {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let SlaveRequest { slave, request } = req;
        let for_us = if self.tcp {
            self.ignore_unit_id || slave == self.unit_id
        } else {
            slave == self.unit_id
        };
        let broadcast = !self.tcp && slave == 0;
        if !for_us && !broadcast {
            return ready(Ok(None));
        }

        self.shared.requests.fetch_add(1, Ordering::Relaxed);
        self.shared.send_event(SlaveEvent::Log {
            dir: "RX",
            text: describe_request(&request),
        });

        match handle_request(&self.shared, &request) {
            Ok(resp) => {
                if is_write(&request) {
                    self.shared.send_event(SlaveEvent::Changed);
                }
                ready(Ok(if broadcast { None } else { Some(resp) }))
            }
            Err(e) => {
                self.shared.send_event(SlaveEvent::Log {
                    dir: "ERR",
                    text: format!("Exception: {e:?}"),
                });
                ready(Err(e))
            }
        }
    }
}

struct SlaveHandle {
    ctrl: mpsc::Sender<SlaveMsg>,
    server: JoinHandle<()>,
    updater: JoinHandle<()>,
    ui: UiSink,
}

impl SlaveHandle {
    fn send(&self, m: SlaveMsg) {
        if self.ctrl.try_send(m).is_err() {
            self.ui
                .slave_message("Slave control queue full: command rejected");
        }
    }
    fn stop(self) {
        self.server.abort();
        self.updater.abort();
    }
}

#[derive(Clone, Copy)]
struct SlaveView {
    area: Area,
    address: u16,
    quantity: u16,
    format: RegFormat,
}

fn bind_error_message(host: &str, port: u16, error: &std::io::Error, english: bool) -> String {
    let address_not_local =
        error.kind() == std::io::ErrorKind::AddrNotAvailable || error.raw_os_error() == Some(10049);

    if address_not_local {
        if english {
            return format!(
                "Start failed: {host} is not currently available for listening. Its network adapter may be disconnected, the IP address may not be active yet, or the address may not be assigned to this computer. Connect the adapter and wait for the IP address to become active, or use 0.0.0.0 to listen on all active interfaces."
            );
        }
        return format!(
            "启动失败：{host} 当前不能用于监听。对应网卡可能未连接、IP 地址尚未生效，或该地址未分配给本机。请连接网卡并等待 IP 生效，或使用 0.0.0.0 监听全部有效网卡。"
        );
    }

    if error.kind() == std::io::ErrorKind::AddrInUse {
        return if english {
            format!("Start failed: port {port} is already in use.")
        } else {
            format!("启动失败：端口 {port} 已被其他程序占用。")
        };
    }

    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return if english {
            format!("Start failed: permission denied while listening on {host}:{port}.")
        } else {
            format!("启动失败：没有权限监听 {host}:{port}，请更换端口或以管理员身份运行。")
        };
    }

    if english {
        format!("Start failed: cannot listen on {host}:{port} ({error}).")
    } else {
        format!("启动失败：无法监听 {host}:{port}（{error}）。")
    }
}

fn pdu_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes: [u8; 2] = data.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

fn decode_server_request_pdu(pdu: &[u8]) -> Result<Request<'static>, ExceptionCode> {
    let malformed = || ExceptionCode::IllegalDataValue;
    if pdu.len() > 253 {
        return Err(malformed());
    }
    let function = *pdu.first().ok_or_else(malformed)?;
    let pair = |offset| Some((pdu_u16(pdu, offset)?, pdu_u16(pdu, offset + 2)?));
    match function {
        0x01 if pdu.len() == 5 => pair(1)
            .map(|(a, q)| Request::ReadCoils(a, q))
            .ok_or_else(malformed),
        0x02 if pdu.len() == 5 => pair(1)
            .map(|(a, q)| Request::ReadDiscreteInputs(a, q))
            .ok_or_else(malformed),
        0x03 if pdu.len() == 5 => pair(1)
            .map(|(a, q)| Request::ReadHoldingRegisters(a, q))
            .ok_or_else(malformed),
        0x04 if pdu.len() == 5 => pair(1)
            .map(|(a, q)| Request::ReadInputRegisters(a, q))
            .ok_or_else(malformed),
        0x05 if pdu.len() == 5 => {
            let (address, raw) = pair(1).ok_or_else(malformed)?;
            let value = match raw {
                0xFF00 => true,
                0x0000 => false,
                _ => return Err(malformed()),
            };
            Ok(Request::WriteSingleCoil(address, value))
        }
        0x06 if pdu.len() == 5 => pair(1)
            .map(|(a, v)| Request::WriteSingleRegister(a, v))
            .ok_or_else(malformed),
        0x0F => {
            let (address, quantity) = pair(1).ok_or_else(malformed)?;
            let byte_count = usize::from(*pdu.get(5).ok_or_else(malformed)?);
            let packed = pdu.get(6..6 + byte_count).ok_or_else(malformed)?;
            if pdu.len() != 6 + byte_count || byte_count != usize::from(quantity).div_ceil(8) {
                return Err(malformed());
            }
            let values = (0..usize::from(quantity))
                .map(|i| packed[i / 8] & (1 << (i % 8)) != 0)
                .collect::<Vec<_>>();
            Ok(Request::WriteMultipleCoils(address, Cow::Owned(values)))
        }
        0x10 => {
            let (address, quantity) = pair(1).ok_or_else(malformed)?;
            let byte_count = usize::from(*pdu.get(5).ok_or_else(malformed)?);
            let bytes = pdu.get(6..6 + byte_count).ok_or_else(malformed)?;
            if pdu.len() != 6 + byte_count || byte_count != usize::from(quantity) * 2 {
                return Err(malformed());
            }
            let values = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            Ok(Request::WriteMultipleRegisters(address, Cow::Owned(values)))
        }
        0x16 if pdu.len() == 7 => {
            let address = pdu_u16(pdu, 1).ok_or_else(malformed)?;
            let and_mask = pdu_u16(pdu, 3).ok_or_else(malformed)?;
            let or_mask = pdu_u16(pdu, 5).ok_or_else(malformed)?;
            Ok(Request::MaskWriteRegister(address, and_mask, or_mask))
        }
        0x17 => {
            let read_address = pdu_u16(pdu, 1).ok_or_else(malformed)?;
            let read_quantity = pdu_u16(pdu, 3).ok_or_else(malformed)?;
            let write_address = pdu_u16(pdu, 5).ok_or_else(malformed)?;
            let write_quantity = pdu_u16(pdu, 7).ok_or_else(malformed)?;
            let byte_count = usize::from(*pdu.get(9).ok_or_else(malformed)?);
            let bytes = pdu.get(10..10 + byte_count).ok_or_else(malformed)?;
            if pdu.len() != 10 + byte_count || byte_count != usize::from(write_quantity) * 2 {
                return Err(malformed());
            }
            let values = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>();
            Ok(Request::ReadWriteMultipleRegisters(
                read_address,
                read_quantity,
                write_address,
                Cow::Owned(values),
            ))
        }
        0x01..=0x06 | 0x16 => Err(malformed()),
        _ => Err(ExceptionCode::IllegalFunction),
    }
}

fn encode_server_response_pdu(function: u8, response: Result<Response, ExceptionCode>) -> Vec<u8> {
    let mut out = Vec::with_capacity(253);
    let response = match response {
        Ok(response) => response,
        Err(exception) => {
            out.push(function | 0x80);
            out.push(exception.into());
            return out;
        }
    };
    out.push(response.function_code().value());
    match response {
        Response::ReadCoils(values) | Response::ReadDiscreteInputs(values) => {
            let byte_count = values.len().div_ceil(8);
            out.push(byte_count as u8);
            out.resize(2 + byte_count, 0);
            for (index, value) in values.into_iter().enumerate() {
                if value {
                    out[2 + index / 8] |= 1 << (index % 8);
                }
            }
        }
        Response::ReadInputRegisters(values)
        | Response::ReadHoldingRegisters(values)
        | Response::ReadWriteMultipleRegisters(values) => {
            out.push((values.len() * 2) as u8);
            for value in values {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        Response::WriteSingleCoil(address, value) => {
            out.extend_from_slice(&address.to_be_bytes());
            out.extend_from_slice(&(if value { 0xFF00u16 } else { 0 }).to_be_bytes());
        }
        Response::WriteSingleRegister(address, value) => {
            out.extend_from_slice(&address.to_be_bytes());
            out.extend_from_slice(&value.to_be_bytes());
        }
        Response::WriteMultipleCoils(address, quantity)
        | Response::WriteMultipleRegisters(address, quantity) => {
            out.extend_from_slice(&address.to_be_bytes());
            out.extend_from_slice(&quantity.to_be_bytes());
        }
        Response::MaskWriteRegister(address, and_mask, or_mask) => {
            out.extend_from_slice(&address.to_be_bytes());
            out.extend_from_slice(&and_mask.to_be_bytes());
            out.extend_from_slice(&or_mask.to_be_bytes());
        }
        Response::ReportServerId(server_id, running, additional) => {
            out.push((additional.len() + 2) as u8);
            out.push(server_id);
            out.push(if running { 0xFF } else { 0 });
            out.extend_from_slice(&additional);
        }
        Response::Custom(_, bytes) => out.extend_from_slice(&bytes),
        Response::ReadDeviceIdentification(_) => {
            out.clear();
            out.push(function | 0x80);
            out.push(ExceptionCode::IllegalFunction.into());
        }
    }
    out
}

fn modbus_crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

async fn serve_udp_slave(
    socket: tokio::net::UdpSocket,
    service: SlaveService,
    traffic: TrafficTx,
    rtu: bool,
) -> std::io::Result<()> {
    let mut buffer = [0u8; 260];
    loop {
        let (size, peer) = socket.recv_from(&mut buffer).await?;
        let frame = &buffer[..size];
        let _ = traffic.send((false, frame.to_vec()));

        let parsed = if rtu {
            if frame.len() < 4 {
                None
            } else {
                let body_len = frame.len() - 2;
                let expected = u16::from_le_bytes([frame[body_len], frame[body_len + 1]]);
                (modbus_crc16(&frame[..body_len]) == expected)
                    .then(|| (None, frame[0], &frame[1..body_len]))
            }
        } else if frame.len() >= 8 && frame[2..4] == [0, 0] {
            let length = usize::from(u16::from_be_bytes([frame[4], frame[5]]));
            let end = 6usize.saturating_add(length);
            (length >= 2 && end == frame.len())
                .then(|| (Some([frame[0], frame[1]]), frame[6], &frame[7..end]))
        } else {
            None
        };

        let Some((transaction, slave, pdu)) = parsed else {
            service.shared.send_event(SlaveEvent::Log {
                dir: "ERR",
                text: "Discarded malformed UDP Modbus frame".into(),
            });
            continue;
        };
        let function = pdu.first().copied().unwrap_or(0);
        let result = match decode_server_request_pdu(pdu) {
            Ok(request) => service
                .call(SlaveRequest { slave, request })
                .await
                .transpose(),
            Err(_) if rtu && slave == 0 => None,
            Err(exception) => Some(Err(exception)),
        };
        let Some(result) = result else {
            continue;
        };
        let response_pdu = encode_server_response_pdu(function, result);
        let mut response = Vec::with_capacity(response_pdu.len() + 7);
        if let Some(transaction) = transaction {
            response.extend_from_slice(&transaction);
            response.extend_from_slice(&[0, 0]);
            response.extend_from_slice(&((response_pdu.len() + 1) as u16).to_be_bytes());
            response.push(slave);
            response.extend_from_slice(&response_pdu);
        } else {
            response.push(slave);
            response.extend_from_slice(&response_pdu);
            response.extend_from_slice(&modbus_crc16(&response).to_le_bytes());
        }
        socket.send_to(&response, peer).await?;
        let _ = traffic.send((true, response));
    }
}

fn spawn_slave(cfg: SlaveCfg, ui: UiSink) -> SlaveHandle {
    // 有界队列 + try_send：高频请求下满则丢弃事件(背压/有损)，避免无界内存增长。
    let (events_tx, mut events_rx) = mpsc::channel::<SlaveEvent>(1024);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<SlaveMsg>(ENGINE_CONTROL_QUEUE_CAPACITY);
    let shared = Arc::new(SlaveShared {
        store: Mutex::new(DataStore::new()),
        requests: AtomicU64::new(0),
        events: events_tx,
        event_drops: AtomicU64::new(0),
        event_high_watermark: AtomicUsize::new(0),
    });

    let (traffic_tx, mut traffic_rx) = traffic_channel();
    let slave_is_tcp = matches!(cfg.transport, Transport::Tcp { .. } | Transport::Udp { .. });

    // --- server task ---
    let server_shared = shared.clone();
    let ui_srv = ui.clone();
    let transport = cfg.transport.clone();
    let unit_id = cfg.unit_id;
    let ignore_unit_id = cfg.ignore_unit_id;
    let english = cfg.english;
    let tls = cfg.tls.clone();
    let server = tokio::spawn(async move {
        match transport {
            Transport::Tcp { host, port } => {
                let listener = match TcpListener::bind((host.as_str(), port)).await {
                    Ok(l) => l,
                    Err(e) => {
                        ui_srv.slave_status(bind_error_message(&host, port, &e, english), false);
                        return;
                    }
                };
                let local = listener
                    .local_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| format!("{host}:{port}"));
                let svc = SlaveService {
                    shared: server_shared.clone(),
                    unit_id,
                    tcp: true,
                    ignore_unit_id,
                };
                let on_error = |e: std::io::Error| eprintln!("modbus tcp server error: {e}");
                // Each accepted stream is wrapped in a Tap so the DECRYPTED raw ADU
                // bytes reach the Communication Traffic monitor (TLS handled below it).
                if let Some(tcfg) = &tls {
                    let acceptor = match crate::tls::server_acceptor(tcfg) {
                        Ok(a) => a,
                        Err(e) => {
                            ui_srv.slave_status(format!("TLS setup failed: {e}"), false);
                            return;
                        }
                    };
                    ui_srv.slave_status(format!("Listening on {local} (TLS)"), true);
                    let on_connected =
                        move |stream: tokio::net::TcpStream, _peer: std::net::SocketAddr| {
                            let svc = svc.clone();
                            let ttx = traffic_tx.clone();
                            let acceptor = acceptor.clone();
                            async move {
                                let tls_stream = acceptor.accept(stream).await?;
                                Ok::<_, std::io::Error>(Some((
                                    svc.clone(),
                                    Tap {
                                        inner: tls_stream,
                                        tx: ttx,
                                    },
                                )))
                            }
                        };
                    let srv = tokio_modbus::server::tcp::Server::new(listener);
                    if let Err(e) = srv.serve(&on_connected, on_error).await {
                        ui_srv.slave_status(format!("Server stopped: {e}"), false);
                    }
                } else {
                    ui_srv.slave_status(format!("Listening on {local}"), true);
                    let on_connected =
                        move |stream: tokio::net::TcpStream, peer: std::net::SocketAddr| {
                            let svc = svc.clone();
                            let ttx = traffic_tx.clone();
                            async move {
                                let accepted = tokio_modbus::server::tcp::accept_tcp_connection(
                                    stream,
                                    peer,
                                    move |_addr| Ok(Some(svc.clone())),
                                )?;
                                Ok(accepted
                                    .map(|(service, s)| (service, Tap { inner: s, tx: ttx })))
                            }
                        };
                    let srv = tokio_modbus::server::tcp::Server::new(listener);
                    if let Err(e) = srv.serve(&on_connected, on_error).await {
                        ui_srv.slave_status(format!("Server stopped: {e}"), false);
                    }
                }
            }
            Transport::Udp { host, port } => {
                let socket = match tokio::net::UdpSocket::bind((host.as_str(), port)).await {
                    Ok(socket) => socket,
                    Err(error) => {
                        ui_srv
                            .slave_status(bind_error_message(&host, port, &error, english), false);
                        return;
                    }
                };
                let local = socket
                    .local_addr()
                    .map(|address| address.to_string())
                    .unwrap_or_else(|_| format!("{host}:{port}"));
                ui_srv.slave_status(format!("Listening on {local} (Modbus UDP)"), true);
                let service = SlaveService {
                    shared: server_shared.clone(),
                    unit_id,
                    tcp: true,
                    ignore_unit_id,
                };
                if let Err(error) = serve_udp_slave(socket, service, traffic_tx, false).await {
                    ui_srv.slave_status(format!("UDP server stopped: {error}"), false);
                }
            }
            Transport::RtuOverTcp { host, port } => {
                let listener = match TcpListener::bind((host.as_str(), port)).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        ui_srv
                            .slave_status(bind_error_message(&host, port, &error, english), false);
                        return;
                    }
                };
                let local = listener
                    .local_addr()
                    .map(|address| address.to_string())
                    .unwrap_or_else(|_| format!("{host}:{port}"));
                ui_srv.slave_status(format!("Listening on {local} (RTU over TCP)"), true);
                let service = SlaveService {
                    shared: server_shared.clone(),
                    unit_id,
                    tcp: false,
                    ignore_unit_id: false,
                };
                let on_connected =
                    move |stream: tokio::net::TcpStream, peer: std::net::SocketAddr| {
                        let service = service.clone();
                        let traffic = traffic_tx.clone();
                        async move {
                            tokio_modbus::server::rtu_over_tcp::accept_tcp_connection(
                                stream,
                                peer,
                                move |_address| Ok(Some(service.clone())),
                            )
                            .map(|accepted| {
                                accepted.map(|(service, stream)| {
                                    (
                                        service,
                                        Tap {
                                            inner: stream,
                                            tx: traffic,
                                        },
                                    )
                                })
                            })
                        }
                    };
                let on_error =
                    |error: std::io::Error| eprintln!("modbus rtu-over-tcp server error: {error}");
                let server = tokio_modbus::server::rtu_over_tcp::Server::new(listener);
                if let Err(error) = server.serve(&on_connected, on_error).await {
                    ui_srv.slave_status(format!("RTU/TCP server stopped: {error}"), false);
                }
            }
            Transport::RtuOverUdp { host, port } => {
                let socket = match tokio::net::UdpSocket::bind((host.as_str(), port)).await {
                    Ok(socket) => socket,
                    Err(error) => {
                        ui_srv
                            .slave_status(bind_error_message(&host, port, &error, english), false);
                        return;
                    }
                };
                let local = socket
                    .local_addr()
                    .map(|address| address.to_string())
                    .unwrap_or_else(|_| format!("{host}:{port}"));
                ui_srv.slave_status(format!("Listening on {local} (RTU over UDP)"), true);
                let service = SlaveService {
                    shared: server_shared.clone(),
                    unit_id,
                    tcp: false,
                    ignore_unit_id: false,
                };
                if let Err(error) = serve_udp_slave(socket, service, traffic_tx, true).await {
                    ui_srv.slave_status(format!("RTU/UDP server stopped: {error}"), false);
                }
            }
            Transport::Rtu {
                path,
                baud,
                data_bits,
                parity,
                stop_bits,
            } => {
                drop(traffic_tx); // the RTU server owns a concrete SerialStream — cannot be tapped
                let serial = match SerialStream::open(&serial_builder(
                    &path, baud, data_bits, parity, stop_bits,
                )) {
                    Ok(s) => s,
                    Err(e) => {
                        ui_srv.slave_status(format!("Open failed: {e}"), false);
                        return;
                    }
                };
                ui_srv.slave_status(format!("Serving on {path}"), true);
                let svc = SlaveService {
                    shared: server_shared.clone(),
                    unit_id,
                    tcp: false,
                    ignore_unit_id,
                };
                let srv = tokio_modbus::server::rtu::Server::new(serial);
                if let Err(e) = srv.serve_forever(svc).await {
                    ui_srv.slave_status(format!("Server stopped: {e}"), false);
                }
            }
        }
    });

    // --- updater task (render, log, traffic, simulation, display settings) ---
    let upd_shared = shared.clone();
    let view0 = SlaveView {
        area: cfg.area,
        address: cfg.address,
        quantity: cfg.quantity,
        format: cfg.format,
    };
    let handle_ui = ui.clone();
    let updater = tokio::spawn(async move {
        let mut view = view0;
        let mut names: HashMap<u16, String> = HashMap::new();
        let mut scaling = Scaling::off();
        let mut colors = ColorRules::off();
        let mut value_names = ValueNames::off();
        let mut sim: Option<SimCfg> = None;
        let mut rng = Xorshift::new(rng_seed());
        let mut chart = ChartState::new();
        let mut logbuf: Vec<crate::LogLine> = Vec::new();
        let mut trafficbuf: Vec<crate::LogLine> = Vec::new();
        let mut last_names: Vec<slint::SharedString> = Vec::new();
        let mut sim_tick = tokio::time::interval(Duration::from_secs(3600));
        let mut traffic_health_tick = tokio::time::interval(Duration::from_secs(1));
        let mut traffic_flush_tick = tokio::time::interval(Duration::from_millis(50));
        let mut log_flush_tick = tokio::time::interval(Duration::from_millis(50));
        let mut reported_traffic_drops = 0;
        let mut reported_event_drops = 0;
        let mut traffic_dirty = false;
        let mut log_dirty = false;
        // Per-register auto-increment toggled from the row context menu.
        let mut auto_inc: std::collections::HashSet<(Area, u16)> = std::collections::HashSet::new();
        let mut ainc_tick = tokio::time::interval(Duration::from_millis(500));
        // Per-cell display-format overrides (Set format… 右键菜单), 与主站对等。
        let mut cell_formats: HashMap<u16, RegFormat> = HashMap::new();
        render_slave(
            &upd_shared,
            &view,
            &names,
            &scaling,
            &colors,
            &value_names,
            &mut chart,
            &mut last_names,
            &auto_inc,
            &cell_formats,
            &ui,
        );
        loop {
            tokio::select! {
                Some(ev) = events_rx.recv() => {
                    ui.slave_requests(upd_shared.requests.load(Ordering::Relaxed));
                    match ev {
                        SlaveEvent::Changed => {
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        SlaveEvent::Log { dir, text } => {
                            logbuf.push(crate::LogLine {
                                time: now_hms().into(),
                                dir: dir.into(),
                                text: text.into(),
                            });
                            if logbuf.len() > 300 {
                                let excess = logbuf.len() - 300;
                                logbuf.drain(0..excess);
                            }
                            log_dirty = true;
                        },
                    }
                }
                Some((is_tx, bytes)) = traffic_rx.rx.recv() => {
                    let hex = bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
                    let text = match parse_modbus_adu(is_tx, &bytes, slave_is_tcp) {
                        Some(p) => format!("{p}   │   {hex}"),
                        None => hex,
                    };
                    trafficbuf.push(crate::LogLine {
                        time: now_hms().into(),
                        dir: if is_tx { "Tx" } else { "Rx" }.into(),
                        text: text.into(),
                    });
                    if trafficbuf.len() > 500 {
                        let excess = trafficbuf.len() - 500;
                        trafficbuf.drain(0..excess);
                    }
                    traffic_dirty = true;
                }
                _ = traffic_flush_tick.tick(), if traffic_dirty => {
                    ui.slave_traffic(trafficbuf.clone());
                    traffic_dirty = false;
                }
                _ = log_flush_tick.tick(), if log_dirty => {
                    ui.slave_log(logbuf.clone());
                    log_dirty = false;
                }
                _ = traffic_health_tick.tick() => {
                    let dropped = traffic_rx.health.dropped_chunks.load(Ordering::Relaxed);
                    if dropped > reported_traffic_drops {
                        let bytes = traffic_rx.health.dropped_bytes.load(Ordering::Relaxed);
                        let high = traffic_rx.health.high_watermark.load(Ordering::Relaxed);
                        trafficbuf.push(crate::LogLine {
                            time: now_hms().into(),
                            dir: "ERR".into(),
                            text: format!(
                                "Traffic monitor queue overflow: dropped {dropped} chunks / {bytes} bytes, H{high}/{TRAFFIC_QUEUE_CAPACITY}"
                            ).into(),
                        });
                        if trafficbuf.len() > 500 {
                            let excess = trafficbuf.len() - 500;
                            trafficbuf.drain(0..excess);
                        }
                        traffic_dirty = true;
                        reported_traffic_drops = dropped;
                    }
                    let event_drops = upd_shared.event_drops.load(Ordering::Relaxed);
                    if event_drops > reported_event_drops {
                        let high = upd_shared.event_high_watermark.load(Ordering::Relaxed);
                        ui.slave_message(format!(
                            "Slave event queue overflow: dropped {event_drops}, H{high}/1024"
                        ));
                        logbuf.push(crate::LogLine {
                            time: now_hms().into(),
                            dir: "ERR".into(),
                            text: format!(
                                "Slave event queue overflow: dropped {event_drops}, H{high}/1024"
                            ).into(),
                        });
                        if logbuf.len() > 300 {
                            let excess = logbuf.len() - 300;
                            logbuf.drain(0..excess);
                        }
                        log_dirty = true;
                        reported_event_drops = event_drops;
                    }
                }
                _ = sim_tick.tick(), if sim.is_some() => {
                    if let Some(cfg) = &sim {
                        {
                            let mut store = upd_shared.store.lock().unwrap();
                            apply_sim(&mut store, cfg, &mut rng);
                        }
                        render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                    }
                }
                _ = ainc_tick.tick(), if !auto_inc.is_empty() => {
                    {
                        let mut store = upd_shared.store.lock().unwrap();
                        for (area, addr) in &auto_inc {
                            bump_one(&mut store, *area, *addr);
                        }
                    }
                    render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                }
                msg = ctrl_rx.recv() => {
                    match msg {
                        Some(SlaveMsg::SetView { area, address, quantity, format }) => {
                            view = SlaveView { area, address, quantity, format };
                            chart.reset();
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::Edit { address, text }) => {
                            apply_edit(&upd_shared, view.area, address, &text);
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::EditAt { area, address, text }) => {
                            // 导入按 Function 列写到指定表(与当前视图无关)
                            apply_edit(&upd_shared, area, address, &text);
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::SetName { address, name }) => {
                            if name.trim().is_empty() { names.remove(&address); } else { names.insert(address, name); }
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::SetCellFormat { address, format }) => {
                            match format { Some(f) => { cell_formats.insert(address, f); }, None => { cell_formats.remove(&address); } }
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::ExportCsv(path)) => {
                            let store = upd_shared.store.lock().unwrap();
                            let out = build_server_csv(&store, &names);
                            drop(store);
                            let result_ui = ui.clone();
                            tokio::task::spawn_blocking(move || {
                                let result = std::fs::write(&path, out);
                                result_ui.file_result(true, "CSV 导出", path, result);
                            });
                        }
                        Some(SlaveMsg::SetScaling(s)) => {
                            scaling = s;
                            chart.reset();
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::SetColors(c)) => {
                            colors = c;
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::SetValueNames(v)) => {
                            value_names = v;
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        Some(SlaveMsg::SimStart(c)) => {
                            sim_tick = tokio::time::interval(Duration::from_millis(c.interval_ms.max(50)));
                            sim = Some(c);
                        }
                        Some(SlaveMsg::SimStop) => sim = None,
                        Some(SlaveMsg::ToggleAutoInc { address }) => {
                            let key = (view.area, address);
                            if !auto_inc.remove(&key) {
                                auto_inc.insert(key);
                            }
                            render_slave(&upd_shared, &view, &names, &scaling, &colors, &value_names, &mut chart, &mut last_names, &auto_inc, &cell_formats, &ui);
                        }
                        None => break,
                    }
                }
                else => break,
            }
        }
    });

    SlaveHandle {
        ctrl: ctrl_tx,
        server,
        updater,
        ui: handle_ui,
    }
}

// ----- simulation -----

struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Self {
        Xorshift(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn rng_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1
}

/// Step one value for the client-side local simulator.
fn step_value(cur: u16, mode: SimMode, step: u16, min: i64, max: i64, rng: &mut Xorshift) -> u16 {
    // 容忍 min>max 的非法配置：先归一，避免 span=1 导致仿真值卡死。
    let (min, max) = (min.min(max), min.max(max));
    let cur = cur as i64;
    let span = (max - min) as u64 + 1;
    let next = match mode {
        SimMode::Increment => {
            let n = cur + step as i64;
            if n > max {
                min
            } else {
                n
            }
        }
        SimMode::Decrement => {
            let n = cur - step as i64;
            if n < min {
                max
            } else {
                n
            }
        }
        SimMode::Random => min + (rng.next() % span) as i64,
        SimMode::Toggle => {
            if cur != min {
                min
            } else {
                max
            }
        }
        SimMode::Off => cur,
    };
    next as u16
}

/// Per-register auto-increment (right-click toggle): +1 (wrapping) for registers,
/// flip for bits, on a single address in the given area.
fn bump_one(store: &mut DataStore, area: Area, addr: u16) {
    let i = addr as usize;
    match area {
        Area::Coils => {
            if let Some(b) = store.coils.get_mut(i) {
                *b = !*b;
            }
        }
        Area::DiscreteInputs => {
            if let Some(b) = store.discrete_inputs.get_mut(i) {
                *b = !*b;
            }
        }
        Area::HoldingRegisters => {
            if let Some(v) = store.holding.get_mut(i) {
                *v = v.wrapping_add(1);
            }
        }
        Area::InputRegisters => {
            if let Some(v) = store.input.get_mut(i) {
                *v = v.wrapping_add(1);
            }
        }
    }
}

fn apply_sim(store: &mut DataStore, cfg: &SimCfg, rng: &mut Xorshift) {
    let lo = cfg.address as usize;
    let hi = lo + cfg.quantity as usize;
    match cfg.area {
        Area::HoldingRegisters => sim_regs(&mut store.holding, lo, hi, cfg, rng),
        Area::InputRegisters => sim_regs(&mut store.input, lo, hi, cfg, rng),
        Area::Coils => sim_bits(&mut store.coils, lo, hi, cfg, rng),
        Area::DiscreteInputs => sim_bits(&mut store.discrete_inputs, lo, hi, cfg, rng),
    }
}

/// Whether absolute index `i` (within [lo,hi)) should be animated this tick,
/// honouring `cfg.target` (-1 = all; else only that single absolute address).
fn sim_hits(cfg: &SimCfg, i: usize) -> bool {
    cfg.target < 0 || cfg.target as usize == i
}

fn sim_regs(mem: &mut [u16], lo: usize, hi: usize, cfg: &SimCfg, rng: &mut Xorshift) {
    let hi = hi.min(mem.len());
    for (i, value) in mem.iter_mut().enumerate().take(hi).skip(lo) {
        if !sim_hits(cfg, i) {
            continue;
        }
        *value = step_value(*value, cfg.mode, cfg.step, cfg.min, cfg.max, rng);
    }
}

fn sim_bits(mem: &mut [bool], lo: usize, hi: usize, cfg: &SimCfg, rng: &mut Xorshift) {
    let hi = hi.min(mem.len());
    for (i, value) in mem.iter_mut().enumerate().take(hi).skip(lo) {
        if !sim_hits(cfg, i) {
            continue;
        }
        *value = match cfg.mode {
            SimMode::Random => rng.next() & 1 == 1,
            SimMode::Off => *value,
            _ => !*value, // increment/decrement/toggle all flip a bit
        };
    }
}

fn handle_request(shared: &SlaveShared, req: &Request) -> Result<Response, ExceptionCode> {
    let mut store = shared.store.lock().unwrap();
    match req {
        Request::ReadCoils(a, c) => read_bits(&store.coils, *a, *c).map(Response::ReadCoils),
        Request::ReadDiscreteInputs(a, c) => {
            read_bits(&store.discrete_inputs, *a, *c).map(Response::ReadDiscreteInputs)
        }
        Request::ReadHoldingRegisters(a, c) => {
            read_words(&store.holding, *a, *c).map(Response::ReadHoldingRegisters)
        }
        Request::ReadInputRegisters(a, c) => {
            read_words(&store.input, *a, *c).map(Response::ReadInputRegisters)
        }
        Request::WriteSingleCoil(a, v) => {
            put_bit(&mut store.coils, *a, *v)?;
            Ok(Response::WriteSingleCoil(*a, *v))
        }
        Request::WriteSingleRegister(a, v) => {
            put_word(&mut store.holding, *a, *v)?;
            Ok(Response::WriteSingleRegister(*a, *v))
        }
        Request::WriteMultipleCoils(a, vs) => {
            put_bits(&mut store.coils, *a, &vs[..])?;
            Ok(Response::WriteMultipleCoils(*a, vs.len() as u16))
        }
        Request::WriteMultipleRegisters(a, vs) => {
            put_words(&mut store.holding, *a, &vs[..])?;
            Ok(Response::WriteMultipleRegisters(*a, vs.len() as u16))
        }
        Request::MaskWriteRegister(a, and_mask, or_mask) => {
            let i = *a as usize;
            if i >= store.holding.len() {
                return Err(ExceptionCode::IllegalDataAddress);
            }
            // result = (current AND and_mask) OR (or_mask AND (NOT and_mask))
            store.holding[i] = (store.holding[i] & and_mask) | (or_mask & !and_mask);
            Ok(Response::MaskWriteRegister(*a, *and_mask, *or_mask))
        }
        Request::ReadWriteMultipleRegisters(read_addr, read_qty, write_addr, vs) => {
            put_words(&mut store.holding, *write_addr, &vs[..])?;
            let r = read_words(&store.holding, *read_addr, *read_qty)?;
            Ok(Response::ReadWriteMultipleRegisters(r))
        }
        _ => Err(ExceptionCode::IllegalFunction),
    }
}

fn is_write(req: &Request) -> bool {
    matches!(
        req,
        Request::WriteSingleCoil(..)
            | Request::WriteSingleRegister(..)
            | Request::WriteMultipleCoils(..)
            | Request::WriteMultipleRegisters(..)
            | Request::MaskWriteRegister(..)
            | Request::ReadWriteMultipleRegisters(..)
    )
}

fn read_bits(mem: &[bool], addr: u16, cnt: u16) -> Result<Vec<bool>, ExceptionCode> {
    let s = addr as usize;
    let e = s + cnt as usize;
    if e > mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    Ok(mem[s..e].to_vec())
}

fn read_words(mem: &[u16], addr: u16, cnt: u16) -> Result<Vec<u16>, ExceptionCode> {
    let s = addr as usize;
    let e = s + cnt as usize;
    if e > mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    Ok(mem[s..e].to_vec())
}

fn put_bit(mem: &mut [bool], addr: u16, v: bool) -> Result<(), ExceptionCode> {
    let i = addr as usize;
    if i >= mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    mem[i] = v;
    Ok(())
}

fn put_word(mem: &mut [u16], addr: u16, v: u16) -> Result<(), ExceptionCode> {
    let i = addr as usize;
    if i >= mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    mem[i] = v;
    Ok(())
}

fn put_bits(mem: &mut [bool], addr: u16, vs: &[bool]) -> Result<(), ExceptionCode> {
    let b = addr as usize;
    if b + vs.len() > mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    mem[b..b + vs.len()].copy_from_slice(vs);
    Ok(())
}

fn put_words(mem: &mut [u16], addr: u16, vs: &[u16]) -> Result<(), ExceptionCode> {
    let b = addr as usize;
    if b + vs.len() > mem.len() {
        return Err(ExceptionCode::IllegalDataAddress);
    }
    mem[b..b + vs.len()].copy_from_slice(vs);
    Ok(())
}

fn describe_request(req: &Request) -> String {
    match req {
        Request::ReadCoils(a, c) => format!("Read Coils @{a} ×{c}"),
        Request::ReadDiscreteInputs(a, c) => format!("Read Discrete Inputs @{a} ×{c}"),
        Request::ReadHoldingRegisters(a, c) => format!("Read Holding Registers @{a} ×{c}"),
        Request::ReadInputRegisters(a, c) => format!("Read Input Registers @{a} ×{c}"),
        Request::WriteSingleCoil(a, v) => format!("Write Coil @{a} = {}", if *v { 1 } else { 0 }),
        Request::WriteSingleRegister(a, v) => format!("Write Register @{a} = {v}"),
        Request::WriteMultipleCoils(a, vs) => format!("Write {} Coils @{a}", vs.len()),
        Request::WriteMultipleRegisters(a, vs) => format!("Write {} Registers @{a}", vs.len()),
        other => format!("{other:?}"),
    }
}

/// 服务端"全部导出": 扫 4 表, 只导"有名字 或 值非零"的寄存器, 每行标功能码(表)。
fn build_server_csv(store: &DataStore, names: &HashMap<u16, String>) -> String {
    let mut out = String::from("Import,Function,Address,Name,Value\n");
    let nm = |i: usize| names.get(&(i as u16)).map(|s| s.as_str()).unwrap_or("");
    for (i, &b) in store.coils.iter().enumerate() {
        if b || names.contains_key(&(i as u16)) {
            out.push_str(&format!(
                "yes,{},{},{},{}\n",
                Area::Coils.csv_name(),
                i,
                csv_field(nm(i)),
                if b { 1 } else { 0 }
            ));
        }
    }
    for (i, &b) in store.discrete_inputs.iter().enumerate() {
        if b || names.contains_key(&(i as u16)) {
            out.push_str(&format!(
                "yes,{},{},{},{}\n",
                Area::DiscreteInputs.csv_name(),
                i,
                csv_field(nm(i)),
                if b { 1 } else { 0 }
            ));
        }
    }
    for (i, &v) in store.holding.iter().enumerate() {
        if v != 0 || names.contains_key(&(i as u16)) {
            out.push_str(&format!(
                "yes,{},{},{},{}\n",
                Area::HoldingRegisters.csv_name(),
                i,
                csv_field(nm(i)),
                v
            ));
        }
    }
    for (i, &v) in store.input.iter().enumerate() {
        if v != 0 || names.contains_key(&(i as u16)) {
            out.push_str(&format!(
                "yes,{},{},{},{}\n",
                Area::InputRegisters.csv_name(),
                i,
                csv_field(nm(i)),
                v
            ));
        }
    }
    out
}

fn apply_edit(shared: &SlaveShared, area: Area, address: u16, text: &str) {
    let mut store = shared.store.lock().unwrap();
    let idx = address as usize;
    match area {
        Area::Coils => {
            if let Some(b) = parse_bit(text) {
                if idx < store.coils.len() {
                    store.coils[idx] = b;
                }
            }
        }
        Area::DiscreteInputs => {
            if let Some(b) = parse_bit(text) {
                if idx < store.discrete_inputs.len() {
                    store.discrete_inputs[idx] = b;
                }
            }
        }
        Area::HoldingRegisters => {
            if let Some(v) = parse_word(text) {
                if idx < store.holding.len() {
                    store.holding[idx] = v;
                }
            }
        }
        Area::InputRegisters => {
            if let Some(v) = parse_word(text) {
                if idx < store.input.len() {
                    store.input[idx] = v;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_slave(
    shared: &SlaveShared,
    view: &SlaveView,
    names: &HashMap<u16, String>,
    scaling: &Scaling,
    colors: &ColorRules,
    value_names: &ValueNames,
    chart: &mut ChartState,
    last_names: &mut Vec<slint::SharedString>,
    auto_inc: &std::collections::HashSet<(Area, u16)>,
    cell_formats: &HashMap<u16, RegFormat>,
    ui: &UiSink,
) {
    let store = shared.store.lock().unwrap();
    let start = view.address as usize;
    let qty = view.quantity as usize;
    let rows = match view.area {
        Area::Coils => render_bits(view.address, window_bool(&store.coils, start, qty)),
        Area::DiscreteInputs => render_bits(
            view.address,
            window_bool(&store.discrete_inputs, start, qty),
        ),
        Area::HoldingRegisters => render_registers(
            view.address,
            window_u16(&store.holding, start, qty),
            view.format,
            cell_formats,
            scaling,
        ),
        Area::InputRegisters => render_registers(
            view.address,
            window_u16(&store.input, start, qty),
            view.format,
            cell_formats,
            scaling,
        ),
    };
    drop(store);
    update_chart(chart, &rows);
    let (mut rr, nn) = build_grid(rows, names, colors, value_names, true, view.area);
    for r in rr.iter_mut() {
        r.auto_inc = auto_inc.contains(&(view.area, r.address as u16));
    }
    ui.slave_rows(rr);
    // Push the names model only when it changes, so editing isn't clobbered.
    if nn != *last_names {
        *last_names = nn.clone();
        ui.slave_names(nn);
    }
    let (charts, has) = build_charts(chart);
    ui.slave_chart(charts, has);
}

/// Render one floating server monitor's view from the shared store and push the
/// rows to its window. Monitors are plain views (no names/colors/value-names).
fn window_bool(mem: &[bool], start: usize, qty: usize) -> &[bool] {
    if start >= mem.len() {
        return &[];
    }
    &mem[start..(start + qty).min(mem.len())]
}

fn window_u16(mem: &[u16], start: usize, qty: usize) -> &[u16] {
    if start >= mem.len() {
        return &[];
    }
    &mem[start..(start + qty).min(mem.len())]
}

// ===========================================================================
// Serial helper
// ===========================================================================

fn serial_builder(
    path: &str,
    baud: u32,
    data_bits: u8,
    parity: u8,
    stop_bits: u8,
) -> tokio_serial::SerialPortBuilder {
    use tokio_serial::{DataBits, Parity, StopBits};
    tokio_serial::new(path, baud)
        .data_bits(match data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        })
        .parity(match parity {
            1 => Parity::Even,
            2 => Parity::Odd,
            _ => Parity::None,
        })
        .stop_bits(match stop_bits {
            2 => StopBits::Two,
            _ => StopBits::One,
        })
}

// ===========================================================================
// UI bridge
// ===========================================================================

/// Cached state of one poll window, so switching windows restores its view.
#[derive(Default, Clone)]
struct WinCache {
    rows: Vec<crate::RegRow>,
    names: Vec<slint::SharedString>,
    connected: bool,
    status: String,
    tx: i32,
    rx: i32,
    err: i32,
    log: Vec<crate::LogLine>,
    traffic: Vec<crate::LogLine>,
    charts: Vec<crate::MiniChart>,
    chart_has: bool,
    series: Vec<crate::ChartSeries>,
    axis_lmin: String,
    axis_lmax: String,
    axis_rmin: String,
    axis_rmax: String,
    chart_right: bool,
    chart_page_start: i32,
    chart_total: i32,
}

/// Per-window sink used by a master engine: updates the window's cache, pushes
/// to the flat UI properties when the window is active, and pushes to a floating
/// monitor window when one is attached.
#[derive(Clone)]
struct WindowSink {
    id: u32,
    active: Arc<AtomicU32>,
    weak: slint::Weak<crate::AppWindow>,
    cache: Arc<Mutex<WinCache>>,
    float: Arc<Mutex<Option<slint::Weak<crate::PollFloat>>>>,
}

impl WindowSink {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed) == self.id
    }
    fn push(&self, f: impl FnOnce(crate::AppWindow) + Send + 'static) {
        if self.is_active() {
            let _ = self.weak.upgrade_in_event_loop(f);
        }
    }
    fn push_float(&self, f: impl FnOnce(crate::PollFloat) + Send + 'static) {
        if let Some(fw) = self.float.lock().unwrap().clone() {
            let _ = fw.upgrade_in_event_loop(f);
        }
    }
    fn status(&self, text: impl Into<String>, connected: bool) {
        let t = text.into();
        {
            let mut c = self.cache.lock().unwrap();
            c.status = t.clone();
            c.connected = connected;
        }
        let t2 = t.clone();
        self.push(move |a| {
            a.set_m_status(t.into());
            a.set_m_connected(connected);
        });
        self.push_float(move |f| f.set_status(t2.into()));
    }
    fn message(&self, text: impl Into<String>) {
        let text = text.into();
        self.cache.lock().unwrap().status = text.clone();
        let float_text = text.clone();
        self.push(move |a| a.set_m_status(text.into()));
        self.push_float(move |f| f.set_status(float_text.into()));
    }
    fn file_result(&self, action: &str, path: String, result: std::io::Result<()>) {
        let message = match result {
            Ok(()) => format!("{action}成功: {path}"),
            Err(error) => format!("{action}失败: {path} — {error}"),
        };
        self.message(message);
    }
    fn counts(&self, tx: u64, rx: u64, err: u64) {
        let (tx, rx, err) = (tx as i32, rx as i32, err as i32);
        {
            let mut c = self.cache.lock().unwrap();
            c.tx = tx;
            c.rx = rx;
            c.err = err;
        }
        self.push(move |a| {
            a.set_m_tx(tx);
            a.set_m_rx(rx);
            a.set_m_err(err);
        });
        self.push_float(move |f| {
            f.set_tx(tx);
            f.set_rx(rx);
            f.set_err(err);
        });
    }
    fn rows(&self, data: Vec<crate::RegRow>, names: Vec<slint::SharedString>) {
        let names_changed = {
            let mut c = self.cache.lock().unwrap();
            c.rows = data.clone();
            if c.names != names {
                c.names = names.clone();
                true
            } else {
                false
            }
        };
        let d2 = data.clone();
        self.push(move |a| a.set_m_rows(slint::ModelRc::new(slint::VecModel::from(data))));
        self.push_float(move |f| f.set_rows(slint::ModelRc::new(slint::VecModel::from(d2))));
        if names_changed {
            let n2 = names.clone();
            self.push(move |a| a.set_m_names(slint::ModelRc::new(slint::VecModel::from(names))));
            self.push_float(move |f| f.set_names(slint::ModelRc::new(slint::VecModel::from(n2))));
        }
    }
    fn log(&self, lines: Vec<crate::LogLine>) {
        self.cache.lock().unwrap().log = lines.clone();
        let l2 = lines.clone();
        self.push(move |a| a.set_m_log(slint::ModelRc::new(slint::VecModel::from(lines))));
        self.push_float(move |f| f.set_log(slint::ModelRc::new(slint::VecModel::from(l2))));
    }
    fn traffic(&self, lines: Vec<crate::LogLine>) {
        self.cache.lock().unwrap().traffic = lines.clone();
        let l2 = lines.clone();
        self.push(move |a| a.set_m_traffic(slint::ModelRc::new(slint::VecModel::from(lines))));
        self.push_float(move |f| f.set_traffic(slint::ModelRc::new(slint::VecModel::from(l2))));
    }
    fn chart(&self, ch: &ChartState) {
        let (charts, has) = build_charts(ch);
        let sb = build_series(ch);
        {
            let mut c = self.cache.lock().unwrap();
            c.charts = charts.clone();
            c.chart_has = has;
            c.series = sb.series.clone();
            c.axis_lmin = sb.left_min.clone();
            c.axis_lmax = sb.left_max.clone();
            c.axis_rmin = sb.right_min.clone();
            c.axis_rmax = sb.right_max.clone();
            c.chart_right = sb.has_right;
            c.chart_page_start = ch.page_start as i32;
            c.chart_total = ch.total as i32;
        }
        let c2 = charts.clone();
        let s1 = sb.series.clone();
        let (page_start, total) = (ch.page_start as i32, ch.total as i32);
        let (lmin, lmax, rmin, rmax, hr) = (
            sb.left_min.clone(),
            sb.left_max.clone(),
            sb.right_min.clone(),
            sb.right_max.clone(),
            sb.has_right,
        );
        self.push(move |a| {
            a.set_m_charts(slint::ModelRc::new(slint::VecModel::from(charts)));
            a.set_m_chart_has(has);
            a.set_m_series(slint::ModelRc::new(slint::VecModel::from(sb.series)));
            a.set_m_axis_lmin(sb.left_min.into());
            a.set_m_axis_lmax(sb.left_max.into());
            a.set_m_axis_rmin(sb.right_min.into());
            a.set_m_axis_rmax(sb.right_max.into());
            a.set_m_chart_has_right(sb.has_right);
            a.set_m_chart_page_start(page_start);
            a.set_m_chart_total(total);
        });
        self.push_float(move |f| {
            f.set_charts(slint::ModelRc::new(slint::VecModel::from(c2)));
            f.set_chart_has(has);
            f.set_series(slint::ModelRc::new(slint::VecModel::from(s1)));
            f.set_axis_lmin(lmin.into());
            f.set_axis_lmax(lmax.into());
            f.set_axis_rmin(rmin.into());
            f.set_axis_rmax(rmax.into());
            f.set_chart_has_right(hr);
        });
    }
}

fn push_cache_to_float(weak: &slint::Weak<crate::PollFloat>, title: &str, cache: &WinCache) {
    let c = cache.clone();
    let t = title.to_string();
    let _ = weak.upgrade_in_event_loop(move |f| {
        f.set_ptitle(t.into());
        f.set_rows(slint::ModelRc::new(slint::VecModel::from(c.rows)));
        f.set_names(slint::ModelRc::new(slint::VecModel::from(c.names)));
        f.set_status(c.status.into());
        f.set_tx(c.tx);
        f.set_rx(c.rx);
        f.set_err(c.err);
        f.set_log(slint::ModelRc::new(slint::VecModel::from(c.log)));
        f.set_traffic(slint::ModelRc::new(slint::VecModel::from(c.traffic)));
        f.set_charts(slint::ModelRc::new(slint::VecModel::from(c.charts)));
        f.set_chart_has(c.chart_has);
        f.set_series(slint::ModelRc::new(slint::VecModel::from(c.series)));
        f.set_axis_lmin(c.axis_lmin.into());
        f.set_axis_lmax(c.axis_lmax.into());
        f.set_axis_rmin(c.axis_rmin.into());
        f.set_axis_rmax(c.axis_rmax.into());
        f.set_chart_has_right(c.chart_right);
    });
}

#[derive(Clone)]
struct UiSink {
    weak: slint::Weak<crate::AppWindow>,
}

impl UiSink {
    fn run(&self, f: impl FnOnce(crate::AppWindow) + Send + 'static) {
        let _ = self.weak.upgrade_in_event_loop(f);
    }

    fn window_sink(
        &self,
        id: u32,
        active: Arc<AtomicU32>,
        cache: Arc<Mutex<WinCache>>,
        float: Arc<Mutex<Option<slint::Weak<crate::PollFloat>>>>,
    ) -> WindowSink {
        WindowSink {
            id,
            active,
            weak: self.weak.clone(),
            cache,
            float,
        }
    }

    fn set_windows(&self, tabs: Vec<crate::WinTab>) {
        self.run(move |a| a.set_windows(slint::ModelRc::new(slint::VecModel::from(tabs))));
    }

    fn set_active_win(&self, id: u32) {
        self.run(move |a| a.set_active_win(id as i32));
    }

    fn push_config(&self, cfg: &UiCfg) {
        let c = cfg.clone();
        self.run(move |a| {
            a.set_m_transport(c.transport);
            a.set_m_host(c.host.into());
            a.set_m_port(c.port);
            a.set_m_serial(c.serial.into());
            a.set_m_baud_index(c.baud_index);
            a.set_m_databits_index(c.databits_index);
            a.set_m_parity(c.parity);
            a.set_m_stopbits(c.stopbits);
            a.set_m_slave_id(c.slave_id);
            a.set_m_function(c.function);
            a.set_m_address(c.address);
            a.set_m_quantity(c.quantity);
            a.set_m_scanrate(c.scanrate);
            a.set_m_format(c.format);
            a.set_scl_enabled(c.scl_enabled);
            a.set_scl_x1(c.scl_x1.into());
            a.set_scl_y1(c.scl_y1.into());
            a.set_scl_x2(c.scl_x2.into());
            a.set_scl_y2(c.scl_y2.into());
            a.set_scl_decimals(c.scl_decimals);
            a.set_col_normal(c.col_normal);
            a.set_col_op1(c.col_op1);
            a.set_col_v1(c.col_v1.into());
            a.set_col_c1(c.col_c1);
            a.set_col_op2(c.col_op2);
            a.set_col_v2(c.col_v2.into());
            a.set_col_c2(c.col_c2);
            a.set_vn_enabled(c.vn_enabled);
            a.set_vn_text(c.vn_text.into());
        });
    }

    fn push_cache(&self, cache: &WinCache) {
        let c = cache.clone();
        self.run(move |a| {
            a.set_m_rows(slint::ModelRc::new(slint::VecModel::from(c.rows)));
            a.set_m_names(slint::ModelRc::new(slint::VecModel::from(c.names)));
            a.set_m_connected(c.connected);
            a.set_m_status(c.status.into());
            a.set_m_tx(c.tx);
            a.set_m_rx(c.rx);
            a.set_m_err(c.err);
            a.set_m_log(slint::ModelRc::new(slint::VecModel::from(c.log)));
            a.set_m_traffic(slint::ModelRc::new(slint::VecModel::from(c.traffic)));
            a.set_m_charts(slint::ModelRc::new(slint::VecModel::from(c.charts)));
            a.set_m_chart_has(c.chart_has);
            a.set_m_series(slint::ModelRc::new(slint::VecModel::from(c.series)));
            a.set_m_axis_lmin(c.axis_lmin.into());
            a.set_m_axis_lmax(c.axis_lmax.into());
            a.set_m_axis_rmin(c.axis_rmin.into());
            a.set_m_axis_rmax(c.axis_rmax.into());
            a.set_m_chart_has_right(c.chart_right);
            a.set_m_chart_page_start(c.chart_page_start);
            a.set_m_chart_total(c.chart_total);
        });
    }

    fn master_status(&self, text: impl Into<String>, connected: bool) {
        let t = text.into();
        self.run(move |a| {
            a.set_m_status(t.into());
            a.set_m_connected(connected);
            if !connected {
                // 断开后自动写入/记录都已失效, 同步复位 UI 状态(否则界面仍显示"运行/记录中")
                a.set_wl_active(false);
                a.set_log_active(false);
            }
        });
    }

    fn file_result(&self, server: bool, action: &str, path: String, result: std::io::Result<()>) {
        let message = match result {
            Ok(()) => format!("{action}成功: {path}"),
            Err(error) => format!("{action}失败: {path} — {error}"),
        };
        self.run(move |a| {
            if server {
                a.set_s_status(message.into());
            } else {
                a.set_m_status(message.into());
            }
        });
    }

    fn scan_rows(&self, lines: Vec<crate::LogLine>) {
        self.run(move |a| a.set_scan_rows(slint::ModelRc::new(slint::VecModel::from(lines))));
    }

    fn scan_status(&self, text: impl Into<String>, running: bool) {
        let t = text.into();
        self.run(move |a| {
            a.set_scan_status(t.into());
            a.set_scan_running(running);
        });
    }

    fn slave_status(&self, text: impl Into<String>, running: bool) {
        let t = text.into();
        self.run(move |a| {
            a.set_s_status(t.into());
            a.set_s_running(running);
            if !running {
                a.set_sim_active(false); // 服务器停了, 模拟也停, 复位 UI
            }
        });
    }

    fn slave_message(&self, text: impl Into<String>) {
        let text = text.into();
        self.run(move |a| a.set_s_status(text.into()));
    }

    fn slave_requests(&self, n: u64) {
        self.run(move |a| a.set_s_requests(n as i32));
    }

    fn slave_rows(&self, data: Vec<crate::RegRow>) {
        self.run(move |a| a.set_s_rows(slint::ModelRc::new(slint::VecModel::from(data))));
    }

    fn slave_names(&self, names: Vec<slint::SharedString>) {
        self.run(move |a| a.set_s_names(slint::ModelRc::new(slint::VecModel::from(names))));
    }

    fn slave_log(&self, lines: Vec<crate::LogLine>) {
        self.run(move |a| a.set_s_log(slint::ModelRc::new(slint::VecModel::from(lines))));
    }

    fn slave_traffic(&self, lines: Vec<crate::LogLine>) {
        self.run(move |a| a.set_s_traffic(slint::ModelRc::new(slint::VecModel::from(lines))));
    }

    fn slave_chart(&self, charts: Vec<crate::MiniChart>, has: bool) {
        self.run(move |a| {
            a.set_s_charts(slint::ModelRc::new(slint::VecModel::from(charts)));
            a.set_s_chart_has(has);
        });
    }
}

fn argb_to_color(argb: u32) -> slint::Color {
    slint::Color::from_argb_u8(
        (argb >> 24) as u8,
        (argb >> 16) as u8,
        (argb >> 8) as u8,
        argb as u8,
    )
}

/// Build the grid rows plus a parallel, index-aligned names vector. The names
/// vector is kept separate so the UI can drive the (editable) name column from a
/// stable model that is only refreshed when a name actually changes — otherwise
/// the per-poll value refresh would clobber in-progress typing.
fn build_grid(
    rows: Vec<DisplayRow>,
    names: &HashMap<u16, String>,
    colors: &ColorRules,
    value_names: &ValueNames,
    editable: bool,
    area: Area,
) -> (Vec<crate::RegRow>, Vec<slint::SharedString>) {
    let mut regrows = Vec::with_capacity(rows.len());
    let mut namevec = Vec::with_capacity(rows.len());
    for r in rows {
        let nm: slint::SharedString = names
            .get(&(r.address as u16))
            .cloned()
            .unwrap_or_default()
            .into();
        let argb = r.num.map(|n| colors.eval(n)).unwrap_or(0);
        // #4: when a value-name annotation matches, show "value (label)".
        let value = match r.num.and_then(|n| value_names.lookup(n)) {
            Some(label) => format!("{} ({})", r.value, label),
            None => r.value,
        };
        regrows.push(crate::RegRow {
            address: r.address,
            name: nm.clone(),
            value: value.into(),
            raw: r.raw.into(),
            editable,
            colored: argb != 0,
            vcolor: argb_to_color(argb),
            hex_addr: format!("0x{:04X}", r.address).into(),
            plc_addr: area.plc_addr(r.address as u16).into(),
            auto_inc: false,
        });
        namevec.push(nm);
    }
    (regrows, namevec)
}

/// Polled values as u16 (bits map to 0/1) for derived-channel formulas.
fn poll_values(data: &PollData) -> Vec<u16> {
    match data {
        PollData::Regs(v) => v.clone(),
        PollData::Bits(v) => v.iter().map(|&b| b as u16).collect(),
    }
}

fn format_derived(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        // trim trailing zeros / dot
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Build the register grid then append derived-channel rows (evaluated over the
/// polled values). Derived rows carry a negative address so the UI shows "fx".
fn master_grid(
    drows: Vec<DisplayRow>,
    vals: &[u16],
    names: &HashMap<u16, String>,
    colors: &ColorRules,
    value_names: &ValueNames,
    derived: &[DerivedCh],
    area: Area,
) -> (Vec<crate::RegRow>, Vec<slint::SharedString>) {
    let (mut rr, mut nn) = build_grid(drows, names, colors, value_names, false, area);
    for (di, ch) in derived.iter().enumerate() {
        let value = match crate::expr::eval_formula(&ch.formula, vals) {
            Ok(v) => format_derived(v),
            Err(_) => "—".to_string(),
        };
        let name: slint::SharedString = ch.name.clone().into();
        rr.push(crate::RegRow {
            address: -1 - di as i32,
            name: name.clone(),
            value: value.into(),
            raw: slint::SharedString::new(),
            editable: false,
            colored: false,
            vcolor: argb_to_color(0),
            hex_addr: "fx".into(),
            plc_addr: "fx".into(),
            auto_inc: false,
        });
        nn.push(name);
    }
    (rr, nn)
}

fn push_log(buf: &mut Vec<crate::LogLine>, dir: &str, text: String) -> Vec<crate::LogLine> {
    buf.push(crate::LogLine {
        time: now_hms().into(),
        dir: dir.into(),
        text: text.into(),
    });
    if buf.len() > 300 {
        let excess = buf.len() - 300;
        buf.drain(0..excess);
    }
    buf.clone()
}

fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// UTC "YYYY-MM-DD HH:MM:SS" timestamp for log files (civil date via Hinnant's algorithm).
fn now_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    if mth <= 2 {
        y += 1;
    }
    format!("{y:04}-{mth:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

// ===========================================================================
// Tests — exercise the real Modbus master/slave stack end to end (no GUI).
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_error_explains_unavailable_windows_address_in_both_languages() {
        let error = std::io::Error::from_raw_os_error(10049);
        let zh = bind_error_message("192.168.1.110", 502, &error, false);
        let en = bind_error_message("192.168.1.110", 502, &error, true);
        assert!(zh.contains("当前不能用于监听"));
        assert!(zh.contains("网卡可能未连接"));
        assert!(zh.contains("IP 地址尚未生效"));
        assert!(zh.contains("0.0.0.0"));
        assert!(en.contains("not currently available for listening"));
        assert!(en.contains("network adapter may be disconnected"));
        assert!(en.contains("IP address may not be active yet"));
        assert!(en.contains("0.0.0.0"));
    }

    #[test]
    fn bind_error_explains_port_conflict() {
        let error = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert!(bind_error_message("0.0.0.0", 502, &error, false).contains("端口 502"));
        assert!(bind_error_message("0.0.0.0", 502, &error, true).contains("port 502"));
    }

    struct TestCertificates {
        _directory: tempfile::TempDir,
        ca_cert: String,
        server_cert: String,
        server_key: String,
        client_cert: String,
        client_key: String,
    }

    fn test_certificates() -> TestCertificates {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
            KeyPair, KeyUsagePurpose,
        };

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "PcanWork ephemeral test CA");
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut server_params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        server_params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

        let mut client_params =
            CertificateParams::new(vec!["pcanwork-test-client".into()]).unwrap();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "pcanwork-test-client");
        client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

        let directory = tempfile::tempdir().unwrap();
        let ca_cert_path = directory.path().join("ca.crt");
        let server_cert_path = directory.path().join("server.crt");
        let server_key_path = directory.path().join("server.key");
        let client_cert_path = directory.path().join("client.crt");
        let client_key_path = directory.path().join("client.key");
        std::fs::write(&ca_cert_path, ca_cert.pem()).unwrap();
        std::fs::write(&server_cert_path, server_cert.pem()).unwrap();
        std::fs::write(&server_key_path, server_key.serialize_pem()).unwrap();
        std::fs::write(&client_cert_path, client_cert.pem()).unwrap();
        std::fs::write(&client_key_path, client_key.serialize_pem()).unwrap();

        TestCertificates {
            ca_cert: ca_cert_path.to_string_lossy().into_owned(),
            server_cert: server_cert_path.to_string_lossy().into_owned(),
            server_key: server_key_path.to_string_lossy().into_owned(),
            client_cert: client_cert_path.to_string_lossy().into_owned(),
            client_key: client_key_path.to_string_lossy().into_owned(),
            _directory: directory,
        }
    }

    #[test]
    fn protocol_spans_enforce_modbus_limits_and_address_space() {
        assert!(validate_read_definition(Area::Coils, 0, 2000).is_ok());
        assert!(validate_read_definition(Area::Coils, 0, 2001).is_err());
        assert!(validate_read_definition(Area::HoldingRegisters, 0, 125).is_ok());
        assert!(validate_read_definition(Area::HoldingRegisters, 0, 126).is_err());
        assert!(validate_read_definition(Area::HoldingRegisters, 65_535, 2).is_err());
        assert!(validate_span(0, 1968, 1968, "FC15").is_ok());
        assert!(validate_span(0, 1969, 1968, "FC15").is_err());
        assert!(validate_span(0, 123, 123, "FC16").is_ok());
        assert!(validate_span(0, 124, 123, "FC16").is_err());
    }

    #[test]
    fn mbap_reassembler_handles_split_and_sticky_frames() {
        let first = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x00, 0x00, 0x01,
        ];
        let second = [
            0x00, 0x02, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x01, 0x00, 0x01,
        ];

        let mut split = MbapReassembler::default();
        assert!(split.push(&first[..5]).is_empty());
        assert_eq!(split.push(&first[5..]), vec![first.to_vec()]);

        let mut sticky = MbapReassembler::default();
        let mut joined = first.to_vec();
        joined.extend_from_slice(&second);
        assert_eq!(sticky.push(&joined), vec![first.to_vec(), second.to_vec()]);
    }

    #[test]
    fn multiple_write_rejects_non_contiguous_addresses() {
        let items = [
            WriteItem {
                address: 100,
                value: 1,
                inc: false,
            },
            WriteItem {
                address: 102,
                value: 3,
                inc: false,
            },
        ];
        assert!(validate_write_items(WriteFunc::MultiRegs, &items).is_err());
        assert!(validate_write_items(WriteFunc::SingleReg, &items).is_ok());
    }

    #[tokio::test]
    async fn udp_master_round_trip() {
        use tokio::net::UdpSocket;
        // 最小 UDP Modbus(MBAP)服务器: 对任意请求回 Read Coils 响应(coil 0,2,9 置位)。
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 260];
            let (_n, peer) = server.recv_from(&mut buf).await.unwrap();
            let (tid_hi, tid_lo, unit) = (buf[0], buf[1], buf[6]); // 回显 TID/unit
                                                                   // MBAP: tid, proto=0, len=5 | PDU: func=01, bytecount=2, data=0x05,0x02
            let resp = [
                tid_hi, tid_lo, 0x00, 0x00, 0x00, 0x05, unit, 0x01, 0x02, 0x05, 0x02,
            ];
            server.send_to(&resp, peer).await.unwrap();
        });
        // 用新增的 Modbus/UDP 传输连过去读 10 个线圈
        let mut ctx = connect_plain(
            &Transport::Udp {
                host: "127.0.0.1".into(),
                port,
            },
            1,
        )
        .await
        .expect("UDP connect");
        let coils = ctx.read_coils(0, 10).await.unwrap().unwrap();
        assert_eq!(coils.len(), 10);
        assert_eq!(
            coils,
            vec![true, false, true, false, false, false, false, false, false, true]
        );
    }

    #[tokio::test]
    async fn rtu_over_tcp_round_trip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 标准 Modbus CRC16
        fn crc16(data: &[u8]) -> u16 {
            let mut crc: u16 = 0xFFFF;
            for &b in data {
                crc ^= b as u16;
                for _ in 0..8 {
                    if crc & 1 != 0 {
                        crc = (crc >> 1) ^ 0xA001
                    } else {
                        crc >>= 1
                    }
                }
            }
            crc
        }
        // 手写 RTU-over-TCP 服务端: 收到请求后回 Read Holding Registers(1 个寄存器=0x1092)。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf).await.unwrap(); // 收请求(RTU 帧)
            let mut resp = vec![0x01u8, 0x03, 0x02, 0x10, 0x92]; // unit,func,bytecount,data
            let c = crc16(&resp);
            resp.push((c & 0xff) as u8);
            resp.push((c >> 8) as u8);
            stream.write_all(&resp).await.unwrap();
        });
        let mut ctx = connect_plain(
            &Transport::RtuOverTcp {
                host: "127.0.0.1".into(),
                port: addr.port(),
            },
            1,
        )
        .await
        .unwrap();
        let h = ctx.read_holding_registers(0, 1).await.unwrap().unwrap();
        assert_eq!(h, vec![0x1092]);
    }

    #[test]
    fn server_csv_export() {
        let mut store = DataStore::new();
        store.holding[0] = 28;
        store.holding[5] = 1000;
        store.coils[2] = true;
        let mut names: HashMap<u16, String> = HashMap::new();
        names.insert(0, "Temp".into());
        let csv = build_server_csv(&store, &names);
        assert_eq!(
            csv.lines().next().unwrap(),
            "Import,Function,Address,Name,Value"
        );
        assert!(csv.contains("yes,Coils,2,,1"), "{csv}"); // 线圈 2 = ON
        assert!(csv.contains("yes,HoldingRegisters,0,Temp,28"), "{csv}"); // 命名 + 值
        assert!(csv.contains("yes,HoldingRegisters,5,,1000"), "{csv}"); // 仅非零值
                                                                        // 命名地址 0 全局 → 各表都出一行(含值为 0 的); 但绝不导出 26 万空寄存器
        assert!(
            csv.lines().count() < 50,
            "filtered empties, got {}",
            csv.lines().count()
        );
        assert!(
            !csv.contains("yes,HoldingRegisters,6,"),
            "空寄存器不应导出\n{csv}"
        );
    }

    #[test]
    fn csv_address_lenient() {
        assert_eq!(
            Area::from_csv_name("HoldingRegisters"),
            Some(Area::HoldingRegisters)
        );
        assert_eq!(Area::from_csv_name("coils"), Some(Area::Coils));
        assert_eq!(Area::from_csv_name("4x"), Some(Area::HoldingRegisters));
        assert_eq!(Area::from_csv_name("9"), None);
    }

    #[test]
    fn modbus_adu_parsing() {
        // TCP 读保持寄存器请求: MBAP(TID=1,proto=0,len=6,unit=1) + PDU(03, addr=0, qty=10)
        let req = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x00, 0x00, 0x0A,
        ];
        let s = parse_modbus_adu(true, &req, true).unwrap();
        assert!(s.contains("TID=1") && s.contains("U=1") && s.contains("FC03"));
        assert!(s.contains("addr=0") && s.contains("qty=10"), "{s}");
        // TCP 响应: 字节数=4 + 2 个寄存器
        let resp = [
            0x00, 0x01, 0x00, 0x00, 0x00, 0x07, 0x01, 0x03, 0x04, 0x12, 0x34, 0x56, 0x78,
        ];
        let s = parse_modbus_adu(false, &resp, true).unwrap();
        assert!(s.contains("FC03") && s.contains("bytes=4"), "{s}");
        // 异常响应: FC=0x83, exception=0x02 (Illegal Data Address)
        let exc = [0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x01, 0x83, 0x02];
        let s = parse_modbus_adu(false, &exc, true).unwrap();
        assert!(
            s.contains("Exception 02") && s.contains("Illegal Data Address"),
            "{s}"
        );
        // RTU 请求: unit=1 + PDU(03,addr=0,qty=10) + CRC(2)
        let rtu = [0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5, 0xCD];
        let s = parse_modbus_adu(true, &rtu, false).unwrap();
        assert!(
            !s.contains("TID") && s.contains("U=1") && s.contains("addr=0") && s.contains("qty=10"),
            "{s}"
        );
        // 太短 → None
        assert!(parse_modbus_adu(true, &[0x00, 0x01], true).is_none());
    }

    fn make_shared() -> Arc<SlaveShared> {
        let (events_tx, rx) = mpsc::channel(1024);
        std::mem::forget(rx);
        Arc::new(SlaveShared {
            store: Mutex::new(DataStore::new()),
            requests: AtomicU64::new(0),
            events: events_tx,
            event_drops: AtomicU64::new(0),
            event_high_watermark: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn tcp_master_slave_round_trip() {
        let shared = make_shared();
        {
            let mut s = shared.store.lock().unwrap();
            s.input[5] = 0x1234;
            s.holding[10] = 7;
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared: shared.clone(),
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        let server = tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, peer: std::net::SocketAddr| {
                let svc = svc.clone();
                async move {
                    tokio_modbus::server::tcp::accept_tcp_connection(stream, peer, move |_addr| {
                        Ok(Some(svc.clone()))
                    })
                }
            };
            let on_error = |e: std::io::Error| eprintln!("server error: {e}");
            let srv = tokio_modbus::server::tcp::Server::new(listener);
            let _ = srv.serve(&on_connected, on_error).await;
        });

        let mut ctx = tcp::connect_slave(addr, Slave(1)).await.unwrap();

        let v = ctx.read_input_registers(5, 1).await.unwrap().unwrap();
        assert_eq!(v, vec![0x1234]);

        ctx.write_single_register(10, 999).await.unwrap().unwrap();
        let h = ctx.read_holding_registers(10, 1).await.unwrap().unwrap();
        assert_eq!(h, vec![999]);
        assert_eq!(shared.store.lock().unwrap().holding[10], 999);

        ctx.write_multiple_coils(0, &[true, false, true])
            .await
            .unwrap()
            .unwrap();
        let c = ctx.read_coils(0, 3).await.unwrap().unwrap();
        assert_eq!(c, vec![true, false, true]);

        ctx.write_multiple_registers(20, &[111, 222, 333])
            .await
            .unwrap()
            .unwrap();
        let r = ctx.read_holding_registers(20, 3).await.unwrap().unwrap();
        assert_eq!(r, vec![111, 222, 333]);

        let exc = ctx.read_holding_registers(65535, 5).await.unwrap();
        assert!(exc.is_err());

        // FC23 over the wire: write [7,8] @30 then read them back.
        let rw = ctx
            .read_write_multiple_registers(30, 2, 30, &[7, 8])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rw, vec![7, 8]);
        assert_eq!(shared.store.lock().unwrap().holding[30], 7);
        assert_eq!(shared.store.lock().unwrap().holding[31], 8);

        server.abort();
    }

    async fn udp_slave_round_trip(rtu: bool) {
        let shared = make_shared();
        shared.store.lock().unwrap().holding[12] = 0x1234;
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let service = SlaveService {
            shared: shared.clone(),
            unit_id: 1,
            tcp: !rtu,
            ignore_unit_id: false,
        };
        let (traffic_tx, traffic_rx) = traffic_channel();
        std::mem::forget(traffic_rx);
        let server = tokio::spawn(serve_udp_slave(socket, service, traffic_tx, rtu));
        let transport = if rtu {
            Transport::RtuOverUdp {
                host: "127.0.0.1".into(),
                port,
            }
        } else {
            Transport::Udp {
                host: "127.0.0.1".into(),
                port,
            }
        };
        let mut context = connect_plain(&transport, 1).await.unwrap();
        assert_eq!(
            context
                .read_holding_registers(12, 1)
                .await
                .unwrap()
                .unwrap(),
            vec![0x1234]
        );
        context
            .write_single_register(12, 0x5678)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shared.store.lock().unwrap().holding[12], 0x5678);
        server.abort();
    }

    #[tokio::test]
    async fn udp_slave_server_round_trip() {
        udp_slave_round_trip(false).await;
    }

    #[tokio::test]
    async fn rtu_over_udp_slave_server_round_trip() {
        udp_slave_round_trip(true).await;
    }

    #[tokio::test]
    async fn rtu_over_tcp_slave_server_round_trip() {
        let shared = make_shared();
        shared.store.lock().unwrap().holding[7] = 0xCAFE;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let service = SlaveService {
            shared,
            unit_id: 1,
            tcp: false,
            ignore_unit_id: false,
        };
        let server = tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, peer: std::net::SocketAddr| {
                let service = service.clone();
                async move {
                    tokio_modbus::server::rtu_over_tcp::accept_tcp_connection(
                        stream,
                        peer,
                        move |_address| Ok(Some(service.clone())),
                    )
                }
            };
            let instance = tokio_modbus::server::rtu_over_tcp::Server::new(listener);
            let _ = instance.serve(&on_connected, |_| {}).await;
        });
        let mut context = connect_plain(
            &Transport::RtuOverTcp {
                host: "127.0.0.1".into(),
                port,
            },
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            context.read_holding_registers(7, 1).await.unwrap().unwrap(),
            vec![0xCAFE]
        );
        server.abort();
    }

    #[test]
    fn handle_request_bounds() {
        let shared = make_shared();
        let r = handle_request(&shared, &Request::ReadHoldingRegisters(65535, 5));
        assert!(matches!(r, Err(ExceptionCode::IllegalDataAddress)));
    }

    #[test]
    fn mask_write_and_read_write_multiple() {
        let shared = make_shared();
        shared.store.lock().unwrap().holding[5] = 0x00F2;
        // FC22: result = (cur & and) | (or & !and) = 0x00F2 | 0xFF00 = 0xFFF2
        let r = handle_request(&shared, &Request::MaskWriteRegister(5, 0x00FF, 0xFF00));
        assert!(r.is_ok());
        assert_eq!(shared.store.lock().unwrap().holding[5], 0xFFF2);

        // FC23: write [10,20,30] @0 then read 3 @0
        let r = handle_request(
            &shared,
            &Request::ReadWriteMultipleRegisters(
                0,
                3,
                0,
                std::borrow::Cow::Owned(vec![10, 20, 30]),
            ),
        );
        match r {
            Ok(Response::ReadWriteMultipleRegisters(v)) => assert_eq!(v, vec![10, 20, 30]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn sim_increment_wraps() {
        let mut store = DataStore::new();
        store.holding[0] = 8;
        let cfg = SimCfg {
            mode: SimMode::Increment,
            area: Area::HoldingRegisters,
            address: 0,
            quantity: 1,
            step: 5,
            min: 0,
            max: 10,
            interval_ms: 100,
            target: -1,
        };
        let mut rng = Xorshift::new(1);
        apply_sim(&mut store, &cfg, &mut rng); // 8+5=13 > 10 → wrap to min 0
        assert_eq!(store.holding[0], 0);
        apply_sim(&mut store, &cfg, &mut rng); // 0+5=5
        assert_eq!(store.holding[0], 5);
    }

    #[test]
    fn color_rules_eval() {
        let rules = ColorRules {
            normal: 0,
            op1: CmpOp::Gt,
            v1: 100.0,
            c1: 0xFFFF0000,
            op2: CmpOp::Lt,
            v2: 10.0,
            c2: 0xFF00FF00,
        };
        assert_eq!(rules.eval(150.0), 0xFFFF0000);
        assert_eq!(rules.eval(5.0), 0xFF00FF00);
        assert_eq!(rules.eval(50.0), 0);
    }

    #[test]
    fn chart_path_built() {
        let mut ch = ChartState::new();
        for v in [10.0f64, 20.0, 30.0] {
            update_chart(
                &mut ch,
                &[
                    DisplayRow {
                        address: 0,
                        value: String::new(),
                        raw: String::new(),
                        num: Some(v),
                    },
                    DisplayRow {
                        address: 1,
                        value: String::new(),
                        raw: String::new(),
                        num: Some(v * 2.0),
                    },
                ],
            );
        }
        assert_eq!(ch.data.len(), 2);
        assert_eq!(ch.data[0].len(), 3);

        let (charts, has) = build_charts(&ch);
        assert!(has);
        assert_eq!(charts.len(), 2);
        let cmd = charts[0].commands.to_string();
        assert!(cmd.starts_with("M "), "path must start with MoveTo: {cmd}");
        assert!(cmd.contains(" L "), "path must contain LineTo: {cmd}");
        for tok in cmd.split_whitespace() {
            if let Ok(n) = tok.parse::<f64>() {
                assert!(
                    (0.0..=1000.0).contains(&n),
                    "coord {n} out of range in {cmd}"
                );
            }
        }
        // each register is scaled to its OWN range
        assert_eq!(charts[0].ymax.to_string(), "30.00"); // series 0 max = 30
        assert_eq!(charts[0].ymin.to_string(), "10.00");
        assert_eq!(charts[1].ymax.to_string(), "60.00"); // series 1 max = 30*2
    }

    #[test]
    fn write_item_inc_wraps() {
        let mut items = [
            WriteItem {
                address: 0,
                value: 65534,
                inc: true,
            },
            WriteItem {
                address: 1,
                value: 50,
                inc: false,
            },
        ];
        // simulate one auto-write cycle's increment step
        for it in items.iter_mut() {
            if it.inc {
                it.value = it.value.wrapping_add(1);
            }
        }
        assert_eq!(items[0].value, 65535);
        assert_eq!(items[1].value, 50); // non-inc unchanged
        for it in items.iter_mut() {
            if it.inc {
                it.value = it.value.wrapping_add(1);
            }
        }
        assert_eq!(items[0].value, 0); // wrapped
    }

    #[test]
    fn slave_sim_single_register() {
        let mut mem = vec![0u16; 5];
        let cfg = SimCfg {
            mode: SimMode::Increment,
            area: Area::HoldingRegisters,
            address: 1,
            quantity: 4,
            step: 5,
            min: 0,
            max: 100,
            interval_ms: 100,
            target: 3, // only absolute index 3 within [1,5)
        };
        let mut rng = Xorshift::new(3);
        sim_regs(&mut mem, 1, 5, &cfg, &mut rng);
        assert_eq!(mem, vec![0, 0, 0, 5, 0]); // only index 3 moved
    }

    #[test]
    fn overlay_series_dual_axis() {
        let mut ch = ChartState::new();
        for v in [10.0f64, 20.0, 30.0] {
            update_chart(
                &mut ch,
                &[
                    DisplayRow {
                        address: 0,
                        value: String::new(),
                        raw: String::new(),
                        num: Some(v),
                    },
                    DisplayRow {
                        address: 1,
                        value: String::new(),
                        raw: String::new(),
                        num: Some(v * 100.0),
                    },
                ],
            );
        }
        // both on the left axis by default → shared range 10..3000
        let sb = build_series(&ch);
        assert_eq!(sb.series.len(), 2);
        assert_eq!(sb.left_min, "10.00");
        assert_eq!(sb.left_max, "3000.00");
        assert!(!sb.has_right);

        // assign addr 1 to the right axis → each axis ranges independently
        ch.right.insert(1);
        let sb = build_series(&ch);
        assert!(sb.has_right);
        assert_eq!(sb.left_min, "10.00");
        assert_eq!(sb.left_max, "30.00"); // only addr 0
        assert_eq!(sb.right_min, "1000.00");
        assert_eq!(sb.right_max, "3000.00"); // only addr 1
        assert_eq!(sb.series[0].axis, 0);
        assert_eq!(sb.series[1].axis, 1);
        let cmd = sb.series[1].commands.to_string();
        assert!(cmd.starts_with("M "));
        // the right-axis series is normalised to ITS own range, so it spans the
        // full 0..300 y space (last sample = max → y≈0, first = min → y≈300)
        assert!(
            cmd.contains(" 0.0") || cmd.contains(" 0 "),
            "max should map near y=0: {cmd}"
        );
    }

    #[test]
    fn chart_pages_cover_addresses_beyond_the_first_six() {
        let rows: Vec<DisplayRow> = (0..25)
            .map(|address| DisplayRow {
                address,
                value: String::new(),
                raw: String::new(),
                num: Some(address as f64),
            })
            .collect();
        let mut ch = ChartState::new();

        update_chart(&mut ch, &rows);
        update_chart(&mut ch, &rows);
        assert_eq!(ch.total, 25);
        assert_eq!(ch.addrs, (0..12).collect::<Vec<_>>());
        assert_eq!(build_series(&ch).series.len(), 12);

        assert!(ch.move_page(1));
        update_chart(&mut ch, &rows);
        assert_eq!(ch.page_start, 12);
        assert_eq!(ch.addrs, (12..24).collect::<Vec<_>>());

        assert!(ch.move_page(1));
        update_chart(&mut ch, &rows);
        assert_eq!(ch.page_start, 24);
        assert_eq!(ch.addrs, vec![24]);
        assert!(!ch.move_page(1));

        ch.page_start = 0;
        assert!(ch.focus_address(&rows, 19));
        update_chart(&mut ch, &rows);
        assert_eq!(ch.page_start, 12);
        assert!(ch.addrs.contains(&19));
    }

    #[test]
    fn log_line_written() {
        let dir = std::env::temp_dir();
        let path = dir.join("modbus_tools_test_log.csv");
        let _ = std::fs::remove_file(&path);
        let file = std::fs::File::create(&path).unwrap();
        let mut logger = Logger {
            file: std::io::BufWriter::new(file),
            cfg: LogCfg {
                path: path.to_string_lossy().to_string(),
                each_read: true,
                period_s: 1,
                delimiter: ',',
                on_change: false,
                timestamp: false,
            },
            last_write: None,
            last_flush: None,
            last_vals: None,
            header_written: false,
        };
        let rows = vec![
            DisplayRow {
                address: 0,
                value: "1".into(),
                raw: String::new(),
                num: Some(1.0),
            },
            DisplayRow {
                address: 1,
                value: "2".into(),
                raw: String::new(),
                num: Some(2.0),
            },
        ];
        logger.maybe_log(&rows).unwrap();
        drop(logger);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("@0,@1"), "header missing: {content}");
        assert!(content.contains("1,2"), "data row missing: {content}");
        let _ = std::fs::remove_file(&path);
    }

    async fn start_test_server(shared: Arc<SlaveShared>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared,
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, peer: std::net::SocketAddr| {
                let svc = svc.clone();
                async move {
                    tokio_modbus::server::tcp::accept_tcp_connection(stream, peer, move |_| {
                        Ok(Some(svc.clone()))
                    })
                }
            };
            let _ = tokio_modbus::server::tcp::Server::new(listener)
                .serve(&on_connected, |_e: std::io::Error| {})
                .await;
        });
        addr
    }

    #[tokio::test]
    async fn raw_traffic_capture_and_scan_probe() {
        let shared = make_shared();
        shared.store.lock().unwrap().holding[0] = 0xBEEF;
        let addr = start_test_server(shared).await;
        let transport = Transport::Tcp {
            host: addr.ip().to_string(),
            port: addr.port(),
        };

        let (mut ctx, mut traffic, _) = connect_tapped(&transport, 1, None).await.unwrap();
        let v = ctx.read_holding_registers(0, 1).await.unwrap().unwrap();
        assert_eq!(v, vec![0xBEEF]);

        let mut tx_seen = false;
        let mut rx_seen = false;
        for _ in 0..20 {
            match traffic.rx.try_recv() {
                Ok((is_tx, bytes)) => {
                    assert!(!bytes.is_empty());
                    if is_tx {
                        tx_seen = true;
                    } else {
                        rx_seen = true;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert!(
            tx_seen && rx_seen,
            "expected raw Tx and Rx bytes to be captured"
        );

        let r = probe(&mut ctx, Area::HoldingRegisters, 0).await;
        assert!(r.is_ok(), "probe should succeed against a live server");
    }

    #[tokio::test]
    async fn tls_roundtrip() {
        let certs = test_certificates();
        let scfg = crate::tls::TlsServerCfg {
            cert_file: certs.server_cert.clone(),
            key_file: certs.server_key.clone(),
            ..Default::default()
        };
        let acceptor = crate::tls::server_acceptor(&scfg).unwrap();

        let shared = make_shared();
        shared.store.lock().unwrap().holding[0] = 0x1234;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared,
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, _peer: std::net::SocketAddr| {
                let svc = svc.clone();
                let acceptor = acceptor.clone();
                async move {
                    let tls = acceptor.accept(stream).await?;
                    Ok::<_, std::io::Error>(Some((svc.clone(), tls)))
                }
            };
            let _ = tokio_modbus::server::tcp::Server::new(listener)
                .serve(&on_connected, |_e: std::io::Error| {})
                .await;
        });

        // TLS client with skip-verify (the cert is self-signed).
        let ccfg = crate::tls::TlsClientCfg {
            ca_file: String::new(),
            skip_verify: true,
            domain: "localhost".into(),
            ..Default::default()
        };
        let connector = crate::tls::client_connector(&ccfg).unwrap();
        let name = crate::tls::server_name(&ccfg, "127.0.0.1").unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(name, stream).await.unwrap();
        let mut ctx = tcp::attach_slave(tls, Slave(1));
        let v = ctx.read_holding_registers(0, 1).await.unwrap().unwrap();
        assert_eq!(
            v,
            vec![0x1234],
            "Modbus read over TLS should return the stored value"
        );
    }

    #[tokio::test]
    async fn mutual_tls_roundtrip() {
        let certs = test_certificates();
        let scfg = crate::tls::TlsServerCfg {
            cert_file: certs.server_cert.clone(),
            key_file: certs.server_key.clone(),
            require_client_cert: true,
            client_ca: certs.ca_cert.clone(),
            ..Default::default()
        };
        let acceptor = crate::tls::server_acceptor(&scfg).unwrap();

        let shared = make_shared();
        shared.store.lock().unwrap().holding[0] = 0x55AA;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared,
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, _peer: std::net::SocketAddr| {
                let svc = svc.clone();
                let acceptor = acceptor.clone();
                async move {
                    let tls = acceptor.accept(stream).await?;
                    Ok::<_, std::io::Error>(Some((svc.clone(), tls)))
                }
            };
            let _ = tokio_modbus::server::tcp::Server::new(listener)
                .serve(&on_connected, |_e: std::io::Error| {})
                .await;
        });

        let ccfg = crate::tls::TlsClientCfg {
            ca_file: String::new(),
            skip_verify: true,
            domain: "localhost".into(),
            client_cert: certs.client_cert.clone(),
            client_key: certs.client_key.clone(),
            ..Default::default()
        };
        let connector = crate::tls::client_connector(&ccfg).unwrap();
        let name = crate::tls::server_name(&ccfg, "127.0.0.1").unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(name, stream).await.unwrap();
        let mut ctx = tcp::attach_slave(tls, Slave(1));
        let v = ctx.read_holding_registers(0, 1).await.unwrap().unwrap();
        assert_eq!(v, vec![0x55AA], "mutual-TLS Modbus read should succeed");
    }

    #[tokio::test]
    async fn tls_cipher_mismatch_fails() {
        let certs = test_certificates();
        // Server offers ONLY AES-256; client offers ONLY AES-128 → no shared suite.
        let scfg = crate::tls::TlsServerCfg {
            cert_file: certs.server_cert.clone(),
            key_file: certs.server_key.clone(),
            cipher: 2,
            ..Default::default()
        };
        let acceptor = crate::tls::server_acceptor(&scfg).unwrap();
        let shared = make_shared();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared,
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, _peer: std::net::SocketAddr| {
                let svc = svc.clone();
                let acceptor = acceptor.clone();
                async move {
                    let tls = acceptor.accept(stream).await?;
                    Ok::<_, std::io::Error>(Some((svc.clone(), tls)))
                }
            };
            let _ = tokio_modbus::server::tcp::Server::new(listener)
                .serve(&on_connected, |_e: std::io::Error| {})
                .await;
        });

        let ccfg = crate::tls::TlsClientCfg {
            skip_verify: true,
            domain: "localhost".into(),
            cipher: 1, // AES-128 only
            ..Default::default()
        };
        let connector = crate::tls::client_connector(&ccfg).unwrap();
        let name = crate::tls::server_name(&ccfg, "127.0.0.1").unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(
            connector.connect(name, stream).await.is_err(),
            "AES-128-only client must fail to negotiate with an AES-256-only server"
        );
    }

    #[tokio::test]
    async fn tls_v13_negotiated() {
        let certs = test_certificates();
        let scfg = crate::tls::TlsServerCfg {
            cert_file: certs.server_cert.clone(),
            key_file: certs.server_key.clone(),
            version: 2, // TLS 1.3 only
            ..Default::default()
        };
        let acceptor = crate::tls::server_acceptor(&scfg).unwrap();
        let shared = make_shared();
        shared.store.lock().unwrap().holding[0] = 0x0BAD;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = SlaveService {
            shared,
            unit_id: 1,
            tcp: true,
            ignore_unit_id: true,
        };
        tokio::spawn(async move {
            let on_connected = move |stream: tokio::net::TcpStream, _peer: std::net::SocketAddr| {
                let svc = svc.clone();
                let acceptor = acceptor.clone();
                async move {
                    let tls = acceptor.accept(stream).await?;
                    Ok::<_, std::io::Error>(Some((svc.clone(), tls)))
                }
            };
            let _ = tokio_modbus::server::tcp::Server::new(listener)
                .serve(&on_connected, |_e: std::io::Error| {})
                .await;
        });

        let ccfg = crate::tls::TlsClientCfg {
            skip_verify: true,
            domain: "localhost".into(),
            version: 2,
            ..Default::default()
        };
        let connector = crate::tls::client_connector(&ccfg).unwrap();
        let name = crate::tls::server_name(&ccfg, "127.0.0.1").unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect(name, stream).await.unwrap();
        let desc = crate::tls::describe(tls.get_ref().1);
        assert!(desc.starts_with("TLS1.3"), "expected TLS1.3, got '{desc}'");
        let mut ctx = tcp::attach_slave(tls, Slave(1));
        assert_eq!(
            ctx.read_holding_registers(0, 1).await.unwrap().unwrap(),
            vec![0x0BAD]
        );
    }
}
