#![allow(clippy::drop_non_drop)]

// Pure CAN backend: compiled in pcanwork-core, independently from Slint UI code.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{
    Receiver as EventReceiver, Sender as EventChannelSender, TryRecvError, TrySendError, bounded,
};

static OTA_CANCEL: AtomicBool = AtomicBool::new(false);

use crate::dbc::DbcDb;
use crate::timestamp_quality::{TimestampQuality, TimestampQualitySnapshot};
use crate::vary::{self, VaryMode};

/// Maps a hardware clock into this process' monotonic capture timeline while preserving
/// device-level deltas. The first hardware sample is anchored to the host arrival time;
/// later samples no longer inherit UI/controller scheduling jitter.
#[derive(Debug)]
struct HardwareTimebase {
    tick_seconds: f64,
    wrap_modulus: Option<u64>,
    wrap_offset: u64,
    previous_raw: Option<u64>,
    anchor_raw: Option<u64>,
    anchor_host: f64,
}

impl HardwareTimebase {
    fn new(tick_seconds: f64, counter_bits: Option<u32>) -> Self {
        Self {
            tick_seconds,
            wrap_modulus: counter_bits.map(|bits| 1u64 << bits),
            wrap_offset: 0,
            previous_raw: None,
            anchor_raw: None,
            anchor_host: 0.0,
        }
    }

    fn map(&mut self, raw: u64, host_now: f64) -> f64 {
        if let (Some(modulus), Some(previous)) = (self.wrap_modulus, self.previous_raw)
            && raw < previous
            && previous - raw > modulus / 2
        {
            self.wrap_offset = self.wrap_offset.saturating_add(modulus);
        }
        self.previous_raw = Some(raw);
        let extended = self.wrap_offset.saturating_add(raw);
        if self.anchor_raw.is_none() {
            self.anchor_host = host_now;
            self.anchor_raw = Some(extended);
        }
        let anchor = self.anchor_raw.unwrap_or(extended);
        self.anchor_host + extended.saturating_sub(anchor) as f64 * self.tick_seconds
    }
}

pub fn cancel_ota() {
    OTA_CANCEL.store(true, Ordering::Relaxed);
}

#[derive(Clone, Debug)]
pub struct CanFrame {
    pub t: f64,
    pub ch: u8,
    pub tx: bool,
    pub id: u32,
    pub ext: bool,
    pub fd: bool, // CAN FD
    pub brs: bool,
    pub remote: bool,
    pub error: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub sw_channel: u8,
    pub is_fd: bool,
    pub device_type: String,
    pub hardware_label: String,
    /// Stable physical endpoint identity. Device indices are only a runtime
    /// fallback because USB enumeration order may change after replugging.
    pub hardware_id: String,
    pub device_index: u32,
    pub channel_index: u32,
    pub baud: String,
    pub data_baud: String,
    /// Optional complete PCAN FD timing string (f_clock/nom_*/data_*).
    pub custom_bitrate: String,
    pub termination: bool,
    pub listen_only: bool,
    pub fd_non_iso: bool,
    pub net_server: bool,
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

#[derive(Default)]
pub struct PollReport {
    pub receive_overruns: u64,
    pub driver_errors: u64,
    pub connection_lost: bool,
    pub message: Option<String>,
}

pub trait CanAdapter: Send {
    fn poll(&mut self, out: &mut Vec<CanFrame>) -> PollReport;
    fn send(&mut self, f: &CanFrame) -> Result<(), String>;
    fn name(&self) -> &str;
}

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
    pub type FnInit = unsafe extern "system" fn(u16, u16, u8, u32, u16) -> u32;
    pub type FnUninit = unsafe extern "system" fn(u16) -> u32;
    pub type FnRead = unsafe extern "system" fn(u16, *mut TPCANMsg, *mut TPCANTimestamp) -> u32;
    pub type FnWrite = unsafe extern "system" fn(u16, *const TPCANMsg) -> u32;
    pub type FnGetValue = unsafe extern "system" fn(u16, u8, *mut std::ffi::c_void, u32) -> u32;

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

    pub const PCAN_NONEBUS: u16 = 0x00;
    pub const PCAN_BAUD_125K: u16 = 0x031C;
    pub const PCAN_BAUD_250K: u16 = 0x011C;
    pub const PCAN_BAUD_500K: u16 = 0x001C;
    pub const PCAN_BAUD_1M: u16 = 0x0014;
    pub const PCAN_ERROR_OK: u32 = 0x0000_0000;
    pub const PCAN_ERROR_QRCVEMPTY: u32 = 0x0000_0020;
    pub const PCAN_ERROR_OVERRUN: u32 = 0x0000_0002;
    pub const PCAN_ERROR_BUSLIGHT: u32 = 0x0000_0004;
    pub const PCAN_ERROR_BUSHEAVY: u32 = 0x0000_0008;
    pub const PCAN_ERROR_BUSOFF: u32 = 0x0000_0010;
    pub const PCAN_ERROR_QOVERRUN: u32 = 0x0000_0040;
    pub const PCAN_ERROR_NODRIVER: u32 = 0x0000_0200;
    pub const PCAN_ERROR_ILLHW: u32 = 0x0000_1400;
    pub const PCAN_ERROR_INITIALIZE: u32 = 0x0400_0000;
    pub const PCAN_ATTACHED_CHANNELS_COUNT: u8 = 0x2A;
    pub const PCAN_ATTACHED_CHANNELS: u8 = 0x2B;
    pub const PCAN_FEATURE_FD_CAPABLE: u32 = 0x01;

    pub const MSGTYPE_STANDARD: u8 = 0x00;
    pub const MSGTYPE_RTR: u8 = 0x01;
    pub const MSGTYPE_EXTENDED: u8 = 0x02;
    pub const MSGTYPE_FD: u8 = 0x04;
    pub const MSGTYPE_BRS: u8 = 0x08;
    pub const MSGTYPE_ERRFRAME: u8 = 0x40;
    pub const MSGTYPE_STATUS: u8 = 0x80;

    pub type FnInitFd = unsafe extern "system" fn(u16, *const std::os::raw::c_char) -> u32;
    pub type FnWriteFd = unsafe extern "system" fn(u16, *const TPCANMsgFD) -> u32;
    pub type FnReadFd = unsafe extern "system" fn(u16, *mut TPCANMsgFD, *mut u64) -> u32;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TPCANMsgFD {
        pub id: u32,
        pub msgtype: u8,
        pub dlc: u8,
        pub data: [u8; 64],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TPCANChannelInformation {
        pub channel_handle: u16,
        pub device_type: u8,
        pub controller_number: u8,
        pub device_features: u32,
        pub device_name: [std::os::raw::c_char; 33],
        pub device_id: u32,
        pub channel_condition: u32,
    }
}

#[derive(Clone, Debug)]
pub struct PcanChannelInfo {
    pub channel_index: u32,
    pub channel_name: String,
    pub device_name: String,
    pub device_id: u32,
    pub fd_capable: bool,
    pub channel_condition: u32,
}

#[derive(Clone, Debug)]
pub struct ZcanUsbChannelInfo {
    pub device_type: String,
    pub hardware_label: String,
    pub serial_number: String,
    pub device_index: u32,
    pub channel_index: u32,
    pub fd_capable: bool,
}

fn pcan_channel_index(handle: u16) -> Option<u32> {
    match handle {
        0x51..=0x58 => Some((handle - 0x51) as u32),
        0x509..=0x510 => Some((handle - 0x509 + 8) as u32),
        _ => None,
    }
}

fn pcan_channel_name(index: u32) -> String {
    format!("PCAN_USBBUS{}", index + 1)
}

fn pcan_usb_channel(index: u32) -> Option<u16> {
    match index {
        0 => Some(0x51),
        1 => Some(0x52),
        2 => Some(0x53),
        3 => Some(0x54),
        4 => Some(0x55),
        5 => Some(0x56),
        6 => Some(0x57),
        7 => Some(0x58),
        8 => Some(0x509),
        9 => Some(0x50A),
        10 => Some(0x50B),
        11 => Some(0x50C),
        12 => Some(0x50D),
        13 => Some(0x50E),
        14 => Some(0x50F),
        15 => Some(0x510),
        _ => None,
    }
}

fn pcan_device_name(raw: &[std::os::raw::c_char; 33]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .copied()
        .take_while(|&c| c != 0)
        .map(|c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

pub fn pcan_attached_channels() -> Vec<PcanChannelInfo> {
    use pcan_ffi::*;

    let Ok(lib) = (unsafe { libloading::Library::new("PCANBasic.dll") }) else {
        return Vec::new();
    };
    let Ok(get_value) = (unsafe { lib.get::<FnGetValue>(b"CAN_GetValue\0") }) else {
        return Vec::new();
    };

    let mut count = 0u32;
    let status = unsafe {
        get_value(
            PCAN_NONEBUS,
            PCAN_ATTACHED_CHANNELS_COUNT,
            (&mut count as *mut u32).cast::<std::ffi::c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if status != PCAN_ERROR_OK || count == 0 {
        return Vec::new();
    }

    let empty = TPCANChannelInformation {
        channel_handle: 0,
        device_type: 0,
        controller_number: 0,
        device_features: 0,
        device_name: [0; 33],
        device_id: 0,
        channel_condition: 0,
    };
    let mut raw = vec![empty; count as usize];
    let status = unsafe {
        get_value(
            PCAN_NONEBUS,
            PCAN_ATTACHED_CHANNELS,
            raw.as_mut_ptr().cast::<std::ffi::c_void>(),
            (raw.len() * std::mem::size_of::<TPCANChannelInformation>()) as u32,
        )
    };
    if status != PCAN_ERROR_OK {
        return Vec::new();
    }

    raw.into_iter()
        .filter_map(|info| {
            let channel_index = pcan_channel_index(info.channel_handle)?;
            let mut device_name = pcan_device_name(&info.device_name);
            let fd_capable = info.device_features & PCAN_FEATURE_FD_CAPABLE != 0;
            if device_name.is_empty() {
                device_name = if fd_capable {
                    "PCAN-USB FD".to_string()
                } else {
                    "PCAN-USB".to_string()
                };
            }
            Some(PcanChannelInfo {
                channel_index,
                channel_name: pcan_channel_name(channel_index),
                device_name,
                device_id: info.device_id,
                fd_capable,
                channel_condition: info.channel_condition,
            })
        })
        .collect()
}

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

fn pcan_fd_bitrate(arb: &str, data: &str) -> Result<String, String> {
    let a = normalize_baud(arb);
    if a.contains("F_CLOCK") || a.contains("NOM_BRP") {
        return Ok(arb.to_string());
    }
    let nom = match a.as_str() {
        "1M" | "1000K" => "nom_brp=2,nom_tseg1=31,nom_tseg2=8,nom_sjw=8",
        "800K" => "nom_brp=5,nom_tseg1=15,nom_tseg2=4,nom_sjw=4",
        "500K" => "nom_brp=2,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        "250K" => "nom_brp=4,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        "125K" => "nom_brp=8,nom_tseg1=63,nom_tseg2=16,nom_sjw=16",
        other => {
            return Err(format!(
                "CAN FD 不支持的仲裁速率: {other}（支持 1M/500K/250K/125K，或直接填完整 f_clock 串）"
            ));
        }
    };
    let dat = match normalize_baud(data).as_str() {
        "8M" | "8000K" => "data_brp=1,data_tseg1=7,data_tseg2=2,data_sjw=2",
        "5M" | "5000K" => "data_brp=1,data_tseg1=12,data_tseg2=3,data_sjw=3",
        "4M" | "4000K" => "data_brp=1,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "2M" | "2000K" => "data_brp=2,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "1M" | "1000K" => "data_brp=4,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "800K" => "data_brp=5,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "500K" => "data_brp=8,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "250K" => "data_brp=16,data_tseg1=15,data_tseg2=4,data_sjw=4",
        "125K" => "data_brp=32,data_tseg1=15,data_tseg2=4,data_sjw=4",
        other => {
            return Err(format!(
                "CAN FD 不支持的数据速率: {other}（支持 8M/5M/4M/2M/1M/500K，或直接填完整 f_clock 串）"
            ));
        }
    };
    Ok(format!("f_clock=80000000,{nom},{dat}"))
}

fn pcan_poll_error(status: u32) -> PollReport {
    use pcan_ffi::*;
    let receive_overruns = u64::from(status & (PCAN_ERROR_QOVERRUN | PCAN_ERROR_OVERRUN) != 0);
    let bus_state = if status & PCAN_ERROR_BUSOFF != 0 {
        " bus-off"
    } else if status & PCAN_ERROR_BUSHEAVY != 0 {
        " bus-heavy"
    } else if status & PCAN_ERROR_BUSLIGHT != 0 {
        " bus-light"
    } else {
        ""
    };
    PollReport {
        receive_overruns,
        driver_errors: 1,
        connection_lost: status == PCAN_ERROR_ILLHW
            || status & PCAN_ERROR_NODRIVER != 0
            || status & PCAN_ERROR_INITIALIZE != 0,
        message: Some(format!("PCAN 接收状态 0x{status:08X}{bus_state}")),
    }
}

pub struct PcanBus {
    lib: libloading::Library,
    channel: u16,
    is_fd: bool,
    start: Instant,
    timestamp: HardwareTimebase,
    name: String,
}

impl PcanBus {
    pub fn open(start: Instant) -> Result<Self, String> {
        Self::open_config(start, 0, "500K")
    }

    pub fn open_config(start: Instant, channel_index: u32, baud: &str) -> Result<Self, String> {
        use pcan_ffi::*;
        let Some(channel) = pcan_usb_channel(channel_index) else {
            return Err(format!(
                "PCAN only supports configured USB channel index 0..15, got {channel_index}"
            ));
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
            drop(init);
            let use_fd_api = if status == PCAN_ERROR_OK {
                false
            } else if status == PCAN_ERROR_ILLHW {
                // Some PCAN-USB FD driver versions expose an attached channel
                // but reject the classic initialization entry point.  The FD
                // API can still run that channel at the requested arbitration
                // bitrate and carry ordinary CAN 2.0 frames (without the FD
                // message flag), so fall back transparently.
                let bitrate = pcan_fd_bitrate(baud, baud)?;
                let bitrate_c = std::ffi::CString::new(bitrate.clone()).unwrap();
                let init_fd: libloading::Symbol<FnInitFd> = lib
                    .get(b"CAN_InitializeFD\0")
                    .map_err(|e| format!("CAN_InitializeFD 未找到: {e}"))?;
                let fd_status = init_fd(channel, bitrate_c.as_ptr());
                drop(init_fd);
                if fd_status != PCAN_ERROR_OK {
                    return Err(format!(
                        "CAN_Initialize 失败 0x{status:08X}，FD API 回退也失败 0x{fd_status:08X}"
                    ));
                }
                true
            } else {
                return Err(format!(
                    "CAN_Initialize 失败, status=0x{status:08X}（设备未连接、通道被其他程序占用或 PEAK 驱动不可用；请关闭其他 CAN 工具后重试）"
                ));
            };
            Ok(Self {
                lib,
                channel,
                is_fd: use_fd_api,
                start,
                timestamp: HardwareTimebase::new(1e-6, None),
                name: format!("PCAN_USBBUS{} @{}", channel_index + 1, normalize_baud(baud)),
            })
        }
    }

    pub fn open_cfg(start: Instant, cfg: &DeviceConfig) -> Result<Self, String> {
        use pcan_ffi::*;
        if !cfg.is_fd {
            // On PCAN-USB FD hardware, the legacy CAN_Initialize timing
            // presets can use a different sample point from modern ZLG
            // adapters at the same nominal bitrate. Initialize the FD-capable
            // controller through CAN_InitializeFD with equal nominal/data
            // rates, while still transmitting ordinary CAN 2.0 frames unless
            // the frame itself carries MSGTYPE_FD.
            let fd_capable = pcan_attached_channels().into_iter().any(|channel| {
                channel.channel_index == cfg.channel_index && channel.fd_capable
            });
            if fd_capable {
                let mut classic_via_fd = cfg.clone();
                classic_via_fd.is_fd = true;
                classic_via_fd.data_baud = cfg.baud.clone();
                classic_via_fd.custom_bitrate.clear();
                let mut bus = Self::open_cfg(start, &classic_via_fd)?;
                bus.name = format!(
                    "PCAN_USBBUS{} @{} (FD API, Classical CAN)",
                    cfg.channel_index + 1,
                    normalize_baud(&cfg.baud)
                );
                return Ok(bus);
            }
            return Self::open_config(start, cfg.channel_index, &cfg.baud);
        }
        let Some(channel) = pcan_usb_channel(cfg.channel_index) else {
            return Err(format!(
                "PCAN only supports configured USB channel index 0..15, got {}",
                cfg.channel_index
            ));
        };
        let bitrate = if cfg.custom_bitrate.trim().is_empty() {
            pcan_fd_bitrate(&cfg.baud, &cfg.data_baud)?
        } else {
            pcan_fd_bitrate(cfg.custom_bitrate.trim(), &cfg.data_baud)?
        };
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
                timestamp: HardwareTimebase::new(1e-6, None),
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
    fn poll(&mut self, out: &mut Vec<CanFrame>) -> PollReport {
        use pcan_ffi::*;
        let mut report = PollReport::default();
        unsafe {
            if self.is_fd {
                let read_fd: libloading::Symbol<FnReadFd> = match self.lib.get(b"CAN_ReadFD\0") {
                    Ok(s) => s,
                    Err(error) => {
                        return PollReport {
                            driver_errors: 1,
                            connection_lost: true,
                            message: Some(format!("找不到 CAN_ReadFD: {error}")),
                            ..Default::default()
                        };
                    }
                };
                for _ in 0..512 {
                    let mut msg = TPCANMsgFD {
                        id: 0,
                        msgtype: 0,
                        dlc: 0,
                        data: [0; 64],
                    };
                    let mut ts: u64 = 0;
                    let st = read_fd(self.channel, &mut msg, &mut ts);
                    if st == PCAN_ERROR_QRCVEMPTY {
                        break;
                    }
                    if st != PCAN_ERROR_OK {
                        report = pcan_poll_error(st);
                        break;
                    }
                    if msg.msgtype & MSGTYPE_STATUS != 0 {
                        continue;
                    }
                    let is_fd = msg.msgtype & MSGTYPE_FD != 0;
                    let len = if is_fd {
                        fd_dlc_to_len(msg.dlc)
                    } else {
                        (msg.dlc as usize).min(8)
                    };
                    out.push(CanFrame {
                        t: self.timestamp.map(ts, self.start.elapsed().as_secs_f64()),
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
                return report;
            }
            let read: libloading::Symbol<FnRead> = match self.lib.get(b"CAN_Read\0") {
                Ok(s) => s,
                Err(error) => {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some(format!("找不到 CAN_Read: {error}")),
                        ..Default::default()
                    };
                }
            };
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
                    report = pcan_poll_error(st);
                    break;
                }
                if msg.msgtype & MSGTYPE_STATUS != 0 {
                    continue;
                }
                let len = (msg.len as usize).min(8);
                let timestamp_micros = ((ts.millis_overflow as u64) << 32)
                    .saturating_add(ts.millis as u64)
                    .saturating_mul(1_000)
                    .saturating_add(ts.micros as u64);
                out.push(CanFrame {
                    t: self
                        .timestamp
                        .map(timestamp_micros, self.start.elapsed().as_secs_f64()),
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
        report
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use pcan_ffi::*;
        if self.is_fd {
            unsafe {
                let write: libloading::Symbol<FnWriteFd> = self
                    .lib
                    .get(b"CAN_WriteFD\0")
                    .map_err(|e| format!("找不到 CAN_WriteFD: {e}"))?;
                let len = f.data.len().min(64);
                let mut data = [0u8; 64];
                data[..len].copy_from_slice(&f.data[..len]);
                let send_fd = f.fd || len > 8;
                let mut msgtype = if f.ext {
                    MSGTYPE_EXTENDED
                } else {
                    MSGTYPE_STANDARD
                };
                if send_fd {
                    msgtype |= MSGTYPE_FD;
                    if f.brs {
                        msgtype |= MSGTYPE_BRS;
                    }
                }
                if f.remote {
                    msgtype |= MSGTYPE_RTR;
                }
                let msg = TPCANMsgFD {
                    id: f.id,
                    msgtype,
                    dlc: fd_len_to_dlc(len),
                    data,
                };
                let st = write(self.channel, &msg);
                if st != PCAN_ERROR_OK {
                    return Err(format!("CAN_WriteFD 失败 status=0x{st:08X}"));
                }
            }
            return Ok(());
        }
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
                msgtype |= MSGTYPE_RTR;
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

#[allow(non_camel_case_types)]
mod zlg_ffi {
    pub type FnOpenDevice = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnCloseDevice = unsafe extern "system" fn(u32, u32) -> u32;
    pub type FnReadBoardInfo = unsafe extern "system" fn(u32, u32, *mut VCI_BOARD_INFO) -> u32;
    pub type FnInitCan = unsafe extern "system" fn(u32, u32, u32, *mut VCI_INIT_CONFIG) -> u32;
    pub type FnStartCan = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnResetCan = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnClearBuffer = unsafe extern "system" fn(u32, u32, u32) -> u32;
    pub type FnReadCanStatus = unsafe extern "system" fn(u32, u32, u32, *mut VCI_CAN_STATUS) -> u32;
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

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct VCI_CAN_STATUS {
        pub err_interrupt: u8,
        pub reg_mode: u8,
        pub reg_status: u8,
        pub reg_al_capture: u8,
        pub reg_ec_capture: u8,
        pub reg_ew_limit: u8,
        pub reg_re_counter: u8,
        pub reg_te_counter: u8,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct VCI_BOARD_INFO {
        pub hw_version: u16,
        pub fw_version: u16,
        pub driver_version: u16,
        pub interface_version: u16,
        pub irq_num: u16,
        pub can_num: u8,
        pub serial_number: [u8; 20],
        pub hardware_type: [u8; 40],
        pub reserved: [u16; 4],
    }

    impl Default for VCI_BOARD_INFO {
        fn default() -> Self {
            Self {
                hw_version: 0,
                fw_version: 0,
                driver_version: 0,
                interface_version: 0,
                irq_num: 0,
                can_num: 0,
                serial_number: [0; 20],
                hardware_type: [0; 40],
                reserved: [0; 4],
            }
        }
    }
}

#[cfg(windows)]
mod win_pnp_ffi {
    use std::ffi::c_void;

    pub const DIGCF_PRESENT: u32 = 0x0000_0002;
    pub const DIGCF_ALLCLASSES: u32 = 0x0000_0004;

    #[repr(C)]
    pub struct SP_DEVINFO_DATA {
        pub cb_size: u32,
        pub class_guid: [u8; 16],
        pub dev_inst: u32,
        pub reserved: usize,
    }

    #[link(name = "setupapi")]
    unsafe extern "system" {
        pub fn SetupDiGetClassDevsW(
            class_guid: *const c_void,
            enumerator: *const u16,
            hwnd_parent: *mut c_void,
            flags: u32,
        ) -> *mut c_void;
        pub fn SetupDiEnumDeviceInfo(
            device_info_set: *mut c_void,
            member_index: u32,
            device_info_data: *mut SP_DEVINFO_DATA,
        ) -> i32;
        pub fn SetupDiGetDeviceInstanceIdW(
            device_info_set: *mut c_void,
            device_info_data: *mut SP_DEVINFO_DATA,
            device_instance_id: *mut u16,
            device_instance_id_size: u32,
            required_size: *mut u32,
        ) -> i32;
        pub fn SetupDiDestroyDeviceInfoList(device_info_set: *mut c_void) -> i32;
    }
}

#[cfg(windows)]
fn windows_pnp_device_present(instance_prefix: &str) -> Option<bool> {
    use std::ffi::c_void;
    use win_pnp_ffi::*;

    unsafe {
        let devices = SetupDiGetClassDevsW(
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        );
        if devices == (-1isize) as *mut c_void {
            return None;
        }
        let expected = instance_prefix.to_ascii_uppercase();
        let mut found = false;
        for index in 0..4096 {
            let mut info = SP_DEVINFO_DATA {
                cb_size: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                class_guid: [0; 16],
                dev_inst: 0,
                reserved: 0,
            };
            if SetupDiEnumDeviceInfo(devices, index, &mut info) == 0 {
                break;
            }
            let mut buffer = [0u16; 512];
            let mut required = 0u32;
            if SetupDiGetDeviceInstanceIdW(
                devices,
                &mut info,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut required,
            ) == 0
            {
                continue;
            }
            let length = buffer
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(buffer.len());
            let instance_id = String::from_utf16_lossy(&buffer[..length]).to_ascii_uppercase();
            if instance_id.starts_with(&expected) {
                found = true;
                break;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(devices);
        Some(found)
    }
}

#[cfg(not(windows))]
fn windows_pnp_device_present(_instance_prefix: &str) -> Option<bool> {
    None
}

struct VciDevice {
    lib: libloading::Library,
    device_type: u32,
    device_index: u32,
    prefix: &'static str,
}

impl VciDevice {
    fn sym(&self, name: &str) -> Vec<u8> {
        let mut value = format!("{}{name}", self.prefix).into_bytes();
        value.push(0);
        value
    }
}

impl Drop for VciDevice {
    fn drop(&mut self) {
        unsafe {
            if let Ok(close) = self
                .lib
                .get::<zlg_ffi::FnCloseDevice>(&*self.sym("CloseDevice"))
            {
                let _ = close(self.device_type, self.device_index);
            }
        }
    }
}

type VciDeviceRegistry = HashMap<String, Arc<VciDevice>>;

fn vci_device_registry() -> &'static Mutex<VciDeviceRegistry> {
    static DEVICES: std::sync::OnceLock<Mutex<VciDeviceRegistry>> = std::sync::OnceLock::new();
    DEVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn vci_device_key(
    dll_candidates: &[&str],
    prefix: &str,
    device_type: u32,
    device_index: u32,
) -> String {
    format!(
        "{}|{prefix}|{device_type}|{device_index}",
        dll_candidates.first().copied().unwrap_or_default()
    )
}

fn get_or_open_vci_device(
    dll_candidates: &[&str],
    prefix: &'static str,
    device_type: u32,
    device_index: u32,
) -> Result<(String, Arc<VciDevice>), String> {
    let key = vci_device_key(dll_candidates, prefix, device_type, device_index);
    let mut registry = vci_device_registry()
        .lock()
        .map_err(|_| "VCI 设备缓存已损坏".to_string())?;
    if let Some(device) = registry.get(&key) {
        return Ok((key, device.clone()));
    }
    unsafe {
        let mut loaded = None;
        let mut last_error = String::new();
        for candidate in dll_candidates {
            match libloading::Library::new(*candidate) {
                Ok(library) => {
                    loaded = Some(library);
                    break;
                }
                Err(error) => last_error = format!("加载 {candidate} 失败: {error}"),
            }
        }
        let lib = loaded.ok_or(last_error)?;
        let mut symbol = format!("{prefix}OpenDevice").into_bytes();
        symbol.push(0);
        let open = lib
            .get::<zlg_ffi::FnOpenDevice>(&*symbol)
            .map_err(|error| format!("{prefix}OpenDevice 未找到: {error}"))?;
        if open(device_type, device_index, 0) != 1 {
            return Err(format!("{prefix}OpenDevice 失败（检查设备/驱动）"));
        }
        drop(open);
        let device = Arc::new(VciDevice {
            lib,
            device_type,
            device_index,
            prefix,
        });
        registry.insert(key.clone(), device.clone());
        Ok((key, device))
    }
}

fn evict_vci_device(key: &str, expected: &Arc<VciDevice>) {
    if let Ok(mut registry) = vci_device_registry().lock()
        && registry
            .get(key)
            .is_some_and(|cached| Arc::ptr_eq(cached, expected))
    {
        registry.remove(key);
    }
}

fn clear_vci_device_registry() {
    if let Ok(mut registry) = vci_device_registry().lock() {
        registry.clear();
    }
}

pub struct VciBus {
    device: Arc<VciDevice>,
    channel_index: u32,
    timing0: u8,
    timing1: u8,
    listen_only: bool,
    start: Instant,
    last_health_check: Instant,
    last_physical_check: Instant,
    last_busoff_recovery: Option<Instant>,
    consecutive_send_failures: u8,
    name: String,
}

impl VciBus {
    fn open(
        start: Instant,
        cfg: &DeviceConfig,
        dll_candidates: &[&str],
        prefix: &'static str,
        device_type: u32,
    ) -> Result<Self, String> {
        use zlg_ffi::*;
        let (timing0, timing1) =
            zlg_timing(&cfg.baud).ok_or_else(|| format!("不支持的波特率: {}", cfg.baud))?;
        unsafe {
            let (key, device) =
                get_or_open_vci_device(dll_candidates, prefix, device_type, cfg.device_index)?;
            let setup = (|| -> Result<(), String> {
                let init = device
                    .lib
                    .get::<FnInitCan>(device.sym("InitCAN").as_slice())
                    .map_err(|error| format!("{prefix}InitCAN 未找到: {error}"))?;
                let mut init_cfg = VCI_INIT_CONFIG {
                    acc_code: 0,
                    acc_mask: 0xFFFF_FFFF,
                    reserved: 0,
                    filter: 1,
                    timing0,
                    timing1,
                    mode: u8::from(cfg.listen_only),
                };
                if init(
                    device_type,
                    cfg.device_index,
                    cfg.channel_index,
                    &mut init_cfg,
                ) != 1
                {
                    return Err(format!("{prefix}InitCAN 失败"));
                }
                drop(init);
                let start_can = device
                    .lib
                    .get::<FnStartCan>(device.sym("StartCAN").as_slice())
                    .map_err(|error| format!("{prefix}StartCAN 未找到: {error}"))?;
                if start_can(device_type, cfg.device_index, cfg.channel_index) != 1 {
                    return Err(format!("{prefix}StartCAN 失败"));
                }
                drop(start_can);
                if let Ok(clear) = device
                    .lib
                    .get::<FnClearBuffer>(device.sym("ClearBuffer").as_slice())
                {
                    let _ = clear(device_type, cfg.device_index, cfg.channel_index);
                }
                Ok(())
            })();
            if let Err(error) = setup {
                evict_vci_device(&key, &device);
                return Err(error);
            }
            Ok(Self {
                device,
                channel_index: cfg.channel_index,
                timing0,
                timing1,
                listen_only: cfg.listen_only,
                start,
                last_health_check: Instant::now() - Duration::from_secs(1),
                last_physical_check: Instant::now() - Duration::from_secs(1),
                last_busoff_recovery: None,
                consecutive_send_failures: 0,
                name: format!(
                    "{} dev{} CAN{} @{}",
                    cfg.device_type,
                    cfg.device_index,
                    cfg.channel_index,
                    normalize_baud(&cfg.baud)
                ),
            })
        }
    }

    fn sym(&self, n: &str) -> Vec<u8> {
        self.device.sym(n)
    }

    fn channel_status(&self) -> Result<Option<zlg_ffi::VCI_CAN_STATUS>, String> {
        unsafe {
            let Ok(read_status) = self
                .device
                .lib
                .get::<zlg_ffi::FnReadCanStatus>(self.sym("ReadCANStatus").as_slice())
            else {
                return Ok(None);
            };
            let mut status = zlg_ffi::VCI_CAN_STATUS::default();
            if read_status(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
                &mut status,
            ) != 1
            {
                return Err(format!("{}ReadCANStatus 失败", self.device.prefix));
            }
            Ok(Some(status))
        }
    }

    fn device_present(&self) -> Result<(), String> {
        unsafe {
            let read_board_info = self
                .device
                .lib
                .get::<zlg_ffi::FnReadBoardInfo>(self.sym("ReadBoardInfo").as_slice())
                .map_err(|error| format!("{}ReadBoardInfo 未找到: {error}", self.device.prefix))?;
            let mut info = zlg_ffi::VCI_BOARD_INFO::default();
            if read_board_info(self.device.device_type, self.device.device_index, &mut info) != 1 {
                return Err(format!(
                    "{}设备物理连接已丢失（ReadBoardInfo 失败）",
                    self.device.prefix
                ));
            }
            Ok(())
        }
    }

    fn recover_bus_off(&mut self) -> Result<(), String> {
        use zlg_ffi::*;
        unsafe {
            let reset = self
                .device
                .lib
                .get::<FnResetCan>(self.sym("ResetCAN").as_slice())
                .map_err(|error| format!("{}ResetCAN 未找到: {error}", self.device.prefix))?;
            if reset(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
            ) != 1
            {
                return Err(format!("{}ResetCAN 失败", self.device.prefix));
            }
            drop(reset);

            let mut init_cfg = VCI_INIT_CONFIG {
                acc_code: 0,
                acc_mask: 0xFFFF_FFFF,
                reserved: 0,
                filter: 1,
                timing0: self.timing0,
                timing1: self.timing1,
                mode: u8::from(self.listen_only),
            };
            let init = self
                .device
                .lib
                .get::<FnInitCan>(self.sym("InitCAN").as_slice())
                .map_err(|error| format!("{}InitCAN 未找到: {error}", self.device.prefix))?;
            if init(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
                &mut init_cfg,
            ) != 1
            {
                return Err(format!("{}InitCAN 恢复失败", self.device.prefix));
            }
            drop(init);

            let start = self
                .device
                .lib
                .get::<FnStartCan>(self.sym("StartCAN").as_slice())
                .map_err(|error| format!("{}StartCAN 未找到: {error}", self.device.prefix))?;
            if start(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
            ) != 1
            {
                return Err(format!("{}StartCAN 恢复失败", self.device.prefix));
            }
            drop(start);
            if let Ok(clear) = self
                .device
                .lib
                .get::<FnClearBuffer>(self.sym("ClearBuffer").as_slice())
            {
                let _ = clear(
                    self.device.device_type,
                    self.device.device_index,
                    self.channel_index,
                );
            }
        }
        Ok(())
    }
}

impl Drop for VciBus {
    fn drop(&mut self) {
        unsafe {
            if let Ok(reset) = self
                .device
                .lib
                .get::<zlg_ffi::FnResetCan>(self.sym("ResetCAN").as_slice())
            {
                let _ = reset(
                    self.device.device_type,
                    self.device.device_index,
                    self.channel_index,
                );
            }
        }
    }
}

impl CanAdapter for VciBus {
    fn poll(&mut self, out: &mut Vec<CanFrame>) -> PollReport {
        use zlg_ffi::*;
        unsafe {
            let recv: libloading::Symbol<FnReceive> =
                match self.device.lib.get(self.sym("Receive").as_slice()) {
                    Ok(s) => s,
                    Err(error) => {
                        return PollReport {
                            driver_errors: 1,
                            connection_lost: true,
                            message: Some(format!("{}Receive 未找到: {error}", self.device.prefix)),
                            ..Default::default()
                        };
                    }
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
            let received = recv(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
                frames.as_mut_ptr(),
                frames.len() as u32,
                0,
            );
            if received == u32::MAX {
                return PollReport {
                    driver_errors: 1,
                    connection_lost: true,
                    message: Some(format!("{}Receive 返回驱动错误", self.device.prefix)),
                    ..Default::default()
                };
            }
            let n = received.min(frames.len() as u32);
            for msg in frames.iter().take(n as usize) {
                let len = (msg.data_len as usize).min(8);
                // Both official legacy VCI headers expose TimeStamp/TimeFlag but
                // specify no clock unit. The attached GCAN and CANalyst-II drivers
                // returned different undocumented counter rates, so a common
                // 0.1 ms conversion produces invalid elapsed times.
                let timestamp = self.start.elapsed().as_secs_f64();
                out.push(CanFrame {
                    t: timestamp,
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
        if self.last_health_check.elapsed() < Duration::from_millis(250) {
            return PollReport::default();
        }
        self.last_health_check = Instant::now();
        if self.device.prefix.is_empty()
            && self.last_physical_check.elapsed() >= Duration::from_secs(1)
        {
            self.last_physical_check = Instant::now();
            if windows_pnp_device_present("USB\\VID_0C66&PID_000C\\") == Some(false) {
                return PollReport {
                    driver_errors: 1,
                    connection_lost: true,
                    message: Some("GCAN USB 设备已从 Windows PnP 设备树移除".into()),
                    ..Default::default()
                };
            }
        }
        if self.consecutive_send_failures >= 10 {
            match self.channel_status() {
                Ok(Some(status))
                    if status.reg_status == 0
                        && status.reg_re_counter == 0
                        && status.reg_te_counter == 0 =>
                {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some(format!(
                            "{}设备连续发送失败且状态寄存器无响应，判定 USB 句柄已失效",
                            self.device.prefix
                        )),
                        ..Default::default()
                    };
                }
                Err(error) => {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some(error),
                        ..Default::default()
                    };
                }
                _ => {}
            }
        }
        if let Err(error) = self.device_present() {
            return PollReport {
                driver_errors: 1,
                connection_lost: true,
                message: Some(error),
                ..Default::default()
            };
        }
        match self.channel_status() {
            Ok(Some(status)) => {
                let bus_off = status.reg_status & 0x80 != 0 || status.reg_te_counter == u8::MAX;
                let error_passive = status.reg_re_counter >= 128 || status.reg_te_counter >= 128;
                // CANalyst-II's ControlCAN driver was observed to stop at an
                // error counter of 135 while repeatedly rejecting transmission,
                // without ever exposing the SJA1000 Bus-Off status bit. Treat
                // that documented error-passive boundary as Bus-Off-equivalent
                // only when the transmit path also failed repeatedly.
                let recoverable_fault =
                    bus_off || (error_passive && self.consecutive_send_failures >= 10);
                let error_warning = status.reg_status & 0x40 != 0
                    || error_passive
                    || (status.reg_ew_limit != 0
                        && (status.reg_re_counter >= status.reg_ew_limit
                            || status.reg_te_counter >= status.reg_ew_limit));
                if recoverable_fault {
                    let can_recover = self
                        .last_busoff_recovery
                        .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
                    if can_recover {
                        self.last_busoff_recovery = Some(Instant::now());
                        let recovery = self.recover_bus_off();
                        if recovery.is_ok() {
                            self.consecutive_send_failures = 0;
                        }
                        let fault_name = if bus_off {
                            "Bus-Off"
                        } else {
                            "Bus-Off 等效错误被动（驱动未上报 Bus-Off 位）"
                        };
                        return match recovery {
                            Ok(()) => PollReport {
                                driver_errors: 1,
                                message: Some(format!(
                                    "{}CAN{} {fault_name}，已自动复位恢复（REC={} TEC={}）",
                                    self.device.prefix,
                                    self.channel_index + 1,
                                    status.reg_re_counter,
                                    status.reg_te_counter
                                )),
                                ..Default::default()
                            },
                            Err(error) => PollReport {
                                driver_errors: 1,
                                connection_lost: true,
                                message: Some(format!(
                                    "{}CAN{} {fault_name}恢复失败: {error}",
                                    self.device.prefix,
                                    self.channel_index + 1
                                )),
                                ..Default::default()
                            },
                        };
                    }
                } else if error_warning {
                    return PollReport {
                        driver_errors: 1,
                        message: Some(format!(
                            "{}CAN{} 总线错误警告（SR=0x{:02X} REC={} TEC={}）",
                            self.device.prefix,
                            self.channel_index + 1,
                            status.reg_status,
                            status.reg_re_counter,
                            status.reg_te_counter
                        )),
                        ..Default::default()
                    };
                }
                PollReport::default()
            }
            Ok(None) => PollReport::default(),
            Err(error) => PollReport {
                driver_errors: 1,
                connection_lost: true,
                message: Some(error),
                ..Default::default()
            },
        }
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use zlg_ffi::*;
        if self.listen_only {
            return Err("监听模式禁止发送 CAN 报文".into());
        }
        if f.fd || f.data.len() > 8 {
            return Err("当前 VCI 适配器不支持 CAN FD 发送".into());
        }
        unsafe {
            let transmit: libloading::Symbol<FnTransmit> = self
                .device
                .lib
                .get(self.sym("Transmit").as_slice())
                .map_err(|e| format!("{}Transmit 未找到: {e}", self.device.prefix))?;
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
            let transmitted = transmit(
                self.device.device_type,
                self.device.device_index,
                self.channel_index,
                &mut msg,
                1,
            );
            drop(transmit);
            if transmitted != 1 {
                self.consecutive_send_failures = self.consecutive_send_failures.saturating_add(1);
                let status = self.channel_status().ok().flatten();
                return Err(if let Some(status) = status {
                    format!(
                        "{}Transmit 失败（SR=0x{:02X} REC={} TEC={}）",
                        self.device.prefix,
                        status.reg_status,
                        status.reg_re_counter,
                        status.reg_te_counter
                    )
                } else {
                    format!("{}Transmit 失败", self.device.prefix)
                });
            }
            self.consecutive_send_failures = 0;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZcanDeviceFamily {
    UsbClassic,
    UsbCanFd,
    NetworkTcp,
    NetworkUdp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ZcanDeviceProfile {
    device_type: u32,
    family: ZcanDeviceFamily,
    fd_capable: bool,
}

impl ZcanDeviceProfile {
    fn is_network(self) -> bool {
        matches!(
            self.family,
            ZcanDeviceFamily::NetworkTcp | ZcanDeviceFamily::NetworkUdp
        )
    }

    fn is_tcp(self) -> bool {
        self.family == ZcanDeviceFamily::NetworkTcp
    }
}

fn zcan_driver_channel_type(profile: ZcanDeviceProfile, frame_fd_enabled: bool) -> u8 {
    if profile.family == ZcanDeviceFamily::UsbCanFd || frame_fd_enabled {
        zcan_ffi::TYPE_CANFD
    } else {
        zcan_ffi::TYPE_CAN
    }
}

fn load_zlg_library(relative_path: &str) -> Result<libloading::Library, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("无法定位程序目录: {error}"))?;
    let path = executable
        .parent()
        .ok_or_else(|| "无法定位程序目录".to_string())?
        .join(relative_path);
    unsafe { libloading::Library::new(&path) }
        .map_err(|error| format!("加载 {} 失败: {error}", path.display()))
}

fn pinned_zlgcan_library() -> Result<&'static libloading::Library, String> {
    static LIBRARY: std::sync::OnceLock<Result<libloading::Library, String>> =
        std::sync::OnceLock::new();
    match LIBRARY.get_or_init(|| load_zlg_library("zlgcan.dll")) {
        Ok(library) => Ok(library),
        Err(error) => Err(error.clone()),
    }
}

fn pin_zlg_kernel_library(profile: ZcanDeviceProfile) -> Result<(), String> {
    static USB_CLASSIC_LEGACY: std::sync::OnceLock<Result<libloading::Library, String>> =
        std::sync::OnceLock::new();
    static USB_CLASSIC: std::sync::OnceLock<Result<libloading::Library, String>> =
        std::sync::OnceLock::new();
    static USB_CAN_FD: std::sync::OnceLock<Result<libloading::Library, String>> =
        std::sync::OnceLock::new();
    static USB_CAN_FD_800: std::sync::OnceLock<Result<libloading::Library, String>> =
        std::sync::OnceLock::new();
    let pinned = match profile.family {
        ZcanDeviceFamily::UsbClassic if matches!(profile.device_type, 3 | 4) => {
            USB_CLASSIC_LEGACY.get_or_init(|| load_zlg_library("kerneldlls/USBCAN.dll"))
        }
        ZcanDeviceFamily::UsbClassic => {
            USB_CLASSIC.get_or_init(|| load_zlg_library("kerneldlls/USBCAN_E_64.dll"))
        }
        ZcanDeviceFamily::UsbCanFd if profile.device_type == 59 => {
            USB_CAN_FD_800.get_or_init(|| load_zlg_library("kerneldlls/USBCANFD800U.dll"))
        }
        ZcanDeviceFamily::UsbCanFd => {
            USB_CAN_FD.get_or_init(|| load_zlg_library("kerneldlls/USBCANFD.dll"))
        }
        ZcanDeviceFamily::NetworkTcp | ZcanDeviceFamily::NetworkUdp => return Ok(()),
    };
    pinned.as_ref().map(|_| ()).map_err(Clone::clone)
}

fn zcan_profile(device_type: &str) -> Option<ZcanDeviceProfile> {
    match device_type
        .trim()
        .to_ascii_uppercase()
        .replace(' ', "")
        .as_str()
    {
        "USBCAN1" => Some(ZcanDeviceProfile {
            device_type: 3,
            family: ZcanDeviceFamily::UsbClassic,
            fd_capable: false,
        }),
        "USBCAN" | "USBCAN2" => Some(ZcanDeviceProfile {
            device_type: 4,
            family: ZcanDeviceFamily::UsbClassic,
            fd_capable: false,
        }),
        "USBCANFD" | "USBCANFD-200U" | "USBCANFD200U" => Some(ZcanDeviceProfile {
            device_type: 41,
            family: ZcanDeviceFamily::UsbCanFd,
            fd_capable: true,
        }),
        "USBCANFD-100U" | "USBCANFD100U" => Some(ZcanDeviceProfile {
            device_type: 42,
            family: ZcanDeviceFamily::UsbCanFd,
            fd_capable: true,
        }),
        "USBCANFD-MINI" | "USBCANFDMINI" => Some(ZcanDeviceProfile {
            device_type: 43,
            family: ZcanDeviceFamily::UsbCanFd,
            fd_capable: true,
        }),
        "USBCANFD-800U" | "USBCANFD800U" => Some(ZcanDeviceProfile {
            device_type: 59,
            family: ZcanDeviceFamily::UsbCanFd,
            fd_capable: true,
        }),
        "USBCAN-E-U" | "USBCANEU" => Some(ZcanDeviceProfile {
            device_type: 20,
            family: ZcanDeviceFamily::UsbClassic,
            fd_capable: false,
        }),
        "USBCAN-2E-U" | "USBCAN2EU" => Some(ZcanDeviceProfile {
            device_type: 21,
            family: ZcanDeviceFamily::UsbClassic,
            fd_capable: false,
        }),
        "CANFDNET" | "CANFDNET-TCP" | "CANFDNETTCP" => Some(ZcanDeviceProfile {
            device_type: 48,
            family: ZcanDeviceFamily::NetworkTcp,
            fd_capable: true,
        }),
        "CANFDNET-UDP" | "CANFDNETUDP" => Some(ZcanDeviceProfile {
            device_type: 49,
            family: ZcanDeviceFamily::NetworkUdp,
            fd_capable: true,
        }),
        "CANFDWIFI" | "CANFDWIFI-TCP" | "CANFDWIFITCP" => Some(ZcanDeviceProfile {
            device_type: 50,
            family: ZcanDeviceFamily::NetworkTcp,
            fd_capable: true,
        }),
        "CANFDWIFI-UDP" | "CANFDWIFIUDP" => Some(ZcanDeviceProfile {
            device_type: 51,
            family: ZcanDeviceFamily::NetworkUdp,
            fd_capable: true,
        }),
        _ => None,
    }
}

fn zcan_info_text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

pub fn zcan_attached_channels() -> Vec<ZcanUsbChannelInfo> {
    use zcan_ffi::*;

    let mut detected = Vec::new();
    // Device types in each family are aliases in the current ZLG driver. Open
    // one canonical type, then use ZCAN_GetDeviceInf to identify the actual
    // model and physical channel count.
    for requested_type in ["USBCANFD-200U", "USBCAN-E-U", "USBCAN1", "USBCAN2"] {
        let Some(profile) = zcan_profile(requested_type) else {
            continue;
        };
        if pin_zlg_kernel_library(profile).is_err() {
            continue;
        }
        let Ok(lib) = pinned_zlgcan_library() else {
            continue;
        };
        unsafe {
            let Ok(open) = lib.get::<FnOpenDevice>(b"ZCAN_OpenDevice\0") else {
                continue;
            };
            let Ok(close) = lib.get::<FnCloseDevice>(b"ZCAN_CloseDevice\0") else {
                continue;
            };
            let Ok(get_info) = lib.get::<FnGetDeviceInfo>(b"ZCAN_GetDeviceInf\0") else {
                continue;
            };
            for device_index in 0..8 {
                let handle = open(profile.device_type, device_index, 0);
                if handle.is_null() {
                    if device_index == 0 {
                        continue;
                    }
                    break;
                }
                let mut info = ZcanDeviceInfo {
                    hw_version: 0,
                    fw_version: 0,
                    driver_version: 0,
                    interface_version: 0,
                    irq_num: 0,
                    can_num: 0,
                    serial_number: [0; 20],
                    hardware_type: [0; 40],
                    reserved: [0; 4],
                };
                let info_ok = get_info(handle, &mut info) == 1;
                let _ = close(handle);
                if !info_ok {
                    continue;
                }
                let raw_hardware = zcan_info_text(&info.hardware_type);
                let upper = raw_hardware.to_ascii_uppercase();
                let fd_capable = profile.family == ZcanDeviceFamily::UsbCanFd;
                let device_type = if profile.device_type == 3 {
                    "USBCAN1"
                } else if profile.device_type == 4 {
                    "USBCAN2"
                } else if fd_capable {
                    if upper.contains("200U") {
                        "USBCANFD-200U"
                    } else if upper.contains("100U") {
                        "USBCANFD-100U"
                    } else if upper.contains("MINI") {
                        "USBCANFD-MINI"
                    } else if upper.contains("800U") {
                        "USBCANFD-800U"
                    } else {
                        "USBCANFD-200U"
                    }
                } else if info.can_num > 1 {
                    "USBCAN-2E-U"
                } else {
                    "USBCAN-E-U"
                };
                let serial_number = zcan_info_text(&info.serial_number);
                let channel_count = u32::from(info.can_num.max(1));
                for channel_index in 0..channel_count {
                    detected.push(ZcanUsbChannelInfo {
                        device_type: device_type.to_string(),
                        hardware_label: if raw_hardware.is_empty() {
                            device_type.to_string()
                        } else {
                            raw_hardware.clone()
                        },
                        serial_number: serial_number.clone(),
                        device_index,
                        channel_index,
                        fd_capable,
                    });
                }
            }
        }
    }
    // Some driver generations accept both legacy USBCAN1 and USBCAN2 type
    // codes for the same box. Keep one physical endpoint when the board serial
    // and channel identity coincide.
    let mut seen_zlg = std::collections::HashSet::new();
    detected.retain(|channel| {
        let identity = if channel.serial_number.is_empty() {
            format!(
                "{}:{}:{}",
                channel.device_type, channel.device_index, channel.channel_index
            )
        } else {
            format!("{}:{}", channel.serial_number, channel.channel_index)
        };
        seen_zlg.insert(identity)
    });
    detected.extend(legacy_vci_attached_channels());
    detected
}

fn legacy_vci_attached_channels() -> Vec<ZcanUsbChannelInfo> {
    let mut detected = Vec::new();
    detected.extend(probe_vci_devices(
        &["ECanVci64.dll", "ECanVci.dll"],
        "",
        "GCAN",
        "GCAN USBCAN-I",
        3,
        1,
    ));
    detected.extend(probe_vci_devices(
        &["ControlCAN.dll"],
        "VCI_",
        "ZHCX",
        "CANalyst-II",
        4,
        0,
    ));
    detected
}

fn probe_vci_devices(
    dll_candidates: &[&str],
    prefix: &'static str,
    device_name: &str,
    fallback_label: &str,
    device_type: u32,
    fixed_channel_count: u8,
) -> Vec<ZcanUsbChannelInfo> {
    use zlg_ffi::*;

    unsafe {
        let mut detected = Vec::new();
        let mut consecutive_misses = 0;
        for device_index in 0..8 {
            let Ok((key, device)) =
                get_or_open_vci_device(dll_candidates, prefix, device_type, device_index)
            else {
                consecutive_misses += 1;
                if consecutive_misses >= 2 {
                    break;
                }
                continue;
            };
            consecutive_misses = 0;
            let Ok(read_board_info) = device
                .lib
                .get::<FnReadBoardInfo>(device.sym("ReadBoardInfo").as_slice())
            else {
                continue;
            };
            let mut info = VCI_BOARD_INFO::default();
            let info_ok = read_board_info(device_type, device_index, &mut info) == 1;
            if !info_ok {
                drop(read_board_info);
                evict_vci_device(&key, &device);
                continue;
            }

            let serial_number = printable_vci_text(&info.serial_number);
            let reported_label = printable_vci_text(&info.hardware_type);
            let hardware_label = if device_name == "GCAN" || reported_label.is_empty() {
                fallback_label.to_string()
            } else {
                reported_label
            };
            let channel_count = if fixed_channel_count == 0 {
                info.can_num.clamp(1, 8)
            } else {
                fixed_channel_count
            };
            for channel_index in 0..u32::from(channel_count) {
                detected.push(ZcanUsbChannelInfo {
                    device_type: device_name.to_string(),
                    hardware_label: hardware_label.clone(),
                    serial_number: serial_number.clone(),
                    device_index,
                    channel_index,
                    fd_capable: false,
                });
            }
        }
        detected
    }
}

fn printable_vci_text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .copied()
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
        .map(char::from)
        .collect::<String>()
        .trim()
        .to_string()
}

fn baud_to_bps(s: &str) -> u32 {
    let b = s
        .trim()
        .to_ascii_uppercase()
        .replace(' ', "")
        .replace("BPS", "");
    if let Some(x) = b.strip_suffix('M') {
        return (x.parse::<f64>().unwrap_or(0.0) * 1_000_000.0) as u32;
    }
    if let Some(x) = b.strip_suffix('K') {
        return (x.parse::<f64>().unwrap_or(0.0) * 1_000.0) as u32;
    }
    b.parse::<u32>().unwrap_or(0)
}

fn adapter_key(cfg: &DeviceConfig) -> String {
    if !cfg.hardware_id.trim().is_empty() {
        return cfg.hardware_id.trim().to_ascii_uppercase();
    }
    let device = cfg
        .device_type
        .trim()
        .to_ascii_uppercase()
        .replace([' ', '_'], "");
    if zcan_profile(&cfg.device_type).is_some_and(ZcanDeviceProfile::is_network) {
        format!(
            "{device}:{}:{}:{}",
            cfg.ip.trim(),
            cfg.port.trim(),
            cfg.channel_index
        )
    } else {
        format!("{device}:{}:{}", cfg.device_index, cfg.channel_index)
    }
}

pub fn validate_device_config(cfg: &DeviceConfig) -> Result<(), String> {
    if cfg.sw_channel == 0 {
        return Err("软件 CAN 通道必须从 1 开始".into());
    }
    let device = cfg.device_type.trim().to_ascii_uppercase();
    if device.is_empty() {
        return Err(format!("CAN{} 未选择设备类型", cfg.sw_channel));
    }
    if matches!(device.as_str(), "VIRTUAL" | "SIM") {
        return Err(format!(
            "CAN{} 的虚拟总线已移除，请选择硬件适配器",
            cfg.sw_channel
        ));
    }

    if device == "PCAN" {
        if cfg.channel_index >= 16 {
            return Err(format!("CAN{} PCAN 硬件通道必须为 0..15", cfg.sw_channel));
        }
        if cfg.is_fd {
            if cfg.custom_bitrate.trim().is_empty() {
                pcan_fd_bitrate(&cfg.baud, &cfg.data_baud)?;
            } else {
                pcan_fd_bitrate(cfg.custom_bitrate.trim(), &cfg.data_baud)?;
            }
        } else if !matches!(
            normalize_baud(&cfg.baud).as_str(),
            "125K" | "250K" | "500K" | "1000K"
        ) {
            return Err(format!(
                "CAN{} PCAN 不支持波特率 {}",
                cfg.sw_channel, cfg.baud
            ));
        }
        if cfg.listen_only {
            return Err(format!(
                "CAN{} PCAN 监听模式尚未由当前后端实现",
                cfg.sw_channel
            ));
        }
        if cfg.fd_non_iso {
            return Err(format!(
                "CAN{} PCAN Non-ISO CAN FD 尚未由当前后端实现",
                cfg.sw_channel
            ));
        }
        return Ok(());
    }

    if matches!(device.as_str(), "GCAN" | "ZHCX" | "ZHCXCAN") {
        if !cfg.custom_bitrate.trim().is_empty() {
            return Err(format!(
                "CAN{} 的 {} 不支持 PCAN 自定义位时序串",
                cfg.sw_channel, cfg.device_type
            ));
        }
        if cfg.is_fd {
            return Err(format!(
                "CAN{} 的 {} 仅支持 Classical CAN",
                cfg.sw_channel, cfg.device_type
            ));
        }
        if cfg.fd_non_iso {
            return Err(format!(
                "CAN{} 的 {} 不支持 Non-ISO CAN FD",
                cfg.sw_channel, cfg.device_type
            ));
        }
        if zlg_timing(&cfg.baud).is_none() {
            return Err(format!(
                "CAN{} 的 {} 不支持波特率 {}",
                cfg.sw_channel, cfg.device_type, cfg.baud
            ));
        }
        return Ok(());
    }

    if let Some(profile) = zcan_profile(&cfg.device_type) {
        if !cfg.custom_bitrate.trim().is_empty() {
            return Err(format!(
                "CAN{} 的 {} 不支持 PCAN 自定义位时序串",
                cfg.sw_channel, cfg.device_type
            ));
        }
        if cfg.is_fd && !profile.fd_capable {
            return Err(format!(
                "CAN{} 的 {} 不支持 CAN FD",
                cfg.sw_channel, cfg.device_type
            ));
        }
        if cfg.fd_non_iso && (!cfg.is_fd || !profile.fd_capable) {
            return Err(format!(
                "CAN{} 只有 CAN FD 硬件才能使用 Non-ISO 模式",
                cfg.sw_channel
            ));
        }
        let arbitration = baud_to_bps(&cfg.baud);
        if arbitration == 0 {
            return Err(format!(
                "CAN{} 仲裁波特率无效: {}",
                cfg.sw_channel, cfg.baud
            ));
        }
        if cfg.is_fd {
            let data = baud_to_bps(&cfg.data_baud);
            if data == 0 {
                return Err(format!(
                    "CAN{} 数据波特率无效: {}",
                    cfg.sw_channel, cfg.data_baud
                ));
            }
            if data < arbitration {
                return Err(format!(
                    "CAN{} 数据波特率 {} 不能低于仲裁波特率 {}",
                    cfg.sw_channel, cfg.data_baud, cfg.baud
                ));
            }
        }
        if profile.is_network() {
            cfg.ip
                .trim()
                .parse::<std::net::IpAddr>()
                .map_err(|_| format!("CAN{} 网络适配器 IP 无效: {}", cfg.sw_channel, cfg.ip))?;
            let port =
                cfg.port.trim().parse::<u16>().map_err(|_| {
                    format!("CAN{} 网络适配器端口无效: {}", cfg.sw_channel, cfg.port)
                })?;
            if port == 0 {
                return Err(format!("CAN{} 网络适配器端口不能为 0", cfg.sw_channel));
            }
        }
        return Ok(());
    }

    Err(format!(
        "CAN{} 未知设备类型: {}",
        cfg.sw_channel, cfg.device_type
    ))
}

pub fn validate_channel_set(cfgs: &[DeviceConfig]) -> Result<(), String> {
    if cfgs.is_empty() {
        return Err("至少需要配置一个 CAN 通道".into());
    }
    let mut software_channels = std::collections::HashSet::new();
    let mut adapters = std::collections::HashSet::new();
    for cfg in cfgs {
        validate_device_config(cfg)?;
        if !software_channels.insert(cfg.sw_channel) {
            return Err(format!("软件通道 CAN{} 重复", cfg.sw_channel));
        }
        let key = adapter_key(cfg);
        if !adapters.insert(key) {
            return Err(format!(
                "CAN{} 与其他通道绑定了同一硬件端点 {} dev{} ch{}",
                cfg.sw_channel, cfg.device_type, cfg.device_index, cfg.channel_index
            ));
        }
    }
    Ok(())
}

#[allow(non_camel_case_types)]
mod zcan_ffi {
    use std::os::raw::{c_char, c_void};
    pub type DevHandle = *mut c_void;
    pub type ChHandle = *mut c_void;

    pub type FnOpenDevice = unsafe extern "system" fn(u32, u32, u32) -> DevHandle;
    pub type FnCloseDevice = unsafe extern "system" fn(DevHandle) -> u32;
    pub type FnGetDeviceInfo = unsafe extern "system" fn(DevHandle, *mut ZcanDeviceInfo) -> u32;
    pub type FnIsDeviceOnline = unsafe extern "system" fn(DevHandle) -> u32;
    pub type FnInitCan =
        unsafe extern "system" fn(DevHandle, u32, *mut ZcanChannelInitConfig) -> ChHandle;
    pub type FnStartCan = unsafe extern "system" fn(ChHandle) -> u32;
    pub type FnResetCan = unsafe extern "system" fn(ChHandle) -> u32;
    pub type FnClearBuffer = unsafe extern "system" fn(ChHandle) -> u32;
    pub type FnReadChannelErrInfo =
        unsafe extern "system" fn(ChHandle, *mut ZcanChannelErrInfo) -> u32;
    pub type FnReadChannelStatus =
        unsafe extern "system" fn(ChHandle, *mut ZcanChannelStatus) -> u32;
    pub type FnSetValue = unsafe extern "system" fn(DevHandle, *const c_char, *const c_char) -> u32;
    pub type FnGetValue = unsafe extern "system" fn(DevHandle, *const c_char) -> *const c_void;
    pub type FnGetReceiveNum = unsafe extern "system" fn(ChHandle, u8) -> u32;
    pub type FnTransmit = unsafe extern "system" fn(ChHandle, *const ZcanTransmitData, u32) -> u32;
    pub type FnTransmitFd =
        unsafe extern "system" fn(ChHandle, *const ZcanTransmitFdData, u32) -> u32;
    pub type FnReceive = unsafe extern "system" fn(ChHandle, *mut ZcanReceiveData, u32, i32) -> u32;
    pub type FnReceiveFd =
        unsafe extern "system" fn(ChHandle, *mut ZcanReceiveFdData, u32, i32) -> u32;

    pub const EFF: u32 = 0x8000_0000;
    pub const RTR: u32 = 0x4000_0000;
    pub const ID_MASK: u32 = 0x1FFF_FFFF;
    pub const TYPE_CAN: u8 = 0;
    pub const TYPE_CANFD: u8 = 1;

    pub const ERROR_CAN_OVERFLOW: u32 = 0x0001;
    pub const ERROR_CAN_ERRALARM: u32 = 0x0002;
    pub const ERROR_CAN_PASSIVE: u32 = 0x0004;
    pub const ERROR_CAN_LOSE: u32 = 0x0008;
    pub const ERROR_CAN_BUSERR: u32 = 0x0010;
    pub const ERROR_CAN_BUSOFF: u32 = 0x0020;
    pub const ERROR_CAN_BUFFER_OVERFLOW: u32 = 0x0040;
    pub const ERROR_DEVICEOPENED: u32 = 0x0100;
    pub const ERROR_DEVICEOPEN: u32 = 0x0200;
    pub const ERROR_DEVICENOTOPEN: u32 = 0x0400;
    pub const ERROR_BUFFEROVERFLOW: u32 = 0x0800;
    pub const ERROR_DEVICENOTEXIST: u32 = 0x1000;
    pub const ERROR_LOADKERNELDLL: u32 = 0x2000;
    pub const ERROR_CMDFAILED: u32 = 0x4000;
    pub const ERROR_BUFFERCREATE: u32 = 0x8000;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanDeviceInfo {
        pub hw_version: u16,
        pub fw_version: u16,
        pub driver_version: u16,
        pub interface_version: u16,
        pub irq_num: u16,
        pub can_num: u8,
        pub serial_number: [u8; 20],
        pub hardware_type: [u8; 40],
        pub reserved: [u16; 4],
    }

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
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanClassicInitConfig {
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
    pub union ZcanChannelConfig {
        pub classic: ZcanClassicInitConfig,
        pub raw: [u8; 28],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanChannelInitConfig {
        pub can_type: u32,
        pub config: ZcanChannelConfig,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ZcanChannelErrInfo {
        pub error_code: u32,
        pub passive_err_data: [u8; 3],
        pub ar_lost_err_data: u8,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ZcanChannelStatus {
        pub err_interrupt: u8,
        pub reg_mode: u8,
        pub reg_status: u8,
        pub reg_al_capture: u8,
        pub reg_ec_capture: u8,
        pub reg_ew_limit: u8,
        pub reg_re_counter: u8,
        pub reg_te_counter: u8,
        pub reserved: u32,
    }
}

fn zcan_error_is_connection_lost(error_code: u32) -> bool {
    use zcan_ffi::*;
    error_code
        & (ERROR_DEVICEOPEN | ERROR_DEVICENOTOPEN | ERROR_DEVICENOTEXIST | ERROR_LOADKERNELDLL)
        != 0
}

fn zcan_error_message(error_code: u32, status: Option<zcan_ffi::ZcanChannelStatus>) -> String {
    use zcan_ffi::*;
    let mut causes = Vec::new();
    if error_code & ERROR_CAN_OVERFLOW != 0 {
        causes.push("控制器接收溢出");
    }
    if error_code & ERROR_CAN_BUFFER_OVERFLOW != 0 || error_code & ERROR_BUFFEROVERFLOW != 0 {
        causes.push("驱动接收缓冲区溢出");
    }
    if error_code & ERROR_CAN_ERRALARM != 0 {
        causes.push("错误计数达到报警阈值");
    }
    if error_code & ERROR_CAN_PASSIVE != 0 {
        causes.push("CAN 控制器进入错误被动状态");
    }
    if error_code & ERROR_CAN_LOSE != 0 {
        causes.push("仲裁丢失");
    }
    if error_code & ERROR_CAN_BUSERR != 0 {
        causes
            .push("CAN 总线错误：检查 CAN_H/CAN_L、共地、两端 120Ω、波特率以及是否存在可应答节点");
    }
    if error_code & ERROR_CAN_BUSOFF != 0 {
        causes.push("CAN 控制器 Bus-Off");
    }
    if error_code & ERROR_DEVICEOPENED != 0 {
        causes.push("设备已被其他程序占用");
    }
    if error_code & ERROR_DEVICEOPEN != 0 {
        causes.push("设备打开失败");
    }
    if error_code & ERROR_DEVICENOTOPEN != 0 {
        causes.push("设备未打开");
    }
    if error_code & ERROR_DEVICENOTEXIST != 0 {
        causes.push("设备不存在或 USB 已断开");
    }
    if error_code & ERROR_LOADKERNELDLL != 0 {
        causes.push("ZLG 内核驱动 DLL 加载失败");
    }
    if error_code & ERROR_CMDFAILED != 0 {
        causes.push("驱动命令执行失败");
    }
    if error_code & ERROR_BUFFERCREATE != 0 {
        causes.push("驱动缓冲区创建失败");
    }
    if causes.is_empty() {
        causes.push("未知 ZLG 驱动错误");
    }
    let counters = status
        .map(|s| {
            format!(
                "，RXErr={} TXErr={} Status=0x{:02X}",
                s.reg_re_counter, s.reg_te_counter, s.reg_status
            )
        })
        .unwrap_or_default();
    format!(
        "ZLG 错误 0x{error_code:08X}：{}{counters}",
        causes.join("；")
    )
}

fn merge_poll_report(target: &mut PollReport, source: PollReport) {
    target.receive_overruns = target
        .receive_overruns
        .saturating_add(source.receive_overruns);
    target.driver_errors = target.driver_errors.saturating_add(source.driver_errors);
    target.connection_lost |= source.connection_lost;
    if source.message.is_some() {
        target.message = source.message;
    }
}

struct ZcanSharedDevice {
    lib: &'static libloading::Library,
    dev: usize,
}

impl Drop for ZcanSharedDevice {
    fn drop(&mut self) {
        unsafe {
            if let Ok(close) = self
                .lib
                .get::<zcan_ffi::FnCloseDevice>(b"ZCAN_CloseDevice\0")
            {
                let _ = close(self.dev as zcan_ffi::DevHandle);
            }
        }
    }
}

fn acquire_zcan_device(
    profile: ZcanDeviceProfile,
    cfg: &DeviceConfig,
) -> Result<Arc<ZcanSharedDevice>, String> {
    use zcan_ffi::*;
    static DEVICES: std::sync::OnceLock<Mutex<HashMap<String, Weak<ZcanSharedDevice>>>> =
        std::sync::OnceLock::new();
    let key = zcan_device_key(profile, cfg);
    let mut devices = DEVICES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "ZLG 设备共享状态已损坏".to_string())?;
    if let Some(device) = devices.get(&key).and_then(Weak::upgrade) {
        return Ok(device);
    }

    pin_zlg_kernel_library(profile)?;
    let lib = pinned_zlgcan_library()?;
    let dev = unsafe {
        let open: libloading::Symbol<FnOpenDevice> = lib
            .get(b"ZCAN_OpenDevice\0")
            .map_err(|error| format!("ZCAN_OpenDevice 未找到: {error}"))?;
        open(profile.device_type, cfg.device_index, 0)
    };
    if dev.is_null() {
        return Err(if profile.family == ZcanDeviceFamily::UsbClassic {
            "ZCAN_OpenDevice 失败（USBCAN-E-U 驱动未启动或设备被占用；请重新插拔设备，或以管理员权限重新安装官方驱动）".into()
        } else {
            "ZCAN_OpenDevice 失败（设备未连接、被其他程序占用或 ZLG 驱动不可用；请关闭 ZCANPRO 等工具、重新插拔设备后重试）".into()
        });
    }
    let device = Arc::new(ZcanSharedDevice {
        lib,
        dev: dev as usize,
    });
    devices.insert(key, Arc::downgrade(&device));
    Ok(device)
}

fn zcan_device_key(profile: ZcanDeviceProfile, cfg: &DeviceConfig) -> String {
    if profile.is_network() {
        format!(
            "net:{}:{}:{}:{}:{}",
            profile.device_type, cfg.device_index, cfg.ip, cfg.port, cfg.net_server
        )
    } else {
        // USB rows can retain hidden network-form values after the user changes
        // device type. Those values must never split one physical multi-channel
        // adapter into multiple ZCAN_OpenDevice calls.
        format!("usb:{}:{}", profile.device_type, cfg.device_index)
    }
}

pub struct ZcanFdBus {
    device: Arc<ZcanSharedDevice>,
    ch: usize,
    channel_is_fd: bool,
    listen_only: bool,
    start: Instant,
    timestamp: HardwareTimebase,
    name: String,
    last_health_check: Instant,
    last_error_code: u32,
    last_busoff_recovery: Option<Instant>,
}

impl ZcanFdBus {
    pub fn open(start: Instant, cfg: &DeviceConfig) -> Result<Self, String> {
        use std::ffi::CString;
        use zcan_ffi::*;
        let profile = zcan_profile(&cfg.device_type)
            .ok_or_else(|| format!("非新版 ZLG 设备类型: {}", cfg.device_type))?;
        let is_net = profile.is_network();
        let is_usbcanfd = profile.family == ZcanDeviceFamily::UsbCanFd;
        let is_usbcan_e_u = profile.family == ZcanDeviceFamily::UsbClassic;
        unsafe {
            let device = acquire_zcan_device(profile, cfg)?;
            let lib = device.lib;
            let dev = device.dev as DevHandle;

            let setval: libloading::Symbol<FnSetValue> = match lib.get(b"ZCAN_SetValue\0") {
                Ok(s) => s,
                Err(e) => {
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
                let mode = if profile.is_tcp() {
                    set("work_mode", if cfg.net_server { "1" } else { "0" })
                } else {
                    set("local_port", &cfg.port)
                };
                mode.and_then(|_| set("ip", &cfg.ip))
                    .and_then(|_| set("work_port", &cfg.port))
            } else if is_usbcanfd {
                let _ = set("canfd_standard", if cfg.fd_non_iso { "1" } else { "0" });
                let _ = set("work_mode", if cfg.listen_only { "1" } else { "0" });
                let abit = baud_to_bps(&cfg.baud).to_string();
                let dbit = if cfg.is_fd {
                    baud_to_bps(&cfg.data_baud).to_string()
                } else {
                    abit.clone()
                };
                set("canfd_abit_baud_rate", &abit).and_then(|_| set("canfd_dbit_baud_rate", &dbit))
            } else if is_usbcan_e_u {
                // The official USBCAN-E-U device property declares
                // channel_N/baud_rate as an at_initcan="pre" setting.  The
                // classic timing bytes below are still populated for API
                // compatibility, but the property is what the kernel driver
                // uses to select the requested bitrate.
                set("baud_rate", &baud_to_bps(&cfg.baud).to_string())
            } else {
                set("baud_rate", &baud_to_bps(&cfg.baud).to_string())
            };
            cfg_res?;
            let bitrate_readback = if is_usbcan_e_u {
                let path = CString::new(format!("{}/baud_rate", cfg.channel_index)).unwrap();
                lib.get::<FnGetValue>(b"ZCAN_GetValue\0")
                    .ok()
                    .and_then(|get| {
                        let value = get(dev, path.as_ptr()).cast::<std::os::raw::c_char>();
                        (!value.is_null()).then(|| {
                            std::ffi::CStr::from_ptr(value).to_string_lossy().into_owned()
                        })
                    })
            } else {
                None
            };
            drop(setval);

            let init: libloading::Symbol<FnInitCan> = match lib.get(b"ZCAN_InitCAN\0") {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("ZCAN_InitCAN 未找到: {e}"));
                }
            };
            // The official driver properties above are the single source of
            // bitrate configuration. In particular, USBCAN-E-U declares
            // `baud_rate` as an at_initcan="pre" property. Mixing that with
            // legacy VCI Timing0/Timing1 bytes can produce a different actual
            // bitrate on some E-U/MINI sales variants.
            let mut init_cfg = ZcanChannelInitConfig {
                // ZLG's USBCANFD kernel driver only starts these physical
                // channels as TYPE_CANFD. A TYPE_CANFD channel still carries
                // ordinary CAN 2.0 frames; cfg.is_fd controls frame formats.
                can_type: zcan_driver_channel_type(profile, cfg.is_fd) as u32,
                config: ZcanChannelConfig { raw: [0u8; 28] },
            };
            let ch = init(dev, cfg.channel_index, &mut init_cfg);
            drop(init);
            if ch.is_null() {
                return Err("ZCAN_InitCAN 失败".into());
            }

            // A process restart does not necessarily power-cycle the USB CAN
            // controller. Clear a Bus-Off/error-passive state left by an
            // earlier bitrate mismatch before starting the newly configured
            // channel.
            if let Ok(reset) = lib.get::<FnResetCan>(b"ZCAN_ResetCAN\0") {
                let _ = reset(ch);
            }
            if let Ok(clear) = lib.get::<FnClearBuffer>(b"ZCAN_ClearBuffer\0") {
                let _ = clear(ch);
            }

            if is_usbcanfd
                && !is_net
                && let Ok(setres) = lib.get::<FnSetValue>(b"ZCAN_SetValue\0")
            {
                let path =
                    CString::new(format!("{}/initenal_resistance", cfg.channel_index)).unwrap();
                let v = CString::new(if cfg.termination { "1" } else { "0" }).unwrap();
                let _ = setres(dev, path.as_ptr(), v.as_ptr());
            }

            let start_can: libloading::Symbol<FnStartCan> = match lib.get(b"ZCAN_StartCAN\0") {
                Ok(s) => s,
                Err(e) => {
                    return Err(format!("ZCAN_StartCAN 未找到: {e}"));
                }
            };
            if start_can(ch) != 1 {
                drop(start_can);
                if let Ok(reset) = lib.get::<FnResetCan>(b"ZCAN_ResetCAN\0") {
                    let _ = reset(ch);
                }
                return Err("ZCAN_StartCAN 失败".into());
            }
            drop(start_can);

            if let Ok(clear) = lib.get::<FnClearBuffer>(b"ZCAN_ClearBuffer\0") {
                let _ = clear(ch);
            }
            if let Ok(online) = lib.get::<FnIsDeviceOnline>(b"ZCAN_IsDeviceOnLine\0")
                && online(dev) == 0
            {
                return Err("ZLG 设备打开后报告离线，请检查 USB、驱动和设备占用状态".into());
            }

            let name = if is_net {
                format!("{} {}:{}", cfg.device_type, cfg.ip, cfg.port)
            } else {
                format!(
                    "{} dev{} CAN{} @{}{}{}",
                    cfg.device_type,
                    cfg.device_index,
                    cfg.channel_index,
                    normalize_baud(&cfg.baud),
                    if cfg.is_fd {
                        format!("/{}", normalize_baud(&cfg.data_baud))
                    } else {
                        String::new()
                    },
                    bitrate_readback
                        .filter(|value| !value.is_empty())
                        .map(|value| format!(" [driver={value}]") )
                        .unwrap_or_default()
                )
            };
            Ok(Self {
                device,
                ch: ch as usize,
                channel_is_fd: cfg.is_fd,
                listen_only: cfg.listen_only,
                start,
                // The vendor zlgcan.h supplied with the driver declares timestamps in us.
                timestamp: HardwareTimebase::new(1e-6, None),
                name,
                last_health_check: Instant::now() - Duration::from_secs(1),
                last_error_code: 0,
                last_busoff_recovery: None,
            })
        }
    }

    fn health_report(&mut self, force: bool) -> PollReport {
        use zcan_ffi::*;
        if !force && self.last_health_check.elapsed() < Duration::from_millis(200) {
            return PollReport::default();
        }
        self.last_health_check = Instant::now();
        let dev = self.device.dev as DevHandle;
        let ch = self.ch as ChHandle;
        unsafe {
            match self
                .device
                .lib
                .get::<FnIsDeviceOnline>(b"ZCAN_IsDeviceOnLine\0")
            {
                Ok(online) if online(dev) == 0 => {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some("ZLG 设备离线或 USB 已断开".into()),
                        ..Default::default()
                    };
                }
                Err(error) => {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some(format!("ZCAN_IsDeviceOnLine 未找到: {error}")),
                        ..Default::default()
                    };
                }
                _ => {}
            }

            let mut error_info = ZcanChannelErrInfo::default();
            let error_code = self
                .device
                .lib
                .get::<FnReadChannelErrInfo>(b"ZCAN_ReadChannelErrInfo\0")
                .ok()
                .filter(|read| read(ch, &mut error_info) == 1)
                .map(|_| error_info.error_code)
                .unwrap_or(0);
            if error_code == 0 {
                self.last_error_code = 0;
                return PollReport::default();
            }

            let mut status = ZcanChannelStatus::default();
            let channel_status = self
                .device
                .lib
                .get::<FnReadChannelStatus>(b"ZCAN_ReadChannelStatus\0")
                .ok()
                .filter(|read| read(ch, &mut status) == 1)
                .map(|_| status);
            let is_new_error = error_code != self.last_error_code;
            self.last_error_code = error_code;
            let overflow = error_code
                & (ERROR_CAN_OVERFLOW | ERROR_CAN_BUFFER_OVERFLOW | ERROR_BUFFEROVERFLOW)
                != 0;
            let mut message = zcan_error_message(error_code, channel_status);

            if error_code & ERROR_CAN_BUSOFF != 0
                && self
                    .last_busoff_recovery
                    .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
            {
                self.last_busoff_recovery = Some(Instant::now());
                let reset_ok = self
                    .device
                    .lib
                    .get::<FnResetCan>(b"ZCAN_ResetCAN\0")
                    .is_ok_and(|reset| reset(ch) == 1);
                let clear_ok = self
                    .device
                    .lib
                    .get::<FnClearBuffer>(b"ZCAN_ClearBuffer\0")
                    .is_ok_and(|clear| clear(ch) == 1);
                let start_ok = self
                    .device
                    .lib
                    .get::<FnStartCan>(b"ZCAN_StartCAN\0")
                    .is_ok_and(|start| start(ch) == 1);
                message.push_str(if reset_ok && clear_ok && start_ok {
                    "；已自动复位并重新启动通道"
                } else {
                    "；自动恢复失败，请断开设备后重新连接"
                });
            }

            PollReport {
                receive_overruns: u64::from(overflow),
                driver_errors: u64::from(is_new_error),
                connection_lost: zcan_error_is_connection_lost(error_code),
                message: Some(message),
            }
        }
    }

    fn transmit_error(&mut self, operation: &str) -> String {
        self.health_report(true)
            .message
            .map(|message| format!("{operation} 失败；{message}"))
            .unwrap_or_else(|| {
                format!(
                    "{operation} 失败：驱动未接收该帧，请检查通道模式、总线接线、终端电阻和节点 ACK"
                )
            })
    }
}

impl Drop for ZcanFdBus {
    fn drop(&mut self) {
        unsafe {
            if let Ok(reset) = self
                .device
                .lib
                .get::<zcan_ffi::FnResetCan>(b"ZCAN_ResetCAN\0")
            {
                let _ = reset(self.ch as zcan_ffi::ChHandle);
            }
            if let Ok(clear) = self
                .device
                .lib
                .get::<zcan_ffi::FnClearBuffer>(b"ZCAN_ClearBuffer\0")
            {
                let _ = clear(self.ch as zcan_ffi::ChHandle);
            }
        }
    }
}

impl CanAdapter for ZcanFdBus {
    fn poll(&mut self, out: &mut Vec<CanFrame>) -> PollReport {
        use zcan_ffi::*;
        let ch = self.ch as ChHandle;
        let mut report = self.health_report(false);
        if report.connection_lost {
            return report;
        }
        unsafe {
            let getnum: FnGetReceiveNum = match self.device.lib.get(b"ZCAN_GetReceiveNum\0") {
                Ok(s) => *s,
                Err(error) => {
                    return PollReport {
                        driver_errors: 1,
                        connection_lost: true,
                        message: Some(format!("ZCAN_GetReceiveNum 未找到: {error}")),
                        ..Default::default()
                    };
                }
            };
            if let Ok(recv) = self
                .device
                .lib
                .get::<FnReceive>(b"ZCAN_Receive\0")
                .map(|s| *s)
            {
                let available = getnum(ch, TYPE_CAN);
                if available == u32::MAX {
                    report.driver_errors = report.driver_errors.saturating_add(1);
                    report.message = Some("ZCAN_GetReceiveNum(CAN) 返回驱动错误".into());
                    merge_poll_report(&mut report, self.health_report(true));
                } else if available > 0 {
                    let n = available.min(256);
                    let empty = ZcanReceiveData {
                        frame: CanFrameC {
                            can_id: 0,
                            can_dlc: 0,
                            pad: 0,
                            res0: 0,
                            res1: 0,
                            data: [0; 8],
                        },
                        timestamp: 0,
                    };
                    let mut buf = [empty; 256];
                    let received = recv(ch, buf.as_mut_ptr(), n, 0);
                    let got = if received == u32::MAX {
                        report.driver_errors = report.driver_errors.saturating_add(1);
                        report.message = Some("ZCAN_Receive 返回驱动错误".into());
                        merge_poll_report(&mut report, self.health_report(true));
                        0
                    } else {
                        received.min(n)
                    };
                    for r in buf.iter().take(got as usize) {
                        let len = (r.frame.can_dlc as usize).min(8);
                        let timestamp = self
                            .timestamp
                            .map(r.timestamp, self.start.elapsed().as_secs_f64());
                        out.push(CanFrame {
                            t: timestamp,
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
            if self.channel_is_fd
                && let Ok(recv) = self
                    .device
                    .lib
                    .get::<FnReceiveFd>(b"ZCAN_ReceiveFD\0")
                    .map(|s| *s)
            {
                let available = getnum(ch, TYPE_CANFD);
                if available == u32::MAX {
                    report.driver_errors = report.driver_errors.saturating_add(1);
                    report.message = Some("ZCAN_GetReceiveNum(CAN FD) 返回驱动错误".into());
                    merge_poll_report(&mut report, self.health_report(true));
                } else if available > 0 {
                    let n = available.min(256);
                    let empty = ZcanReceiveFdData {
                        frame: CanfdFrameC {
                            can_id: 0,
                            len: 0,
                            flags: 0,
                            res0: 0,
                            res1: 0,
                            data: [0; 64],
                        },
                        timestamp: 0,
                    };
                    let mut buf = [empty; 256];
                    let received = recv(ch, buf.as_mut_ptr(), n, 0);
                    let got = if received == u32::MAX {
                        report.driver_errors = report.driver_errors.saturating_add(1);
                        report.message = Some("ZCAN_ReceiveFD 返回驱动错误".into());
                        merge_poll_report(&mut report, self.health_report(true));
                        0
                    } else {
                        received.min(n)
                    };
                    for r in buf.iter().take(got as usize) {
                        let len = (r.frame.len as usize).min(64);
                        let timestamp = self
                            .timestamp
                            .map(r.timestamp, self.start.elapsed().as_secs_f64());
                        out.push(CanFrame {
                            t: timestamp,
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
        report
    }

    fn send(&mut self, f: &CanFrame) -> Result<(), String> {
        use zcan_ffi::*;
        if self.listen_only {
            return Err("监听模式禁止发送 CAN 报文".into());
        }
        if f.fd && !self.channel_is_fd {
            return Err("当前 ZLG 通道按 Classical CAN 初始化，不能发送 CAN FD 帧".into());
        }
        if !f.fd && f.data.len() > 8 {
            return Err("Classical CAN 数据长度不能超过 8 字节".into());
        }
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
                let transmit: FnTransmitFd = *self
                    .device
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
                    return Err(self.transmit_error("ZCAN_TransmitFD"));
                }
            } else {
                let transmit: FnTransmit = *self
                    .device
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
                    return Err(self.transmit_error("ZCAN_Transmit"));
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
    ConnectChannels(Vec<DeviceConfig>),
    Disconnect,
    Start,
    Stop,
    SendOnce(CanFrame),
    SendSequence {
        frame: CanFrame,
        count: u64,
        id_increment: bool,
        data_increment: bool,
    },
    SendBatch {
        frames: Vec<CanFrame>,
        repeat: u32,
        ack: Option<std::sync::mpsc::SyncSender<Result<u64, String>>>,
    },
    OtaRun(OtaJob),
    SetPeriodic {
        handle: u64,
        frame: CanFrame,
        period_ms: u64,
        repeat: i64,
        enable: bool,
    },
    SetDynamicPeriodic {
        handle: u64,
        config: Option<DynamicPeriodicConfig>,
    },
    SetSimulationPeriodics(Vec<SimPeriodicConfig>),
    PlaybackLoad(Vec<CanFrame>),
    PlaybackPlay {
        online: bool,
        speed: f64,
        loop_play: bool,
    },
    PlaybackStep,
    PlaybackPause,
    PlaybackCancel,
    PlaybackSeek(f64),
    Shutdown,
}

#[derive(Clone)]
pub struct DynamicPeriodicConfig {
    pub frame: CanFrame,
    pub dbcs: Vec<DbcDb>,
    pub dbc_id: u32,
    pub signal_values: Vec<(String, f64)>,
    pub varies: Vec<(String, VaryMode)>,
    pub period_ms: u64,
    pub repeat: i64,
    pub start_sent: u64,
}

#[derive(Clone, Debug)]
pub enum SimGeneratorMode {
    Constant { value: f64 },
    Ramp { min: f64, max: f64, step: f64 },
    Sine { min: f64, max: f64 },
}

#[derive(Clone, Debug)]
pub struct SimSignalGenerator {
    pub signal: String,
    pub mode: SimGeneratorMode,
    pub period_ms: u64,
}

#[derive(Clone)]
pub struct SimPeriodicConfig {
    pub frame: CanFrame,
    pub dbc: Option<DbcDb>,
    pub dbc_id: u32,
    pub generators: Vec<SimSignalGenerator>,
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
    Frames(Vec<CanFrame>),
    PlaybackFrame(CanFrame),
    Log(String),
    OtaProgress(usize, usize, String),
    Connected {
        channels: Vec<u8>,
        name: String,
        error: Option<String>,
    },
    Running(bool),
    Playback(usize, usize, bool),
    PeriodicDone(u64),
    DynamicUpdate {
        handle: u64,
        data: Vec<u8>,
        signal_values: Vec<(String, f64)>,
        sent: u64,
    },
    CaptureHealth {
        dropped_frames: u64,
        dropped_events: u64,
        hardware_overruns: u64,
        hardware_errors: u64,
        queue_depth: usize,
        queue_capacity: usize,
        queue_high_watermark: usize,
        command_rejected: u64,
        command_queue_depth: usize,
        command_queue_capacity: usize,
        command_queue_high_watermark: usize,
        timestamp_samples: u64,
        timestamp_latest_jitter_us: f64,
        timestamp_max_jitter_us: f64,
        timestamp_drift_ppm: f64,
        timestamp_monotonic_violations: u64,
    },
    ShutdownFinished,
}

const EVENT_QUEUE_CAPACITY: usize = 1024;
const EVENT_QUEUE_CONTROL_RESERVE: usize = 64;
const COMMAND_QUEUE_CAPACITY: usize = 512;

#[derive(Clone, Default)]
struct CommandHealth {
    rejected: Arc<AtomicU64>,
    high_watermark: Arc<AtomicUsize>,
    shutdown_requested: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct CommandSender {
    tx: EventChannelSender<Cmd>,
    health: CommandHealth,
}

#[derive(Clone, Copy, Debug)]
pub struct CommandRejected;

impl CommandSender {
    pub fn send(&self, command: Cmd) -> Result<(), CommandRejected> {
        if matches!(&command, Cmd::Shutdown) {
            self.health
                .shutdown_requested
                .store(true, Ordering::Release);
        }
        match self.tx.try_send(command) {
            Ok(()) => {
                self.health
                    .high_watermark
                    .fetch_max(self.tx.len(), Ordering::Relaxed);
                Ok(())
            }
            Err(_) => {
                self.health.rejected.fetch_add(1, Ordering::Relaxed);
                Err(CommandRejected)
            }
        }
    }

    pub fn send_critical(&self, command: Cmd, timeout: Duration) -> Result<(), CommandRejected> {
        if matches!(&command, Cmd::Shutdown) {
            self.health
                .shutdown_requested
                .store(true, Ordering::Release);
        }
        match self.tx.send_timeout(command, timeout) {
            Ok(()) => {
                self.health
                    .high_watermark
                    .fetch_max(self.tx.len(), Ordering::Relaxed);
                Ok(())
            }
            Err(_) => {
                self.health.rejected.fetch_add(1, Ordering::Relaxed);
                Err(CommandRejected)
            }
        }
    }
}

#[derive(Clone)]
struct EventSender {
    tx: EventChannelSender<Evt>,
    dropped_frames: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
    high_watermark: Arc<AtomicUsize>,
    started: Instant,
    timestamp_quality: Arc<Mutex<HashMap<u8, TimestampQuality>>>,
}

impl EventSender {
    fn new(tx: EventChannelSender<Evt>) -> Self {
        Self {
            tx,
            dropped_frames: Arc::new(AtomicU64::new(0)),
            dropped_events: Arc::new(AtomicU64::new(0)),
            high_watermark: Arc::new(AtomicUsize::new(0)),
            started: Instant::now(),
            timestamp_quality: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn update_high_watermark(&self) {
        self.high_watermark
            .fetch_max(self.tx.len(), Ordering::Relaxed);
    }

    fn begin_timestamp_session(&self) {
        if let Ok(mut channels) = self.timestamp_quality.lock() {
            for quality in channels.values_mut() {
                quality.begin_session();
            }
        }
    }

    /// Non-blocking control delivery. Data producers leave a reserved tail in the queue so
    /// connection/error/shutdown state remains observable even during sustained bus load.
    fn send(&self, event: Evt) -> Result<(), ()> {
        match self.tx.try_send(event) {
            Ok(()) => {
                self.update_high_watermark();
                Ok(())
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
        }
    }

    fn send_critical(&self, event: Evt, timeout: Duration) -> Result<(), ()> {
        match self.tx.send_timeout(event, timeout) {
            Ok(()) => {
                self.update_high_watermark();
                Ok(())
            }
            Err(_) => {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
        }
    }

    /// Capture frames are deliberately dropped as a whole batch before they can block the
    /// hardware polling loop. Every dropped frame is counted and reported to the UI.
    fn send_frames(&self, frames: Vec<CanFrame>) {
        if frames.is_empty() {
            return;
        }
        let host_receive_s = self.started.elapsed().as_secs_f64();
        if let Ok(mut quality) = self.timestamp_quality.lock() {
            for frame in &frames {
                quality
                    .entry(frame.ch)
                    .or_default()
                    .observe(frame.t, host_receive_s);
            }
        }
        let count = frames.len() as u64;
        if self.tx.len() >= EVENT_QUEUE_CAPACITY - EVENT_QUEUE_CONTROL_RESERVE {
            self.dropped_frames.fetch_add(count, Ordering::Relaxed);
            return;
        }
        match self.tx.try_send(Evt::Frames(frames)) {
            Ok(()) => self.update_high_watermark(),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped_frames.fetch_add(count, Ordering::Relaxed);
            }
        }
    }

    fn report_health(
        &self,
        hardware_overruns: u64,
        hardware_errors: u64,
        commands: &CommandHealth,
        command_depth: usize,
    ) {
        let timestamp = self.timestamp_quality_snapshot();
        let event = Evt::CaptureHealth {
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            dropped_events: self.dropped_events.load(Ordering::Relaxed),
            hardware_overruns,
            hardware_errors,
            queue_depth: self.tx.len(),
            queue_capacity: EVENT_QUEUE_CAPACITY,
            queue_high_watermark: self.high_watermark.load(Ordering::Relaxed),
            command_rejected: commands.rejected.load(Ordering::Relaxed),
            command_queue_depth: command_depth,
            command_queue_capacity: COMMAND_QUEUE_CAPACITY,
            command_queue_high_watermark: commands.high_watermark.load(Ordering::Relaxed),
            timestamp_samples: timestamp.samples,
            timestamp_latest_jitter_us: timestamp.latest_transport_jitter_us,
            timestamp_max_jitter_us: timestamp.max_transport_jitter_us,
            timestamp_drift_ppm: timestamp.clock_drift_ppm,
            timestamp_monotonic_violations: timestamp.monotonic_violations,
        };
        if self.tx.try_send(event).is_ok() {
            self.update_high_watermark();
        }
    }

    fn timestamp_quality_snapshot(&self) -> TimestampQualitySnapshot {
        let Ok(channels) = self.timestamp_quality.lock() else {
            return TimestampQualitySnapshot::default();
        };
        let mut aggregate = TimestampQualitySnapshot::default();
        for quality in channels.values() {
            let snapshot = quality.snapshot();
            aggregate.samples += snapshot.samples;
            aggregate.latest_transport_jitter_us = aggregate
                .latest_transport_jitter_us
                .max(snapshot.latest_transport_jitter_us);
            aggregate.max_transport_jitter_us = aggregate
                .max_transport_jitter_us
                .max(snapshot.max_transport_jitter_us);
            if snapshot.clock_drift_ppm.abs() > aggregate.clock_drift_ppm.abs() {
                aggregate.clock_drift_ppm = snapshot.clock_drift_ppm;
            }
            aggregate.monotonic_violations += snapshot.monotonic_violations;
        }
        aggregate
    }
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
    loop_play: bool,
}

struct Periodic {
    frame: CanFrame,
    period: Duration,
    next: Instant,
    remaining: i64,
}

struct DynamicPeriodic {
    config: DynamicPeriodicConfig,
    next: Instant,
    sent: u64,
}

struct SimSignalState {
    config: SimSignalGenerator,
    next: Instant,
    tick: u64,
}

struct SimPeriodic {
    frame: CanFrame,
    dbc: Option<DbcDb>,
    dbc_id: u32,
    generators: Vec<SimSignalState>,
    failed: bool,
}

fn sim_generator_value(generator: &SimSignalState) -> f64 {
    match generator.config.mode {
        SimGeneratorMode::Constant { value } => value,
        SimGeneratorMode::Ramp { min, max, step } => {
            let span = (max - min).abs().max(1e-9);
            let step = step.abs().max(1e-9);
            let pos = (generator.tick as f64 * step) % (2.0 * span);
            if pos <= span {
                min + pos
            } else {
                min + 2.0 * span - pos
            }
        }
        SimGeneratorMode::Sine { min, max } => {
            min + (max - min) * (0.5 + 0.5 * (generator.tick as f64 * 0.2).sin())
        }
    }
}

fn update_sim_periodic(periodic: &mut SimPeriodic, now: Instant) -> Result<bool, String> {
    let mut changed = false;
    for generator in &mut periodic.generators {
        if now < generator.next {
            continue;
        }
        let period = Duration::from_millis(generator.config.period_ms.max(10));
        generator.next += period;
        if generator.next <= now {
            generator.next = now + period;
        }
        let value = sim_generator_value(generator);
        generator.tick = generator.tick.wrapping_add(1);
        if generator.config.signal.is_empty() {
            if let Some(first) = periodic.frame.data.first_mut() {
                *first = value.clamp(0.0, 255.0) as u8;
            }
        } else if let Some(dbc) = periodic.dbc.as_ref() {
            periodic.frame.data = dbc.encode_signal_into_ext(
                periodic.dbc_id,
                periodic.frame.ext,
                &periodic.frame.data,
                &generator.config.signal,
                value,
            )?;
        } else {
            return Err("DBC signal generator has no DBC database".into());
        }
        changed = true;
    }
    Ok(changed)
}

const MAX_PENDING_SEND_JOBS: usize = 64;
const MAX_PENDING_SEND_FRAMES: u64 = 100_000;
const SEND_FRAMES_PER_SLICE: usize = 32;
const SEND_SLICE_BUDGET: Duration = Duration::from_millis(2);

enum PendingSendSource {
    Sequence {
        next: CanFrame,
        id_increment: bool,
        data_increment: bool,
    },
    Batch {
        frames: Vec<CanFrame>,
        index: usize,
    },
}

struct PendingSendJob {
    source: PendingSendSource,
    total: u64,
    emitted: u64,
}

impl PendingSendJob {
    fn sequence(
        frame: CanFrame,
        count: u64,
        id_increment: bool,
        data_increment: bool,
    ) -> Option<Self> {
        (count > 0).then_some(Self {
            source: PendingSendSource::Sequence {
                next: frame,
                id_increment,
                data_increment,
            },
            total: count.min(MAX_PENDING_SEND_FRAMES),
            emitted: 0,
        })
    }

    fn batch(frames: Vec<CanFrame>, repeat: u32) -> Option<Self> {
        if frames.is_empty() || repeat == 0 {
            return None;
        }
        let total = (frames.len() as u64)
            .saturating_mul(repeat as u64)
            .min(MAX_PENDING_SEND_FRAMES);
        Some(Self {
            source: PendingSendSource::Batch { frames, index: 0 },
            total,
            emitted: 0,
        })
    }

    fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.emitted)
    }

    fn next_frame(&mut self) -> Option<CanFrame> {
        if self.emitted >= self.total {
            return None;
        }
        let frame = match &mut self.source {
            PendingSendSource::Sequence {
                next,
                id_increment,
                data_increment,
            } => {
                let frame = next.clone();
                if *id_increment {
                    let id_mask = if next.ext { 0x1FFF_FFFF } else { 0x7FF };
                    next.id = next.id.wrapping_add(1) & id_mask;
                }
                if *data_increment {
                    increment_frame_data(&mut next.data);
                }
                frame
            }
            PendingSendSource::Batch { frames, index } => {
                let frame = frames[*index].clone();
                *index = (*index + 1) % frames.len();
                frame
            }
        };
        self.emitted += 1;
        Some(frame)
    }
}

fn increment_frame_data(data: &mut [u8]) {
    for byte in data {
        let (value, carry) = byte.overflowing_add(1);
        *byte = value;
        if !carry {
            break;
        }
    }
}

fn pending_send_frames(queue: &VecDeque<PendingSendJob>) -> u64 {
    queue.iter().map(PendingSendJob::remaining).sum()
}

fn enqueue_send_job(
    queue: &mut VecDeque<PendingSendJob>,
    job: PendingSendJob,
) -> Result<u64, &'static str> {
    if queue.len() >= MAX_PENDING_SEND_JOBS {
        return Err("发送任务过多，请等待当前任务完成");
    }
    let remaining = job.remaining();
    if pending_send_frames(queue).saturating_add(remaining) > MAX_PENDING_SEND_FRAMES {
        return Err("待发送帧已达到 100000 帧安全上限");
    }
    queue.push_back(job);
    Ok(remaining)
}

fn process_pending_sends(
    queue: &mut VecDeque<PendingSendJob>,
    adapters: &mut Vec<(u8, Box<dyn CanAdapter>)>,
    evt_tx: &EventSender,
    start: Instant,
) {
    if queue.is_empty() || adapters.is_empty() {
        return;
    }
    let deadline = Instant::now() + SEND_SLICE_BUDGET;
    let mut echoes = Vec::with_capacity(SEND_FRAMES_PER_SLICE);
    for _ in 0..SEND_FRAMES_PER_SLICE {
        if Instant::now() >= deadline {
            break;
        }
        let Some(job) = queue.front_mut() else {
            break;
        };
        let Some(mut frame) = job.next_frame() else {
            queue.pop_front();
            continue;
        };
        let job_complete = job.remaining() == 0;
        frame.t = start.elapsed().as_secs_f64();
        frame.tx = true;
        match send_on(adapters, &frame) {
            Ok(channel) => {
                frame.ch = channel;
                echoes.push(frame);
                if job_complete {
                    queue.pop_front();
                }
            }
            Err(error) => {
                queue.pop_front();
                let _ = evt_tx.send(Evt::Log(format!("发送任务已停止: {error}")));
                break;
            }
        }
    }
    if !echoes.is_empty() {
        let _ = evt_tx.send(Evt::Frames(echoes));
    }
}

fn dynamic_rand01(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / ((1u64 << 53) as f64)
}

fn build_dynamic_frame(
    handle: u64,
    periodic: &DynamicPeriodic,
) -> Result<(CanFrame, Vec<(String, f64)>), String> {
    let mut values: HashMap<String, f64> = periodic.config.signal_values.iter().cloned().collect();
    for (signal, mode) in &periodic.config.varies {
        let base = values.get(signal).copied().unwrap_or(0.0);
        let mut seed = handle ^ periodic.sent.wrapping_mul(0x0100_0001);
        for byte in signal.bytes() {
            seed = seed.wrapping_mul(31).wrapping_add(byte as u64);
        }
        values.insert(
            signal.clone(),
            vary::eval(mode, periodic.sent, base, dynamic_rand01(seed)),
        );
    }
    let mut frame = periodic.config.frame.clone();
    frame.data = periodic
        .config
        .dbcs
        .iter()
        .find_map(|dbc| dbc.encode_ext(periodic.config.dbc_id, frame.ext, &values))
        .ok_or_else(|| {
            format!(
                "DBC message 0x{:X} ext={} not found for dynamic send",
                periodic.config.dbc_id, frame.ext
            )
        })?;
    Ok((frame, values.into_iter().collect()))
}

pub fn spawn() -> (CommandSender, EventReceiver<Evt>) {
    let (cmd_tx, cmd_rx) = bounded::<Cmd>(COMMAND_QUEUE_CAPACITY);
    let (evt_tx, evt_rx) = bounded::<Evt>(EVENT_QUEUE_CAPACITY);
    let command_health = CommandHealth::default();
    let sender = CommandSender {
        tx: cmd_tx,
        health: command_health.clone(),
    };
    std::thread::spawn(move || controller(cmd_rx, EventSender::new(evt_tx), command_health));
    (sender, evt_rx)
}

fn open_adapter(start: Instant, cfg: &DeviceConfig) -> Result<Box<dyn CanAdapter>, String> {
    let device = cfg.device_type.trim().to_ascii_uppercase();
    if device == "VIRTUAL" || device == "SIM" {
        Err("不支持的设备类型: 虚拟总线已移除".into())
    } else if device == "PCAN" {
        PcanBus::open_cfg(start, cfg).map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "GCAN" {
        VciBus::open(start, cfg, &["ECanVci64.dll", "ECanVci.dll"], "", 3)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "ZHCX" || device == "ZHCXCAN" {
        VciBus::open(start, cfg, &["ControlCAN.dll"], "VCI_", 4)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if zcan_profile(&cfg.device_type).is_some() {
        ZcanFdBus::open(start, cfg).map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else {
        Err(format!("未知设备类型: {}", cfg.device_type))
    }
}

fn send_on(adapters: &mut [(u8, Box<dyn CanAdapter>)], f: &CanFrame) -> Result<u8, String> {
    let (channel, adapter) = adapters
        .iter_mut()
        .find(|(channel, _)| *channel == f.ch)
        .ok_or_else(|| format!("CAN 通道 {} 未连接", f.ch))?;
    adapter.send(f).map(|()| *channel)
}

fn emit_playback_frame(
    adapters: &mut [(u8, Box<dyn CanAdapter>)],
    evt_tx: &EventSender,
    mut frame: CanFrame,
    online: bool,
) -> bool {
    if online {
        match send_on(adapters, &frame) {
            Ok(channel) => {
                frame.ch = channel;
                frame.tx = true;
            }
            Err(error) => {
                let _ = evt_tx.send(Evt::Log(format!("在线回放发送失败: {error}")));
                return false;
            }
        }
    }
    let _ = evt_tx.send(Evt::PlaybackFrame(frame));
    true
}

fn connect_channels(
    adapters: &mut Vec<(u8, Box<dyn CanAdapter>)>,
    running: &mut bool,
    periodics: &mut HashMap<u64, Periodic>,
    evt_tx: &EventSender,
    start: Instant,
    cfgs: Vec<DeviceConfig>,
) {
    if let Err(error) = validate_channel_set(&cfgs) {
        let _ = evt_tx.send(Evt::Log(format!("CAN 通道配置无效: {error}")));
        let names = adapters
            .iter()
            .map(|(channel, adapter)| format!("CAN{channel}:{}", adapter.name()))
            .collect::<Vec<_>>()
            .join("  ");
        let _ = evt_tx.send(Evt::Connected {
            channels: adapters.iter().map(|(channel, _)| *channel).collect(),
            name: names,
            error: Some(error),
        });
        return;
    }
    *running = false;
    periodics.clear();
    adapters.clear();
    let _ = evt_tx.send(Evt::Running(false));
    let mut names: Vec<String> = Vec::new();
    let mut failures = Vec::new();
    for cfg in &cfgs {
        let ch = if cfg.sw_channel == 0 {
            1
        } else {
            cfg.sw_channel
        };
        match open_adapter(start, cfg) {
            Ok(bus) => {
                let name = bus.name().to_string();
                adapters.push((ch, bus));
                let _ = evt_tx.send(Evt::Log(format!("CAN{ch} 已连接: {name}")));
                names.push(format!("CAN{ch}:{name}"));
            }
            Err(e) => {
                let _ = evt_tx.send(Evt::Log(format!("CAN{ch} 连接失败: {e}")));
                failures.push(format!("CAN{ch}: {e}"));
            }
        }
    }
    if !failures.is_empty() {
        adapters.clear();
        let _ = evt_tx.send(Evt::Log(format!(
            "多通道连接已回滚，所有通道均保持断开: {}",
            failures.join("；")
        )));
        let _ = evt_tx.send(Evt::Connected {
            channels: Vec::new(),
            name: String::new(),
            error: Some(failures.join("；")),
        });
    } else if adapters.is_empty() {
        let _ = evt_tx.send(Evt::Connected {
            channels: Vec::new(),
            name: String::new(),
            error: Some("没有可连接的 CAN 通道".into()),
        });
    } else {
        let _ = evt_tx.send(Evt::Connected {
            channels: adapters.iter().map(|(channel, _)| *channel).collect(),
            name: names.join("  "),
            error: None,
        });
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
            ota_response_id_matches(response, frame.id)
                && frame.data.len() == 1
                && frame.data[0] == 0xFF
        }
        OtaAck::UdsFlowControl => frame.data.first().copied() == Some(0x30),
        OtaAck::UdsPositive { service } => {
            frame.data.get(1).copied() == Some(service.wrapping_add(0x40))
        }
    }
}

fn ota_ack_matches_on_channel(
    ack: OtaAck,
    expected_channel: u8,
    actual_channel: u8,
    frame: &CanFrame,
) -> bool {
    actual_channel == expected_channel && ota_ack_matches(ack, frame)
}

fn poll_for_ota_ack(
    adapters: &mut [(u8, Box<dyn CanAdapter>)],
    evt_tx: &EventSender,
    buf: &mut Vec<CanFrame>,
    ack: OtaAck,
    expected_channel: u8,
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
            let _ = adapter.poll(buf);
            for mut frame in buf.drain(..) {
                frame.ch = *ch;
                let matched = ota_ack_matches_on_channel(ack, expected_channel, *ch, &frame);
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

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn controlcan_board_info_matches_vendor_abi() {
        assert_eq!(std::mem::size_of::<zlg_ffi::VCI_BOARD_INFO>(), 80);
        assert_eq!(std::mem::size_of::<zlg_ffi::VCI_CAN_STATUS>(), 12);
        assert_eq!(std::mem::size_of::<zlg_ffi::VCI_CAN_OBJ>(), 24);
        assert_eq!(std::mem::size_of::<zlg_ffi::VCI_INIT_CONFIG>(), 16);
        assert_eq!(printable_vci_text(b"CANalyst-II\0ignored"), "CANalyst-II");
    }

    #[test]
    #[ignore = "requires locally attached GCAN/ZHCX hardware"]
    fn attached_usb_can_hardware_probe() {
        for device in zcan_attached_channels() {
            println!(
                "{} dev{} CAN{} {} SN {}",
                device.device_type,
                device.device_index,
                device.channel_index + 1,
                device.hardware_label,
                device.serial_number
            );
        }
    }

    #[test]
    #[ignore = "requires GCAN USBCAN-I and CANalyst-II on one 500K bus"]
    fn shared_vci_three_channel_hardware_matrix() {
        clear_vci_device_registry();
        let start = Instant::now();
        let gcan = device("GCAN", 1);
        let mut zhcx_can1 = device("ZHCX", 2);
        zhcx_can1.channel_index = 0;
        let mut zhcx_can2 = device("ZHCX", 3);
        zhcx_can2.channel_index = 1;
        let configs = [gcan, zhcx_can1, zhcx_can2];
        let mut adapters = configs
            .iter()
            .map(|config| (config.sw_channel, open_adapter(start, config).unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(vci_device_registry().lock().unwrap().len(), 2);
        std::thread::sleep(Duration::from_millis(100));

        for source in 0..adapters.len() {
            let id = 0x6E1 + source as u32;
            let frame = CanFrame {
                t: 0.0,
                ch: adapters[source].0,
                tx: true,
                id,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0xA0 + source as u8, 1, 2, 3, 4, 5, 6, 7],
            };
            adapters[source].1.send(&frame).unwrap();
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut received = vec![false; adapters.len()];
            while Instant::now() < deadline {
                for (target, (_, adapter)) in adapters.iter_mut().enumerate() {
                    let mut frames = Vec::new();
                    let report = adapter.poll(&mut frames);
                    assert!(!report.connection_lost, "{:?}", report.message);
                    if target != source && frames.iter().any(|candidate| candidate.id == id) {
                        received[target] = true;
                    }
                }
                if received
                    .iter()
                    .enumerate()
                    .all(|(target, hit)| target == source || *hit)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                received
                    .iter()
                    .enumerate()
                    .all(|(target, hit)| target == source || *hit),
                "source={} receive matrix={received:?}",
                source
            );
        }

        adapters.clear();
        assert_eq!(vci_device_registry().lock().unwrap().len(), 2);
        clear_vci_device_registry();
        assert!(vci_device_registry().lock().unwrap().is_empty());
    }

    fn device(device_type: &str, channel: u8) -> DeviceConfig {
        DeviceConfig {
            sw_channel: channel,
            is_fd: false,
            device_type: device_type.into(),
            hardware_label: String::new(),
            hardware_id: String::new(),
            device_index: 0,
            channel_index: (channel - 1) as u32,
            baud: "500K".into(),
            data_baud: "2M".into(),
            custom_bitrate: String::new(),
            termination: false,
            listen_only: false,
            fd_non_iso: false,
            net_server: false,
            ip: "192.168.0.10".into(),
            port: "8000".into(),
        }
    }

    #[test]
    fn pcan_unplug_status_is_classified_as_connection_loss() {
        let report = pcan_poll_error(pcan_ffi::PCAN_ERROR_ILLHW);
        assert!(report.connection_lost);
        let bus_off = pcan_poll_error(pcan_ffi::PCAN_ERROR_BUSOFF);
        assert!(!bus_off.connection_lost);
    }

    #[test]
    fn connection_loss_requires_three_consecutive_reports() {
        let mut streaks = HashMap::new();
        assert!(!connection_loss_confirmed(&mut streaks, 1, true));
        assert!(!connection_loss_confirmed(&mut streaks, 1, false));
        assert!(!connection_loss_confirmed(&mut streaks, 1, true));
        assert!(!connection_loss_confirmed(&mut streaks, 1, false));
        assert!(connection_loss_confirmed(&mut streaks, 1, true));
    }

    #[test]
    fn channel_validation_is_adapter_aware() {
        assert!(validate_device_config(&device("PCAN", 1)).is_ok());

        let mut classic = device("USBCAN-E-U", 1);
        classic.is_fd = true;
        assert!(validate_device_config(&classic).is_err());

        let mut network = device("CANFDNET", 1);
        network.is_fd = true;
        network.ip = "not-an-ip".into();
        assert!(validate_device_config(&network).is_err());
        network.ip = "192.168.0.10".into();
        assert!(validate_device_config(&network).is_ok());
    }

    #[test]
    fn zlg_e_u_and_canfd_mini_use_distinct_driver_families() {
        let usbcan1 = zcan_profile("USBCAN1").unwrap();
        let usbcan2 = zcan_profile("USBCAN2").unwrap();
        let eu = zcan_profile("USBCAN-E-U").unwrap();
        let two_eu = zcan_profile("USBCAN-2E-U").unwrap();
        let mini = zcan_profile("USBCANFD-MINI").unwrap();
        assert_eq!(usbcan1.device_type, 3);
        assert_eq!(usbcan2.device_type, 4);
        assert_eq!(usbcan1.family, ZcanDeviceFamily::UsbClassic);
        assert_eq!(usbcan2.family, ZcanDeviceFamily::UsbClassic);
        assert_eq!(eu.device_type, 20);
        assert_eq!(two_eu.device_type, 21);
        assert_eq!(eu.family, ZcanDeviceFamily::UsbClassic);
        assert!(!eu.fd_capable);
        assert_eq!(mini.device_type, 43);
        assert_eq!(mini.family, ZcanDeviceFamily::UsbCanFd);
        assert!(mini.fd_capable);
    }

    #[test]
    fn zlg_canfd_hardware_uses_canfd_driver_channel_for_classic_frames() {
        let profile = zcan_profile("USBCANFD-200U").unwrap();
        assert_eq!(profile.family, ZcanDeviceFamily::UsbCanFd);
        assert!(profile.fd_capable);
        // ZLG's CAN FD USB kernel rejects ZCAN_StartCAN after TYPE_CAN init.
        // Classic-vs-FD remains a frame policy; it is not the driver channel type.
        let driver_type = zcan_driver_channel_type(profile, false);
        assert_eq!(driver_type, zcan_ffi::TYPE_CANFD);
    }

    #[test]
    fn zlg_usb_shared_device_key_ignores_stale_network_fields() {
        let profile = zcan_profile("USBCANFD-200U").unwrap();
        let first = device("USBCANFD-200U", 1);
        let mut second = device("USBCANFD-200U", 2);
        second.channel_index = 1;
        second.net_server = true;
        second.ip = "192.168.0.178".into();
        second.port = "8000".into();
        assert_eq!(
            zcan_device_key(profile, &first),
            zcan_device_key(profile, &second)
        );
    }

    #[test]
    fn zlg_e_u_classic_init_uses_sja1000_timing_layout() {
        use zcan_ffi::{ZcanChannelConfig, ZcanChannelInitConfig, ZcanClassicInitConfig};

        let (timing0, timing1) = zlg_timing("500K").unwrap();
        let cfg = ZcanChannelInitConfig {
            can_type: 0,
            config: ZcanChannelConfig {
                classic: ZcanClassicInitConfig {
                    acc_code: 0,
                    acc_mask: 0xFFFF_FFFF,
                    reserved: 0,
                    filter: 1,
                    timing0,
                    timing1,
                    mode: 0,
                },
            },
        };
        let classic = unsafe { cfg.config.classic };
        assert_eq!(std::mem::size_of::<ZcanChannelInitConfig>(), 32);
        assert_eq!(classic.timing0, timing0);
        assert_eq!(classic.timing1, timing1);
        assert_eq!(classic.acc_mask, 0xFFFF_FFFF);
    }

    #[test]
    fn zlg_bus_error_is_actionable_but_not_a_usb_disconnect() {
        use zcan_ffi::{ERROR_CAN_BUSERR, ERROR_CAN_BUSOFF, ZcanChannelStatus};
        let status = ZcanChannelStatus {
            reg_re_counter: 7,
            reg_te_counter: 31,
            ..Default::default()
        };
        let message = zcan_error_message(ERROR_CAN_BUSERR | ERROR_CAN_BUSOFF, Some(status));
        assert!(message.contains("CAN_H/CAN_L"));
        assert!(message.contains("Bus-Off"));
        assert!(message.contains("RXErr=7"));
        assert!(!zcan_error_is_connection_lost(
            ERROR_CAN_BUSERR | ERROR_CAN_BUSOFF
        ));
    }

    #[test]
    fn send_sequence_increments_id_and_little_endian_payload() {
        let mut source = frame(1);
        source.id = 0x7FE;
        source.data = vec![0xFF, 0x00];
        let mut job = PendingSendJob::sequence(source, 3, true, true).unwrap();
        let first = job.next_frame().unwrap();
        let second = job.next_frame().unwrap();
        let third = job.next_frame().unwrap();
        assert_eq!((first.id, first.data), (0x7FE, vec![0xFF, 0x00]));
        assert_eq!((second.id, second.data), (0x7FF, vec![0x00, 0x01]));
        assert_eq!((third.id, third.data), (0x000, vec![0x01, 0x01]));
        assert_eq!(job.remaining(), 0);
    }

    #[test]
    fn pending_send_queue_rejects_unbounded_work() {
        let mut queue = VecDeque::new();
        let job =
            PendingSendJob::sequence(frame(1), MAX_PENDING_SEND_FRAMES, false, false).unwrap();
        assert_eq!(
            enqueue_send_job(&mut queue, job),
            Ok(MAX_PENDING_SEND_FRAMES)
        );
        let extra = PendingSendJob::sequence(frame(1), 1, false, false).unwrap();
        assert!(enqueue_send_job(&mut queue, extra).is_err());
    }

    #[test]
    fn channel_set_rejects_duplicate_software_and_hardware_bindings() {
        let first = device("PCAN", 1);
        let mut duplicate_software = device("PCAN", 1);
        duplicate_software.channel_index = 1;
        assert!(validate_channel_set(&[first.clone(), duplicate_software]).is_err());

        let mut duplicate_hardware = first.clone();
        duplicate_hardware.sw_channel = 2;
        assert!(validate_channel_set(&[first, duplicate_hardware]).is_err());
    }

    #[test]
    fn stable_hardware_identity_survives_runtime_index_changes() {
        let mut first = device("USBCANFD-200U", 1);
        first.is_fd = true;
        first.hardware_id = "USBCANFD-200U:46716A0:0".into();

        let mut same_endpoint = first.clone();
        same_endpoint.sw_channel = 2;
        same_endpoint.device_index = 7;
        same_endpoint.channel_index = 1;

        let error = validate_channel_set(&[first, same_endpoint]).unwrap_err();
        assert!(error.contains("同一硬件端点"));
    }

    #[test]
    fn pcan_fd_accepts_complete_custom_timing_and_rejects_partial_text() {
        let mut config = device("PCAN", 1);
        config.is_fd = true;
        config.custom_bitrate = "f_clock=80000000,nom_brp=2,nom_tseg1=63,nom_tseg2=16,nom_sjw=16,data_brp=2,data_tseg1=15,data_tseg2=4,data_sjw=4".into();
        assert!(validate_device_config(&config).is_ok());

        config.custom_bitrate = "nominal 500K / data 2M".into();
        assert!(validate_device_config(&config).is_err());
    }

    #[test]
    fn capability_validation_rejects_incompatible_modes() {
        let mut classic = device("GCAN", 1);
        classic.fd_non_iso = true;
        assert!(validate_device_config(&classic).is_err());

        let mut fd = device("USBCANFD-200U", 1);
        fd.is_fd = true;
        fd.fd_non_iso = true;
        assert!(validate_device_config(&fd).is_ok());

        let mut pcan = device("PCAN", 1);
        pcan.listen_only = true;
        assert!(validate_device_config(&pcan).is_err());
    }

    #[test]
    fn hardware_timebase_preserves_device_deltas() {
        let mut clock = HardwareTimebase::new(1e-6, None);
        assert!((clock.map(1_000_000, 5.0) - 5.0).abs() < 1e-12);
        assert!((clock.map(1_250_000, 8.0) - 5.25).abs() < 1e-12);
    }

    #[test]
    fn hardware_timebase_extends_32_bit_wrap() {
        let mut clock = HardwareTimebase::new(1e-4, Some(32));
        let before_wrap = u32::MAX as u64 - 4;
        assert!((clock.map(before_wrap, 2.0) - 2.0).abs() < 1e-12);
        assert!((clock.map(5, 9.0) - 2.001).abs() < 1e-12);
    }

    #[test]
    fn event_queue_drops_capture_before_control_reserve_and_reports_it() {
        let (raw_tx, rx) = bounded(EVENT_QUEUE_CAPACITY);
        let sender = EventSender::new(raw_tx);
        for index in 0..(EVENT_QUEUE_CAPACITY - EVENT_QUEUE_CONTROL_RESERVE) {
            sender
                .send(Evt::Log(format!("queued control {index}")))
                .unwrap();
        }

        sender.send_frames(vec![frame(1), frame(1)]);
        assert_eq!(sender.dropped_frames.load(Ordering::Relaxed), 2);
        while rx.try_recv().is_ok() {}
        sender.report_health(3, 4, &CommandHealth::default(), 0);
        let health = rx.try_recv().unwrap();
        assert!(matches!(
            health,
            Evt::CaptureHealth {
                dropped_frames: 2,
                hardware_overruns: 3,
                hardware_errors: 4,
                ..
            }
        ));
    }

    #[test]
    fn command_queue_never_blocks_and_reports_rejection() {
        let (tx, _rx) = bounded(1);
        let health = CommandHealth::default();
        let sender = CommandSender {
            tx,
            health: health.clone(),
        };
        sender.send(Cmd::Start).unwrap();
        assert!(sender.send(Cmd::Stop).is_err());
        assert_eq!(health.rejected.load(Ordering::Relaxed), 1);
        assert_eq!(health.high_watermark.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn critical_command_waits_for_bounded_queue_space() {
        let (tx, rx) = bounded(1);
        let health = CommandHealth::default();
        let sender = CommandSender {
            tx,
            health: health.clone(),
        };
        sender.send(Cmd::Start).unwrap();
        let consumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            rx.recv().unwrap();
            rx.recv().unwrap();
        });
        sender
            .send_critical(Cmd::Shutdown, Duration::from_millis(250))
            .unwrap();
        consumer.join().unwrap();
        assert_eq!(health.rejected.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shutdown_signal_bypasses_a_full_command_queue() {
        let (tx, _rx) = bounded(1);
        let health = CommandHealth::default();
        let sender = CommandSender {
            tx,
            health: health.clone(),
        };
        sender.send(Cmd::Start).unwrap();
        assert!(sender.send_critical(Cmd::Shutdown, Duration::ZERO).is_err());
        assert!(health.shutdown_requested.load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "24-hour product gate; run through scripts/run-product-gates.ps1"]
    fn capture_queue_soak_has_no_hidden_loss() {
        let seconds = std::env::var("PCANWORK_SOAK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(24 * 60 * 60);
        let frames_per_second = std::env::var("PCANWORK_SOAK_FPS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(20_000);
        let batch_interval = Duration::from_millis(10);
        let frames_per_batch = (frames_per_second / 100).max(1) as usize;
        let (raw_tx, rx) = bounded(EVENT_QUEUE_CAPACITY);
        let sender = EventSender::new(raw_tx);
        let received = Arc::new(AtomicU64::new(0));
        let received_worker = received.clone();
        let consumer = std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                match event {
                    Evt::Frames(frames) => {
                        received_worker.fetch_add(frames.len() as u64, Ordering::Relaxed);
                    }
                    Evt::Frame(_) => {
                        received_worker.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });

        let started = Instant::now();
        let mut sent = 0u64;
        while started.elapsed() < Duration::from_secs(seconds) {
            sender.send_frames(vec![frame(1); frames_per_batch]);
            sent += frames_per_batch as u64;
            std::thread::sleep(batch_interval);
        }
        let dropped = sender.dropped_frames.load(Ordering::Relaxed);
        drop(sender);
        consumer.join().unwrap();

        assert_eq!(
            dropped, 0,
            "capture queue reported {dropped} dropped frames"
        );
        assert_eq!(
            received.load(Ordering::Relaxed),
            sent,
            "capture queue lost frames without an explicit gate failure"
        );
    }

    struct MockAdapter {
        fail_send: bool,
        sent: usize,
    }

    impl CanAdapter for MockAdapter {
        fn poll(&mut self, _out: &mut Vec<CanFrame>) -> PollReport {
            PollReport::default()
        }

        fn send(&mut self, _frame: &CanFrame) -> Result<(), String> {
            self.sent += 1;
            if self.fail_send {
                Err("mock send failure".into())
            } else {
                Ok(())
            }
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    fn frame(channel: u8) -> CanFrame {
        CanFrame {
            t: 0.0,
            ch: channel,
            tx: false,
            id: 0x123,
            ext: false,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data: vec![0xFF],
        }
    }

    #[test]
    fn send_rejects_missing_target_channel() {
        let mut adapters: Vec<(u8, Box<dyn CanAdapter>)> = vec![(
            1,
            Box::new(MockAdapter {
                fail_send: false,
                sent: 0,
            }),
        )];

        let error = send_on(&mut adapters, &frame(2)).unwrap_err();
        assert!(error.contains('2'));
    }

    #[test]
    fn pending_send_processing_is_bounded_per_controller_slice() {
        let mut queue = VecDeque::new();
        enqueue_send_job(
            &mut queue,
            PendingSendJob::sequence(frame(1), 100, false, false).unwrap(),
        )
        .unwrap();
        let mut adapters: Vec<(u8, Box<dyn CanAdapter>)> = vec![(
            1,
            Box::new(MockAdapter {
                fail_send: false,
                sent: 0,
            }),
        )];
        let (raw_tx, rx) = bounded(EVENT_QUEUE_CAPACITY);
        let events = EventSender::new(raw_tx);

        process_pending_sends(&mut queue, &mut adapters, &events, Instant::now());

        let emitted = rx
            .try_iter()
            .map(|event| match event {
                Evt::Frames(frames) => frames.len(),
                Evt::Frame(_) => 1,
                _ => 0,
            })
            .sum::<usize>();
        assert!(emitted > 0 && emitted <= SEND_FRAMES_PER_SLICE);
        assert_eq!(pending_send_frames(&queue), 100 - emitted as u64);
    }

    #[test]
    fn online_playback_does_not_emit_frame_after_hardware_failure() {
        let mut adapters: Vec<(u8, Box<dyn CanAdapter>)> = vec![(
            1,
            Box::new(MockAdapter {
                fail_send: true,
                sent: 0,
            }),
        )];
        let (raw_tx, rx) = bounded(8);
        let tx = EventSender::new(raw_tx);

        assert!(!emit_playback_frame(&mut adapters, &tx, frame(1), true));
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|event| matches!(event, Evt::Log(_))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Evt::PlaybackFrame(_)))
        );
    }

    #[test]
    fn ota_ack_requires_the_expected_channel() {
        let ack = OtaAck::XcpAck {
            response: OtaResponseId::Exact(0x123),
        };
        let response = frame(1);

        assert!(!ota_ack_matches_on_channel(ack, 2, 1, &response));
        assert!(ota_ack_matches_on_channel(ack, 2, 2, &response));
    }

    #[test]
    fn simulation_scheduler_atomically_merges_due_signals_and_preserves_others() {
        let text = "VERSION \"\"\nBO_ 256 SimFrame: 2 ECU\n SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let path = std::env::temp_dir().join("pcanwork_sim_atomic_scheduler.dbc");
        std::fs::write(&path, text).unwrap();
        let dbc = DbcDb::load(&path.to_string_lossy()).unwrap();
        let now = Instant::now();
        let mut periodic = SimPeriodic {
            frame: CanFrame {
                t: 0.0,
                ch: 1,
                tx: true,
                id: 0x100,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0, 0],
            },
            dbc: Some(dbc),
            dbc_id: 0x100,
            generators: vec![
                SimSignalState {
                    config: SimSignalGenerator {
                        signal: "A".into(),
                        mode: SimGeneratorMode::Ramp {
                            min: 10.0,
                            max: 20.0,
                            step: 2.0,
                        },
                        period_ms: 10,
                    },
                    next: now,
                    tick: 0,
                },
                SimSignalState {
                    config: SimSignalGenerator {
                        signal: "B".into(),
                        mode: SimGeneratorMode::Constant { value: 77.0 },
                        period_ms: 100,
                    },
                    next: now,
                    tick: 0,
                },
            ],
            failed: false,
        };

        assert_eq!(update_sim_periodic(&mut periodic, now), Ok(true));
        assert_eq!(periodic.frame.data, [10, 77]);
        assert_eq!(
            update_sim_periodic(&mut periodic, now + Duration::from_millis(10)),
            Ok(true)
        );
        assert_eq!(periodic.frame.data, [12, 77]);
        let _ = std::fs::remove_file(path);
    }
}

fn run_ota_job(
    adapters: &mut Vec<(u8, Box<dyn CanAdapter>)>,
    evt_tx: &EventSender,
    start: Instant,
    buf: &mut Vec<CanFrame>,
    job: OtaJob,
) {
    if adapters.is_empty() {
        let _ = evt_tx.send(Evt::OtaProgress(
            0,
            job.steps.len(),
            "OTA failed: no device connected".into(),
        ));
        return;
    }

    let total = job.steps.len();
    OTA_CANCEL.store(false, Ordering::Relaxed);
    let _ = evt_tx.send(Evt::OtaProgress(0, total, format!("{} started", job.name)));

    for (idx, step) in job.steps.into_iter().enumerate() {
        if OTA_CANCEL.load(Ordering::Relaxed) {
            let _ = evt_tx.send(Evt::OtaProgress(
                idx,
                total,
                format!("{} cancelled", job.name),
            ));
            return;
        }
        let mut ok = false;
        let timeout = Duration::from_millis(step.timeout_ms.max(job.timeout_ms).max(1));
        let retries = step.retries.max(job.retries);
        for attempt in 0..=retries {
            let mut frame = step.frame.clone();
            if OTA_CANCEL.load(Ordering::Relaxed) {
                let _ = evt_tx.send(Evt::OtaProgress(
                    idx,
                    total,
                    format!("{} cancelled", job.name),
                ));
                return;
            }
            frame.t = start.elapsed().as_secs_f64();
            frame.tx = true;
            let expected_channel = frame.ch;
            match send_on(adapters, &frame) {
                Ok(used) => {
                    frame.ch = used;
                    let _ = evt_tx.send(Evt::Frame(frame));
                }
                Err(e) => {
                    let _ = evt_tx.send(Evt::OtaProgress(
                        idx,
                        total,
                        format!("OTA send failed: {e}"),
                    ));
                    return;
                }
            }

            if poll_for_ota_ack(adapters, evt_tx, buf, step.ack, expected_channel, timeout) {
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
            let _ = evt_tx.send(Evt::OtaProgress(
                idx,
                total,
                format!("{} failed at step {}", job.name, idx + 1),
            ));
            return;
        }

        let _ = evt_tx.send(Evt::OtaProgress(
            idx + 1,
            total,
            format!("{} {}/{}", job.name, idx + 1, total),
        ));
    }

    let _ = evt_tx.send(Evt::OtaProgress(
        total,
        total,
        format!("{} complete", job.name),
    ));
}

const CONNECTION_LOSS_CONFIRMATIONS: u8 = 3;
const CONNECTION_LOSS_WINDOW: Duration = Duration::from_secs(2);

fn should_log_send_error(
    recent: &mut HashMap<u8, (String, Instant)>,
    channel: u8,
    message: &str,
) -> bool {
    let should_log = recent.get(&channel).is_none_or(|(previous, when)| {
        previous != message || when.elapsed() >= Duration::from_secs(1)
    });
    if should_log {
        recent.insert(channel, (message.to_string(), Instant::now()));
    }
    should_log
}

fn connection_loss_confirmed(
    streaks: &mut HashMap<u8, (u8, Instant)>,
    channel: u8,
    connection_lost: bool,
) -> bool {
    if !connection_lost {
        // Most adapters only perform an expensive health probe periodically;
        // empty receive polls between two probes are not positive proof that
        // the device recovered. Keep the fault streak, but expire it below if
        // the next explicit fault is outside the confirmation window.
        return false;
    }
    let now = Instant::now();
    let streak = streaks.entry(channel).or_insert((0, now));
    if now.duration_since(streak.1) > CONNECTION_LOSS_WINDOW {
        streak.0 = 0;
    }
    streak.0 = streak.0.saturating_add(1);
    streak.1 = now;
    streak.0 >= CONNECTION_LOSS_CONFIRMATIONS
}

fn controller(cmd_rx: EventReceiver<Cmd>, evt_tx: EventSender, command_health: CommandHealth) {
    let start = Instant::now();
    let mut last_health_report = Instant::now();
    let mut hardware_overruns = 0u64;
    let mut hardware_errors = 0u64;
    let mut last_adapter_error: HashMap<u8, (String, Instant)> = HashMap::new();
    let mut last_send_error: HashMap<u8, (String, Instant)> = HashMap::new();
    let mut connection_loss_streak: HashMap<u8, (u8, Instant)> = HashMap::new();
    let mut adapters: Vec<(u8, Box<dyn CanAdapter>)> = Vec::new();
    let mut running = false;
    let mut periodics: HashMap<u64, Periodic> = HashMap::new();
    let mut dynamic_periodics: HashMap<u64, DynamicPeriodic> = HashMap::new();
    let mut sim_periodics: Vec<SimPeriodic> = Vec::new();
    let mut pending_sends: VecDeque<PendingSendJob> = VecDeque::new();
    let mut buf: Vec<CanFrame> = Vec::with_capacity(1024);
    let mut playback: Option<Playback> = None;

    loop {
        if command_health.shutdown_requested.load(Ordering::Acquire) {
            pending_sends.clear();
            adapters.clear();
            clear_vci_device_registry();
            periodics.clear();
            dynamic_periodics.clear();
            sim_periodics.clear();
            let _ = evt_tx.send_critical(Evt::ShutdownFinished, Duration::from_secs(1));
            return;
        }
        for _ in 0..64 {
            if command_health.shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            match cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    Cmd::Connect => {
                        evt_tx.begin_timestamp_session();
                        pending_sends.clear();
                        hardware_overruns = 0;
                        hardware_errors = 0;
                        last_adapter_error.clear();
                        connection_loss_streak.clear();
                        match PcanBus::open(start) {
                            Ok(p) => {
                                let n = p.name().to_string();
                                adapters = vec![(1, Box::new(p))];
                                let _ = evt_tx.send(Evt::Log(format!("已连接真实 PCAN 卡: {n}")));
                                let _ = evt_tx.send(Evt::Connected {
                                    channels: vec![1],
                                    name: n,
                                    error: None,
                                });
                            }
                            Err(e) => {
                                let _ = evt_tx.send(Evt::Log(format!("连接 PCAN 卡失败: {e}")));
                                let _ = evt_tx.send(Evt::Connected {
                                    channels: Vec::new(),
                                    name: String::new(),
                                    error: Some(e),
                                });
                            }
                        }
                    }
                    Cmd::ConnectConfig(cfg) => {
                        evt_tx.begin_timestamp_session();
                        pending_sends.clear();
                        hardware_overruns = 0;
                        hardware_errors = 0;
                        last_adapter_error.clear();
                        connection_loss_streak.clear();
                        dynamic_periodics.clear();
                        connect_channels(
                            &mut adapters,
                            &mut running,
                            &mut periodics,
                            &evt_tx,
                            start,
                            vec![cfg],
                        );
                    }
                    Cmd::ConnectChannels(cfgs) => {
                        evt_tx.begin_timestamp_session();
                        pending_sends.clear();
                        hardware_overruns = 0;
                        hardware_errors = 0;
                        last_adapter_error.clear();
                        connection_loss_streak.clear();
                        dynamic_periodics.clear();
                        connect_channels(
                            &mut adapters,
                            &mut running,
                            &mut periodics,
                            &evt_tx,
                            start,
                            cfgs,
                        );
                    }
                    Cmd::Disconnect => {
                        pending_sends.clear();
                        adapters.clear();
                        running = false;
                        periodics.clear();
                        dynamic_periodics.clear();
                        connection_loss_streak.clear();
                        let _ = evt_tx.send(Evt::Running(false));
                        let _ = evt_tx.send(Evt::Connected {
                            channels: Vec::new(),
                            name: String::new(),
                            error: None,
                        });
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
                        if !pending_sends.is_empty() {
                            pending_sends.clear();
                            let _ = evt_tx.send(Evt::Log("已取消待发送任务".into()));
                        }
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
                                    echo.ch = used;
                                    let _ = evt_tx.send(Evt::Frame(echo));
                                }
                                Err(e) => {
                                    let _ = evt_tx.send(Evt::Log(format!("发送失败: {e}")));
                                }
                            }
                        }
                    }
                    Cmd::SendSequence {
                        frame,
                        count,
                        id_increment,
                        data_increment,
                    } => {
                        if adapters.is_empty() {
                            let _ =
                                evt_tx.send(Evt::Log("发送失败: 当前没有已连接的 CAN 通道".into()));
                        } else if let Some(job) =
                            PendingSendJob::sequence(frame, count, id_increment, data_increment)
                        {
                            match enqueue_send_job(&mut pending_sends, job) {
                                Ok(queued) => {
                                    let _ = evt_tx
                                        .send(Evt::Log(format!("已加入发送队列: {queued} 帧")));
                                }
                                Err(error) => {
                                    let _ = evt_tx
                                        .send(Evt::Log(format!("发送任务未加入队列: {error}")));
                                }
                            }
                        }
                    }
                    Cmd::SendBatch {
                        frames,
                        repeat,
                        ack,
                    } => {
                        let result = if adapters.is_empty() {
                            Err("当前没有已连接的 CAN 通道".to_string())
                        } else if let Some(job) = PendingSendJob::batch(frames, repeat.max(1)) {
                            enqueue_send_job(&mut pending_sends, job).map_err(str::to_string)
                        } else {
                            Err("批量发送列表不能为空".to_string())
                        };
                        match &result {
                            Ok(queued) => {
                                let _ = evt_tx
                                    .send(Evt::Log(format!("批量发送已加入队列: {queued} 帧")));
                            }
                            Err(error) => {
                                let _ = evt_tx
                                    .send(Evt::Log(format!("批量发送任务未加入队列: {error}")));
                            }
                        }
                        if let Some(ack) = ack {
                            let _ = ack.send(result);
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
                            dynamic_periodics.remove(&handle);
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
                    Cmd::SetDynamicPeriodic { handle, config } => {
                        periodics.remove(&handle);
                        if let Some(config) = config.filter(|cfg| cfg.repeat != 0) {
                            dynamic_periodics.insert(
                                handle,
                                DynamicPeriodic {
                                    sent: config.start_sent,
                                    config,
                                    next: Instant::now(),
                                },
                            );
                        } else {
                            dynamic_periodics.remove(&handle);
                        }
                    }
                    Cmd::SetSimulationPeriodics(configs) => {
                        let now = Instant::now();
                        sim_periodics = configs
                            .into_iter()
                            .map(|config| SimPeriodic {
                                frame: config.frame,
                                dbc: config.dbc,
                                dbc_id: config.dbc_id,
                                generators: config
                                    .generators
                                    .into_iter()
                                    .map(|config| SimSignalState {
                                        config,
                                        next: now,
                                        tick: 0,
                                    })
                                    .collect(),
                                failed: false,
                            })
                            .collect();
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
                    Cmd::PlaybackPlay {
                        online,
                        speed,
                        loop_play,
                    } => {
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
                                    let frame = pb.frames[pb.idx].clone();
                                    if !emit_playback_frame(
                                        &mut adapters,
                                        &evt_tx,
                                        frame,
                                        pb.online,
                                    ) {
                                        break;
                                    }
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
                            && !pb.frames.is_empty()
                        {
                            let i = ((frac.clamp(0.0, 1.0) * pb.frames.len() as f64) as usize)
                                .min(pb.frames.len() - 1);
                            pb.idx = i;
                            pb.base = Instant::now();
                            pb.base_t = pb.frames[i].t;
                            let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), pb.playing));
                        }
                    }
                    Cmd::Shutdown => {
                        pending_sends.clear();
                        adapters.clear();
                        clear_vci_device_registry();
                        periodics.clear();
                        dynamic_periodics.clear();
                        sim_periodics.clear();
                        let _ = evt_tx.send_critical(Evt::ShutdownFinished, Duration::from_secs(1));
                        return;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    adapters.clear();
                    clear_vci_device_registry();
                    return;
                }
            }
        }

        if command_health.shutdown_requested.load(Ordering::Acquire) {
            continue;
        }

        process_pending_sends(&mut pending_sends, &mut adapters, &evt_tx, start);

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
            let mut sent_frames = Vec::with_capacity(due.len());
            for mut f in due {
                match send_on(&mut adapters, &f) {
                    Ok(used) => {
                        f.ch = used;
                        sent_frames.push(f);
                    }
                    Err(error) => {
                        if should_log_send_error(&mut last_send_error, f.ch, &error) {
                            let _ = evt_tx.send(Evt::Log(format!("周期发送失败: {error}")));
                        }
                    }
                }
            }
            if !sent_frames.is_empty() {
                let _ = evt_tx.send(Evt::Frames(sent_frames));
            }
            for h in done {
                periodics.remove(&h);
                let _ = evt_tx.send(Evt::PeriodicDone(h));
            }
        }

        if !dynamic_periodics.is_empty() && !adapters.is_empty() {
            let now = Instant::now();
            let mut due = Vec::new();
            let mut done = Vec::new();
            for (handle, periodic) in dynamic_periodics.iter_mut() {
                if now < periodic.next {
                    continue;
                }
                periodic.next += Duration::from_millis(periodic.config.period_ms.max(1));
                if periodic.next <= now {
                    periodic.next = now + Duration::from_millis(periodic.config.period_ms.max(1));
                }
                match build_dynamic_frame(*handle, periodic) {
                    Ok((frame, signal_values)) => {
                        due.push((*handle, frame, signal_values, periodic.sent + 1))
                    }
                    Err(error) => {
                        let _ = evt_tx.send(Evt::Log(format!("动态周期发送编码失败: {error}")));
                        done.push(*handle);
                    }
                }
                periodic.sent += 1;
                if periodic.config.repeat > 0 && periodic.sent >= periodic.config.repeat as u64 {
                    done.push(*handle);
                }
            }
            for (handle, mut frame, signal_values, sent) in due {
                frame.t = start.elapsed().as_secs_f64();
                frame.tx = true;
                match send_on(&mut adapters, &frame) {
                    Ok(channel) => {
                        frame.ch = channel;
                        let _ = evt_tx.send(Evt::DynamicUpdate {
                            handle,
                            data: frame.data.clone(),
                            signal_values,
                            sent,
                        });
                        let _ = evt_tx.send(Evt::Frame(frame));
                    }
                    Err(error) => {
                        if should_log_send_error(&mut last_send_error, frame.ch, &error) {
                            let _ = evt_tx.send(Evt::Log(format!("动态周期发送失败: {error}")));
                        }
                        done.push(handle);
                    }
                }
            }
            done.sort_unstable();
            done.dedup();
            for handle in done {
                dynamic_periodics.remove(&handle);
                let _ = evt_tx.send(Evt::PeriodicDone(handle));
            }
        }

        if !sim_periodics.is_empty() && !adapters.is_empty() {
            let now = Instant::now();
            let mut due = Vec::new();
            for periodic in &mut sim_periodics {
                match update_sim_periodic(periodic, now) {
                    Ok(true) => {
                        periodic.failed = false;
                        let mut frame = periodic.frame.clone();
                        frame.t = start.elapsed().as_secs_f64();
                        frame.tx = true;
                        due.push(frame);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        if !periodic.failed {
                            let _ = evt_tx.send(Evt::Log(format!(
                                "仿真发生器编码失败: CAN{} 0x{:X}: {error}",
                                periodic.frame.ch, periodic.frame.id
                            )));
                        }
                        periodic.failed = true;
                    }
                }
            }
            for mut frame in due {
                match send_on(&mut adapters, &frame) {
                    Ok(channel) => {
                        frame.ch = channel;
                        let _ = evt_tx.send(Evt::Frame(frame));
                    }
                    Err(error) => {
                        let _ = evt_tx.send(Evt::Log(format!(
                            "仿真发生器发送失败: CAN{} 0x{:X}: {error}",
                            frame.ch, frame.id
                        )));
                    }
                }
            }
        }

        let mut pb_active = false;
        if let Some(pb) = playback.as_mut()
            && pb.playing
            && !pb.paused
        {
            pb_active = true;
            if pb.speed <= 0.0 {
                for _ in 0..500 {
                    if pb.idx >= pb.frames.len() {
                        break;
                    }
                    let frame = pb.frames[pb.idx].clone();
                    if !emit_playback_frame(&mut adapters, &evt_tx, frame, pb.online) {
                        pb.playing = false;
                        break;
                    }
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
                        let frame = pb.frames[pb.idx].clone();
                        if !emit_playback_frame(&mut adapters, &evt_tx, frame, pb.online) {
                            pb.playing = false;
                            break;
                        }
                        pb.idx += 1;
                    } else {
                        break;
                    }
                }
            }
            if pb.idx >= pb.frames.len() {
                if pb.loop_play && !pb.frames.is_empty() {
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
                let _ = evt_tx.send(Evt::Playback(pb.idx, pb.frames.len(), pb.playing));
            }
        }

        if running {
            let mut fatal_disconnect: Option<(u8, String)> = None;
            for (ch, a) in adapters.iter_mut() {
                buf.clear();
                let report = a.poll(&mut buf);
                hardware_overruns = hardware_overruns.saturating_add(report.receive_overruns);
                hardware_errors = hardware_errors.saturating_add(report.driver_errors);
                let connection_lost = report.connection_lost;
                let loss_reason = report
                    .message
                    .clone()
                    .unwrap_or_else(|| "设备连接已丢失".into());
                if let Some(message) = report.message {
                    let should_log = last_adapter_error.get(ch).is_none_or(|(previous, when)| {
                        previous != &message || when.elapsed() >= Duration::from_secs(1)
                    });
                    if should_log {
                        let _ = evt_tx.send(Evt::Log(format!("CAN{ch}: {message}")));
                        last_adapter_error.insert(*ch, (message, Instant::now()));
                    }
                }
                if fatal_disconnect.is_none()
                    && connection_loss_confirmed(&mut connection_loss_streak, *ch, connection_lost)
                {
                    fatal_disconnect = Some((*ch, loss_reason));
                }
                for f in &mut buf {
                    f.ch = *ch;
                }
                for batch in buf.chunks(128) {
                    evt_tx.send_frames(batch.to_vec());
                }
            }
            if let Some((channel, reason)) = fatal_disconnect {
                pending_sends.clear();
                adapters.clear();
                clear_vci_device_registry();
                running = false;
                periodics.clear();
                dynamic_periodics.clear();
                connection_loss_streak.clear();
                let _ = evt_tx.send(Evt::Running(false));
                let _ = evt_tx.send(Evt::Connected {
                    channels: Vec::new(),
                    name: String::new(),
                    error: Some(reason.clone()),
                });
                let _ = evt_tx.send(Evt::Log(format!(
                    "CAN{channel} 设备连接已丢失，采集已安全停止: {reason}"
                )));
            }
            if last_health_report.elapsed() >= Duration::from_millis(250) {
                evt_tx.report_health(
                    hardware_overruns,
                    hardware_errors,
                    &command_health,
                    cmd_rx.len(),
                );
                last_health_report = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(1));
        } else if pb_active {
            std::thread::sleep(Duration::from_millis(1));
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
