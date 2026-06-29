//! CAN 设备抽象层：统一帧结构 + 适配器 trait + 虚拟总线 + PCAN(PEAK) 后端 + 控制线程。
//!
//! 设计要点：
//! - `CanAdapter` 把硬件差异隐藏在 poll/send 之后；上层只认 `CanFrame`。
//! - 连接时优先尝试真实 PCAN 卡，失败则自动回退到虚拟总线，保证无硬件也能跑通全链路。
//! - 控制线程独占适配器，UI 线程通过 channel 下发命令 / 收取事件，绝不直接碰硬件。

// FFI 后端里多处 `drop(symbol)` 是故意提前结束 libloading Symbol 的借用（如卸载设备前），
// 这些类型并不实现 Drop，clippy::drop_non_drop 在此为误报。
#![allow(clippy::drop_non_drop)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static OTA_CANCEL: AtomicBool = AtomicBool::new(false);

pub fn cancel_ota() {
    OTA_CANCEL.store(true, Ordering::Relaxed);
}

/// 统一 CAN/CAN FD 帧。
#[derive(Clone, Debug)]
pub struct CanFrame {
    pub t: f64,        // 相对时间(秒)，从控制线程启动算起
    pub ch: u8,        // 通道号，从 1 开始
    pub tx: bool,      // true=发送帧(回显)，false=接收帧
    pub id: u32,       // 不含扩展标志的 ID
    pub ext: bool,     // 扩展帧
    pub fd: bool,      // CAN FD
    pub brs: bool,     // 波特率切换
    pub remote: bool,  // 远程帧
    pub error: bool,   // 错误帧
    pub data: Vec<u8>, // 数据区
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)] // 缺字段用类型默认值填充，避免 IPC 传入部分配置时整条静默丢弃
pub struct DeviceConfig {
    pub sw_channel: u8, // 软件通道号（CAN1=1, CAN2=2...）
    pub is_fd: bool,    // 协议：false=CAN, true=CAN FD
    pub device_type: String,
    pub device_index: u32,
    pub channel_index: u32,
    pub baud: String,
    pub data_baud: String,
    pub termination: bool,
    pub net_server: bool, // 网络设备：true=服务器, false=客户端
    pub ip: String,
    pub port: String,
}

impl CanFrame {
    pub fn data_hex(&self) -> String {
        self.data
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 适配器接口：不同 CAN 硬件实现这套即可接入。
pub trait CanAdapter: Send {
    /// 把当前可读到的帧追加到 out（非阻塞）。
    fn poll(&mut self, out: &mut Vec<CanFrame>);
    /// 发送一帧。
    fn send(&mut self, f: &CanFrame) -> Result<(), String>;
    fn name(&self) -> &str;
}

// ======================= PCAN (PEAK) 后端 =======================

fn normalize_baud(baud: &str) -> String {
    let b = baud.trim().to_ascii_uppercase().replace(' ', "");
    match b.as_str() {
        "125" | "125KBPS" | "125K" => "125K".to_string(),
        "250" | "250KBPS" | "250K" => "250K".to_string(),
        "500" | "500KBPS" | "500K" => "500K".to_string(),
        "1000" | "1000KBPS" | "1000K" | "1M" | "1MBPS" => "1000K".to_string(),
        _ => b,
    }
}

/// SJA1000 定时寄存器值（ControlCAN/ECanVci 通用），覆盖常用波特率。
fn zlg_timing(baud: &str) -> Option<(u8, u8)> {
    match normalize_baud(baud).as_str() {
        "1000K" => Some((0x00, 0x14)),
        "800K" => Some((0x00, 0x16)),
        "500K" => Some((0x00, 0x1C)),
        "250K" => Some((0x01, 0x1C)),
        "125K" => Some((0x03, 0x1C)),
        "100K" => Some((0x04, 0x1C)),
        "50K" => Some((0x09, 0x1C)),
        "20K" => Some((0x18, 0x1C)),
        "10K" => Some((0x31, 0x1C)),
        "5K" => Some((0xBF, 0xFF)),
        _ => None,
    }
}

#[allow(non_camel_case_types)]
mod pcan_ffi {
    // PCANBasic.dll 导出为 __stdcall（WINAPI）。用 "system" 而非 "C"：在 x86(32位)上
    // "C"=cdecl 会与 stdcall 栈清理不匹配导致每次调用栈损坏；x86_64 两者等价。
    pub type FnInit = unsafe extern "system" fn(u16, u16, u8, u32, u16) -> u32;
    pub type FnUninit = unsafe extern "system" fn(u16) -> u32;
    pub type FnRead = unsafe extern "system" fn(u16, *mut TPCANMsg, *mut TPCANTimestamp) -> u32;
    pub type FnWrite = unsafe extern "system" fn(u16, *const TPCANMsg) -> u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TPCANMsg {
        pub id: u32,
        pub msgtype: u8,
        pub len: u8,
        pub data: [u8; 8],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TPCANTimestamp {
        pub millis: u32,
        pub millis_overflow: u16,
        pub micros: u16,
    }

    pub const PCAN_USBBUS1: u16 = 0x51;
    pub const PCAN_USBBUS2: u16 = 0x52;
    pub const PCAN_USBBUS3: u16 = 0x53;
    pub const PCAN_USBBUS4: u16 = 0x54;
    pub const PCAN_BAUD_125K: u16 = 0x031C;
    pub const PCAN_BAUD_250K: u16 = 0x011C;
    pub const PCAN_BAUD_500K: u16 = 0x001C;
    pub const PCAN_BAUD_1M: u16 = 0x0014;
    pub const PCAN_ERROR_OK: u32 = 0x0000_0000;
    pub const PCAN_ERROR_QRCVEMPTY: u32 = 0x0000_0020;

    pub const MSGTYPE_STANDARD: u8 = 0x00;
    pub const MSGTYPE_RTR: u8 = 0x01;
    pub const MSGTYPE_EXTENDED: u8 = 0x02;
    pub const MSGTYPE_FD: u8 = 0x04; // CAN FD 帧
    pub const MSGTYPE_BRS: u8 = 0x08; // 比特率切换
    pub const MSGTYPE_ERRFRAME: u8 = 0x40;
    pub const MSGTYPE_STATUS: u8 = 0x80;

    // ---- CAN FD 接口（CAN_InitializeFD/WriteFD/ReadFD，PCANBasic.h 镜像）----
    pub type FnInitFd = unsafe extern "system" fn(u16, *const std::os::raw::c_char) -> u32;
    pub type FnWriteFd = unsafe extern "system" fn(u16, *const TPCANMsgFD) -> u32;
    pub type FnReadFd = unsafe extern "system" fn(u16, *mut TPCANMsgFD, *mut u64) -> u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TPCANMsgFD {
        pub id: u32,
        pub msgtype: u8,
        pub dlc: u8, // DLC 码 0..15（非字节数）
        pub data: [u8; 64],
    }
}

/// CAN FD 数据字节数 → DLC 码（0..15 ↔ 0,1..8,12,16,20,24,32,48,64）。
fn fd_len_to_dlc(len: usize) -> u8 {
    match len {
        0..=8 => len as u8,
        9..=12 => 9,
        13..=16 => 10,
        17..=20 => 11,
        21..=24 => 12,
        25..=32 => 13,
        33..=48 => 14,
        _ => 15,
    }
}

/// DLC 码 → CAN FD 数据字节数。
fn fd_dlc_to_len(dlc: u8) -> usize {
    match dlc & 0x0F {
        n @ 0..=8 => n as usize,
        9 => 12,
        10 => 16,
        11 => 20,
        12 => 24,
        13 => 32,
        14 => 48,
        _ => 64,
    }
}

/// 由仲裁/数据速率构造 PEAK CAN FD 比特率串（f_clock=80MHz，采样点≈80%）。
/// 也支持直接传入完整串（含 "f_clock"）作为高级覆盖。所有节点只需仲裁/数据"速率"一致，
/// 采样点在容差内可不同，故此处用统一推导值即可与其它设备互通。
fn pcan_fd_bitrate(arb: &str, data: &str) -> Result<String, String> {
    // 高级覆盖：用户直接给了完整时序串（normalize_baud 已转大写，故匹配大写；返回原串保留大小写）
    let a = normalize_baud(arb);
    if a.contains("F_CLOCK") || a.contains("NOM_BRP") {
        return Ok(arb.to_string());
    }
    // 仲裁段（80MHz，ntq=80，tseg1=63/tseg2=16/sjw=16，brp 缩放速率）
    let nom = match a.as_str() {
        "1M" | "1000K" => "nom_brp=2,nom_tseg1=31,nom_tseg2=8,nom_sjw=8",
        "500K" => "nom_brp=2,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        "250K" => "nom_brp=4,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        "125K" => "nom_brp=8,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        other => return Err(format!("CAN FD 不支持的仲裁速率: {other}（支持 1M/500K/250K/125K，或直接填完整 f_clock 串）")),
    };
    // 数据段（80MHz，dtq=20，tseg1=15/tseg2=4/sjw=4，brp 缩放；8M/5M 用更小 dtq）
    let dat = match normalize_baud(data).as_str() {
        "8M" | "8000K" => "data_brp=1,data_tseg1=7,data_tseg2=2,data_sjw=2",
        "5M" | "5000K" => "data_brp=1,data_tseg1=12,data_tseg2=3,data_sjw=3",
        "4M" | "4000K" => "data_brp=1,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "2M" | "2000K" => "data_brp=2,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "1M" | "1000K" => "data_brp=4,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "500K" => "data_brp=8,data_tseg1=15,data_tseg2=4,data_sjw=4",
        other => return Err(format!("CAN FD 不支持的数据速率: {other}（支持 8M/5M/4M/2M/1M/500K，或直接填完整 f_clock 串）")),
    };
    Ok(format!("f_clock=80000000,{nom},{dat}"))
}

pub struct PcanBus {
    lib: libloading::Library,
    channel: u16,
    is_fd: bool, // 通道是否按 CAN FD 初始化（决定走 CAN_ReadFD/WriteFD）
    start: Instant,
    name: String,
}

impl PcanBus {
    /// 尝试打开 PCAN_USBBUS1 @500K。失败返回 Err（上层据此回退到虚拟总线）。
    pub fn open(start: Instant) -> Result<Self, String> {
        Self::open_config(start, 0, "500K")
    }

    pub fn open_config(start: Instant, channel_index: u32, baud: &str) -> Result<Self, String> {
        use pcan_ffi::*;
        let channel = match channel_index {
            0 => PCAN_USBBUS1,
            1 => PCAN_USBBUS2,
            2 => PCAN_USBBUS3,
            3 => PCAN_USBBUS4,
            _ => {
                return Err(format!(
                    "PCAN only supports configured USB channel index 0..3, got {channel_index}"
                ));
            }
        };
        let baud_code = match normalize_baud(baud).as_str() {
            "125K" => PCAN_BAUD_125K,
            "250K" => PCAN_BAUD_250K,
            "500K" => PCAN_BAUD_500K,
            "1000K" | "1M" => PCAN_BAUD_1M,
            other => return Err(format!("Unsupported PCAN baud rate: {other}")),
        };
        unsafe {
            let lib = libloading::Library::new("PCANBasic.dll")
                .map_err(|e| format!("加载 PCANBasic.dll 失败: {e}"))?;
            let init: libloading::Symbol<FnInit> = lib
                .get(b"CAN_Initialize\0")
                .map_err(|e| format!("找不到 CAN_Initialize: {e}"))?;
            let status = init(channel, baud_code, 0, 0, 0);
            if status != PCAN_ERROR_OK {
                return Err(format!(
                    "CAN_Initialize 失败, status=0x{status:08X} (可能未插卡)"
                ));
            }
            drop(init);
            Ok(Self {
                lib,
                channel,
                is_fd: false,
                start,
                name: format!("PCAN_USBBUS{} @{}", channel_index + 1, normalize_baud(baud)),
            })
        }
    }

    /// 按 DeviceConfig 打开：cfg.is_fd=true 走 CAN_InitializeFD（CAN FD），否则经典。
    pub fn open_cfg(start: Instant, cfg: &DeviceConfig) -> Result<Self, String> {
        use pcan_ffi::*;
        if !cfg.is_fd {
            return Self::open_config(start, cfg.channel_index, &cfg.baud);
        }
        let channel = match cfg.channel_index {
            0 => PCAN_USBBUS1,
            1 => PCAN_USBBUS2,
            2 => PCAN_USBBUS3,
            3 => PCAN_USBBUS4,
            other => return Err(format!("PCAN 仅支持 USB 通道 0..3，收到 {other}")),
        };
        let bitrate = pcan_fd_bitrate(&cfg.baud, &cfg.data_baud)?;
        let bitrate_c = std::ffi::CString::new(bitrate.clone()).unwrap();
        unsafe {
            let lib = libloading::Library::new("PCANBasic.dll")
                .map_err(|e| format!("加载 PCANBasic.dll 失败: {e}"))?;
            let init_fd: libloading::Symbol<FnInitFd> = lib
                .get(b"CAN_InitializeFD\0")
                .map_err(|e| format!("找不到 CAN_InitializeFD（驱动太旧?）: {e}"))?;
            let status = init_fd(channel, bitrate_c.as_ptr());
            if status != PCAN_ERROR_OK {
                return Err(format!(
                    "CAN_InitializeFD 失败, status=0x{status:08X}（检查卡是否支持 FD/时钟，比特率串: {bitrate}）"
                ));
            }
            drop(init_fd);
            Ok(Self {
                lib,
                channel,
                is_fd: true,
                start,
                name: format!(
                    "PCAN_USBBUS{} FD @{}/{}",
                    cfg.channel_index + 1,
                    normalize_baud(&cfg.baud),
                    normalize_baud(&cfg.data_baud)
                ),
            })
        }
    }
}

impl Drop for PcanBus {
    fn drop(&mut self) {
        unsafe {
            if let Ok(uninit) = self.lib.get::<pcan_ffi::FnUninit>(b"CAN_Uninitialize\0") {
                let _ = uninit(self.channel);
            }
        }
    }
}

impl CanAdapter for PcanBus {
    fn poll(&mut self, out: &mut Vec<CanFrame>) {
        use pcan_ffi::*;
        unsafe {
            // CAN FD 通道：用 CAN_ReadFD（同一队列里既有经典也有 FD 帧）
            if self.is_fd {
                let read_fd: libloading::Symbol<FnReadFd> = match self.lib.get(b"CAN_ReadFD\0") {
                    Ok(s) => s,
                    Err(_) => return,
                };
                for _ in 0..512 {
                    let mut msg = TPCANMsgFD { id: 0, msgtype: 0, dlc: 0, data: [0; 64] };
                    let mut ts: u64 = 0;
                    let st = read_fd(self.channel, &mut msg, &mut ts);
                    if st == PCAN_ERROR_QRCVEMPTY || st != PCAN_ERROR_OK {
                        break;
                    }
                    if msg.msgtype & MSGTYPE_STATUS != 0 {
                        continue;
                    }
                    let is_fd = msg.msgtype & MSGTYPE_FD != 0;
                    let len = if is_fd { fd_dlc_to_len(msg.dlc) } else { (msg.dlc as usize).min(8) };
                    out.push(CanFrame {
                        t: self.start.elapsed().as_secs_f64(),
                        ch: 1,
                        tx: false,
                        id: msg.id,
                        ext: msg.msgtype & MSGTYPE_EXTENDED != 0,
                        fd: is_fd,
                        brs: msg.msgtype & MSGTYPE_BRS != 0,
                        remote: msg.msgtype & MSGTYPE_RTR != 0,
                        error: msg.msgtype & MSGTYPE_ERRFRAME != 0,
                        data: msg.data[..len.min(64)].to_vec(),
                    });
                }
                return;
            }
            let read: libloading::Symbol<FnRead> = match self.lib.get(b"CAN_Read\0") {
                Ok(s) => s,
                Err(_) => return,
            };
            // 一次性把接收队列读空
            for _ in 0..512 {
                let mut msg = TPCANMsg {
                    id: 0,
                    msgtype: 0,
                    len: 0,
                    data: [0; 8],
                };
                let mut ts = TPCANTimestamp {
                    millis: 0,
                    millis_overflow: 0,
                    micros: 0,
                };
                let st = read(self.channel, &mut msg, &mut ts);
                if st == PCAN_ERROR_QRCVEMPTY {
                    break;
                }
                if st != PCAN_ERROR_OK {
                    break;
                }
                if msg.msgtype & MSGTYPE_STATUS != 0 {
                    continue;
                }
                let len = (msg.len as usize).min(8);
                out.push(CanFrame {
                    t: self.start.elapsed().as_secs_f64(),
                    ch: 1,
                    tx: false,
                    id: msg.id,
                    ext: msg.msgtype & MSGTYPE_EXTENDED != 0,
                    fd: false,
                    brs: false,
                    remote: msg.msgtype & MSGTYPE_RTR != 0,
                    error: msg.msgtype & MSGTYPE_ERRFRAME != 0,
                    data: msg.data[..len].to_vec(),
                });
            }
        }
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use pcan_ffi::*;
        // FD 通道：用 CAN_WriteFD（经典帧与 FD 帧都能发）
        if self.is_fd {
            unsafe {
                let write: libloading::Symbol<FnWriteFd> = self
                    .lib
                    .get(b"CAN_WriteFD\0")
                    .map_err(|e| format!("找不到 CAN_WriteFD: {e}"))?;
                let len = f.data.len().min(64);
                let mut data = [0u8; 64];
                data[..len].copy_from_slice(&f.data[..len]);
                // >8 字节必须以 FD 帧发送：自动置 FD 位，避免"无 FD 位却 DLC>8"的非法经典帧。
                let send_fd = f.fd || len > 8;
                let mut msgtype = if f.ext { MSGTYPE_EXTENDED } else { MSGTYPE_STANDARD };
                if send_fd {
                    msgtype |= MSGTYPE_FD;
                    if f.brs {
                        msgtype |= MSGTYPE_BRS;
                    }
                }
                if f.remote {
                    msgtype |= MSGTYPE_RTR;
                }
                let msg = TPCANMsgFD { id: f.id, msgtype, dlc: fd_len_to_dlc(len), data };
                let st = write(self.channel, &msg);
                if st != PCAN_ERROR_OK {
                    return Err(format!("CAN_WriteFD 失败 status=0x{st:08X}"));
                }
            }
            return Ok(());
        }
        // 经典通道：不支持 FD 帧，明确报错而非静默截断成 8 字节
        if f.fd || f.data.len() > 8 {
            return Err("当前 PCAN 适配器未按 CAN FD 初始化（请在设备配置里选 CAN FD）".into());
        }
        unsafe {
            let write: libloading::Symbol<FnWrite> = self
                .lib
                .get(b"CAN_Write\0")
                .map_err(|e| format!("找不到 CAN_Write: {e}"))?;
            let mut data = [0u8; 8];
            let len = f.data.len().min(8);
            data[..len].copy_from_slice(&f.data[..len]);
            let mut msgtype = if f.ext {
                MSGTYPE_EXTENDED
            } else {
                MSGTYPE_STANDARD
            };
            if f.remote {
                msgtype |= MSGTYPE_RTR; // 远程帧标志(此前漏置, 远程帧被当普通数据帧发)
            }
            let msg = TPCANMsg {
                id: f.id,
                msgtype,
                len: len as u8,
                data,
            };
            let st = write(self.channel, &msg);
            if st != PCAN_ERROR_OK {
                return Err(format!("CAN_Write 失败 status=0x{st:08X}"));
            }
        }
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

// ======================= 控制线程 =======================

#[allow(non_camel_case_types)]
mod zlg_ffi {
    pub type FnOpenDevice = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnCloseDevice = unsafe extern "system" fn(u32, u32) -> u32;
    pub type FnInitCan = unsafe extern "system" fn(u32, u32, u32, *mut VCI_INIT_CONFIG) -> u32;
    pub type FnStartCan = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnReceive =
        unsafe extern "system" fn(u32, u32, u32, *mut VCI_CAN_OBJ, u32, i32) -> u32;
    pub type FnTransmit = unsafe extern "system" fn(u32, u32, u32, *mut VCI_CAN_OBJ, u32) -> u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VCI_INIT_CONFIG {
        pub acc_code: u32,
        pub acc_mask: u32,
        pub reserved: u32,
        pub filter: u8,
        pub timing0: u8,
        pub timing1: u8,
        pub mode: u8,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VCI_CAN_OBJ {
        pub id: u32,
        pub time_stamp: u32,
        pub time_flag: u8,
        pub send_type: u8,
        pub remote_flag: u8,
        pub extern_flag: u8,
        pub data_len: u8,
        pub data: [u8; 8],
        pub reserved: [u8; 3],
    }
}

/// 通用 VCI 风格适配器：覆盖结构体/函数签名完全相同、只是 DLL 名与符号前缀不同的几家：
/// - ZLG/zhcxCAN：ControlCAN.dll + "VCI_" 前缀（VCI_OpenDevice...）
/// - GCAN（广成）：ECanVci64.dll + 无前缀（OpenDevice...）
pub struct VciBus {
    lib: libloading::Library,
    device_type: u32,
    device_index: u32,
    channel_index: u32,
    start: Instant,
    name: String,
    prefix: &'static str,
}

impl VciBus {
    /// dll_candidates 按序尝试加载；prefix 为符号前缀（"VCI_" 或 ""）。
    pub fn open(
        start: Instant,
        cfg: &DeviceConfig,
        dll_candidates: &[&str],
        prefix: &'static str,
        device_type: u32,
    ) -> Result<Self, String> {
        use zlg_ffi::*;
        let (timing0, timing1) = zlg_timing(&cfg.baud)
            .ok_or_else(|| format!("不支持的波特率: {}", cfg.baud))?;
        let sym = |n: &str| -> Vec<u8> {
            let mut v = format!("{prefix}{n}").into_bytes();
            v.push(0);
            v
        };
        unsafe {
            let mut lib_opt = None;
            let mut last_err = String::new();
            for name in dll_candidates {
                match libloading::Library::new(*name) {
                    Ok(l) => {
                        lib_opt = Some(l);
                        break;
                    }
                    Err(e) => last_err = format!("加载 {name} 失败: {e}"),
                }
            }
            let lib = lib_opt.ok_or(last_err)?;
            let open: libloading::Symbol<FnOpenDevice> = lib
                .get(sym("OpenDevice").as_slice())
                .map_err(|e| format!("{prefix}OpenDevice 未找到: {e}"))?;
            if open(device_type, cfg.device_index, 0) != 1 {
                return Err(format!("{prefix}OpenDevice 失败（检查设备/驱动）"));
            }
            drop(open);
            let init: libloading::Symbol<FnInitCan> = lib
                .get(sym("InitCAN").as_slice())
                .map_err(|e| format!("{prefix}InitCAN 未找到: {e}"))?;
            let mut init_cfg = VCI_INIT_CONFIG {
                acc_code: 0,
                acc_mask: 0xFFFF_FFFF,
                reserved: 0,
                filter: 1,
                timing0,
                timing1,
                mode: 0,
            };
            if init(device_type, cfg.device_index, cfg.channel_index, &mut init_cfg) != 1 {
                return Err(format!("{prefix}InitCAN 失败"));
            }
            drop(init);
            let start_can: libloading::Symbol<FnStartCan> = lib
                .get(sym("StartCAN").as_slice())
                .map_err(|e| format!("{prefix}StartCAN 未找到: {e}"))?;
            if start_can(device_type, cfg.device_index, cfg.channel_index) != 1 {
                return Err(format!("{prefix}StartCAN 失败"));
            }
            drop(start_can);
            Ok(Self {
                lib,
                device_type,
                device_index: cfg.device_index,
                channel_index: cfg.channel_index,
                start,
                name: format!(
                    "{} dev{} CAN{} @{}",
                    cfg.device_type,
                    cfg.device_index,
                    cfg.channel_index,
                    normalize_baud(&cfg.baud)
                ),
                prefix,
            })
        }
    }

    fn sym(&self, n: &str) -> Vec<u8> {
        let mut v = format!("{}{}", self.prefix, n).into_bytes();
        v.push(0);
        v
    }
}

impl Drop for VciBus {
    fn drop(&mut self) {
        unsafe {
            if let Ok(close) = self.lib.get::<zlg_ffi::FnCloseDevice>(self.sym("CloseDevice").as_slice()) {
                let _ = close(self.device_type, self.device_index);
            }
        }
    }
}

impl CanAdapter for VciBus {
    fn poll(&mut self, out: &mut Vec<CanFrame>) {
        use zlg_ffi::*;
        unsafe {
            let recv: libloading::Symbol<FnReceive> = match self.lib.get(self.sym("Receive").as_slice()) {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut frames = [VCI_CAN_OBJ {
                id: 0,
                time_stamp: 0,
                time_flag: 0,
                send_type: 0,
                remote_flag: 0,
                extern_flag: 0,
                data_len: 0,
                data: [0; 8],
                reserved: [0; 3],
            }; 256];
            let n = recv(
                self.device_type,
                self.device_index,
                self.channel_index,
                frames.as_mut_ptr(),
                frames.len() as u32,
                0,
            )
            .min(frames.len() as u32);
            for msg in frames.iter().take(n as usize) {
                let len = (msg.data_len as usize).min(8);
                out.push(CanFrame {
                    t: self.start.elapsed().as_secs_f64(),
                    ch: (self.channel_index + 1) as u8,
                    tx: false,
                    id: msg.id,
                    ext: msg.extern_flag != 0,
                    fd: false,
                    brs: false,
                    remote: msg.remote_flag != 0,
                    error: false,
                    data: msg.data[..len].to_vec(),
                });
            }
        }
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use zlg_ffi::*;
        // VCI(ControlCAN/ECanVci) 后端为经典 CAN（VCI_Transmit/VCI_CAN_OBJ，≤8 字节）；
        // FD 须 VCI_TransmitFD。明确报错，避免把 FD 帧静默截断成 8 字节。
        if f.fd || f.data.len() > 8 {
            return Err("当前 VCI 适配器不支持 CAN FD 发送".into());
        }
        unsafe {
            let transmit: libloading::Symbol<FnTransmit> = self
                .lib
                .get(self.sym("Transmit").as_slice())
                .map_err(|e| format!("{}Transmit 未找到: {e}", self.prefix))?;
            let mut data = [0u8; 8];
            let len = f.data.len().min(8);
            data[..len].copy_from_slice(&f.data[..len]);
            let mut msg = VCI_CAN_OBJ {
                id: f.id,
                time_stamp: 0,
                time_flag: 0,
                send_type: 0,
                remote_flag: if f.remote { 1 } else { 0 },
                extern_flag: if f.ext { 1 } else { 0 },
                data_len: len as u8,
                data,
                reserved: [0; 3],
            };
            if transmit(
                self.device_type,
                self.device_index,
                self.channel_index,
                &mut msg,
                1,
            ) != 1
            {
                return Err(format!("{}Transmit 失败", self.prefix));
            }
        }
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

fn zlg_device_type(device_type: &str) -> Option<u32> {
    match device_type.trim().to_ascii_uppercase().as_str() {
        "USBCAN1" => Some(3),
        "USBCAN2" | "USBCAN" => Some(4),
        "USBCAN-E-U" | "USBCANEU" => Some(20),
        "USBCAN-2E-U" | "USBCAN2EU" => Some(21),
        _ => None,
    }
}

// ======================= ZLG 新版 zlgcan.dll（ZCAN_ 接口，支持 CAN FD） =======================

/// 解析新版 zlgcan.dll 设备：返回 (类型号, 是否网络设备, 网络是否 TCP, 是否 USBCANFD)。
/// 类型号见手册附录1；网络设备波特率不经 API 设置（在设备本身配置）。
fn zcan_resolve(device_type: &str) -> Option<(u32, bool, bool, bool)> {
    match device_type.trim().to_ascii_uppercase().replace(' ', "").as_str() {
        "USBCANFD" | "USBCANFD-200U" | "USBCANFD200U" => Some((41, false, false, true)),
        "USBCANFD-100U" | "USBCANFD100U" => Some((42, false, false, true)),
        "USBCANFD-MINI" | "USBCANFDMINI" => Some((43, false, false, true)),
        "USBCANFD-800U" | "USBCANFD800U" => Some((59, false, false, true)),
        "USBCAN-E-U" | "USBCANEU" => Some((20, false, false, false)),
        "USBCAN-2E-U" | "USBCAN2EU" => Some((21, false, false, false)),
        "CANFDNET" | "CANFDNET-TCP" | "CANFDNETTCP" => Some((48, true, true, true)),
        "CANFDNET-UDP" | "CANFDNETUDP" => Some((49, true, false, true)),
        "CANFDWIFI" | "CANFDWIFI-TCP" | "CANFDWIFITCP" => Some((50, true, true, true)),
        "CANFDWIFI-UDP" | "CANFDWIFIUDP" => Some((51, true, false, true)),
        _ => None,
    }
}

/// 波特率字符串 → bps（用于 ZCAN_SetValue）。支持 "500K"/"1M"/"2M"/"1000K"/裸数字。
fn baud_to_bps(s: &str) -> u32 {
    let b = s.trim().to_ascii_uppercase().replace(' ', "").replace("BPS", "");
    if let Some(x) = b.strip_suffix('M') {
        return (x.parse::<f64>().unwrap_or(0.0) * 1_000_000.0) as u32;
    }
    if let Some(x) = b.strip_suffix('K') {
        return (x.parse::<f64>().unwrap_or(0.0) * 1_000.0) as u32;
    }
    b.parse::<u32>().unwrap_or(0)
}

#[allow(non_camel_case_types)]
mod zcan_ffi {
    use std::os::raw::{c_char, c_void};
    pub type DevHandle = *mut c_void;
    pub type ChHandle = *mut c_void;

    pub type FnOpenDevice = unsafe extern "system" fn(u32, u32, u32) -> DevHandle;
    pub type FnCloseDevice = unsafe extern "system" fn(DevHandle) -> u32;
    pub type FnInitCan =
        unsafe extern "system" fn(DevHandle, u32, *mut ZcanChannelInitConfig) -> ChHandle;
    pub type FnStartCan = unsafe extern "system" fn(ChHandle) -> u32;
    pub type FnSetValue = unsafe extern "system" fn(DevHandle, *const c_char, *const c_char) -> u32;
    pub type FnGetReceiveNum = unsafe extern "system" fn(ChHandle, u8) -> u32;
    pub type FnTransmit = unsafe extern "system" fn(ChHandle, *const ZcanTransmitData, u32) -> u32;
    pub type FnTransmitFd =
        unsafe extern "system" fn(ChHandle, *const ZcanTransmitFdData, u32) -> u32;
    pub type FnReceive =
        unsafe extern "system" fn(ChHandle, *mut ZcanReceiveData, u32, i32) -> u32;
    pub type FnReceiveFd =
        unsafe extern "system" fn(ChHandle, *mut ZcanReceiveFdData, u32, i32) -> u32;

    pub const EFF: u32 = 0x8000_0000; // 扩展帧标志
    pub const RTR: u32 = 0x4000_0000; // 远程帧标志
    pub const ID_MASK: u32 = 0x1FFF_FFFF;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CanFrameC {
        pub can_id: u32,
        pub can_dlc: u8,
        pub pad: u8,
        pub res0: u8,
        pub res1: u8,
        pub data: [u8; 8],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct CanfdFrameC {
        pub can_id: u32,
        pub len: u8,
        pub flags: u8,
        pub res0: u8,
        pub res1: u8,
        pub data: [u8; 64],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanTransmitData {
        pub frame: CanFrameC,
        pub transmit_type: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanTransmitFdData {
        pub frame: CanfdFrameC,
        pub transmit_type: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanReceiveData {
        pub frame: CanFrameC,
        pub timestamp: u64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanReceiveFdData {
        pub frame: CanfdFrameC,
        pub timestamp: u64,
    }
    // can_type(u32) + union(28 字节，取 canfd 较大者)，总 32 字节。
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanChannelInitConfig {
        pub can_type: u32,
        pub raw: [u8; 28],
    }
}

/// 新版 ZLG CAN(FD) 适配器（zlgcan.dll）。句柄以 usize 保存以满足 Send。
pub struct ZcanFdBus {
    lib: libloading::Library,
    dev: usize,
    ch: usize,
    fd_capable: bool, // 通道是否按 CANFD 初始化（决定是否抽取 FD 接收队列；USBCANFD 恒为真）
    start: Instant,
    name: String,
}

impl ZcanFdBus {
    pub fn open(start: Instant, cfg: &DeviceConfig) -> Result<Self, String> {
        use std::ffi::CString;
        use zcan_ffi::*;
        let (device_type, is_net, net_tcp, is_usbcanfd) = zcan_resolve(&cfg.device_type)
            .ok_or_else(|| format!("非新版 ZLG 设备类型: {}", cfg.device_type))?;
        unsafe {
            let lib = libloading::Library::new("zlgcan.dll").map_err(|e| {
                format!("加载 zlgcan.dll 失败（需把 zlgcan.dll 与 kerneldlls 放到 exe 同目录）: {e}")
            })?;
            let open: libloading::Symbol<FnOpenDevice> = lib
                .get(b"ZCAN_OpenDevice\0")
                .map_err(|e| format!("ZCAN_OpenDevice 未找到: {e}"))?;
            let dev = open(device_type, cfg.device_index, 0);
            if dev.is_null() {
                return Err("ZCAN_OpenDevice 失败（检查设备是否插好、驱动是否安装）".into());
            }

            // 打开成功后，后续任何失败都必须先关闭设备，否则句柄泄漏会导致再也打不开
            let close_dev = || {
                if let Ok(close) = lib.get::<FnCloseDevice>(b"ZCAN_CloseDevice\0") {
                    let _ = close(dev);
                }
            };

            // 属性设置（波特率/网络参数，均须在 InitCAN/StartCAN 之前）
            let setval: libloading::Symbol<FnSetValue> = match lib.get(b"ZCAN_SetValue\0") {
                Ok(s) => s,
                Err(e) => {
                    close_dev();
                    return Err(format!("ZCAN_SetValue 未找到: {e}"));
                }
            };
            let set = |key: &str, val: &str| -> Result<(), String> {
                let path = CString::new(format!("{}/{}", cfg.channel_index, key)).unwrap();
                let v = CString::new(val).unwrap();
                if setval(dev, path.as_ptr(), v.as_ptr()) != 1 {
                    Err(format!("设置 {key}={val} 失败"))
                } else {
                    Ok(())
                }
            };
            let cfg_res: Result<(), String> = if is_net {
                // 网络设备（CANFDNET/CANFDWIFI）：波特率在设备自身配置，这里只设连接参数
                let mode = if net_tcp {
                    // work_mode: 0=客户端, 1=服务器（连对端 server 时本端应为客户端）
                    set("work_mode", if cfg.net_server { "1" } else { "0" })
                } else {
                    // UDP：设本地监听端口
                    set("local_port", &cfg.port)
                };
                mode.and_then(|_| set("ip", &cfg.ip))
                    .and_then(|_| set("work_port", &cfg.port))
            } else if is_usbcanfd {
                // USBCANFD（200U/100U/MINI/800U）：CANFD 控制器，始终按 CANFD 初始化。
                // 没有 baud_rate 键；仲裁域=经典 CAN 波特率；abit/dbit 必须都设，否则 StartCAN 失败。
                // 跑经典 CAN 时数据域沿用仲裁域波特率（不影响经典帧）。
                let _ = set("canfd_standard", "0"); // 0=ISO
                let abit = baud_to_bps(&cfg.baud).to_string();
                let dbit = if cfg.is_fd {
                    baud_to_bps(&cfg.data_baud).to_string()
                } else {
                    abit.clone()
                };
                set("canfd_abit_baud_rate", &abit)
                    .and_then(|_| set("canfd_dbit_baud_rate", &dbit))
            } else {
                // 经典 ZLG USB（USBCAN-E-U / USBCAN-2E-U）：用 baud_rate
                set("baud_rate", &baud_to_bps(&cfg.baud).to_string())
            };
            if let Err(e) = cfg_res {
                close_dev();
                return Err(e);
            }
            drop(setval);

            // 初始化通道
            let init: libloading::Symbol<FnInitCan> = match lib.get(b"ZCAN_InitCAN\0") {
                Ok(s) => s,
                Err(e) => {
                    close_dev();
                    return Err(format!("ZCAN_InitCAN 未找到: {e}"));
                }
            };
            // USBCANFD 设备恒为 CANFD 初始化；经典 USB 卡按协议；网络设备 InitCAN 不做实际操作。
            // 通道一旦按 CANFD 初始化，总线上的 FD 帧就会进 FD 接收队列，须照此抽取。
            let fd_capable = cfg.is_fd || (is_usbcanfd && !is_net);
            let mut init_cfg = ZcanChannelInitConfig {
                can_type: if fd_capable { 1 } else { 0 },
                raw: [0u8; 28],
            };
            let ch = init(dev, cfg.channel_index, &mut init_cfg);
            drop(init);
            if ch.is_null() {
                close_dev();
                return Err("ZCAN_InitCAN 失败".into());
            }

            // 终端电阻（仅 USBCANFD 内置，须在 ZCAN_InitCAN 之后设置，尽力而为）
            if is_usbcanfd && !is_net
                && let Ok(setres) = lib.get::<FnSetValue>(b"ZCAN_SetValue\0") {
                    let path = CString::new(format!("{}/initenal_resistance", cfg.channel_index))
                        .unwrap();
                    let v = CString::new(if cfg.termination { "1" } else { "0" }).unwrap();
                    let _ = setres(dev, path.as_ptr(), v.as_ptr());
                }

            // 启动通道
            let start_can: libloading::Symbol<FnStartCan> = match lib.get(b"ZCAN_StartCAN\0") {
                Ok(s) => s,
                Err(e) => {
                    close_dev();
                    return Err(format!("ZCAN_StartCAN 未找到: {e}"));
                }
            };
            if start_can(ch) != 1 {
                drop(start_can);
                close_dev();
                return Err("ZCAN_StartCAN 失败".into());
            }
            drop(start_can);

            let name = if is_net {
                format!("{} {}:{}", cfg.device_type, cfg.ip, cfg.port)
            } else {
                format!(
                    "{} dev{} CAN{} @{}{}",
                    cfg.device_type,
                    cfg.device_index,
                    cfg.channel_index,
                    normalize_baud(&cfg.baud),
                    if cfg.is_fd {
                        format!("/{}", normalize_baud(&cfg.data_baud))
                    } else {
                        String::new()
                    }
                )
            };
            Ok(Self {
                lib,
                dev: dev as usize,
                ch: ch as usize,
                fd_capable,
                start,
                name,
            })
        }
    }
}

impl Drop for ZcanFdBus {
    fn drop(&mut self) {
        unsafe {
            if let Ok(close) = self.lib.get::<zcan_ffi::FnCloseDevice>(b"ZCAN_CloseDevice\0") {
                let _ = close(self.dev as zcan_ffi::DevHandle);
            }
        }
    }
}

impl CanAdapter for ZcanFdBus {
    fn poll(&mut self, out: &mut Vec<CanFrame>) {
        use zcan_ffi::*;
        let ch = self.ch as ChHandle;
        unsafe {
            let getnum: libloading::Symbol<FnGetReceiveNum> =
                match self.lib.get(b"ZCAN_GetReceiveNum\0") {
                    Ok(s) => s,
                    Err(_) => return,
                };
            // 标准 CAN 帧（type=0）
            if let Ok(recv) = self.lib.get::<FnReceive>(b"ZCAN_Receive\0") {
                let n = getnum(ch, 0).min(100);
                if n > 0 {
                    let mut buf = vec![
                        ZcanReceiveData {
                            frame: CanFrameC { can_id: 0, can_dlc: 0, pad: 0, res0: 0, res1: 0, data: [0; 8] },
                            timestamp: 0,
                        };
                        n as usize
                    ];
                    let got = recv(ch, buf.as_mut_ptr(), n, 0).min(n);
                    for r in buf.iter().take(got as usize) {
                        let len = (r.frame.can_dlc as usize).min(8);
                        out.push(CanFrame {
                            t: self.start.elapsed().as_secs_f64(),
                            ch: 1,
                            tx: false,
                            id: r.frame.can_id & ID_MASK,
                            ext: r.frame.can_id & EFF != 0,
                            fd: false,
                            brs: false,
                            remote: r.frame.can_id & RTR != 0,
                            error: false,
                            data: r.frame.data[..len].to_vec(),
                        });
                    }
                }
            }
            // CAN FD 帧（type=1）：只要通道按 CANFD 初始化(fd_capable)就必须抽取，
            // 否则 USBCANFD 在"经典模式"下总线上的 FD 帧会进 FD 队列却从不读出→丢帧+FIFO堆积。
            if self.fd_capable
                && let Ok(recv) = self.lib.get::<FnReceiveFd>(b"ZCAN_ReceiveFD\0") {
                    let n = getnum(ch, 1).min(100);
                    if n > 0 {
                        let mut buf = vec![
                            ZcanReceiveFdData {
                                frame: CanfdFrameC { can_id: 0, len: 0, flags: 0, res0: 0, res1: 0, data: [0; 64] },
                                timestamp: 0,
                            };
                            n as usize
                        ];
                        let got = recv(ch, buf.as_mut_ptr(), n, 0).min(n);
                        for r in buf.iter().take(got as usize) {
                            let len = (r.frame.len as usize).min(64);
                            out.push(CanFrame {
                                t: self.start.elapsed().as_secs_f64(),
                                ch: 1,
                                tx: false,
                                id: r.frame.can_id & ID_MASK,
                                ext: r.frame.can_id & EFF != 0,
                                fd: true,
                                brs: r.frame.flags & 0x01 != 0,
                                remote: r.frame.can_id & RTR != 0,
                                error: false,
                                data: r.frame.data[..len].to_vec(),
                            });
                        }
                    }
                }
        }
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use zcan_ffi::*;
        let ch = self.ch as ChHandle;
        let mut can_id = f.id & ID_MASK;
        if f.ext {
            can_id |= EFF;
        }
        if f.remote {
            can_id |= RTR;
        }
        unsafe {
            if f.fd {
                let transmit: libloading::Symbol<FnTransmitFd> = self
                    .lib
                    .get(b"ZCAN_TransmitFD\0")
                    .map_err(|e| format!("ZCAN_TransmitFD 未找到: {e}"))?;
                let mut data = [0u8; 64];
                let len = f.data.len().min(64);
                data[..len].copy_from_slice(&f.data[..len]);
                let msg = ZcanTransmitFdData {
                    frame: CanfdFrameC {
                        can_id,
                        len: len as u8,
                        flags: if f.brs { 0x01 } else { 0x00 },
                        res0: 0,
                        res1: 0,
                        data,
                    },
                    transmit_type: 0,
                };
                if transmit(ch, &msg, 1) != 1 {
                    return Err("ZCAN_TransmitFD 失败".into());
                }
            } else {
                let transmit: libloading::Symbol<FnTransmit> = self
                    .lib
                    .get(b"ZCAN_Transmit\0")
                    .map_err(|e| format!("ZCAN_Transmit 未找到: {e}"))?;
                let mut data = [0u8; 8];
                let len = f.data.len().min(8);
                data[..len].copy_from_slice(&f.data[..len]);
                let msg = ZcanTransmitData {
                    frame: CanFrameC {
                        can_id,
                        can_dlc: len as u8,
                        pad: 0,
                        res0: 0,
                        res1: 0,
                        data,
                    },
                    transmit_type: 0,
                };
                if transmit(ch, &msg, 1) != 1 {
                    return Err("ZCAN_Transmit 失败".into());
                }
            }
        }
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

pub enum Cmd {
    #[allow(dead_code)]
    Connect,
    #[allow(dead_code)]
    ConnectConfig(DeviceConfig),
    /// 连接一组通道（多通道）。
    ConnectChannels(Vec<DeviceConfig>),
    Disconnect,
    Start,
    Stop,
    SendOnce(CanFrame),
    OtaRun(OtaJob),
    /// 周期发送：handle 唯一标识任务；enable=false 表示停止该任务。
    SetPeriodic {
        handle: u64,
        frame: CanFrame,
        period_ms: u64,
        /// 发送次数：-1 = 无限循环；N>=1 = 发 N 次后自动停止。
        repeat: i64,
        enable: bool,
    },
    /// 载入回放帧序列（已在上层做好通道映射/过滤/时间段裁剪）。
    PlaybackLoad(Vec<CanFrame>),
    /// 开始/继续回放。online=true 发到总线；speed<=0 表示尽可能快，否则为倍率。loop_play=到尾循环。
    PlaybackPlay { online: bool, speed: f64, loop_play: bool },
    PlaybackStep,
    PlaybackPause,
    PlaybackCancel,
    /// 跳转到指定进度(0.0~1.0)。
    PlaybackSeek(f64),
    #[allow(dead_code)]
    Quit,
}

#[derive(Clone, Debug)]
pub struct OtaJob {
    pub name: String,
    pub steps: Vec<OtaStep>,
    pub timeout_ms: u64,
    pub retries: u32,
}

#[derive(Clone, Debug)]
pub struct OtaStep {
    pub frame: CanFrame,
    pub ack: OtaAck,
    pub timeout_ms: u64,
    pub retries: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum OtaResponseId {
    Exact(u32),
    WildcardBase(u32),
}

#[derive(Clone, Copy, Debug)]
pub enum OtaAck {
    None,
    XcpConnect { response: OtaResponseId },
    XcpAck { response: OtaResponseId },
    UdsFlowControl,
    UdsPositive { service: u8 },
}

pub enum Evt {
    Frame(CanFrame),
    PlaybackFrame(CanFrame),
    Log(String),
    OtaProgress(usize, usize, String),
    /// 连接状态变化：(connected, 后端名称)
    Connected(bool, String),
    Running(bool),
    /// 回放进度：(已回放, 总数, 正在回放)
    Playback(usize, usize, bool),
    /// 周期任务发够设定次数后自动停止：携带任务 handle，供上层更新 UI 状态。
    PeriodicDone(u64),
}

struct Playback {
    frames: Vec<CanFrame>,
    idx: usize,
    online: bool,
    speed: f64,
    playing: bool,
    paused: bool,
    base: Instant,
    base_t: f64,
    loop_play: bool, // 回放到尾后自动从头循环
}

struct Periodic {
    frame: CanFrame,
    period: Duration,
    next: Instant,
    /// 剩余发送次数：-1 = 无限；否则发完归 0 即移除。
    remaining: i64,
}

/// 启动控制线程，返回 (命令发送端, 事件接收端)。
pub fn spawn() -> (Sender<Cmd>, Receiver<Evt>) {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (evt_tx, evt_rx) = channel::<Evt>();
    std::thread::spawn(move || controller(cmd_rx, evt_tx));
    (cmd_tx, evt_rx)
}

/// 按配置打开一个适配器（设备类型分派）。
fn open_adapter(start: Instant, cfg: &DeviceConfig) -> Result<Box<dyn CanAdapter>, String> {
    let device = cfg.device_type.trim().to_ascii_uppercase();
    if device == "VIRTUAL" || device == "SIM" {
        Err("不支持的设备类型: 虚拟总线已移除".into())
    } else if device == "PCAN" {
        PcanBus::open_cfg(start, cfg)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "GCAN" {
        // 广成 GCAN：ECanVci64.dll，符号无前缀，设备类型 USBCAN2=4
        VciBus::open(start, cfg, &["ECanVci64.dll", "ECanVci.dll"], "", 4)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "ZHCX" || device == "ZHCXCAN" {
        // zhcxCAN：ControlCAN.dll + VCI_ 前缀，设备类型 4
        VciBus::open(start, cfg, &["ControlCAN.dll"], "VCI_", 4)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if zcan_resolve(&cfg.device_type).is_some() {
        // 新版统一 ZLG 设备（USBCANFD-*/USBCAN-E-U/2E-U/CANFDNET/CANFDWIFI）走 zlgcan.dll
        ZcanFdBus::open(start, cfg).map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if let Some(dtype) = zlg_device_type(&cfg.device_type) {
        // 老 ZLG USBCAN-I/II 走 ControlCAN.dll + VCI_ 前缀
        VciBus::open(start, cfg, &["ControlCAN.dll"], "VCI_", dtype)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else {
        Err(format!("未知设备类型: {}", cfg.device_type))
    }
}

/// 在指定通道（或首个可用）上发送一帧。返回实际发送所用的软件通道号，
/// 以便回显帧标注真实通道（请求的通道无匹配时回退到首个适配器）。
fn send_on(adapters: &mut [(u8, Box<dyn CanAdapter>)], f: &CanFrame) -> Result<u8, String> {
    if adapters.is_empty() {
        return Err("无已连接通道".into());
    }
    if let Some((c, a)) = adapters.iter_mut().find(|(c, _)| *c == f.ch) {
        let used = *c;
        a.send(f).map(|()| used)
    } else {
        let used = adapters[0].0;
        adapters[0].1.send(f).map(|()| used)
    }
}

/// 连接一组通道：逐个打开适配器，成功的按软件通道号收进 adapters。
fn connect_channels(
    adapters: &mut Vec<(u8, Box<dyn CanAdapter>)>,
    running: &mut bool,
    periodics: &mut HashMap<u64, Periodic>,
    evt_tx: &Sender<Evt>,
    start: Instant,
    cfgs: Vec<DeviceConfig>,
) {
    *running = false;
    periodics.clear();
    adapters.clear();
    let _ = evt_tx.send(Evt::Running(false));
    let mut names: Vec<String> = Vec::new();
    for cfg in &cfgs {
        let ch = if cfg.sw_channel == 0 { 1 } else { cfg.sw_channel };
        match open_adapter(start, cfg) {
            Ok(bus) => {
                let name = bus.name().to_string();
                adapters.push((ch, bus));
                let _ = evt_tx.send(Evt::Log(format!("CAN{ch} 已连接: {name}")));
                names.push(format!("CAN{ch}:{name}"));
            }
            Err(e) => {
                let _ = evt_tx.send(Evt::Log(format!("CAN{ch} 连接失败: {e}")));
            }
        }
    }
    if adapters.is_empty() {
        let _ = evt_tx.send(Evt::Connected(false, String::new()));
    } else {
        let _ = evt_tx.send(Evt::Connected(true, names.join("  ")));
    }
}

fn ota_response_id_matches(spec: OtaResponseId, id: u32) -> bool {
    match spec {
        OtaResponseId::Exact(expected) => id == expected,
        OtaResponseId::WildcardBase(base) => (id & 0xFFFF_FF00) == base,
    }
}

fn ota_ack_matches(ack: OtaAck, frame: &CanFrame) -> bool {
    match ack {
        OtaAck::None => true,
        OtaAck::XcpConnect { response } => {
            ota_response_id_matches(response, frame.id)
                && frame.data.len() >= 8
                && frame.data[0] == 0xFF
                && frame.data[1] == 0x10
                && frame.data[4] == 0x08
                && frame.data[5] == 0x00
                && frame.data[6] == 0x01
                && frame.data[7] == 0x01
        }
        OtaAck::XcpAck { response } => {
            ota_response_id_matches(response, frame.id) && frame.data.len() == 1 && frame.data[0] == 0xFF
        }
        OtaAck::UdsFlowControl => frame.data.first().copied() == Some(0x30),
        OtaAck::UdsPositive { service } => frame.data.get(1).copied() == Some(service.wrapping_add(0x40)),
    }
}

fn poll_for_ota_ack(
    adapters: &mut [(u8, Box<dyn CanAdapter>)],
    evt_tx: &Sender<Evt>,
    buf: &mut Vec<CanFrame>,
    ack: OtaAck,
    timeout: Duration,
) -> bool {
    if matches!(ack, OtaAck::None) {
        return true;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if OTA_CANCEL.load(Ordering::Relaxed) {
            return false;
        }
        for (ch, adapter) in adapters.iter_mut() {
            buf.clear();
            adapter.poll(buf);
            for mut frame in buf.drain(..) {
                frame.ch = *ch;
                let matched = ota_ack_matches(ack, &frame);
                let _ = evt_tx.send(Evt::Frame(frame));
                if matched {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

fn run_ota_job(
    adapters: &mut Vec<(u8, Box<dyn CanAdapter>)>,
    evt_tx: &Sender<Evt>,
    start: Instant,
    buf: &mut Vec<CanFrame>,
    job: OtaJob,
) {
    if adapters.is_empty() {
        let _ = evt_tx.send(Evt::OtaProgress(0, job.steps.len(), "OTA failed: no device connected".into()));
        return;
    }

    let total = job.steps.len();
    OTA_CANCEL.store(false, Ordering::Relaxed);
    let _ = evt_tx.send(Evt::OtaProgress(0, total, format!("{} started", job.name)));

    for (idx, step) in job.steps.into_iter().enumerate() {
        if OTA_CANCEL.load(Ordering::Relaxed) {
            let _ = evt_tx.send(Evt::OtaProgress(idx, total, format!("{} cancelled", job.name)));
            return;
        }
        let mut ok = false;
        let timeout = Duration::from_millis(step.timeout_ms.max(job.timeout_ms).max(1));
        let retries = step.retries.max(job.retries);
        for attempt in 0..=retries {
            let mut frame = step.frame.clone();
            if OTA_CANCEL.load(Ordering::Relaxed) {
                let _ = evt_tx.send(Evt::OtaProgress(idx, total, format!("{} cancelled", job.name)));
                return;
            }
            frame.t = start.elapsed().as_secs_f64();
            frame.tx = true;
            match send_on(adapters, &frame) {
                Ok(used) => {
                    frame.ch = used;
                    let _ = evt_tx.send(Evt::Frame(frame));
                }
                Err(e) => {
                    let _ = evt_tx.send(Evt::OtaProgress(idx, total, format!("OTA send failed: {e}")));
                    return;
                }
            }

            if poll_for_ota_ack(adapters, evt_tx, buf, step.ack, timeout) {
                ok = true;
                break;
            }
            let _ = evt_tx.send(Evt::Log(format!(
                "{} step {}/{} timeout, retry {}/{}",
                job.name,
                idx + 1,
                total,
                attempt + 1,
                retries
            )));
        }

        if !ok {
            let _ = evt_tx.send(Evt::OtaProgress(idx, total, format!("{} failed at step {}", job.name, idx + 1)));
            return;
        }

        let _ = evt_tx.send(Evt::OtaProgress(idx + 1, total, format!("{} {}/{}", job.name, idx + 1, total)));
    }

    let _ = evt_tx.send(Evt::OtaProgress(total, total, format!("{} complete", job.name)));
}

fn controller(cmd_rx: Receiver<Cmd>, evt_tx: Sender<Evt>) {
    let start = Instant::now();
    let mut adapters: Vec<(u8, Box<dyn CanAdapter>)> = Vec::new();
    let mut running = false;
    let mut periodics: HashMap<u64, Periodic> = HashMap::new();
    let mut buf: Vec<CanFrame> = Vec::with_capacity(1024);
    let mut playback: Option<Playback> = None;

    loop {
        // 处理命令（非阻塞批量）
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    Cmd::Connect => match PcanBus::open(start) {
                        Ok(p) => {
                            let n = p.name().to_string();
                            adapters = vec![(1, Box::new(p))];
                            let _ = evt_tx.send(Evt::Log(format!("已连接真实 PCAN 卡: {n}")));
                            let _ = evt_tx.send(Evt::Connected(true, n));
                        }
                        Err(e) => {
                            let _ = evt_tx
                                .send(Evt::Log(format!("连接 PCAN 卡失败: {e}")));
                            let _ = evt_tx.send(Evt::Connected(false, String::new()));
                        }
                    },
                    Cmd::ConnectConfig(cfg) => {
                        connect_channels(&mut adapters, &mut running, &mut periodics, &evt_tx, start, vec![cfg]);
                    }
                    Cmd::ConnectChannels(cfgs) => {
                        connect_channels(&mut adapters, &mut running, &mut periodics, &evt_tx, start, cfgs);
                    }
                    Cmd::Disconnect => {
                        adapters.clear();
                        running = false;
                        periodics.clear();
                        let _ = evt_tx.send(Evt::Running(false));
                        let _ = evt_tx.send(Evt::Connected(false, String::new()));
                        let _ = evt_tx.send(Evt::Log("已断开设备".into()));
                    }
                    Cmd::Start => {
                        if !adapters.is_empty() {
                            running = true;
                            let _ = evt_tx.send(Evt::Running(true));
                            let _ = evt_tx.send(Evt::Log("启动接收".into()));
                        } else {
                            let _ = evt_tx.send(Evt::Log("未连接设备，无法启动".into()));
                        }
                    }
                    Cmd::Stop => {
                        running = false;
                        let _ = evt_tx.send(Evt::Running(false));
                        let _ = evt_tx.send(Evt::Log("停止接收".into()));
                    }
                    Cmd::SendOnce(mut f) => {
                        if !adapters.is_empty() {
                            f.t = start.elapsed().as_secs_f64();
                            match send_on(&mut adapters, &f) {
                                Ok(used) => {
                                    let mut echo = f.clone();
                                    echo.tx = true;
                                    echo.ch = used; // 标注实际发送通道(回退时与请求不同)
                                    let _ = evt_tx.send(Evt::Frame(echo));
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(Evt::Log(format!("发送失败: {e}")));
                                }
                            }
                        }
                    }
                    Cmd::OtaRun(job) => {
                        run_ota_job(&mut adapters, &evt_tx, start, &mut buf, job);
                    }
                    Cmd::SetPeriodic {
                        handle,
                        frame,
                        period_ms,
                        repeat,
                        enable,
                    } => {
                        if enable && repeat != 0 {
                            periodics.insert(
                                handle,
                                Periodic {
                                    frame,
                                    period: Duration::from_millis(period_ms.max(1)),
                                    next: Instant::now(),
                                    remaining: repeat,
                                },
                            );
                        } else {
                            periodics.remove(&handle);
                        }
                    }
                    Cmd::PlaybackLoad(frames) => {
                        let total = frames.len();
                        playback = Some(Playback {
                            frames,
                            idx: 0,
                            online: false,
                            speed: 1.0,
                            playing: false,
                            paused: false,
                            base: Instant::now(),
                            base_t: 0.0,
                            loop_play: false,
                        });
                        let _ = evt_tx.send(Evt::Log(format!("已载入回放文件，共 {total} 帧")));
                        let _ = evt_tx.send(Evt::Playback(0, total, false));
                    }
                    Cmd::PlaybackPlay { online, speed, loop_play } => {
                        if let Some(pb) = playback.as_mut() {
                            if pb.idx >= pb.frames.len() {
                                pb.idx = 0;
                            }
                            pb.online = online;
                            pb.speed = speed;
                            pb.loop_play = loop_play;
                            pb.playing = true;
                            pb.paused = false;
                            pb.base = Instant::now();
                            pb.base_t = pb.frames.get(pb.idx).map(|f| f.t).unwrap_or(0.0);
                            let _ = evt_tx.send(Evt::Log(format!(
                                "开始回放（{}，{}）",
                                if online { "在线" } else { "离线" },
                                if speed <= 0.0 {
                                    "尽可能快".to_string()
                                } else {
                                    format!("{speed}x")
                                }
                            )));
                        }
                    }
                    Cmd::PlaybackPause => {
                        if let Some(pb) = playback.as_mut() {
                            pb.paused = true;
                            let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), false));
                        }
                    }
                    Cmd::PlaybackStep => {
                        if let Some(pb) = playback.as_mut() {
                            pb.playing = false;
                            if pb.idx < pb.frames.len() {
                                let t0 = pb.frames[pb.idx].t;
                                while pb.idx < pb.frames.len() && pb.frames[pb.idx].t < t0 + 0.1 {
                                    let mut e = pb.frames[pb.idx].clone();
                                    if pb.online {
                                        if let Ok(used) = send_on(&mut adapters, &e) {
                                            e.ch = used;
                                        }
                                        e.tx = true;
                                    }
                                    let _ = evt_tx.send(Evt::PlaybackFrame(e));
                                    pb.idx += 1;
                                }
                            }
                            let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), false));
                        }
                    }
                    Cmd::PlaybackCancel => {
                        if let Some(pb) = playback.as_mut() {
                            pb.idx = 0;
                            pb.playing = false;
                            pb.paused = false;
                            let _ = evt_tx.send(Evt::Playback(0, pb.frames.len(), false));
                            let _ = evt_tx.send(Evt::Log("已取消回放".into()));
                        }
                    }
                    Cmd::PlaybackSeek(frac) => {
                        if let Some(pb) = playback.as_mut()
                            && !pb.frames.is_empty() {
                                let i = ((frac.clamp(0.0, 1.0) * pb.frames.len() as f64) as usize)
                                    .min(pb.frames.len() - 1);
                                pb.idx = i;
                                pb.base = Instant::now();
                                pb.base_t = pb.frames[i].t;
                                let _ = evt_tx.send(Evt::Playback(
                                    pb.idx,
                                    pb.frames.len(),
                                    pb.playing,
                                ));
                            }
                    }
                    Cmd::Quit => {
                        adapters.clear(); // 触发各适配器 Drop → CloseDevice，释放设备
                        return;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    adapters.clear(); // UI 退出、命令通道断开时也要关闭设备
                    return;
                }
            }
        }

        // 周期发送
        if !periodics.is_empty() && !adapters.is_empty() {
            let now = Instant::now();
            let mut due: Vec<CanFrame> = Vec::new();
            let mut done: Vec<u64> = Vec::new();
            for (h, p) in periodics.iter_mut() {
                if now >= p.next {
                    p.next = now + p.period;
                    let mut f = p.frame.clone();
                    f.t = start.elapsed().as_secs_f64();
                    f.tx = true;
                    due.push(f);
                    if p.remaining > 0 {
                        p.remaining -= 1;
                        if p.remaining == 0 {
                            done.push(*h);
                        }
                    }
                }
            }
            for mut f in due {
                if let Ok(used) = send_on(&mut adapters, &f) {
                    f.ch = used; // 标注实际发送通道
                    let _ = evt_tx.send(Evt::Frame(f));
                }
            }
            for h in done {
                periodics.remove(&h);
                let _ = evt_tx.send(Evt::PeriodicDone(h));
            }
        }

        // 回放推进
        let mut pb_active = false;
        if let Some(pb) = playback.as_mut()
            && pb.playing && !pb.paused {
                pb_active = true;
                if pb.speed <= 0.0 {
                    // 尽可能快：每轮放一批，避免一次性灌爆
                    for _ in 0..500 {
                        if pb.idx >= pb.frames.len() {
                            break;
                        }
                        let mut e = pb.frames[pb.idx].clone();
                        if pb.online {
                            if let Ok(used) = send_on(&mut adapters, &e) {
                                e.ch = used;
                            }
                            e.tx = true;
                        }
                        let _ = evt_tx.send(Evt::PlaybackFrame(e));
                        pb.idx += 1;
                    }
                } else {
                    let now = Instant::now();
                    loop {
                        if pb.idx >= pb.frames.len() {
                            break;
                        }
                        let dt = (pb.frames[pb.idx].t - pb.base_t).max(0.0) / pb.speed;
                        if now >= pb.base + Duration::from_secs_f64(dt) {
                            let mut e = pb.frames[pb.idx].clone();
                            if pb.online {
                                if let Ok(used) = send_on(&mut adapters, &e) {
                                    e.ch = used;
                                }
                                e.tx = true;
                            }
                            let _ = evt_tx.send(Evt::PlaybackFrame(e));
                            pb.idx += 1;
                        } else {
                            break;
                        }
                    }
                }
                if pb.idx >= pb.frames.len() {
                    if pb.loop_play && !pb.frames.is_empty() {
                        // 循环：回到开头继续播
                        pb.idx = 0;
                        pb.base = Instant::now();
                        pb.base_t = pb.frames[0].t;
                        let _ = evt_tx.send(Evt::Playback(0, pb.frames.len(), true));
                    } else {
                        pb.playing = false;
                        let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), false));
                        let _ = evt_tx.send(Evt::Log("回放完成".into()));
                    }
                } else {
                    let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), true));
                }
            }

        // 接收（逐通道轮询，按软件通道号给帧打标签）
        if running {
            for (ch, a) in adapters.iter_mut() {
                buf.clear();
                a.poll(&mut buf);
                for mut f in buf.drain(..) {
                    f.ch = *ch;
                    let _ = evt_tx.send(Evt::Frame(f));
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        } else if pb_active {
            std::thread::sleep(Duration::from_millis(1));
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
