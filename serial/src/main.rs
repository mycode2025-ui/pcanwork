// GUI subsystem: do not attach a console window on Windows.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use anyhow::{Context, Result, bail};
use chrono::Local;
use rfd::FileDialog;
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod windows_dpi;

slint::include_modules!();

// 高频定时发送需要 1ms 级 sleep 精度，Windows 默认计时器粒度约 15.6ms
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
}

#[derive(Clone)]
struct SerialConfig {
    port_name: String,
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
    rts: bool,
    dtr: bool,
}

struct SerialSession {
    port: Arc<Mutex<Box<dyn SerialPort>>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
}

/// 原生 SSH 串口: 用系统 ssh 客户端在远端跑 socat, 把远端串口接到 ssh 的 stdin/stdout。
/// 子进程 stdout = 远端设备数据 → 接收; 写 stdin = 发往远端设备。
struct SshSession {
    child: Arc<Mutex<std::process::Child>>,
    stdin: Arc<Mutex<std::process::ChildStdin>>,
    stop: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<()>>,
    err_reader: Option<thread::JoinHandle<()>>,
}

enum Connection {
    Serial(SerialSession),
    Network(NetworkSession),
    Ssh(SshSession),
}

enum NetworkSession {
    TcpClient {
        stream: Arc<Mutex<TcpStream>>,
        stop: Arc<AtomicBool>,
        reader: Option<thread::JoinHandle<()>>,
    },
    TcpServer {
        client: Arc<Mutex<Option<TcpStream>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    },
    Udp {
        socket: Arc<Mutex<UdpSocket>>,
        remote: String,
        stop: Arc<AtomicBool>,
        reader: Option<thread::JoinHandle<()>>,
    },
}

#[derive(Clone)]
enum SendTarget {
    Serial(Arc<Mutex<Box<dyn SerialPort>>>),
    TcpClient(Arc<Mutex<TcpStream>>),
    TcpServer(Arc<Mutex<Option<TcpStream>>>),
    Udp(Arc<Mutex<UdpSocket>>, String),
    Ssh(Arc<Mutex<std::process::ChildStdin>>),
}

impl Drop for NetworkSession {
    fn drop(&mut self) {
        match self {
            NetworkSession::TcpClient { stop, reader, .. } => {
                stop.store(true, Ordering::Relaxed);
                if let Some(reader) = reader.take() {
                    let _ = reader.join();
                }
            }
            NetworkSession::TcpServer { stop, worker, .. } => {
                stop.store(true, Ordering::Relaxed);
                if let Some(worker) = worker.take() {
                    let _ = worker.join();
                }
            }
            NetworkSession::Udp { stop, reader, .. } => {
                stop.store(true, Ordering::Relaxed);
                if let Some(reader) = reader.take() {
                    let _ = reader.join();
                }
            }
        }
    }
}

impl Drop for SerialSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill(); // 关闭 stdout 管道, 让读线程退出
        }
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        if let Some(r) = self.err_reader.take() {
            let _ = r.join();
        }
    }
}

enum UiEvent {
    Received(Vec<u8>, Instant),
    Sent(Vec<u8>),
    Status(String),
    /// 重要提示：同时写入接收区和状态栏
    Notice(String),
    Closed,
}

fn main() -> Result<()> {
    #[cfg(windows)]
    windows_dpi::force_system_dpi_awareness();

    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
    }
    let app = AppWindow::new()?;
    let quick = QuickWindow::new()?;
    let app_weak = app.as_weak();
    let (tx, rx) = mpsc::channel::<UiEvent>();
    let session: Rc<RefCell<Option<Connection>>> = Rc::new(RefCell::new(None));
    let auto_sender: Rc<RefCell<Option<AutoSender>>> = Rc::new(RefCell::new(None));
    let current_file: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let quick_stop = Arc::new(AtomicBool::new(false));
    let rx_file: Rc<RefCell<Option<fs::File>>> = Rc::new(RefCell::new(None));
    let last_rx: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));

    install_defaults(&app);
    quick.set_quick_commands(ModelRc::new(VecModel::from(default_quick_commands())));
    refresh_ports(&app);
    load_settings(&app);

    // 多条命令窗口：独立 OS 窗口，可拖出主窗口
    {
        let app = app.as_weak();
        let quick_weak = quick.as_weak();
        app.unwrap().on_toggle_quick_panel(move || {
            if let (Some(app), Some(quick)) = (app.upgrade(), quick_weak.upgrade()) {
                if app.get_quick_panel_visible() {
                    let _ = quick.hide();
                    app.set_quick_panel_visible(false);
                } else {
                    quick.global::<I18n>().set_en(app.global::<I18n>().get_en());
                    let _ = quick.show();
                    app.set_quick_panel_visible(true);
                }
            }
        });
    }

    {
        let app = app.as_weak();
        quick.window().on_close_requested(move || {
            if let Some(app) = app.upgrade() {
                app.set_quick_panel_visible(false);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 两个窗口各持一份 I18n 全局，主窗口切换语言时同步
    {
        let app = app.as_weak();
        let quick_weak = quick.as_weak();
        app.unwrap().on_english_changed(move || {
            if let (Some(app), Some(quick)) = (app.upgrade(), quick_weak.upgrade()) {
                quick.global::<I18n>().set_en(app.global::<I18n>().get_en());
            }
        });
    }

    {
        let app = app.as_weak();
        app.unwrap().on_refresh_ports(move || {
            if let Some(app) = app.upgrade() {
                refresh_ports(&app);
            }
        });
    }

    {
        let app = app.as_weak();
        let session = Rc::clone(&session);
        let auto_sender = Rc::clone(&auto_sender);
        let quick_stop = Arc::clone(&quick_stop);
        let tx = tx.clone();
        app.unwrap().on_toggle_port(move || {
            if let Some(app) = app.upgrade() {
                if session.borrow().is_some() {
                    // 先停掉定时/循环发送线程，避免它们持有的句柄让连接关不掉
                    quick_stop.store(true, Ordering::Relaxed);
                    *auto_sender.borrow_mut() = None;
                    app.set_auto_sending(false);
                    *session.borrow_mut() = None;
                    app.set_connected(false);
                    append_status(&app, "连接已关闭");
                    return;
                }

                match open_connection(&app, tx.clone()) {
                    Ok(sess) => {
                        let port_name = app.get_selected_port().to_string();
                        *session.borrow_mut() = Some(sess);
                        app.set_connected(true);
                        append_status(&app, &format!("已打开 {}", port_name));
                    }
                    Err(err) => append_status(&app, &format!("打开失败: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let session = Rc::clone(&session);
        app.unwrap().on_send_data(move || {
            if let Some(app) = app.upgrade() {
                let payload = app.get_send_text().to_string();
                match build_payload(
                    &payload,
                    app.get_send_hex(),
                    app.get_append_newline(),
                    app.get_selected_checksum().as_str(),
                    app.get_from_byte(),
                ) {
                    Ok(bytes) => send_bytes(&app, &session, &bytes),
                    Err(err) => append_status(&app, &format!("发送内容无效: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let session = Rc::clone(&session);
        quick.on_send_quick(move |payload, hex| {
            if let Some(app) = app.upgrade() {
                match build_payload(
                    &payload,
                    hex,
                    app.get_append_newline(),
                    app.get_selected_checksum().as_str(),
                    app.get_from_byte(),
                ) {
                    Ok(bytes) => send_bytes(&app, &session, &bytes),
                    Err(err) => append_notice(&app, &format!("多条发送内容无效: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let quick_weak = quick.as_weak();
        let session = Rc::clone(&session);
        let quick_stop = Arc::clone(&quick_stop);
        let tx = tx.clone();
        quick.on_run_quick_sequence(move || {
            if let (Some(app), Some(quick)) = (app.upgrade(), quick_weak.upgrade()) {
                let Some(conn) = session.borrow().as_ref().map(clone_send_target) else {
                    append_notice(&app, "顺序发送未执行：请先打开串口或网络连接");
                    return;
                };
                let rows = quick.get_quick_commands();
                let mut commands = Vec::new();
                for i in 0..rows.row_count() {
                    if let Some(row) = rows.row_data(i) {
                        if row.enabled && !row.payload.trim().is_empty() {
                            let payload = match build_payload(
                                &row.payload,
                                row.hex,
                                app.get_append_newline(),
                                app.get_selected_checksum().as_str(),
                                app.get_from_byte(),
                            ) {
                                Ok(payload) => payload,
                                Err(err) => {
                                    append_notice(&app, &format!("第 {} 条内容无效: {err:#}", i + 1));
                                    return;
                                }
                            };
                            commands.push((payload, row.delay_ms.max(0) as u64));
                        }
                    }
                }
                if commands.is_empty() {
                    append_notice(&app, "顺序发送未执行：没有勾选启用且内容非空的指令行");
                    return;
                }
                let loop_enabled = quick.get_quick_loop_enabled();
                let stop = Arc::clone(&quick_stop);
                stop.store(false, Ordering::Relaxed);
                let tx = tx.clone();
                let total = commands.len();
                thread::spawn(move || {
                    'outer: loop {
                        for (payload, delay_ms) in &commands {
                            if stop.load(Ordering::Relaxed) {
                                break 'outer;
                            }
                            match write_send_target(&conn, payload) {
                                Ok(()) => {
                                    let _ = tx.send(UiEvent::Sent(payload.clone()));
                                }
                                Err(err) => {
                                    let _ = tx.send(UiEvent::Notice(format!("多条发送失败: {err:#}")));
                                    return;
                                }
                            }
                            // 分段休眠，保证“停止”能在 50ms 内生效
                            let mut remaining = *delay_ms;
                            while remaining > 0 && !stop.load(Ordering::Relaxed) {
                                let step = remaining.min(50);
                                thread::sleep(Duration::from_millis(step));
                                remaining -= step;
                            }
                        }
                        if !loop_enabled || stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    let _ = tx.send(UiEvent::Status("多条发送结束".into()));
                });
                append_notice(&app, &format!("顺序发送已启动，共 {total} 条"));
            }
        });
    }

    {
        let app = app.as_weak();
        let quick_weak = quick.as_weak();
        quick.on_import_quick_ini(move || {
            if let (Some(app), Some(quick)) = (app.upgrade(), quick_weak.upgrade()) {
                match import_quick_ini() {
                    Ok(rows) => {
                        quick.set_quick_commands(ModelRc::new(VecModel::from(rows)));
                        append_status(&app, "已导入多条字符串 ini");
                    }
                    Err(err) => append_status(&app, &format!("导入 ini 失败: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let quick_weak = quick.as_weak();
        quick.on_export_quick_ini(move || {
            if let (Some(app), Some(quick)) = (app.upgrade(), quick_weak.upgrade()) {
                match export_quick_ini(&quick.get_quick_commands()) {
                    Ok(path) => append_status(&app, &format!("已导出 ini: {}", path.display())),
                    Err(err) => append_status(&app, &format!("导出 ini 失败: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        app.unwrap().on_clear_receive(move || {
            if let Some(app) = app.upgrade() {
                app.set_receive_text("".into());
                app.set_rx_count(0);
            }
        });
    }

    {
        let app = app.as_weak();
        app.unwrap().on_clear_send(move || {
            if let Some(app) = app.upgrade() {
                app.set_send_text("".into());
                app.set_tx_count(0);
            }
        });
    }

    {
        let app = app.as_weak();
        app.unwrap().on_save_receive(move || {
            if let Some(app) = app.upgrade() {
                match save_receive_log(&app) {
                    Ok(path) => append_status(&app, &format!("已保存接收区: {}", path.display())),
                    Err(err) => append_status(&app, &format!("保存失败: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let current_file = Rc::clone(&current_file);
        app.unwrap().on_open_file(move || {
            if let Some(app) = app.upgrade() {
                match pick_default_file() {
                    Some(path) => match fs::read_to_string(&path) {
                        Ok(text) => {
                            app.set_send_text(text.into());
                            app.set_selected_file(path.display().to_string().into());
                            *current_file.borrow_mut() = Some(path);
                        }
                        Err(err) => append_status(&app, &format!("读取文件失败: {err}")),
                    },
                    None => append_status(&app, "请把要发送的文件放到当前目录，或直接在发送区输入内容"),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let session = Rc::clone(&session);
        let current_file = Rc::clone(&current_file);
        app.unwrap().on_send_file(move || {
            if let Some(app) = app.upgrade() {
                match current_file.borrow().as_ref().cloned() {
                    Some(path) => match fs::read(&path) {
                        Ok(bytes) => send_bytes(&app, &session, &bytes),
                        Err(err) => append_status(&app, &format!("发送文件失败: {err}")),
                    },
                    None => append_status(&app, "未选择文件"),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let session = Rc::clone(&session);
        let auto_sender = Rc::clone(&auto_sender);
        let tx = tx.clone();
        app.unwrap().on_toggle_auto_send(move || {
            if let Some(app) = app.upgrade() {
                if auto_sender.borrow().is_some() {
                    *auto_sender.borrow_mut() = None;
                    app.set_auto_sending(false);
                    append_status(&app, "定时发送已停止");
                    return;
                }

                let payload = match build_payload(
                    &app.get_send_text().to_string(),
                    app.get_send_hex(),
                    app.get_append_newline(),
                    app.get_selected_checksum().as_str(),
                    app.get_from_byte(),
                ) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        append_status(&app, &format!("定时发送内容无效: {err:#}"));
                        return;
                    }
                };
                if let Some(conn) = session.borrow().as_ref() {
                    let interval = app.get_auto_interval_ms().max(1) as u64;
                    let stop = Arc::new(AtomicBool::new(false));
                    let thread_stop = Arc::clone(&stop);
                    let target = clone_send_target(conn);
                    let tx = tx.clone();
                    let handle = thread::spawn(move || {
                        while !thread_stop.load(Ordering::Relaxed) {
                            match write_send_target(&target, &payload) {
                                Ok(()) => {
                                    let _ = tx.send(UiEvent::Sent(payload.clone()));
                                }
                                Err(err) => {
                                    let _ = tx.send(UiEvent::Notice(format!("定时发送失败: {err:#}")));
                                    break;
                                }
                            }
                            thread::sleep(Duration::from_millis(interval));
                        }
                    });
                    *auto_sender.borrow_mut() = Some(AutoSender { stop, handle: Some(handle) });
                    app.set_auto_sending(true);
                    append_status(&app, "定时发送已启动");
                } else {
                    append_status(&app, "请先打开串口");
                }
            }
        });
    }

    {
        let app = app.as_weak();
        app.unwrap().on_save_settings(move || {
            if let Some(app) = app.upgrade() {
                match save_settings(&app) {
                    Ok(path) => append_status(&app, &format!("参数已保存: {}", path.display())),
                    Err(err) => append_status(&app, &format!("保存参数失败: {err:#}")),
                }
            }
        });
    }

    {
        let app = app.as_weak();
        let auto_sender = Rc::clone(&auto_sender);
        let quick_stop = Arc::clone(&quick_stop);
        app.unwrap().on_stop_sending(move || {
            if let Some(app) = app.upgrade() {
                quick_stop.store(true, Ordering::Relaxed);
                *auto_sender.borrow_mut() = None;
                app.set_auto_sending(false);
                append_status(&app, "已停止发送任务");
            }
        });
    }

    {
        let app = app.as_weak();
        let auto_sender = Rc::clone(&auto_sender);
        let quick_stop = Arc::clone(&quick_stop);
        quick.on_stop_sending(move || {
            if let Some(app) = app.upgrade() {
                quick_stop.store(true, Ordering::Relaxed);
                *auto_sender.borrow_mut() = None;
                app.set_auto_sending(false);
                append_notice(&app, "已停止发送任务");
            }
        });
    }

    {
        let app = app.as_weak();
        let rx_file = Rc::clone(&rx_file);
        app.unwrap().on_toggle_receive_file(move || {
            if let Some(app) = app.upgrade() {
                if app.get_receive_to_file() {
                    let default_name = format!("serial-rx-{}.dat", Local::now().format("%Y%m%d-%H%M%S"));
                    let picked = FileDialog::new()
                        .set_title("接收数据保存到文件")
                        .set_file_name(&default_name)
                        .add_filter("Data", &["dat", "bin", "txt", "log"])
                        .save_file();
                    match picked {
                        Some(path) => match fs::OpenOptions::new().create(true).append(true).open(&path) {
                            Ok(file) => {
                                *rx_file.borrow_mut() = Some(file);
                                append_status(&app, &format!("接收数据将写入: {}", path.display()));
                            }
                            Err(err) => {
                                app.set_receive_to_file(false);
                                append_status(&app, &format!("无法打开文件: {err}"));
                            }
                        },
                        None => {
                            app.set_receive_to_file(false);
                            append_status(&app, "已取消接收数据到文件");
                        }
                    }
                } else {
                    *rx_file.borrow_mut() = None;
                    append_status(&app, "已停止接收数据到文件");
                }
            }
        });
    }

    {
        let quick_weak = quick.as_weak();
        quick.on_add_quick_row(move || {
            if let Some(quick) = quick_weak.upgrade() {
                let rows = quick.get_quick_commands();
                if let Some(rows) = rows.as_any().downcast_ref::<VecModel<QuickCommand>>() {
                    let index = rows.row_count() + 1;
                    rows.push(QuickCommand {
                        enabled: false,
                        hex: false,
                        payload: "".into(),
                        remark: format!("{index}无注释").into(),
                        delay_ms: 1000,
                    });
                }
            }
        });
    }

    {
        let quick_weak = quick.as_weak();
        quick.on_remove_quick_row(move || {
            if let Some(quick) = quick_weak.upgrade() {
                let rows = quick.get_quick_commands();
                if let Some(rows) = rows.as_any().downcast_ref::<VecModel<QuickCommand>>() {
                    if rows.row_count() > 1 {
                        rows.remove(rows.row_count() - 1);
                    }
                }
            }
        });
    }

    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            if let Some(app) = app_weak.upgrade() {
                // 高速数据下事件可能一秒成百上千条：本周期内全部先拼到字符串里，
                // 最后只做一次 TextEdit 回写（整块重排是卡顿根源，必须限频）
                let mut pending: Option<String> = None;
                let mut rx_add: i32 = 0;
                let mut tx_add: i32 = 0;
                while let Ok(event) = rx.try_recv() {
                    match event {
                        UiEvent::Received(bytes, at) => {
                            let timeout = app.get_timeout_ms().max(1) as u128;
                            let new_packet = last_rx
                                .borrow()
                                .map_or(true, |prev| at.duration_since(prev).as_millis() >= timeout);
                            *last_rx.borrow_mut() = Some(at);
                            if app.get_receive_to_file() {
                                if let Some(file) = rx_file.borrow_mut().as_mut() {
                                    let _ = file.write_all(&bytes);
                                }
                            }
                            rx_add += bytes.len() as i32;
                            let buf = pending.get_or_insert_with(|| app.get_receive_text().to_string());
                            push_received(&app, buf, &bytes, new_packet);
                        }
                        UiEvent::Sent(bytes) => {
                            tx_add += bytes.len() as i32;
                            // 高频定时发送可关掉回显，避免回显行刷爆接收区
                            if app.get_echo_tx() {
                                let buf = pending.get_or_insert_with(|| app.get_receive_text().to_string());
                                push_sent(&app, buf, &bytes);
                                // 回显打断了接收流，下一包重新带时间戳
                                *last_rx.borrow_mut() = None;
                            }
                        }
                        UiEvent::Status(text) => append_status(&app, &text),
                        UiEvent::Notice(text) => {
                            let buf = pending.get_or_insert_with(|| app.get_receive_text().to_string());
                            push_notice_line(buf, &text);
                            append_status(&app, &text);
                        }
                        UiEvent::Closed => {
                            app.set_connected(false);
                            append_status(&app, "串口连接已断开");
                        }
                    }
                }
                if let Some(text) = pending {
                    if rx_add != 0 {
                        app.set_rx_count(app.get_rx_count() + rx_add);
                    }
                    if tx_add != 0 {
                        app.set_tx_count(app.get_tx_count() + tx_add);
                    }
                    commit_receive_text(&app, text);
                }
            }
        },
    );

    // 标题栏锁品牌深蓝(与 pcanwork/modbus 统一), 低频定时器兼顾后开的子窗口
    #[cfg(windows)]
    let _titlebar_timer = {
        let t = slint::Timer::default();
        t.start(slint::TimerMode::Repeated, Duration::from_millis(500), apply_brand_titlebar);
        apply_brand_titlebar();
        t
    };

    app.run()?;
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

struct AutoSender {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for AutoSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn install_defaults(app: &AppWindow) {
    app.set_baud_rates(model(&["9600", "19200", "38400", "57600", "115200", "230400", "460800", "921600"]));
    app.set_selected_baud("115200".into());
    app.set_data_bits(model(&["8", "7", "6", "5"]));
    app.set_selected_data_bits("8".into());
    app.set_stop_bits(model(&["1", "1.5", "2"]));
    app.set_selected_stop_bits("1".into());
    app.set_parities(model(&["None", "Odd", "Even"]));
    app.set_selected_parity("None".into());
    app.set_flow_controls(model(&["None", "Software", "Hardware"]));
    app.set_selected_flow_control("None".into());
    app.set_auto_interval_ms(1000);
    app.set_local_host("0.0.0.0".into());
    app.set_local_port(502);
    app.set_remote_host("192.168.1.110".into());
    app.set_remote_port(502);
    app.set_receive_text(
        "检测到您第一次使用本工具。\n\n为了顺利使用，请在使用前：\n1. 选择端口和波特率。\n2. 点击打开串口。\n3. 在发送区输入文本或 HEX 数据。\n4. 可保存接收数据、发送文件或启用定时发送。".into(),
    );
}

fn default_quick_commands() -> Vec<QuickCommand> {
    let presets = [
        ("AT+CAL 2255", "读取绝缘数据", false, true, 10),
        ("", "读取信息", false, true, 1000),
        ("", "欢迎语", false, true, 1000),
        ("01 04 00 48 00 02", "读电压", true, true, 1000),
        ("05 04 00 00 00 01", "读状态", true, true, 1000),
        ("30 0f 15 01 00 01 02 ff 00", "配置1", true, true, 1000),
        ("30 01 00 00 00 10", "配置2", true, true, 1000),
        ("30 10 17 02 00 01 02 00 00", "配置3", true, true, 1000),
    ];
    let mut rows = presets
        .iter()
        .map(|(payload, remark, hex, enabled, delay_ms)| QuickCommand {
            enabled: *enabled,
            hex: *hex,
            payload: SharedString::from(*payload),
            remark: SharedString::from(*remark),
            delay_ms: *delay_ms,
        })
        .collect::<Vec<_>>();
    for index in rows.len() + 1..=41 {
        rows.push(QuickCommand {
            enabled: false,
            hex: false,
            payload: SharedString::from(""),
            remark: SharedString::from(format!("{index}无注释")),
            delay_ms: 1000,
        });
    }
    rows
}

fn model(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}

fn refresh_ports(app: &AppWindow) {
    // 端口列表只列真实串口; TCP/UDP/SSH 已拆到 Transport 选择器
    let mut ports: Vec<SharedString> = Vec::new();
    // 端口显示设备信息，与设备管理器一致，如 "USB-SERIAL CH340 (COM3)"
    if let Ok(infos) = serialport::available_ports() {
        for p in infos {
            let label = match &p.port_type {
                serialport::SerialPortType::UsbPort(usb) => {
                    let product = usb.product.clone().unwrap_or_default();
                    if product.is_empty() || product.contains(&p.port_name) {
                        if product.is_empty() { p.port_name.clone() } else { product }
                    } else {
                        format!("{} ({})", product, p.port_name)
                    }
                }
                serialport::SerialPortType::BluetoothPort => format!("Bluetooth ({})", p.port_name),
                _ => p.port_name.clone(),
            };
            ports.push(SharedString::from(label));
        }
    }
    // 刷新时保留用户当前选择（仍存在时）
    let current = app.get_selected_port();
    let selected = if !current.is_empty() && ports.iter().any(|p| *p == current) {
        current
    } else {
        ports.first().cloned().unwrap_or_else(|| SharedString::from(""))
    };
    app.set_ports(ModelRc::new(VecModel::from(ports)));
    app.set_selected_port(selected);
}

fn open_connection(app: &AppWindow, tx: mpsc::Sender<UiEvent>) -> Result<Connection> {
    // transport: 0=Serial 1=TCP Client 2=TCP Server 3=UDP 4=SSH Serial 5=SSH Shell
    match app.get_transport() {
        1 => open_tcp_client(app, tx).map(Connection::Network),
        2 => open_tcp_server(app, tx).map(Connection::Network),
        3 => open_udp(app, tx).map(Connection::Network),
        4 => open_ssh(app, tx).map(Connection::Ssh),
        _ => read_config(app)
            .and_then(|cfg| open_session(cfg, tx))
            .map(Connection::Serial),
    }
}

fn open_tcp_client(app: &AppWindow, tx: mpsc::Sender<UiEvent>) -> Result<NetworkSession> {
    let remote = format!("{}:{}", app.get_remote_host(), app.get_remote_port());
    let addr = remote
        .to_socket_addrs()
        .with_context(|| format!("TCP 地址无效: {remote}"))?
        .next()
        .with_context(|| format!("TCP 地址无效: {remote}"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800))
        .with_context(|| format!("无法连接 TCP 服务器 {remote}，请检查 IP、端口或网络"))?;
    stream.set_nonblocking(true)?;
    let mut reader = stream.try_clone()?;
    let stream = Arc::new(Mutex::new(stream));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while !reader_stop.load(Ordering::Relaxed) {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let _ = tx.send(UiEvent::Received(buf[..n].to_vec(), Instant::now()));
                }
                Ok(_) => thread::sleep(Duration::from_millis(20)),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(20)),
                Err(err) => {
                    let _ = tx.send(UiEvent::Status(format!("TCP 客户端读取失败: {err}")));
                    break;
                }
            }
        }
    });
    Ok(NetworkSession::TcpClient { stream, stop, reader: Some(handle) })
}

fn open_tcp_server(app: &AppWindow, tx: mpsc::Sender<UiEvent>) -> Result<NetworkSession> {
    let bind = format!("{}:{}", app.get_local_host(), app.get_local_port());
    let listener = TcpListener::bind(&bind).with_context(|| format!("无法监听 {bind}"))?;
    listener.set_nonblocking(true)?;
    let client: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    let worker_client = Arc::clone(&client);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while !worker_stop.load(Ordering::Relaxed) {
            if worker_client.lock().ok().and_then(|g| g.as_ref().map(|_| ())).is_none() {
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nonblocking(true);
                        if let Ok(mut slot) = worker_client.lock() {
                            *slot = Some(stream);
                        }
                        let _ = tx.send(UiEvent::Status(format!("TCP 客户端已连接: {addr}")));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(30)),
                    Err(err) => {
                        let _ = tx.send(UiEvent::Status(format!("TCP 监听失败: {err}")));
                        break;
                    }
                }
            }

            if let Ok(mut guard) = worker_client.lock() {
                if let Some(stream) = guard.as_mut() {
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let _ = tx.send(UiEvent::Received(buf[..n].to_vec(), Instant::now()));
                        }
                        Ok(0) => {
                            *guard = None;
                            let _ = tx.send(UiEvent::Status("TCP 客户端已断开".into()));
                        }
                        Ok(_) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(err) => {
                            *guard = None;
                            let _ = tx.send(UiEvent::Status(format!("TCP 读取失败: {err}")));
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    Ok(NetworkSession::TcpServer { client, stop, worker: Some(handle) })
}

fn open_udp(app: &AppWindow, tx: mpsc::Sender<UiEvent>) -> Result<NetworkSession> {
    let bind = format!("{}:{}", app.get_local_host(), app.get_local_port());
    let remote = format!("{}:{}", app.get_remote_host(), app.get_remote_port());
    let socket = UdpSocket::bind(&bind).with_context(|| format!("无法绑定 UDP {bind}"))?;
    socket.set_nonblocking(true)?;
    let reader = socket.try_clone()?;
    let socket = Arc::new(Mutex::new(socket));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while !reader_stop.load(Ordering::Relaxed) {
            match reader.recv_from(&mut buf) {
                Ok((n, addr)) if n > 0 => {
                    let _ = tx.send(UiEvent::Status(format!("UDP 来自 {addr}")));
                    let _ = tx.send(UiEvent::Received(buf[..n].to_vec(), Instant::now()));
                }
                Ok(_) => thread::sleep(Duration::from_millis(20)),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(20)),
                Err(err) => {
                    let _ = tx.send(UiEvent::Status(format!("UDP 接收失败: {err}")));
                    break;
                }
            }
        }
    });
    Ok(NetworkSession::Udp { socket, remote, stop, reader: Some(handle) })
}

/// 从 "USB-SERIAL CH340 (COM3)" 形式的标签中取出实际端口名
fn port_name_of(label: &str) -> &str {
    if let (Some(start), Some(end)) = (label.rfind('('), label.rfind(')')) {
        if start < end {
            return label[start + 1..end].trim();
        }
    }
    label.trim()
}

/// 写一个通用 askpass 助手脚本到临时目录(密码经环境变量 PCANWORK_SSH_PW 传入, 脚本本身不含明文)。
/// ssh 在需要密码且 SSH_ASKPASS_REQUIRE=force 时调用它取密码。
fn write_askpass_helper() -> Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join("pcanwork_ssh_askpass.cmd");
    std::fs::write(&path, "@echo off\r\necho %PCANWORK_SSH_PW%\r\n")?;
    Ok(path)
}

/// SSH 串口: 用系统 ssh 客户端连远端, 在远端跑 socat 把串口接到 ssh 的 stdin/stdout。
/// 认证: 私钥(默认密钥/agent 或 -i 文件) 或 密码(经 SSH_ASKPASS 助手)。
fn open_ssh(app: &AppWindow, tx: mpsc::Sender<UiEvent>) -> Result<SshSession> {
    use std::process::{Command, Stdio};
    let host = app.get_ssh_host().trim().to_string();
    let user = app.get_ssh_user().trim().to_string();
    let port = app.get_ssh_port();
    let key = app.get_ssh_key().trim().to_string();
    let device = app.get_ssh_device().trim().to_string();
    let baud = app.get_ssh_baud();
    let auth = app.get_ssh_auth(); // 0=私钥 1=密码
    let password = app.get_ssh_password().to_string();
    let template = app.get_ssh_cmd_template().trim().to_string();
    if host.is_empty() || user.is_empty() || device.is_empty() {
        bail!("SSH 串口需填写 主机 / 用户 / 远端设备路径(如 /dev/ttyUSB0)");
    }
    // 远端命令: 用模板替换 {dev}/{baud}(默认 socat 把串口双向接到 stdin/stdout, 远端需装 socat)
    let remote_cmd = if template.is_empty() {
        format!("socat - {device},raw,echo=0,b{baud}")
    } else {
        template.replace("{dev}", &device).replace("{baud}", &baud.to_string())
    };
    let mut cmd = Command::new("ssh");
    cmd.arg("-p").arg(port.to_string());
    if auth == 1 {
        // 密码认证: 经 SSH_ASKPASS(强制) + 助手脚本从环境变量回显密码(密码不落盘)。
        // 需 Windows OpenSSH 8.4+; 若不支持会在 stderr 报错, 建议改用密钥。
        if password.is_empty() {
            bail!("已选密码认证, 请填写密码");
        }
        let helper = write_askpass_helper().context("创建 askpass 助手失败")?;
        cmd.env("PCANWORK_SSH_PW", &password)
            .env("SSH_ASKPASS", &helper)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "localhost:0")
            .arg("-o").arg("PubkeyAuthentication=no")
            .arg("-o").arg("PreferredAuthentications=password,keyboard-interactive")
            .arg("-o").arg("NumberOfPasswordPrompts=1");
    } else {
        if !key.is_empty() {
            cmd.arg("-i").arg(&key);
        }
        cmd.arg("-o").arg("BatchMode=yes"); // 密钥模式: 无可用密钥快速失败而非挂起
    }
    cmd.arg("-o").arg("StrictHostKeyChecking=accept-new")
        .arg("-o").arg("ConnectTimeout=8")
        .arg("-o").arg("ServerAliveInterval=15")
        .arg(format!("{user}@{host}"))
        .arg(&remote_cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .context("无法启动 ssh(确认系统已安装 OpenSSH 客户端)")?;
    let stdin = child.stdin.take().context("无法获取 ssh stdin")?;
    let mut stdout = child.stdout.take().context("无法获取 ssh stdout")?;
    let stderr = child.stderr.take().context("无法获取 ssh stderr")?;
    let stop = Arc::new(AtomicBool::new(false));

    // stdout 读线程 → 接收区(管道阻塞读, 子进程被 kill 时返回 0 退出)
    let rstop = Arc::clone(&stop);
    let rtx = tx.clone();
    let reader = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            if rstop.load(Ordering::Relaxed) {
                break;
            }
            match stdout.read(&mut buf) {
                Ok(0) => {
                    let _ = rtx.send(UiEvent::Closed);
                    break;
                }
                Ok(n) => {
                    let _ = rtx.send(UiEvent::Received(buf[..n].to_vec(), Instant::now()));
                }
                Err(err) => {
                    let _ = rtx.send(UiEvent::Status(format!("SSH 读取失败: {err}")));
                    break;
                }
            }
        }
    });

    // stderr 读线程 → 状态栏(ssh / socat 的报错: 认证失败、socat not found 等)
    let estop = Arc::clone(&stop);
    let etx = tx.clone();
    let err_reader = thread::spawn(move || {
        let mut r = std::io::BufReader::new(stderr);
        let mut line = String::new();
        loop {
            if estop.load(Ordering::Relaxed) {
                break;
            }
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let t = line.trim();
                    if !t.is_empty() {
                        let _ = etx.send(UiEvent::Status(format!("SSH: {t}")));
                    }
                }
                Err(_) => break,
            }
        }
    });

    Ok(SshSession {
        child: Arc::new(Mutex::new(child)),
        stdin: Arc::new(Mutex::new(stdin)),
        stop,
        reader: Some(reader),
        err_reader: Some(err_reader),
    })
}

fn read_config(app: &AppWindow) -> Result<SerialConfig> {
    let port_name = port_name_of(app.get_selected_port().as_str()).to_string();
    if port_name.trim().is_empty() {
        bail!("未检测到可用端口");
    }
    Ok(SerialConfig {
        port_name,
        baud_rate: app.get_selected_baud().parse().context("波特率格式错误")?,
        data_bits: match app.get_selected_data_bits().as_str() {
            "5" => DataBits::Five,
            "6" => DataBits::Six,
            "7" => DataBits::Seven,
            _ => DataBits::Eight,
        },
        parity: match app.get_selected_parity().as_str() {
            "Odd" => Parity::Odd,
            "Even" => Parity::Even,
            _ => Parity::None,
        },
        stop_bits: match app.get_selected_stop_bits().as_str() {
            "2" => StopBits::Two,
            _ => StopBits::One,
        },
        flow_control: match app.get_selected_flow_control().as_str() {
            "Software" => FlowControl::Software,
            "Hardware" => FlowControl::Hardware,
            _ => FlowControl::None,
        },
        rts: app.get_rts_enabled(),
        dtr: app.get_dtr_enabled(),
    })
}

fn open_session(cfg: SerialConfig, tx: mpsc::Sender<UiEvent>) -> Result<SerialSession> {
    let mut port = serialport::new(&cfg.port_name, cfg.baud_rate)
        .data_bits(cfg.data_bits)
        .parity(cfg.parity)
        .stop_bits(cfg.stop_bits)
        .flow_control(cfg.flow_control)
        .timeout(Duration::from_millis(80))
        .open()
        .with_context(|| format!("无法打开 {}", cfg.port_name))?;
    let _ = port.write_request_to_send(cfg.rts);
    let _ = port.write_data_terminal_ready(cfg.dtr);

    let port = Arc::new(Mutex::new(port));
    let reader_port = Arc::clone(&port);
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader = thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while !reader_stop.load(Ordering::Relaxed) {
            // 保留 Result 以区分"超时"(正常)与"真实 I/O 错误"(设备拔出)；锁在本块内释放，不跨 sleep。
            let read_res = match reader_port.lock() {
                Ok(mut port) => Some(port.read(&mut buf)),
                Err(_) => None,
            };
            match read_res {
                Some(Ok(n)) if n > 0 => {
                    let _ = tx.send(UiEvent::Received(buf[..n].to_vec(), Instant::now()));
                }
                Some(Ok(_)) => thread::sleep(Duration::from_millis(20)),
                Some(Err(e))
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                // 真实 I/O 错误(如串口被拔出)：退出循环，下方上报断开。
                Some(Err(_)) => break,
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
        let _ = tx.send(UiEvent::Closed);
    });

    Ok(SerialSession { port, stop, reader: Some(reader) })
}

fn build_payload(text: &str, hex: bool, newline: bool, checksum: &str, from_byte: i32) -> Result<Vec<u8>> {
    let mut bytes = if hex {
        parse_hex(text)?
    } else {
        text.as_bytes().to_vec()
    };
    apply_checksum(&mut bytes, checksum, from_byte);
    if newline {
        bytes.extend_from_slice(b"\r\n");
    }
    Ok(bytes)
}

/// 对 bytes[from_byte-1..] 计算校验并追加到末尾（from_byte 为 1 起始的字节序号）
fn apply_checksum(bytes: &mut Vec<u8>, kind: &str, from_byte: i32) {
    let start = from_byte.max(1) as usize - 1;
    if start >= bytes.len() {
        return;
    }
    match kind {
        "SUM8" => {
            let sum = bytes[start..].iter().fold(0_u8, |acc, b| acc.wrapping_add(*b));
            bytes.push(sum);
        }
        "XOR8" => {
            let xor = bytes[start..].iter().fold(0_u8, |acc, b| acc ^ *b);
            bytes.push(xor);
        }
        "CRC8" => {
            let crc = crc8(&bytes[start..]);
            bytes.push(crc);
        }
        "CRC16-Modbus" => {
            let crc = crc16_modbus(&bytes[start..]);
            bytes.push((crc & 0xFF) as u8);
            bytes.push((crc >> 8) as u8);
        }
        _ => {}
    }
}

/// CRC-8，多项式 0x07，初值 0x00（充电桩协议常用）
fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

fn parse_hex(text: &str) -> Result<Vec<u8>> {
    // 容忍 "0x01, 0x04" 这类写法：去掉 0x 前缀、空白和逗号
    let cleaned = text.replace("0x", "").replace("0X", "");
    let compact: String = cleaned.chars().filter(|c| !c.is_whitespace() && *c != ',').collect();
    // 必须全为 ASCII：否则按字节切片会切到多字节 UTF-8 字符中间而 panic。
    if !compact.is_ascii() {
        bail!("HEX 只能包含 0-9 A-F");
    }
    if compact.len() % 2 != 0 {
        bail!("HEX 字符数量必须为偶数");
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).context("HEX 只能包含 0-9 A-F"))
        .collect()
}

/// 非阻塞 TCP 套接字上完整写入：遇 WouldBlock 重试，避免 write_all 在发送缓冲满时半包/丢数据。
fn tcp_write_all<W: Write>(w: &mut W, mut buf: &[u8]) -> std::io::Result<()> {
    use std::io::ErrorKind;
    while !buf.is_empty() {
        match w.write(buf) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn send_bytes(app: &AppWindow, session: &Rc<RefCell<Option<Connection>>>, bytes: &[u8]) {
    if bytes.is_empty() {
        append_status(app, "发送内容为空");
        return;
    }
    if let Some(conn) = session.borrow().as_ref() {
        let result = match conn {
            Connection::Serial(sess) => sess
                .port
                .lock()
                .map_err(|_| anyhow::anyhow!("串口忙"))
                .and_then(|mut port| port.write_all(bytes).context("写入串口失败")),
            Connection::Network(NetworkSession::TcpClient { stream, .. }) => stream
                .lock()
                .map_err(|_| anyhow::anyhow!("TCP 连接忙"))
                .and_then(|mut stream| tcp_write_all(&mut *stream, bytes).context("写入 TCP 失败")),
            Connection::Network(NetworkSession::TcpServer { client, .. }) => {
                let mut guard = match client.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        append_status(app, "TCP 客户端状态忙");
                        return;
                    }
                };
                match guard.as_mut() {
                    Some(stream) => tcp_write_all(stream, bytes).context("写入 TCP 客户端失败"),
                    None => Err(anyhow::anyhow!("尚无 TCP 客户端连接")),
                }
            }
            Connection::Network(NetworkSession::Udp { socket, remote, .. }) => socket
                .lock()
                .map_err(|_| anyhow::anyhow!("UDP 套接字忙"))
                .and_then(|socket| socket.send_to(bytes, remote).map(|_| ()).context("发送 UDP 失败")),
            Connection::Ssh(sess) => sess
                .stdin
                .lock()
                .map_err(|_| anyhow::anyhow!("SSH 通道忙"))
                .and_then(|mut g| g.write_all(bytes).and_then(|_| g.flush()).context("写入 SSH 通道失败")),
        };

        match result {
            Ok(()) => {
                app.set_tx_count(app.get_tx_count() + bytes.len() as i32);
                append_sent(app, bytes);
                append_status(app, &format!("已发送 {} 字节", bytes.len()));
            }
            Err(err) => append_status(app, &format!("发送失败: {err:#}")),
        }
    } else {
        append_notice(app, "发送未执行：请先打开串口或网络连接");
    }
}

fn clone_send_target(conn: &Connection) -> SendTarget {
    match conn {
        Connection::Serial(sess) => SendTarget::Serial(Arc::clone(&sess.port)),
        Connection::Network(NetworkSession::TcpClient { stream, .. }) => SendTarget::TcpClient(Arc::clone(stream)),
        Connection::Network(NetworkSession::TcpServer { client, .. }) => SendTarget::TcpServer(Arc::clone(client)),
        Connection::Network(NetworkSession::Udp { socket, remote, .. }) => {
            SendTarget::Udp(Arc::clone(socket), remote.clone())
        }
        Connection::Ssh(sess) => SendTarget::Ssh(Arc::clone(&sess.stdin)),
    }
}

fn write_send_target(target: &SendTarget, bytes: &[u8]) -> Result<()> {
    match target {
        SendTarget::Serial(port) => port
            .lock()
            .map_err(|_| anyhow::anyhow!("串口忙"))?
            .write_all(bytes)
            .context("写入串口失败"),
        SendTarget::TcpClient(stream) => {
            let mut g = stream.lock().map_err(|_| anyhow::anyhow!("TCP 连接忙"))?;
            tcp_write_all(&mut *g, bytes).context("写入 TCP 失败")
        }
        SendTarget::TcpServer(client) => {
            let mut guard = client.lock().map_err(|_| anyhow::anyhow!("TCP 客户端状态忙"))?;
            match guard.as_mut() {
                Some(stream) => tcp_write_all(stream, bytes).context("写入 TCP 客户端失败"),
                None => bail!("尚无 TCP 客户端连接"),
            }
        }
        SendTarget::Udp(socket, remote) => socket
            .lock()
            .map_err(|_| anyhow::anyhow!("UDP 套接字忙"))?
            .send_to(bytes, remote)
            .map(|_| ())
            .context("发送 UDP 失败"),
        SendTarget::Ssh(stdin) => {
            let mut g = stdin.lock().map_err(|_| anyhow::anyhow!("SSH 通道忙"))?;
            g.write_all(bytes).and_then(|_| g.flush()).context("写入 SSH 通道失败")
        }
    }
}

/// 重要提示：写进接收区（行首 ※ 标记）并同步到状态栏，保证用户看得见
fn append_notice(app: &AppWindow, message: &str) {
    let mut text = app.get_receive_text().to_string();
    push_notice_line(&mut text, message);
    commit_receive_text(app, text);
    append_status(app, message);
}

fn push_notice_line(text: &mut String, message: &str) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("[{}] ※ {}\n", Local::now().format("%H:%M:%S%.3f"), message));
}

/// 截断 + 回写控件 + 自动滚动，整块文本重排开销大，每个 UI 周期至多调用一次
fn commit_receive_text(app: &AppWindow, mut text: String) {
    cap_receive_text(&mut text);
    app.set_receive_text(text.into());
    autoscroll_receive(app);
}

/// 自动滚动开启时，把接收区滚动条贴到最底部
fn autoscroll_receive(app: &AppWindow) {
    if app.get_autoscroll() {
        app.invoke_scroll_rx_to_end();
    }
}

/// 发送回显：写进接收区，行首用时间戳 + “→” 标记与接收数据区分
fn append_sent(app: &AppWindow, bytes: &[u8]) {
    if !app.get_echo_tx() {
        return;
    }
    let mut text = app.get_receive_text().to_string();
    push_sent(app, &mut text, bytes);
    commit_receive_text(app, text);
}

fn push_sent(app: &AppWindow, text: &mut String, bytes: &[u8]) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    // 文本模式下若含不可打印字节（HEX 发送的原始数据），强制按 HEX 回显，避免乱码
    let printable = std::str::from_utf8(bytes)
        .map(|s| s.chars().all(|c| !c.is_control() || matches!(c, '\r' | '\n' | '\t')))
        .unwrap_or(false);
    let body = if app.get_receive_hex() || !printable {
        bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
    } else {
        String::from_utf8_lossy(bytes).trim_end_matches(['\r', '\n']).to_string()
    };
    text.push_str(&format!("[{}] → {}\n", Local::now().format("%H:%M:%S%.3f"), body));
}

/// 限制接收区长度：自动换行 TextEdit 的重排开销随文本长度线性增长，
/// 上限太大会在高速数据下拖死 UI 线程
fn cap_receive_text(text: &mut String) {
    if text.len() > 150_000 {
        let mut cut = text.len() - 100_000;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}

fn push_received(app: &AppWindow, text: &mut String, bytes: &[u8], new_packet: bool) {
    let framed = app.get_timestamp_enabled();
    if framed && new_packet {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[{}] ", Local::now().format("%H:%M:%S%.3f")));
    }
    if app.get_receive_hex() {
        if !text.is_empty() && !text.ends_with(' ') && !text.ends_with('\n') {
            text.push(' ');
        }
        // 串口一次 read 可能只取到半帧，HEX 流按空格连续拼接；
        // 按帧换行交给“时间戳分包”（超过分包超时才另起一行），不能按 read 边界断行
        text.push_str(&bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
    } else {
        text.push_str(&String::from_utf8_lossy(bytes));
    }
}

fn append_status(app: &AppWindow, message: &str) {
    app.set_status_message(format!("{}  {}", Local::now().format("%H:%M:%S"), message).into());
}

fn save_receive_log(app: &AppWindow) -> Result<PathBuf> {
    let default_name = format!("serial-log-{}.txt", Local::now().format("%Y%m%d-%H%M%S"));
    let path = FileDialog::new()
        .set_title("保存接收数据")
        .set_file_name(&default_name)
        .add_filter("Text", &["txt", "log"])
        .save_file()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(default_name));
    fs::write(&path, app.get_receive_text().as_bytes())?;
    Ok(path)
}

fn pick_default_file() -> Option<PathBuf> {
    FileDialog::new()
        .set_title("选择要发送的文件")
        .add_filter("Serial payload", &["txt", "hex", "bin", "csv", "log"])
        .pick_file()
}

fn settings_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .unwrap_or(std::env::current_dir()?)
        .join("serial-tool")
        .join("settings.ini"))
}

fn save_settings(app: &AppWindow) -> Result<PathBuf> {
    let path = settings_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let content = format!(
        "port={}\nbaud={}\ndata_bits={}\nstop_bits={}\nparity={}\nflow_control={}\nhex_receive={}\nhex_send={}\ntimestamp={}\nappend_newline={}\nauto_interval_ms={}\nremote_host={}\nremote_port={}\nlocal_host={}\nlocal_port={}\ntimeout_ms={}\nfrom_byte={}\nchecksum={}\nenglish={}\ntopmost={}\nrts={}\ndtr={}\n",
        app.get_selected_port(),
        app.get_selected_baud(),
        app.get_selected_data_bits(),
        app.get_selected_stop_bits(),
        app.get_selected_parity(),
        app.get_selected_flow_control(),
        app.get_receive_hex(),
        app.get_send_hex(),
        app.get_timestamp_enabled(),
        app.get_append_newline(),
        app.get_auto_interval_ms(),
        app.get_remote_host(),
        app.get_remote_port(),
        app.get_local_host(),
        app.get_local_port(),
        app.get_timeout_ms(),
        app.get_from_byte(),
        app.get_selected_checksum(),
        app.global::<I18n>().get_en(),
        app.get_topmost_enabled(),
        app.get_rts_enabled(),
        app.get_dtr_enabled(),
    );
    // 记住窗口大小与位置，下次启动恢复
    let pos = app.window().position();
    let size = app.window().size();
    // SSH 字段也持久化(密码不存, 明文安全); transport 记住上次的传输方式。
    let content = format!(
        "{content}autoscroll={}\necho_tx={}\nwin_x={}\nwin_y={}\nwin_w={}\nwin_h={}\ntransport={}\nssh_host={}\nssh_port={}\nssh_user={}\nssh_key={}\nssh_device={}\nssh_baud={}\nssh_auth={}\nssh_cmd_template={}\n",
        app.get_autoscroll(),
        app.get_echo_tx(),
        pos.x,
        pos.y,
        size.width,
        size.height,
        app.get_transport(),
        app.get_ssh_host(),
        app.get_ssh_port(),
        app.get_ssh_user(),
        app.get_ssh_key(),
        app.get_ssh_device(),
        app.get_ssh_baud(),
        app.get_ssh_auth(),
        app.get_ssh_cmd_template(),
    );
    fs::write(&path, content)?;
    Ok(path)
}

fn load_settings(app: &AppWindow) {
    let Ok(path) = settings_path() else { return };
    let Ok(content) = fs::read_to_string(&path) else { return };
    let parse_bool = |v: &str| v == "true" || v == "1";
    let mut win: (Option<i32>, Option<i32>, Option<u32>, Option<u32>) = (None, None, None, None);
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "port" => app.set_selected_port(value.into()),
            "baud" => app.set_selected_baud(value.into()),
            "data_bits" => app.set_selected_data_bits(value.into()),
            "stop_bits" => app.set_selected_stop_bits(value.into()),
            "parity" => app.set_selected_parity(value.into()),
            "flow_control" => app.set_selected_flow_control(value.into()),
            "hex_receive" => app.set_receive_hex(parse_bool(value)),
            "hex_send" => app.set_send_hex(parse_bool(value)),
            "timestamp" => app.set_timestamp_enabled(parse_bool(value)),
            "append_newline" => app.set_append_newline(parse_bool(value)),
            "auto_interval_ms" => app.set_auto_interval_ms(value.parse().unwrap_or(1000)),
            "remote_host" => app.set_remote_host(value.into()),
            "remote_port" => app.set_remote_port(value.parse().unwrap_or(502)),
            "local_host" => app.set_local_host(value.into()),
            "local_port" => app.set_local_port(value.parse().unwrap_or(502)),
            "timeout_ms" => app.set_timeout_ms(value.parse().unwrap_or(20)),
            "from_byte" => app.set_from_byte(value.parse().unwrap_or(1)),
            "checksum" => app.set_selected_checksum(value.into()),
            "english" => app.global::<I18n>().set_en(parse_bool(value)),
            "topmost" => app.set_topmost_enabled(parse_bool(value)),
            "rts" => app.set_rts_enabled(parse_bool(value)),
            "dtr" => app.set_dtr_enabled(parse_bool(value)),
            "autoscroll" => app.set_autoscroll(parse_bool(value)),
            "echo_tx" => app.set_echo_tx(parse_bool(value)),
            // SSH 终端(旧值 5)已移除, 持久化里若残留 5 钳回 0, 防 ComboBox 越界空白
            "transport" => app.set_transport(
                value.parse::<i32>().ok().filter(|v| (0..=4).contains(v)).unwrap_or(0),
            ),
            "ssh_host" => app.set_ssh_host(value.into()),
            "ssh_port" => app.set_ssh_port(value.parse().unwrap_or(22)),
            "ssh_user" => app.set_ssh_user(value.into()),
            "ssh_key" => app.set_ssh_key(value.into()),
            "ssh_device" => app.set_ssh_device(value.into()),
            "ssh_baud" => app.set_ssh_baud(value.parse().unwrap_or(115200)),
            "ssh_auth" => app.set_ssh_auth(value.parse().unwrap_or(0)),
            "ssh_cmd_template" => app.set_ssh_cmd_template(value.into()),
            "win_x" => win.0 = value.parse().ok(),
            "win_y" => win.1 = value.parse().ok(),
            "win_w" => win.2 = value.parse().ok(),
            "win_h" => win.3 = value.parse().ok(),
            _ => {}
        }
    }
    // 恢复窗口大小与位置（带最小值保护，避免恢复出不可用的窗口）
    if let (Some(w), Some(h)) = (win.2, win.3) {
        if w >= 600 && h >= 400 {
            app.window().set_size(slint::PhysicalSize::new(w, h));
        }
    }
    if let (Some(x), Some(y)) = (win.0, win.1) {
        if x > -10000 && y > -10000 {
            app.window().set_position(slint::PhysicalPosition::new(x, y));
        }
    }
}

fn import_quick_ini() -> Result<Vec<QuickCommand>> {
    let path = FileDialog::new()
        .set_title("导入多条字符串 ini")
        .add_filter("INI", &["ini", "txt"])
        .pick_file()
        .context("未选择 ini 文件")?;
    let content = fs::read_to_string(&path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut rows = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        let parts = line.split('|').collect::<Vec<_>>();
        if parts.len() >= 5 {
            rows.push(QuickCommand {
                enabled: parts[0].trim() == "1" || parts[0].trim().eq_ignore_ascii_case("true"),
                hex: parts[1].trim() == "1" || parts[1].trim().eq_ignore_ascii_case("true"),
                payload: parts[2].trim().into(),
                remark: parts[3].trim().into(),
                delay_ms: parts[4].trim().parse().unwrap_or(1000),
            });
        } else if let Some((_, value)) = line.split_once('=') {
            rows.push(QuickCommand {
                enabled: true,
                hex: looks_like_hex(value),
                payload: value.trim().into(),
                remark: format!("{}无注释", rows.len() + 1).into(),
                delay_ms: 1000,
            });
        }
    }
    while rows.len() < 41 {
        let index = rows.len() + 1;
        rows.push(QuickCommand {
            enabled: false,
            hex: false,
            payload: "".into(),
            remark: format!("{index}无注释").into(),
            delay_ms: 1000,
        });
    }
    Ok(rows)
}

fn export_quick_ini(rows: &ModelRc<QuickCommand>) -> Result<PathBuf> {
    let path = FileDialog::new()
        .set_title("导出多条字符串 ini")
        .set_file_name("quick-commands.ini")
        .add_filter("INI", &["ini"])
        .save_file()
        .context("未选择保存位置")?;
    let mut content = String::from("[QuickCommands]\n# enabled|hex|payload|remark|delay_ms\n");
    for i in 0..rows.row_count() {
        if let Some(row) = rows.row_data(i) {
            content.push_str(&format!(
                "{}|{}|{}|{}|{}\n",
                row.enabled as u8,
                row.hex as u8,
                row.payload.replace('|', " "),
                row.remark.replace('|', " "),
                row.delay_ms
            ));
        }
    }
    fs::write(&path, content)?;
    Ok(path)
}

fn looks_like_hex(value: &str) -> bool {
    let compact = value.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    !compact.is_empty() && compact.len() % 2 == 0 && compact.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parse_basic() {
        assert_eq!(parse_hex("1111").unwrap(), vec![0x11, 0x11]);
        assert_eq!(parse_hex("01 04 00 48 00 02").unwrap(), vec![1, 4, 0, 0x48, 0, 2]);
        assert_eq!(parse_hex("0x01, 0x04").unwrap(), vec![1, 4]);
        assert!(parse_hex("123").is_err());
        assert!(parse_hex("zz").is_err());
    }

    #[test]
    fn crc_standard_check_values() {
        // CRC-16/MODBUS("123456789") = 0x4B37，CRC-8(poly 0x07)("123456789") = 0xF4
        assert_eq!(crc16_modbus(b"123456789"), 0x4B37);
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn payload_hex_with_crlf() {
        let bytes = build_payload("1111", true, true, "None", 1).unwrap();
        assert_eq!(bytes, vec![0x11, 0x11, 0x0D, 0x0A]);
    }

    #[test]
    fn payload_checksum_sum8_xor8() {
        assert_eq!(build_payload("01 02 03", true, false, "SUM8", 1).unwrap(), vec![1, 2, 3, 6]);
        assert_eq!(build_payload("01 02 03", true, false, "XOR8", 1).unwrap(), vec![1, 2, 3, 0]);
        // 从第 2 字节起：跳过 0x01，SUM8 = 2+3 = 5
        assert_eq!(build_payload("01 02 03", true, false, "SUM8", 2).unwrap(), vec![1, 2, 3, 5]);
    }

    #[test]
    fn payload_crc16_modbus_frame() {
        // 校验在数据后、回车换行在最后
        let bytes = build_payload("01 04 00 48 00 02", true, true, "CRC16-Modbus", 1).unwrap();
        let crc = crc16_modbus(&[1, 4, 0, 0x48, 0, 2]);
        assert_eq!(bytes[6], (crc & 0xFF) as u8);
        assert_eq!(bytes[7], (crc >> 8) as u8);
        assert_eq!(&bytes[8..], &[0x0D, 0x0A]);
    }

    #[test]
    fn port_label_parsing() {
        assert_eq!(port_name_of("USB-SERIAL CH340 (COM3)"), "COM3");
        assert_eq!(port_name_of("COM7"), "COM7");
        assert_eq!(port_name_of("TCPClient"), "TCPClient");
    }
}
