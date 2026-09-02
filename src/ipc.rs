//! Python 自动化测试的 IPC 服务端：TCP loopback (127.0.0.1) + NDJSON 协议。
//!
//! PcanWork 是服务端；外部 python.exe（运行用户测试脚本）作为客户端连接，驱动总线。
//! 本模块**只用 Send 安全类型**（std + serde + crate::can/dbc 的克隆快照），绝不触碰
//! App / Rc<RefCell> / 硬件：
//!   - 只读操作（status/get_last/get_signal/decode/encode）在 handler 线程直接从
//!     `Arc<Mutex<Snapshot>>` 即时回复，不经 100ms tick（避免 UI 卡顿时读操作全超时）；
//!   - 状态变更操作（connect/send/set_periodic/...）封成 `UiReq` 投递给主线程 tick 处理。

use crate::can::{CanFrame, DeviceConfig};
use crate::dbc::{DbcDb, DbcDiagnosticSeverity, Decoded};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EVT_QUEUE: usize = 4096; // 每客户端输出队列上限（事件满则丢最新并计数）
const UI_REQ_QUEUE: usize = 64; // 单客户端请求/响应协议，留出突发余量且绝不无界增长
const MAX_LINE: usize = 1 << 20; // 单行 1 MiB 上限，超长仅断开该客户端

// ---------------- 线协议信封（仅请求需要反序列化；响应/事件用 json! 构造）----------------
#[derive(Deserialize)]
struct ReqEnvelope {
    #[serde(default)]
    id: u64,
    op: String,
    #[serde(default)]
    args: serde_json::Value,
    #[serde(default)]
    token: String,
}

// ---------------- 状态变更操作（投递到 UI tick 处理）----------------
pub enum IpcReq {
    Invalid {
        code: String,
        msg: String,
    },
    SendOnce {
        ch: u8,
        id: u32,
        data: Vec<u8>,
        ext: bool,
        fd: bool,
        brs: bool,
        remote: bool,
    },
    SendBatch {
        frames: Vec<IpcTxFrame>,
        repeat: u32,
    },
    SetPeriodic {
        client_handle: u64,
        ch: u8,
        id: u32,
        data: Vec<u8>,
        period_ms: u64,
        repeat: i64,
        ext: bool,
        fd: bool,
        brs: bool,
        remote: bool,
    },
    StopPeriodic {
        client_handle: u64,
    },
    Connect {
        channels: Vec<DeviceConfig>,
    },
    ConnectConfigured, // 连接主界面"设备"对话框里已配置的多通道列表（a.channels）
    // 脚本加载 DBC：在 handler 线程预解析(loaded)，UI tick 仅推入，避免大文件解析阻塞界面。
    LoadDbc {
        path: String,
        loaded: Result<DbcDb, String>,
    },
    Disconnect,
    Start,
    Stop,
    Log {
        msg: String,
    },
    RunResult {
        passed: bool,
        summary: String,
    },
    // CAN 报文日志(printf-over-CAN)配置: 字段为 None 表示该项不改
    ConsoleSet {
        enabled: Option<bool>,
        id: Option<i64>,
        ch: Option<u8>,
        clear: bool,
    },
    ClientGone,
}

#[derive(Clone, Deserialize)]
pub struct IpcTxFrame {
    #[serde(default = "default_channel")]
    pub ch: u8,
    #[serde(default)]
    pub id: u32,
    pub data: Vec<u8>,
    #[serde(default)]
    pub ext: bool,
    #[serde(default)]
    pub fd: bool,
    #[serde(default)]
    pub brs: bool,
    #[serde(default)]
    pub remote: bool,
}

fn default_channel() -> u8 {
    1
}

#[derive(Deserialize)]
struct IpcSendBatchArgs {
    frames: Vec<IpcTxFrame>,
    #[serde(default = "default_repeat")]
    repeat: u32,
}

fn default_repeat() -> u32 {
    1
}

pub enum IpcResp {
    Ok(serde_json::Value),
    Err { code: String, msg: String },
}

/// 一条投递给 UI 线程的请求 + 回复通道（tick 处理后写回 IpcResp）。
pub struct UiReq {
    pub client_id: u64,
    pub req: IpcReq,
    pub reply: SyncSender<IpcResp>,
}

// ---------------- 客户端订阅注册表 ----------------
pub struct ClientSub {
    pub client_id: u64,
    pub ids: HashSet<u32>,       // 空集 = 订阅全部帧
    pub out: SyncSender<String>, // 串行化输出队列（事件经此写给该客户端）
    pub dropped: Arc<AtomicU64>,
}

pub struct SubRegistry {
    pub subs: Mutex<Vec<ClientSub>>,
    pub active_client: AtomicU64, // 0 = 无（单运行闸门）
    next_client_id: AtomicU64,
    pub stop: AtomicBool,
}

impl SubRegistry {
    fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
            active_client: AtomicU64::new(0),
            next_client_id: AtomicU64::new(1),
            stop: AtomicBool::new(false),
        }
    }
}

// ---------------- 快照（只读操作从这里取，不等 tick）----------------
pub struct LastSnap {
    pub t: f64,
    pub count: u64,
    pub data: Vec<u8>,
    pub ext: bool,
}

/// 单通道统计快照（供 IPC status 的 channels 数组）。
#[derive(Clone)]
pub struct ChanStatSnap {
    pub ch: u8,
    pub rx: u64,
    pub tx: u64,
    pub err: u64,
    pub bus_load: f64,
    pub fps: f64,
}

pub struct Snapshot {
    pub connected: bool,
    pub running: bool,
    pub rx: u64,
    pub tx: u64,
    pub err: u64,
    pub no_counter: u64,
    pub bus_load: f64,
    pub fps: f64,
    pub dropped_frames: u64,
    pub dropped_events: u64,
    pub hardware_overruns: u64,
    pub hardware_errors: u64,
    pub event_queue_depth: usize,
    pub event_queue_capacity: usize,
    pub event_queue_high_watermark: usize,
    pub command_rejected: u64,
    pub command_queue_depth: usize,
    pub command_queue_capacity: usize,
    pub command_queue_high_watermark: usize,
    pub timestamp_samples: u64,
    pub timestamp_latest_jitter_us: f64,
    pub timestamp_max_jitter_us: f64,
    pub timestamp_drift_ppm: f64,
    pub timestamp_monotonic_violations: u64,
    pub channels: Vec<ChanStatSnap>,
    pub last_log: String,
    pub recent_logs: Vec<String>,
    pub console_text: String, // CAN 报文日志(printf-over-CAN)当前文本
    pub console_enabled: bool,
    pub last: HashMap<u64, LastSnap>,
    pub dbc: Arc<DbcSnapshot>,
}

impl Snapshot {
    pub fn new(dbc: Arc<DbcSnapshot>) -> Self {
        Self {
            connected: false,
            running: false,
            rx: 0,
            tx: 0,
            err: 0,
            no_counter: 0,
            bus_load: 0.0,
            fps: 0.0,
            dropped_frames: 0,
            dropped_events: 0,
            hardware_overruns: 0,
            hardware_errors: 0,
            event_queue_depth: 0,
            event_queue_capacity: 0,
            event_queue_high_watermark: 0,
            command_rejected: 0,
            command_queue_depth: 0,
            command_queue_capacity: 0,
            command_queue_high_watermark: 0,
            timestamp_samples: 0,
            timestamp_latest_jitter_us: 0.0,
            timestamp_max_jitter_us: 0.0,
            timestamp_drift_ppm: 0.0,
            timestamp_monotonic_violations: 0,
            channels: Vec::new(),
            last_log: String::new(),
            recent_logs: Vec::new(),
            console_text: String::new(),
            console_enabled: false,
            last: HashMap::new(),
            dbc,
        }
    }
}

/// 不可变 DBC 快照：DBC 变化时在主线程重建并经 Arc 换入，供只读 decode/encode 无锁/无 App 访问。
pub struct DbcSnapshot {
    dbcs: Vec<DbcDb>,
}

impl DbcSnapshot {
    pub fn empty() -> Self {
        Self { dbcs: Vec::new() }
    }
    pub fn from_dbcs(dbcs: &[DbcDb]) -> Self {
        Self {
            dbcs: dbcs.to_vec(),
        }
    }
    pub fn decode_ext(&self, id: u32, ext: bool, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let decoded = d.decode_ext(id, ext, data);
            if !decoded.is_empty() {
                return decoded;
            }
        }
        Vec::new()
    }
    pub fn encode_ext(&self, id: u32, ext: bool, vals: &HashMap<String, f64>) -> Option<Vec<u8>> {
        self.dbcs
            .iter()
            .find_map(|database| database.encode_ext(id, ext, vals))
    }
    /// 列出全部已加载 DBC 的报文与信号（供脚本发现真实信号名）。
    pub fn info(&self) -> serde_json::Value {
        let mut msgs = Vec::new();
        for d in &self.dbcs {
            for m in d.messages() {
                let signals: Vec<serde_json::Value> = m
                    .signals
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name, "unit": s.unit, "min": s.min, "max": s.max,
                            "start_bit": s.start_bit, "size": s.size,
                            "little_endian": s.little_endian, "signed": s.signed,
                            "factor": s.factor, "offset": s.offset,
                            "layout_valid": s.fits_in_bytes(m.size)
                        })
                    })
                    .collect();
                msgs.push(serde_json::json!({
                    "id": m.id, "extended": m.extended, "name": m.name, "dlc": m.size,
                    "file": d.file_name, "signals": signals
                }));
            }
        }
        serde_json::json!({ "messages": msgs })
    }

    /// Machine-readable diagnostics for automation and release gates.
    pub fn diagnostics(&self) -> serde_json::Value {
        let mut findings = Vec::new();
        let mut errors = 0u64;
        let mut warnings = 0u64;
        let mut infos = 0u64;
        for database in &self.dbcs {
            for diagnostic in database.diagnostics() {
                let severity = match diagnostic.severity {
                    DbcDiagnosticSeverity::Error => {
                        errors += 1;
                        "error"
                    }
                    DbcDiagnosticSeverity::Warning => {
                        warnings += 1;
                        "warning"
                    }
                    DbcDiagnosticSeverity::Info => {
                        infos += 1;
                        "info"
                    }
                };
                findings.push(serde_json::json!({
                    "file": database.file_name,
                    "severity": severity,
                    "code": diagnostic.code,
                    "id": diagnostic.message_id,
                    "extended": diagnostic.extended,
                    "message": diagnostic.message_name,
                    "signal": diagnostic.signal_name,
                    "title_zh": diagnostic.title_zh,
                    "title_en": diagnostic.title_en,
                    "detail_zh": diagnostic.detail_zh,
                    "detail_en": diagnostic.detail_en,
                }));
            }
        }
        findings.sort_by(|left, right| {
            let rank = |value: &serde_json::Value| match value["severity"].as_str() {
                Some("error") => 0,
                Some("warning") => 1,
                _ => 2,
            };
            rank(left)
                .cmp(&rank(right))
                .then_with(|| left["file"].as_str().cmp(&right["file"].as_str()))
                .then_with(|| left["id"].as_u64().cmp(&right["id"].as_u64()))
                .then_with(|| left["signal"].as_str().cmp(&right["signal"].as_str()))
                .then_with(|| left["code"].as_str().cmp(&right["code"].as_str()))
        });
        serde_json::json!({
            "files": self.dbcs.len(),
            "summary": {
                "errors": errors,
                "warnings": warnings,
                "infos": infos,
                "total": errors + warnings + infos,
                "blocking": errors > 0,
            },
            "findings": findings,
        })
    }
}

// ---------------- token：16 字节 CSPRNG（RtlGenRandom），失败回退最佳努力 ----------------
fn gen_token() -> String {
    let mut buf = [0u8; 16];
    let ok = unsafe {
        match libloading::Library::new("advapi32.dll") {
            Ok(lib) => {
                type RtlGenRandom = unsafe extern "system" fn(*mut u8, u32) -> u8;
                match lib.get::<RtlGenRandom>(b"SystemFunction036\0") {
                    Ok(f) => f(buf.as_mut_ptr(), buf.len() as u32) != 0,
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    };
    if !ok {
        use std::hash::{Hash, Hasher};
        static C: AtomicU64 = AtomicU64::new(0);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        C.fetch_add(1, Ordering::Relaxed).hash(&mut h);
        let a = h.finish();
        let b = a.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        buf[..8].copy_from_slice(&a.to_le_bytes());
        buf[8..].copy_from_slice(&b.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 帧事件 JSON 行（fan-out 在 tick 里调用，每帧构造一次再克隆给各订阅者）。
pub fn frame_event_json(f: &CanFrame) -> String {
    serde_json::json!({
        "v": 1, "id": 0, "event": "frame",
        "data": {
            "ch": f.ch, "id": f.id, "ext": f.ext, "fd": f.fd, "brs": f.brs,
            "remote": f.remote, "data": f.data, "t": f.t, "tx": f.tx, "count": 0
        }
    })
    .to_string()
}

// ---------------- 服务端启动 ----------------
/// 绑定 127.0.0.1:0，返回 (端口, token, UiReq 接收端, 订阅注册表)。
/// accept 循环线程 + 每客户端一个 handler 线程 + 一个 writer 线程。
pub fn spawn_ipc_server(
    snapshot: Arc<Mutex<Snapshot>>,
) -> (
    u16,
    String,
    crossbeam_channel::Receiver<UiReq>,
    Arc<SubRegistry>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 127.0.0.1:0 失败");
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let token = gen_token();
    let (ui_tx, ui_rx) = crossbeam_channel::bounded::<UiReq>(UI_REQ_QUEUE);
    let registry = Arc::new(SubRegistry::new());
    {
        let token = token.clone();
        let registry = registry.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if registry.stop.load(Ordering::Relaxed) {
                    break;
                }
                let stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let client_id = registry.next_client_id.fetch_add(1, Ordering::Relaxed);
                let token = token.clone();
                let registry = registry.clone();
                let snapshot = snapshot.clone();
                let ui_tx = ui_tx.clone();
                std::thread::spawn(move || {
                    handle_client(stream, client_id, &token, ui_tx, snapshot, registry)
                });
            }
        });
    }
    (port, token, ui_rx, registry)
}

fn read_line_capped(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<usize> {
    line.clear();
    let mut total = 0;
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(total);
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if total.saturating_add(consumed) > MAX_LINE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "line exceeds 1 MiB limit",
                ));
            }
            let text = std::str::from_utf8(&available[..consumed]).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "line is not valid UTF-8")
            })?;
            line.push_str(text);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        total += consumed;
        if complete {
            return Ok(total);
        }
    }
}

fn handle_client(
    stream: TcpStream,
    client_id: u64,
    token: &str,
    ui_tx: crossbeam_channel::Sender<UiReq>,
    snapshot: Arc<Mutex<Snapshot>>,
    registry: Arc<SubRegistry>,
) {
    let _ = stream.set_nodelay(true);
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);

    // 输出队列：唯一 writer 线程独占 socket 写端，response 与 event 都经它串行化（避免交错）。
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(EVT_QUEUE);
    {
        let mut w = write_stream;
        std::thread::spawn(move || {
            for line in out_rx {
                if w.write_all(line.as_bytes()).is_err() || w.write_all(b"\n").is_err() {
                    break;
                }
                let _ = w.flush();
            }
        });
    }

    // 握手：第一行必须 op==hello + token 正确 + 单运行闸门 CAS 成功。
    let mut line = String::new();
    match read_line_capped(&mut reader, &mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let req: ReqEnvelope = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let hid = req.id;
    if req.op != "hello" || req.token != token {
        let _ = out_tx.send(resp_err(hid, "BAD_TOKEN", "无效 token"));
        return;
    }
    if registry
        .active_client
        .compare_exchange(0, client_id, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        let _ = out_tx.send(resp_err(hid, "BAD_TOKEN", "已有脚本在运行"));
        return;
    }
    // 注册订阅（默认订阅全部）。
    registry.subs.lock().unwrap().push(ClientSub {
        client_id,
        ids: HashSet::new(),
        out: out_tx.clone(),
        dropped: Arc::new(AtomicU64::new(0)),
    });
    let _ = out_tx.send(resp_ok(hid, serde_json::json!({"app_version": crate::product_version::current(), "proto": 1, "caps": ["frame_sub", "dbc"]})));

    // 主循环。
    loop {
        match read_line_capped(&mut reader, &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let req: ReqEnvelope = match serde_json::from_str(t) {
            Ok(r) => r,
            Err(_) => {
                let _ = out_tx.send(resp_err(0, "PROTOCOL", "无法解析的 JSON 行"));
                continue;
            }
        };
        let id = req.id;
        // 只读操作：直接从 snapshot 回，不经 tick。
        if let Some(resp) = serve_readonly(&req.op, &req.args, &snapshot) {
            let _ = out_tx.send(resp_from(id, resp));
            continue;
        }
        // subscribe：在 handler 线程直接改注册表（Send 安全），不经 tick。
        if req.op == "subscribe" {
            let ids: HashSet<u32> = req
                .args
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64().map(|n| n as u32))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(s) = registry
                .subs
                .lock()
                .unwrap()
                .iter_mut()
                .find(|s| s.client_id == client_id)
            {
                s.ids = ids;
            }
            let _ = out_tx.send(resp_ok(id, serde_json::json!({})));
            continue;
        }
        // 状态变更操作 → 投递 tick，等回复（8s，超时合成 TIMEOUT，确保每请求恰好一条响应）。
        match parse_mutating(&req.op, &req.args) {
            Some(ipc_req) => {
                let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<IpcResp>(1);
                match ui_tx.try_send(UiReq {
                    client_id,
                    req: ipc_req,
                    reply: reply_tx,
                }) {
                    Ok(()) => {}
                    Err(crossbeam_channel::TrySendError::Full(_)) => {
                        let _ = out_tx.send(resp_err(
                            id,
                            "BUSY",
                            "UI 请求队列已满，操作未执行，请稍后重试",
                        ));
                        continue;
                    }
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                        let _ = out_tx.send(resp_err(id, "TIMEOUT", "应用已退出"));
                        continue;
                    }
                }
                match reply_rx.recv_timeout(Duration::from_secs(8)) {
                    Ok(resp) => {
                        let _ = out_tx.send(resp_from(id, resp));
                    }
                    Err(_) => {
                        let _ = out_tx.send(resp_err(id, "TIMEOUT", "UI 未在 8s 内响应"));
                    }
                }
            }
            None => {
                let _ = out_tx.send(resp_err(id, "UNKNOWN_OP", &format!("未知操作: {}", req.op)));
            }
        }
    }

    // 断开清理：通知 tick 停掉该客户端的周期任务；移除订阅；释放单运行闸门。
    let (cg_tx, cg_rx) = std::sync::mpsc::sync_channel::<IpcResp>(1);
    let _ = ui_tx.send_timeout(
        UiReq {
            client_id,
            req: IpcReq::ClientGone,
            reply: cg_tx,
        },
        Duration::from_millis(250),
    );
    registry
        .subs
        .lock()
        .unwrap()
        .retain(|s| s.client_id != client_id);
    // 先等 tick 确认已停掉本客户端的周期任务，再释放闸门，避免重连的下个客户端
    // 在旧周期帧仍被驱动到总线的窗口里抢到闸门并开始(最多等 8s 兜底)。
    let _ = cg_rx.recv_timeout(Duration::from_secs(8));
    let _ =
        registry
            .active_client
            .compare_exchange(client_id, 0, Ordering::SeqCst, Ordering::SeqCst);
}

fn arg_u64(args: &serde_json::Value, k: &str, dflt: u64) -> u64 {
    args.get(k).and_then(|v| v.as_u64()).unwrap_or(dflt)
}
fn arg_bool(args: &serde_json::Value, k: &str) -> bool {
    args.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn arg_bytes(args: &serde_json::Value) -> Vec<u8> {
    args.get("data")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as u8))
                .collect()
        })
        .unwrap_or_default()
}

fn serve_readonly(
    op: &str,
    args: &serde_json::Value,
    snapshot: &Arc<Mutex<Snapshot>>,
) -> Option<IpcResp> {
    match op {
        "status" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(serde_json::json!({
                "connected": s.connected, "running": s.running,
                "rx": s.rx, "tx": s.tx, "err": s.err, "no_counter": s.no_counter,
                "bus_load": s.bus_load, "fps": s.fps,
                "last_log": s.last_log,
                "capture_health": {
                    "dropped_frames": s.dropped_frames,
                    "dropped_events": s.dropped_events,
                    "hardware_overruns": s.hardware_overruns,
                    "hardware_errors": s.hardware_errors,
                    "event_queue_depth": s.event_queue_depth,
                    "event_queue_capacity": s.event_queue_capacity,
                    "event_queue_high_watermark": s.event_queue_high_watermark,
                    "command_rejected": s.command_rejected,
                    "command_queue_depth": s.command_queue_depth,
                    "command_queue_capacity": s.command_queue_capacity,
                    "command_queue_high_watermark": s.command_queue_high_watermark,
                },
                "timestamp_quality": {
                    "samples": s.timestamp_samples,
                    "latest_jitter_us": s.timestamp_latest_jitter_us,
                    "max_jitter_us": s.timestamp_max_jitter_us,
                    "drift_ppm": s.timestamp_drift_ppm,
                    "monotonic_violations": s.timestamp_monotonic_violations,
                },
                "channels": s.channels.iter().map(|c| serde_json::json!({
                    "ch": c.ch, "rx": c.rx, "tx": c.tx, "err": c.err,
                    "bus_load": c.bus_load, "fps": c.fps
                })).collect::<Vec<_>>()
            })))
        }
        "console" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(serde_json::json!({
                "enabled": s.console_enabled,
                "text": s.console_text,
            })))
        }
        "get_last" => {
            let ch = arg_u64(args, "ch", 1) as u8;
            let id = arg_u64(args, "id", 0) as u32;
            let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("rx");
            let ext = arg_bool(args, "ext");
            let key = crate::key_of(ch, dir == "tx", ext, id);
            let s = snapshot.lock().unwrap();
            Some(match s.last.get(&key) {
                Some(l) => IpcResp::Ok(
                    serde_json::json!({"present": true, "t": l.t, "count": l.count, "data": l.data, "ext": l.ext, "dir": dir}),
                ),
                None => IpcResp::Ok(serde_json::json!({"present": false, "dir": dir})),
            })
        }
        "get_signal" => {
            let ch = arg_u64(args, "ch", 1) as u8;
            let id = arg_u64(args, "id", 0) as u32;
            let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("rx");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let ext = arg_bool(args, "ext");
            let key = crate::key_of(ch, dir == "tx", ext, id);
            let s = snapshot.lock().unwrap();
            let phys = s.last.get(&key).and_then(|l| {
                s.dbc
                    .decode_ext(id, l.ext, &l.data)
                    .into_iter()
                    .find(|d| d.name == name)
                    .map(|d| d.physical)
            });
            Some(IpcResp::Ok(
                serde_json::json!({"present": phys.is_some(), "physical": phys, "dir": dir}),
            ))
        }
        "decode" => {
            let id = arg_u64(args, "id", 0) as u32;
            let ext = arg_bool(args, "ext");
            let data = arg_bytes(args);
            let s = snapshot.lock().unwrap();
            let sigs: Vec<serde_json::Value> = s
                .dbc
                .decode_ext(id, ext, &data)
                .into_iter()
                .map(|d| {
                    let raw = d
                        .raw_unsigned
                        .map(serde_json::Value::from)
                        .unwrap_or_else(|| serde_json::Value::from(d.raw));
                    serde_json::json!({"name": d.name, "physical": d.physical, "unit": d.unit, "raw": raw, "raw_text": d.raw_text, "min": d.min, "max": d.max, "out_of_range": d.out_of_range})
                })
                .collect();
            Some(IpcResp::Ok(serde_json::json!({"signals": sigs})))
        }
        "encode" => {
            let id = arg_u64(args, "id", 0) as u32;
            let ext = arg_bool(args, "ext");
            let signals: HashMap<String, f64> = args
                .get("signals")
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                        .collect()
                })
                .unwrap_or_default();
            let s = snapshot.lock().unwrap();
            Some(match s.dbc.encode_ext(id, ext, &signals) {
                Some(b) => IpcResp::Ok(serde_json::json!({"present": true, "data": b})),
                None => IpcResp::Ok(serde_json::json!({"present": false})),
            })
        }
        "dbc_info" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(s.dbc.info()))
        }
        "dbc_diagnostics" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(s.dbc.diagnostics()))
        }
        "logs" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(serde_json::json!({"lines": s.recent_logs})))
        }
        _ => None,
    }
}

fn parse_mutating(op: &str, args: &serde_json::Value) -> Option<IpcReq> {
    match op {
        "send_once" => Some(match serde_json::from_value::<IpcTxFrame>(args.clone()) {
            Ok(frame) => IpcReq::SendOnce {
                ch: frame.ch,
                id: frame.id,
                data: frame.data,
                ext: frame.ext,
                fd: frame.fd,
                brs: frame.brs,
                remote: frame.remote,
            },
            Err(error) => IpcReq::Invalid {
                code: "BAD_ARG".into(),
                msg: format!("send_once 参数无效: {error}"),
            },
        }),
        "send_batch" => Some(
            match serde_json::from_value::<IpcSendBatchArgs>(args.clone()) {
                Ok(batch) => IpcReq::SendBatch {
                    frames: batch.frames,
                    repeat: batch.repeat,
                },
                Err(error) => IpcReq::Invalid {
                    code: "BAD_ARG".into(),
                    msg: format!("send_batch 参数无效: {error}"),
                },
            },
        ),
        "set_periodic" => Some(IpcReq::SetPeriodic {
            client_handle: arg_u64(args, "handle", 0),
            ch: arg_u64(args, "ch", 1) as u8,
            id: arg_u64(args, "id", 0) as u32,
            data: arg_bytes(args),
            period_ms: arg_u64(args, "period_ms", 100),
            repeat: args.get("repeat").and_then(|v| v.as_i64()).unwrap_or(-1),
            ext: arg_bool(args, "ext"),
            fd: arg_bool(args, "fd"),
            brs: arg_bool(args, "brs"),
            remote: arg_bool(args, "remote"),
        }),
        "stop_periodic" => Some(IpcReq::StopPeriodic {
            client_handle: arg_u64(args, "handle", 0),
        }),
        "connect" => {
            let channels: Vec<DeviceConfig> = args
                .get("channels")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            Some(IpcReq::Connect { channels })
        }
        "connect_configured" => Some(IpcReq::ConnectConfigured),
        "load_dbc" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // 在 handler 线程解析(读文件+解析)，不占用 UI tick；UI 侧只做推入+重建快照。
            let loaded = if path.trim().is_empty() {
                Err("空路径".to_string())
            } else {
                DbcDb::load(&path)
            };
            Some(IpcReq::LoadDbc { path, loaded })
        }
        "disconnect" => Some(IpcReq::Disconnect),
        "start" => Some(IpcReq::Start),
        "stop" => Some(IpcReq::Stop),
        "log" => Some(IpcReq::Log {
            msg: args
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "run_result" => Some(IpcReq::RunResult {
            passed: arg_bool(args, "passed"),
            summary: args
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        "console_set" => Some(IpcReq::ConsoleSet {
            enabled: args.get("enabled").and_then(|v| v.as_bool()),
            id: args.get("id").and_then(|v| v.as_i64()),
            ch: args.get("ch").and_then(|v| v.as_u64()).map(|n| n as u8),
            clear: args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false),
        }),
        _ => None,
    }
}

fn resp_ok(id: u64, result: serde_json::Value) -> String {
    serde_json::json!({"v": 1, "id": id, "ok": true, "result": result}).to_string()
}
fn resp_err(id: u64, code: &str, msg: &str) -> String {
    serde_json::json!({"v": 1, "id": id, "ok": false, "err": {"code": code, "msg": msg}})
        .to_string()
}
fn resp_from(id: u64, r: IpcResp) -> String {
    match r {
        IpcResp::Ok(result) => resp_ok(id, result),
        IpcResp::Err { code, msg } => resp_err(id, &code, &msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn overlong_line_without_newline_is_rejected_before_growth() {
        let input = vec![b'x'; MAX_LINE + 4096];
        let mut reader = BufReader::with_capacity(64, Cursor::new(input));
        let mut line = String::new();
        let error = read_line_capped(&mut reader, &mut line).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(line.len() <= MAX_LINE);
    }

    #[test]
    fn capped_reader_preserves_complete_ndjson_line() {
        let mut reader = BufReader::with_capacity(3, Cursor::new(b"{\"op\":\"hello\"}\n"));
        let mut line = String::new();
        let count = read_line_capped(&mut reader, &mut line).unwrap();
        assert_eq!(count, line.len());
        assert_eq!(line, "{\"op\":\"hello\"}\n");
    }

    #[test]
    fn batch_parser_preserves_every_frame_field() {
        let args = serde_json::json!({
            "repeat": 3,
            "frames": [
                {"ch": 2, "id": 0x123, "data": [1, 2, 3]},
                {"ch": 4, "id": 0x18FF50E5u32, "data": vec![4; 12], "ext": true, "fd": true, "brs": true}
            ]
        });
        let Some(IpcReq::SendBatch { frames, repeat }) = parse_mutating("send_batch", &args) else {
            panic!("batch should parse");
        };
        assert_eq!(repeat, 3);
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[1].ch, frames[1].id), (4, 0x18FF50E5));
        assert!(frames[1].ext && frames[1].fd && frames[1].brs);
        assert_eq!(frames[1].data.len(), 12);
    }

    #[test]
    fn send_parser_rejects_out_of_byte_range_without_truncation() {
        let args = serde_json::json!({"ch": 1, "id": 1, "data": [256]});
        assert!(matches!(
            parse_mutating("send_once", &args),
            Some(IpcReq::Invalid { .. })
        ));
    }
}
