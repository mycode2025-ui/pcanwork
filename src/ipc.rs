//! Python 自动化测试的 IPC 服务端：TCP loopback (127.0.0.1) + NDJSON 协议。
//!
//! PcanWork 是服务端；外部 python.exe（运行用户测试脚本）作为客户端连接，驱动总线。
//! 本模块**只用 Send 安全类型**（std + serde + crate::can/dbc 的克隆快照），绝不触碰
//! App / Rc<RefCell> / 硬件：
//!   - 只读操作（status/get_last/get_signal/decode/encode）在 handler 线程直接从
//!     `Arc<Mutex<Snapshot>>` 即时回复，不经 100ms tick（避免 UI 卡顿时读操作全超时）；
//!   - 状态变更操作（connect/send/set_periodic/...）封成 `UiReq` 投递给主线程 tick 处理。

use crate::can::{CanFrame, DeviceConfig};
use crate::dbc::{DbcDb, Decoded};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EVT_QUEUE: usize = 4096; // 每客户端输出队列上限（事件满则丢最新并计数）
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
    SendOnce { ch: u8, id: u32, data: Vec<u8>, ext: bool, fd: bool, brs: bool, remote: bool },
    SetPeriodic { client_handle: u64, ch: u8, id: u32, data: Vec<u8>, period_ms: u64, repeat: i64, ext: bool, fd: bool, brs: bool, remote: bool },
    StopPeriodic { client_handle: u64 },
    Connect { channels: Vec<DeviceConfig> },
    ConnectVirtual,
    ConnectConfigured, // 连接主界面"设备"对话框里已配置的多通道列表（a.channels）
    // 脚本加载 DBC：在 handler 线程预解析(loaded)，UI tick 仅推入，避免大文件解析阻塞界面。
    LoadDbc { path: String, loaded: Result<DbcDb, String> },
    Disconnect,
    Start,
    Stop,
    Log { msg: String },
    RunResult { passed: bool, summary: String },
    // CAN 报文日志(printf-over-CAN)配置: 字段为 None 表示该项不改
    ConsoleSet { enabled: Option<bool>, id: Option<i64>, ch: Option<u8>, clear: bool },
    ClientGone,
}

pub enum IpcResp {
    Ok(serde_json::Value),
    Err { code: String, msg: String },
}

/// 一条投递给 UI 线程的请求 + 回复通道（tick 处理后写回 IpcResp）。
pub struct UiReq {
    pub client_id: u64,
    pub req: IpcReq,
    pub reply: Sender<IpcResp>,
}

// ---------------- 客户端订阅注册表 ----------------
pub struct ClientSub {
    pub client_id: u64,
    pub ids: HashSet<u32>, // 空集 = 订阅全部帧
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
    pub channels: Vec<ChanStatSnap>,
    pub console_text: String, // CAN 报文日志(printf-over-CAN)当前文本
    pub console_enabled: bool,
    pub last: HashMap<u64, LastSnap>,
    pub dbc: Arc<DbcSnapshot>,
}

impl Snapshot {
    pub fn new(dbc: Arc<DbcSnapshot>) -> Self {
        Self { connected: false, running: false, rx: 0, tx: 0, err: 0, no_counter: 0, bus_load: 0.0, fps: 0.0, channels: Vec::new(), console_text: String::new(), console_enabled: false, last: HashMap::new(), dbc }
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
        Self { dbcs: dbcs.to_vec() }
    }
    /// 合并全部 DBC（先加载者优先）：返回首个非空解码。
    pub fn decode(&self, id: u32, data: &[u8]) -> Vec<Decoded> {
        for d in &self.dbcs {
            let dec = d.decode(id, data);
            if !dec.is_empty() {
                return dec;
            }
        }
        Vec::new()
    }
    pub fn encode(&self, id: u32, vals: &HashMap<String, f64>) -> Option<Vec<u8>> {
        for d in &self.dbcs {
            if let Some(b) = d.encode(id, vals) {
                return Some(b);
            }
        }
        None
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
                            "factor": s.factor, "offset": s.offset
                        })
                    })
                    .collect();
                msgs.push(serde_json::json!({
                    "id": m.id, "name": m.name, "dlc": m.size,
                    "file": d.file_name, "signals": signals
                }));
            }
        }
        serde_json::json!({ "messages": msgs })
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
pub fn spawn_ipc_server(snapshot: Arc<Mutex<Snapshot>>) -> (u16, String, Receiver<UiReq>, Arc<SubRegistry>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定 127.0.0.1:0 失败");
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let token = gen_token();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiReq>();
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
                std::thread::spawn(move || handle_client(stream, client_id, &token, ui_tx, snapshot, registry));
            }
        });
    }
    (port, token, ui_rx, registry)
}

fn read_line_capped(reader: &mut impl BufRead, line: &mut String) -> std::io::Result<usize> {
    line.clear();
    let n = reader.read_line(line)?;
    if line.len() > MAX_LINE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "行超长"));
    }
    Ok(n)
}

fn handle_client(stream: TcpStream, client_id: u64, token: &str, ui_tx: Sender<UiReq>, snapshot: Arc<Mutex<Snapshot>>, registry: Arc<SubRegistry>) {
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
    if registry.active_client.compare_exchange(0, client_id, Ordering::SeqCst, Ordering::SeqCst).is_err() {
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
    let _ = out_tx.send(resp_ok(hid, serde_json::json!({"app_version": env!("CARGO_PKG_VERSION"), "proto": 1, "caps": ["frame_sub", "dbc"]})));

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
                .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u32)).collect())
                .unwrap_or_default();
            if let Some(s) = registry.subs.lock().unwrap().iter_mut().find(|s| s.client_id == client_id) {
                s.ids = ids;
            }
            let _ = out_tx.send(resp_ok(id, serde_json::json!({})));
            continue;
        }
        // 状态变更操作 → 投递 tick，等回复（8s，超时合成 TIMEOUT，确保每请求恰好一条响应）。
        match parse_mutating(&req.op, &req.args) {
            Some(ipc_req) => {
                let (reply_tx, reply_rx) = std::sync::mpsc::channel::<IpcResp>();
                if ui_tx.send(UiReq { client_id, req: ipc_req, reply: reply_tx }).is_err() {
                    let _ = out_tx.send(resp_err(id, "TIMEOUT", "应用已退出"));
                    continue;
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
    let (cg_tx, cg_rx) = std::sync::mpsc::channel::<IpcResp>();
    let _ = ui_tx.send(UiReq { client_id, req: IpcReq::ClientGone, reply: cg_tx });
    registry.subs.lock().unwrap().retain(|s| s.client_id != client_id);
    // 先等 tick 确认已停掉本客户端的周期任务，再释放闸门，避免重连的下个客户端
    // 在旧周期帧仍被驱动到总线的窗口里抢到闸门并开始(最多等 8s 兜底)。
    let _ = cg_rx.recv_timeout(Duration::from_secs(8));
    let _ = registry.active_client.compare_exchange(client_id, 0, Ordering::SeqCst, Ordering::SeqCst);
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
        .map(|a| a.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect())
        .unwrap_or_default()
}

fn serve_readonly(op: &str, args: &serde_json::Value, snapshot: &Arc<Mutex<Snapshot>>) -> Option<IpcResp> {
    match op {
        "status" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(serde_json::json!({
                "connected": s.connected, "running": s.running,
                "rx": s.rx, "tx": s.tx, "err": s.err, "no_counter": s.no_counter,
                "bus_load": s.bus_load, "fps": s.fps,
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
            let key = crate::key_of(ch, dir == "tx", id);
            let s = snapshot.lock().unwrap();
            Some(match s.last.get(&key) {
                Some(l) => IpcResp::Ok(serde_json::json!({"present": true, "t": l.t, "count": l.count, "data": l.data, "ext": l.ext, "dir": dir})),
                None => IpcResp::Ok(serde_json::json!({"present": false, "dir": dir})),
            })
        }
        "get_signal" => {
            let ch = arg_u64(args, "ch", 1) as u8;
            let id = arg_u64(args, "id", 0) as u32;
            let dir = args.get("dir").and_then(|v| v.as_str()).unwrap_or("rx");
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let key = crate::key_of(ch, dir == "tx", id);
            let s = snapshot.lock().unwrap();
            let phys = s
                .last
                .get(&key)
                .and_then(|l| s.dbc.decode(id, &l.data).into_iter().find(|d| d.name == name).map(|d| d.physical));
            Some(IpcResp::Ok(serde_json::json!({"present": phys.is_some(), "physical": phys, "dir": dir})))
        }
        "decode" => {
            let id = arg_u64(args, "id", 0) as u32;
            let data = arg_bytes(args);
            let s = snapshot.lock().unwrap();
            let sigs: Vec<serde_json::Value> = s
                .dbc
                .decode(id, &data)
                .into_iter()
                .map(|d| serde_json::json!({"name": d.name, "physical": d.physical, "unit": d.unit, "raw": d.raw, "min": d.min, "max": d.max, "out_of_range": d.out_of_range}))
                .collect();
            Some(IpcResp::Ok(serde_json::json!({"signals": sigs})))
        }
        "encode" => {
            let id = arg_u64(args, "id", 0) as u32;
            let signals: HashMap<String, f64> = args
                .get("signals")
                .and_then(|v| v.as_object())
                .map(|o| o.iter().filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f))).collect())
                .unwrap_or_default();
            let s = snapshot.lock().unwrap();
            Some(match s.dbc.encode(id, &signals) {
                Some(b) => IpcResp::Ok(serde_json::json!({"present": true, "data": b})),
                None => IpcResp::Ok(serde_json::json!({"present": false})),
            })
        }
        "dbc_info" => {
            let s = snapshot.lock().unwrap();
            Some(IpcResp::Ok(s.dbc.info()))
        }
        _ => None,
    }
}

fn parse_mutating(op: &str, args: &serde_json::Value) -> Option<IpcReq> {
    match op {
        "send_once" => Some(IpcReq::SendOnce {
            ch: arg_u64(args, "ch", 1) as u8,
            id: arg_u64(args, "id", 0) as u32,
            data: arg_bytes(args),
            ext: arg_bool(args, "ext"),
            fd: arg_bool(args, "fd"),
            brs: arg_bool(args, "brs"),
            remote: arg_bool(args, "remote"),
        }),
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
        "stop_periodic" => Some(IpcReq::StopPeriodic { client_handle: arg_u64(args, "handle", 0) }),
        "connect" => {
            let channels: Vec<DeviceConfig> = args.get("channels").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            Some(IpcReq::Connect { channels })
        }
        "connect_virtual" => Some(IpcReq::ConnectVirtual),
        "connect_configured" => Some(IpcReq::ConnectConfigured),
        "load_dbc" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
        "log" => Some(IpcReq::Log { msg: args.get("msg").and_then(|v| v.as_str()).unwrap_or("").to_string() }),
        "run_result" => Some(IpcReq::RunResult {
            passed: arg_bool(args, "passed"),
            summary: args.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string(),
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
    serde_json::json!({"v": 1, "id": id, "ok": false, "err": {"code": code, "msg": msg}}).to_string()
}
fn resp_from(id: u64, r: IpcResp) -> String {
    match r {
        IpcResp::Ok(result) => resp_ok(id, result),
        IpcResp::Err { code, msg } => resp_err(id, &code, &msg),
    }
}
