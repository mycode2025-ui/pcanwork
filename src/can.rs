//!

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

#[derive(Clone, Debug)]
pub struct CanFrame {
    pub t: f64,
    pub ch: u8,
    pub tx: bool,
    pub id: u32,
    pub ext: bool,
    pub fd: bool,      // CAN FD
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
    pub device_index: u32,
    pub channel_index: u32,
    pub baud: String,
    pub data_baud: String,
    pub termination: bool,
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

pub trait CanAdapter: Send {
    fn poll(&mut self, out: &mut Vec<CanFrame>);
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
        other => return Err(format!("CAN FD 不支持的仲裁速率: {other}（支持 1M/500K/250K/125K，或直接填完整 f_clock 串）")),
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
        other => return Err(format!("CAN FD 不支持的数据速率: {other}（支持 8M/5M/4M/2M/1M/500K，或直接填完整 f_clock 串）")),
    };
    Ok(format!("f_clock=80000000,{nom},{dat}"))
}

pub struct PcanBus {
    lib: libloading::Library,
    channel: u16,
    is_fd: bool,
    start: Instant,
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

    pub fn open_cfg(start: Instant, cfg: &DeviceConfig) -> Result<Self, String> {
        use pcan_ffi::*;
        if !cfg.is_fd {
            return Self::open_config(start, cfg.channel_index, &cfg.baud);
        }
        let Some(channel) = pcan_usb_channel(cfg.channel_index) else {
            return Err(format!(
                "PCAN only supports configured USB channel index 0..15, got {}",
                cfg.channel_index
            ));
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

    pub const EFF: u32 = 0x8000_0000;
    pub const RTR: u32 = 0x4000_0000;
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
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ZcanChannelInitConfig {
        pub can_type: u32,
        pub raw: [u8; 28],
    }
}

pub struct ZcanFdBus {
    lib: libloading::Library,
    dev: usize,
    ch: usize,
    fd_capable: bool,
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

            let close_dev = || {
                if let Ok(close) = lib.get::<FnCloseDevice>(b"ZCAN_CloseDevice\0") {
                    let _ = close(dev);
                }
            };

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
                let mode = if net_tcp {
                    set("work_mode", if cfg.net_server { "1" } else { "0" })
                } else {
                    set("local_port", &cfg.port)
                };
                mode.and_then(|_| set("ip", &cfg.ip))
                    .and_then(|_| set("work_port", &cfg.port))
            } else if is_usbcanfd {
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
                set("baud_rate", &baud_to_bps(&cfg.baud).to_string())
            };
            if let Err(e) = cfg_res {
                close_dev();
                return Err(e);
            }
            drop(setval);

            let init: libloading::Symbol<FnInitCan> = match lib.get(b"ZCAN_InitCAN\0") {
                Ok(s) => s,
                Err(e) => {
                    close_dev();
                    return Err(format!("ZCAN_InitCAN 未找到: {e}"));
                }
            };
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

            if is_usbcanfd && !is_net
                && let Ok(setres) = lib.get::<FnSetValue>(b"ZCAN_SetValue\0") {
                    let path = CString::new(format!("{}/initenal_resistance", cfg.channel_index))
                        .unwrap();
                    let v = CString::new(if cfg.termination { "1" } else { "0" }).unwrap();
                    let _ = setres(dev, path.as_ptr(), v.as_ptr());
                }

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
    ConnectChannels(Vec<DeviceConfig>),
    Disconnect,
    Start,
    Stop,
    SendOnce(CanFrame),
    OtaRun(OtaJob),
    SetPeriodic {
        handle: u64,
        frame: CanFrame,
        period_ms: u64,
        repeat: i64,
        enable: bool,
    },
    PlaybackLoad(Vec<CanFrame>),
    PlaybackPlay { online: bool, speed: f64, loop_play: bool },
    PlaybackStep,
    PlaybackPause,
    PlaybackCancel,
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
    Connected(bool, String),
    Running(bool),
    Playback(usize, usize, bool),
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
    loop_play: bool,
}

struct Periodic {
    frame: CanFrame,
    period: Duration,
    next: Instant,
    remaining: i64,
}

pub fn spawn() -> (Sender<Cmd>, Receiver<Evt>) {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (evt_tx, evt_rx) = channel::<Evt>();
    std::thread::spawn(move || controller(cmd_rx, evt_tx));
    (cmd_tx, evt_rx)
}

fn open_adapter(start: Instant, cfg: &DeviceConfig) -> Result<Box<dyn CanAdapter>, String> {
    let device = cfg.device_type.trim().to_ascii_uppercase();
    if device == "VIRTUAL" || device == "SIM" {
        Err("不支持的设备类型: 虚拟总线已移除".into())
    } else if device == "PCAN" {
        PcanBus::open_cfg(start, cfg)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "GCAN" {
        VciBus::open(start, cfg, &["ECanVci64.dll", "ECanVci.dll"], "", 4)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if device == "ZHCX" || device == "ZHCXCAN" {
        VciBus::open(start, cfg, &["ControlCAN.dll"], "VCI_", 4)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if zcan_resolve(&cfg.device_type).is_some() {
        ZcanFdBus::open(start, cfg).map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else if let Some(dtype) = zlg_device_type(&cfg.device_type) {
        VciBus::open(start, cfg, &["ControlCAN.dll"], "VCI_", dtype)
            .map(|b| Box::new(b) as Box<dyn CanAdapter>)
    } else {
        Err(format!("未知设备类型: {}", cfg.device_type))
    }
}

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
                                    echo.ch = used;
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
                        adapters.clear();
                        return;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    adapters.clear();
                    return;
                }
            }
        }

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
                    f.ch = used;
                    let _ = evt_tx.send(Evt::Frame(f));
                }
            }
            for h in done {
                periodics.remove(&h);
                let _ = evt_tx.send(Evt::PeriodicDone(h));
            }
        }

        let mut pb_active = false;
        if let Some(pb) = playback.as_mut()
            && pb.playing && !pb.paused {
                pb_active = true;
                if pb.speed <= 0.0 {
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
