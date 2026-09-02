//! Vector BLF（二进制日志）读写。布局对照 python-can 的 blf.py（事实标准）。
//!
//! 读：解析顶层 LOG_CONTAINER（无压缩 / zlib），容器内解 CAN_MESSAGE / CAN_MESSAGE2 / CAN_FD_MESSAGE。
//! 写：CAN/CAN FD 帧写成单个无压缩 LOG_CONTAINER 内的 CAN_MESSAGE / CAN_FD_MESSAGE 对象（CANoe/ZXDoc 可打开）。

use crate::can::CanFrame;
use chrono::{DateTime, Datelike, Local, Timelike};
use std::io::{Read, Seek, SeekFrom, Write};

const FILE_SIG: &[u8; 4] = b"LOGG";
const OBJ_SIG: &[u8; 4] = b"LOBJ";
const FILE_HEADER_SIZE: u32 = 144;

const OBJ_CAN_MESSAGE: u32 = 1;
const OBJ_LOG_CONTAINER: u32 = 10;
const OBJ_CAN_MESSAGE2: u32 = 86;
const OBJ_CAN_FD_MESSAGE: u32 = 100;

const NO_COMPRESSION: u16 = 0;
const ZLIB_DEFLATE: u16 = 2;

const CAN_ID_EXT: u32 = 0x8000_0000;
const STREAM_CONTAINER_TARGET: usize = 256 * 1024;

// ---- 小端读取助手 ----
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// 把对象头 flags + 原始时间戳换算成秒。
fn ts_to_secs(flags: u32, ts: u64) -> f64 {
    if flags & 0x2 != 0 {
        ts as f64 / 1e9 // ONE_NANS
    } else if flags & 0x1 != 0 {
        ts as f64 / 1e5 // TEN_MICS = 10µs
    } else {
        ts as f64 / 1e9
    }
}

/// 读取 BLF 文件 → 帧序列（时间为文件内相对秒）。
pub fn read(path: &str) -> Result<Vec<CanFrame>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    if raw.len() < FILE_HEADER_SIZE as usize || &raw[0..4] != FILE_SIG {
        return Err("不是合法的 BLF 文件（缺 LOGG 头）".into());
    }
    let mut frames = Vec::new();
    let mut pos = FILE_HEADER_SIZE as usize;

    // 顶层：一串 LOG_CONTAINER 对象
    while pos + 16 <= raw.len() {
        if &raw[pos..pos + 4] != OBJ_SIG {
            break;
        }
        let header_size = u16le(&raw, pos + 4) as usize;
        let object_size = u32le(&raw, pos + 8) as usize;
        let object_type = u32le(&raw, pos + 12);
        // object_size < 16 会让 pos 不前进(死循环)且不足以容纳基础头
        if object_size < header_size || object_size < 16 || pos + object_size > raw.len() {
            break;
        }

        if object_type == OBJ_LOG_CONTAINER {
            // 容器头 = base(16)+LOG_CONTAINER(16)，不足 32 字节则畸形，停止
            if object_size < 32 {
                break;
            }
            // 容器：base(16) + LOG_CONTAINER(16) + 压缩载荷
            let method = u16le(&raw, pos + 16);
            let uncompressed_size = u32le(&raw, pos + 24) as usize;
            let payload = &raw[pos + 32..pos + object_size];
            let data = match method {
                NO_COMPRESSION => payload.to_vec(),
                ZLIB_DEFLATE => {
                    let mut d = flate2::read::ZlibDecoder::new(payload);
                    let mut out = Vec::with_capacity(uncompressed_size);
                    d.read_to_end(&mut out)
                        .map_err(|e| format!("zlib 解压失败: {e}"))?;
                    out
                }
                _ => return Err(format!("不支持的压缩方式: {method}")),
            };
            parse_container(&data, &mut frames);
        }

        // 推进并 4 字节对齐
        pos += object_size;
        pos = (pos + 3) & !3;
    }
    Ok(frames)
}

/// 解析一个（解压后的）容器载荷中的对象流。
fn parse_container(data: &[u8], out: &mut Vec<CanFrame>) {
    let mut pos = 0usize;
    while pos + 16 <= data.len() {
        if &data[pos..pos + 4] != OBJ_SIG {
            break;
        }
        let header_size = u16le(data, pos + 4) as usize;
        let object_size = u32le(data, pos + 8) as usize;
        let object_type = u32le(data, pos + 12);
        if object_size < header_size || object_size < 16 || pos + object_size > data.len() {
            break;
        }
        // 完整 v1 头(flags@+16, ts@+24)需要 ≥32 字节；不足则跳过该对象(不读越界)
        if object_size < 32 {
            pos += object_size;
            pos = (pos + 3) & !3;
            continue;
        }
        // V1/V2 头：flags 在 +16，timestamp 在 +24（两种版本一致）
        let flags = u32le(data, pos + 16);
        let ts = u64le(data, pos + 24);
        let t = ts_to_secs(flags, ts);
        let payload = pos + header_size;

        match object_type {
            OBJ_CAN_MESSAGE | OBJ_CAN_MESSAGE2
                if payload + 16 <= pos + object_size => {
                    let channel = u16le(data, payload);
                    let mflags = data[payload + 2];
                    let dlc = data[payload + 3] as usize;
                    let can_id = u32le(data, payload + 4);
                    let ext = can_id & CAN_ID_EXT != 0;
                    let id = can_id & 0x1FFF_FFFF;
                    let len = dlc.min(8);
                    let bytes = data[payload + 8..payload + 8 + len].to_vec();
                    out.push(CanFrame {
                        t,
                        ch: channel as u8,
                        tx: mflags & 0x1 != 0,
                        id,
                        ext,
                        fd: false,
                        brs: false,
                        remote: mflags & 0x80 != 0, // 远程帧标志(本工具约定 bit7；正常数据帧此位=0)
                        error: false,
                        data: bytes,
                    });
                }
            OBJ_CAN_FD_MESSAGE
                // CAN_FD_MSG_STRUCT "<H B B L L B B B B L 64s>" = 84 字节：
                // channel(2) flags(1) dlc(1) id(4) frameLen(4) bitCount(1) fdFlags(1)
                // validBytes(1) reserved(1) reserved(4) data[64]@offset 20
                if payload + 84 <= pos + object_size => {
                    let channel = u16le(data, payload);
                    let mflags = data[payload + 2];
                    let can_id = u32le(data, payload + 4);
                    let fd_flags = data[payload + 13];
                    let valid = data[payload + 14] as usize;
                    let ext = can_id & CAN_ID_EXT != 0;
                    let id = can_id & 0x1FFF_FFFF;
                    let len = valid.min(64);
                    let bytes = data[payload + 20..payload + 20 + len].to_vec();
                    out.push(CanFrame {
                        t,
                        ch: channel as u8,
                        tx: mflags & 0x1 != 0,
                        id,
                        ext,
                        fd: true,
                        brs: fd_flags & 0x2 != 0,
                        remote: false,
                        error: false,
                        data: bytes,
                    });
                }
            _ => {} // 其它对象类型跳过
        }

        pos += object_size;
        pos = (pos + 3) & !3;
    }
}

/// CAN FD 数据长度 → DLC 码（0..15，对应 0,1,..8,12,16,20,24,32,48,64）。
fn fd_dlc(len: usize) -> u8 {
    match len {
        0..=8 => len as u8,
        9..=12 => 9,
        13..=16 => 10,
        17..=20 => 11,
        21..=24 => 12,
        25..=32 => 13,
        33..=48 => 14,
        _ => 15, // 49..=64
    }
}

fn append_frame_object(payload: &mut Vec<u8>, f: &CanFrame) {
    let can_id = (f.id & 0x1FFF_FFFF) | if f.ext { CAN_ID_EXT } else { 0 };
    if f.fd {
        let obj_size: u32 = 16 + 16 + 84;
        payload.extend_from_slice(OBJ_SIG);
        payload.extend_from_slice(&32u16.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&obj_size.to_le_bytes());
        payload.extend_from_slice(&OBJ_CAN_FD_MESSAGE.to_le_bytes());
        payload.extend_from_slice(&2u32.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&((f.t * 1e9) as u64).to_le_bytes());
        let len = f.data.len().min(64);
        payload.extend_from_slice(&(f.ch as u16).to_le_bytes());
        payload.push(if f.tx { 0x1 } else { 0x0 });
        payload.push(fd_dlc(len));
        payload.extend_from_slice(&can_id.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0);
        payload.push(0x1 | if f.brs { 0x2 } else { 0x0 });
        payload.push(len as u8);
        payload.push(0);
        payload.extend_from_slice(&0u32.to_le_bytes());
        let mut data = [0u8; 64];
        data[..len].copy_from_slice(&f.data[..len]);
        payload.extend_from_slice(&data);
        return;
    }

    let obj_size: u32 = 16 + 16 + 16;
    payload.extend_from_slice(OBJ_SIG);
    payload.extend_from_slice(&32u16.to_le_bytes());
    payload.extend_from_slice(&1u16.to_le_bytes());
    payload.extend_from_slice(&obj_size.to_le_bytes());
    payload.extend_from_slice(&OBJ_CAN_MESSAGE.to_le_bytes());
    payload.extend_from_slice(&2u32.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&((f.t * 1e9) as u64).to_le_bytes());
    payload.extend_from_slice(&(f.ch as u16).to_le_bytes());
    payload.push((if f.tx { 0x1 } else { 0x0 }) | (if f.remote { 0x80 } else { 0x0 }));
    let len = f.data.len().min(8);
    payload.push(len as u8);
    payload.extend_from_slice(&can_id.to_le_bytes());
    let mut data = [0u8; 8];
    data[..len].copy_from_slice(&f.data[..len]);
    payload.extend_from_slice(&data);
}

fn build_container(payload: &[u8]) -> Vec<u8> {
    let container_obj_size = (16 + 16 + payload.len()) as u32;
    let mut container = Vec::with_capacity(container_obj_size as usize + 3);
    container.extend_from_slice(OBJ_SIG);
    container.extend_from_slice(&16u16.to_le_bytes());
    container.extend_from_slice(&1u16.to_le_bytes());
    container.extend_from_slice(&container_obj_size.to_le_bytes());
    container.extend_from_slice(&OBJ_LOG_CONTAINER.to_le_bytes());
    container.extend_from_slice(&NO_COMPRESSION.to_le_bytes());
    container.extend_from_slice(&[0u8; 6]);
    container.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    container.extend_from_slice(&[0u8; 4]);
    container.extend_from_slice(payload);
    // 4 字节对齐
    while !container.len().is_multiple_of(4) {
        container.push(0);
    }
    container
}

fn system_time_bytes(value: &DateTime<Local>) -> [u8; 16] {
    let fields = [
        value.year() as u16,
        value.month() as u16,
        value.weekday().num_days_from_sunday() as u16,
        value.day() as u16,
        value.hour() as u16,
        value.minute() as u16,
        value.second() as u16,
        value.timestamp_subsec_millis() as u16,
    ];
    let mut bytes = [0u8; 16];
    for (index, field) in fields.into_iter().enumerate() {
        bytes[index * 2..index * 2 + 2].copy_from_slice(&field.to_le_bytes());
    }
    bytes
}

fn build_file_header(
    file_size: u64,
    uncompressed_size: u64,
    object_count: u32,
    started: &DateTime<Local>,
    ended: &DateTime<Local>,
) -> [u8; FILE_HEADER_SIZE as usize] {
    let mut header: Vec<u8> = Vec::with_capacity(FILE_HEADER_SIZE as usize);
    header.extend_from_slice(FILE_SIG);
    header.extend_from_slice(&FILE_HEADER_SIZE.to_le_bytes());
    header.extend_from_slice(&[0u8; 8]);
    header.extend_from_slice(&file_size.to_le_bytes());
    header.extend_from_slice(&uncompressed_size.to_le_bytes());
    header.extend_from_slice(&object_count.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&system_time_bytes(started));
    header.extend_from_slice(&system_time_bytes(ended));
    while header.len() < FILE_HEADER_SIZE as usize {
        header.push(0);
    }
    header
        .try_into()
        .expect("BLF file header has a fixed 144-byte size")
}

/// Incremental BLF writer. Each completed container is independently readable and the file
/// header is checkpointed after every container, bounding crash loss to the in-memory payload.
pub struct StreamWriter {
    writer: std::io::BufWriter<std::fs::File>,
    payload: Vec<u8>,
    object_count: u32,
    uncompressed_size: u64,
    started: DateTime<Local>,
    ended: DateTime<Local>,
    finished: bool,
}

impl StreamWriter {
    pub fn create(path: &std::path::Path) -> Result<Self, String> {
        let file = std::fs::File::create(path).map_err(|e| format!("创建 BLF 失败: {e}"))?;
        let now = Local::now();
        let mut writer = std::io::BufWriter::new(file);
        writer
            .write_all(&build_file_header(
                FILE_HEADER_SIZE as u64,
                0,
                0,
                &now,
                &now,
            ))
            .map_err(|e| format!("写入 BLF 文件头失败: {e}"))?;
        writer
            .flush()
            .map_err(|e| format!("刷新 BLF 文件头失败: {e}"))?;
        Ok(Self {
            writer,
            payload: Vec::with_capacity(STREAM_CONTAINER_TARGET + 128),
            object_count: 0,
            uncompressed_size: 0,
            started: now,
            ended: now,
            finished: false,
        })
    }

    pub fn push(&mut self, frame: &CanFrame) -> Result<(), String> {
        append_frame_object(&mut self.payload, frame);
        self.object_count = self.object_count.saturating_add(1);
        self.ended = Local::now();
        if self.payload.len() >= STREAM_CONTAINER_TARGET {
            self.flush_checkpoint()?;
        }
        Ok(())
    }

    fn flush_container(&mut self) -> Result<(), String> {
        if self.payload.is_empty() {
            return Ok(());
        }
        let payload = std::mem::take(&mut self.payload);
        self.uncompressed_size = self.uncompressed_size.saturating_add(payload.len() as u64);
        self.writer
            .write_all(&build_container(&payload))
            .map_err(|e| format!("写入 BLF 容器失败: {e}"))?;
        self.payload = Vec::with_capacity(STREAM_CONTAINER_TARGET + 128);
        Ok(())
    }

    fn checkpoint_header(&mut self) -> Result<(), String> {
        self.writer
            .flush()
            .map_err(|e| format!("刷新 BLF 数据失败: {e}"))?;
        let end = self
            .writer
            .stream_position()
            .map_err(|e| format!("读取 BLF 写入位置失败: {e}"))?;
        self.writer
            .seek(SeekFrom::Start(0))
            .and_then(|_| {
                self.writer.write_all(&build_file_header(
                    end,
                    self.uncompressed_size,
                    self.object_count,
                    &self.started,
                    &self.ended,
                ))
            })
            .and_then(|_| self.writer.seek(SeekFrom::Start(end)).map(|_| ()))
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("更新 BLF 检查点失败: {e}"))
    }

    pub fn flush_checkpoint(&mut self) -> Result<(), String> {
        self.flush_container()?;
        self.checkpoint_header()
    }

    pub fn finish(mut self) -> Result<u32, String> {
        self.flush_checkpoint()?;
        self.writer
            .get_ref()
            .sync_data()
            .map_err(|e| format!("同步 BLF 到磁盘失败: {e}"))?;
        self.finished = true;
        Ok(self.object_count)
    }
}

impl Drop for StreamWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.flush_checkpoint();
        }
    }
}

/// Compatibility helper for conversion/tests; production capture uses `StreamWriter` directly.
pub fn write(path: &str, frames: &[CanFrame]) -> Result<(), String> {
    let mut writer = StreamWriter::create(std::path::Path::new(path))?;
    for frame in frames {
        writer.push(frame)?;
    }
    writer.finish().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blf_round_trip() {
        let frames = vec![
            CanFrame {
                t: 0.0,
                ch: 1,
                tx: false,
                id: 0x180,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            },
            CanFrame {
                t: 0.123456,
                ch: 1,
                tx: true,
                id: 0x18FF50E5,
                ext: true,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0xAA, 0xBB],
            },
            // CAN FD 帧：24 字节数据 + BRS，验证 FD 对象往返
            CanFrame {
                t: 0.5,
                ch: 2,
                tx: false,
                id: 0x2A0,
                ext: false,
                fd: true,
                brs: true,
                remote: false,
                error: false,
                data: (1u8..=24).collect(),
            },
        ];
        let path = std::env::temp_dir().join("pcanwork_blf_test.blf");
        let p = path.to_string_lossy().to_string();
        write(&p, &frames).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].id, 0x180);
        assert!(!back[0].ext);
        assert_eq!(back[0].data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(back[1].id, 0x18FF50E5);
        assert!(back[1].ext);
        assert!(back[1].tx);
        assert_eq!(back[1].data, vec![0xAA, 0xBB]);
        assert!((back[1].t - 0.123456).abs() < 1e-6);
        // FD 帧往返：FD 标志、BRS、通道、24 字节数据、时间都应保留
        assert!(back[2].fd);
        assert!(back[2].brs);
        assert_eq!(back[2].ch, 2);
        assert_eq!(back[2].id, 0x2A0);
        assert_eq!(back[2].data, (1u8..=24).collect::<Vec<u8>>());
        assert!((back[2].t - 0.5).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn streaming_writer_checkpoints_multiple_containers() {
        let path = std::env::temp_dir().join("pcanwork_blf_stream_test.blf");
        let mut writer = StreamWriter::create(&path).unwrap();
        let first = CanFrame {
            t: 1.0,
            ch: 1,
            tx: false,
            id: 0x101,
            ext: false,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data: vec![1, 2, 3],
        };
        writer.push(&first).unwrap();
        writer.flush_checkpoint().unwrap();
        let mut second = first.clone();
        second.t = 2.0;
        second.id = 0x102;
        writer.push(&second).unwrap();
        assert_eq!(writer.finish().unwrap(), 2);

        let raw = std::fs::read(&path).unwrap();
        assert_eq!(u64le(&raw, 16), raw.len() as u64);
        assert_eq!(u32le(&raw, 32), 2);
        let back = read(&path.to_string_lossy()).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, 0x101);
        assert_eq!(back[1].id, 0x102);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blf_malformed_no_panic() {
        // 合法 LOGG 头 + 一个 object_size=20(<32) 的容器对象 → 应优雅停止，绝不 panic。
        let mut raw = Vec::new();
        raw.extend_from_slice(FILE_SIG);
        raw.extend_from_slice(&FILE_HEADER_SIZE.to_le_bytes());
        while raw.len() < FILE_HEADER_SIZE as usize {
            raw.push(0);
        }
        raw.extend_from_slice(OBJ_SIG);
        raw.extend_from_slice(&16u16.to_le_bytes()); // header_size
        raw.extend_from_slice(&1u16.to_le_bytes()); // version
        raw.extend_from_slice(&20u32.to_le_bytes()); // object_size = 20 (< 32)
        raw.extend_from_slice(&OBJ_LOG_CONTAINER.to_le_bytes());
        raw.extend_from_slice(&[0u8; 4]); // 补足到 object_size=20
        let path = std::env::temp_dir().join("pcanwork_blf_malformed.blf");
        let p = path.to_string_lossy().to_string();
        std::fs::write(&p, &raw).unwrap();
        assert!(read(&p).is_ok(), "畸形 BLF 应优雅返回而非 panic");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn blf_remote_round_trip() {
        let frames = vec![CanFrame {
            t: 0.0,
            ch: 1,
            tx: false,
            id: 0x100,
            ext: false,
            fd: false,
            brs: false,
            remote: true,
            error: false,
            data: vec![],
        }];
        let path = std::env::temp_dir().join("pcanwork_blf_remote.blf");
        let p = path.to_string_lossy().to_string();
        write(&p, &frames).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].remote, "远程帧标志应往返保留");
        assert!(!back[0].tx);
        let _ = std::fs::remove_file(&path);
    }
}
