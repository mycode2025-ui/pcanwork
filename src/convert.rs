//! Record-file read/write and format conversion (CSV / Vector ASC / BLF).
//! Read: dispatched by extension; Write: to a chosen target format.
//! Shared by playback loading and the "record file conversion" feature.

use crate::blf;
use crate::can::CanFrame;

/// Conversion target format.
#[derive(Clone, Copy, PartialEq)]
pub enum LogFmt {
    Csv,
    Asc,
    Blf,
}

impl LogFmt {
    pub fn from_index(i: i32) -> LogFmt {
        match i {
            1 => LogFmt::Asc,
            2 => LogFmt::Blf,
            _ => LogFmt::Csv,
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            LogFmt::Csv => "csv",
            LogFmt::Asc => "asc",
            LogFmt::Blf => "blf",
        }
    }
}

/// Read a record file into a frame sequence, dispatched by extension
/// (.blf / .asc / otherwise treated as CSV).
pub fn parse_playback_file(path: &str) -> Result<Vec<CanFrame>, String> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".blf") {
        return blf::read(path);
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    if lower.ends_with(".asc") {
        Ok(parse_asc_frames(&text))
    } else {
        Ok(parse_csv_frames(&text))
    }
}

/// This app's CSV layout: Time,Ch,Dir,ID,Len,Data
pub fn parse_csv_frames(text: &str) -> Vec<CanFrame> {
    let mut v = Vec::new();
    for line in text.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split(',').collect();
        if p.len() < 6 {
            continue;
        }
        let Ok(t) = p[0].trim().parse::<f64>() else {
            continue;
        };
        let ch = p[1]
            .trim()
            .trim_start_matches("CAN")
            .parse::<u8>()
            .unwrap_or(1);
        let tx = p[2].trim().eq_ignore_ascii_case("Tx");
        let ids = p[3]
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        let Ok(id) = u32::from_str_radix(ids, 16) else {
            continue;
        };
        let data: Vec<u8> = p[5]
            .split_whitespace()
            .filter_map(|x| u8::from_str_radix(x, 16).ok())
            .collect();
        v.push(CanFrame {
            t,
            ch,
            tx,
            id,
            ext: id > 0x7FF,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data,
        });
    }
    v
}

/// Vector ASC. 经典: `<t> <ch> <id>[x] Rx|Tx d <dlc> <bytes...>`(远程帧用 r);
/// CAN FD: `<t> CANFD <ch> Rx|Tx <id>[x] <name> <brs> <esi> <dlc> <datalen> <bytes...>`。
pub fn parse_asc_frames(text: &str) -> Vec<CanFrame> {
    let mut v = Vec::new();
    let mut date_epoch: Option<f64> = None;
    let mut timestamps_absolute = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("date ") {
            date_epoch = parse_asc_date_epoch(rest);
            continue;
        }
        if line.to_ascii_lowercase().contains("timestamps absolute") {
            timestamps_absolute = true;
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 5 {
            // 远程帧行最少 5 个 token: <t> <ch> <id> <dir> r
            continue;
        }
        let Ok(t) = toks[0].parse::<f64>() else {
            continue;
        };
        let t = if timestamps_absolute {
            date_epoch.map(|base| base + t).unwrap_or(t)
        } else {
            t
        };
        // ---- CAN FD 行 ----
        if toks.get(1) == Some(&"CANFD") {
            let ch = toks.get(2).and_then(|s| s.parse::<u8>().ok()).unwrap_or(1);
            let Some(dir_idx) = toks.iter().position(|x| *x == "Rx" || *x == "Tx") else {
                continue;
            };
            let tx = toks[dir_idx] == "Tx";
            let Some(idtok) = toks.get(dir_idx + 1) else {
                continue;
            };
            let ext = idtok.ends_with('x') || idtok.ends_with('X');
            let Ok(id) = u32::from_str_radix(idtok.trim_end_matches(['x', 'X']), 16) else {
                continue;
            };
            // dir+1=id, +2=name, +3=brs, +4=esi, +5=dlc, +6=datalen, +7..=data
            let brs = toks.get(dir_idx + 3).map(|s| *s == "1").unwrap_or(false);
            let datalen = toks
                .get(dir_idx + 6)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let data: Vec<u8> = toks
                .get(dir_idx + 7..)
                .map(|b| {
                    b.iter()
                        .take(datalen)
                        .filter_map(|x| u8::from_str_radix(x, 16).ok())
                        .collect()
                })
                .unwrap_or_default();
            v.push(CanFrame {
                t,
                ch,
                tx,
                id,
                ext,
                fd: true,
                brs,
                remote: false,
                error: false,
                data,
            });
            continue;
        }
        let ch = toks[1].parse::<u8>().unwrap_or(1);
        let idtok = toks[2];
        let ext = idtok.ends_with('x') || idtok.ends_with('X');
        let idclean = idtok.trim_end_matches(['x', 'X']);
        let Ok(id) = u32::from_str_radix(idclean, 16) else {
            continue;
        };
        let Some(dir_idx) = toks.iter().position(|t| *t == "Rx" || *t == "Tx") else {
            continue;
        };
        let tx = toks[dir_idx] == "Tx";
        let mut data = Vec::new();
        let mut remote = false;
        if let Some(kind) = toks.get(dir_idx + 1) {
            if kind.eq_ignore_ascii_case("r") {
                remote = true;
            } else if let Some(bytes) = toks.get(dir_idx + 3..) {
                data = bytes
                    .iter()
                    .filter_map(|x| u8::from_str_radix(x, 16).ok())
                    .collect();
            }
        }
        v.push(CanFrame {
            t,
            ch,
            tx,
            id,
            ext,
            fd: false,
            brs: false,
            remote,
            error: false,
            data,
        });
    }
    v
}

fn parse_asc_date_epoch(s: &str) -> Option<f64> {
    let s = s.trim();
    let formats = [
        "%a %b %d %I:%M:%S%.f %p %Y",
        "%a %b %e %I:%M:%S%.f %p %Y",
        "%a %b %d %H:%M:%S%.f %Y",
        "%a %b %e %H:%M:%S%.f %Y",
    ];
    for fmt in formats {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return dt.and_local_timezone(chrono::Local).single().map(|local| {
                local.timestamp() as f64 + local.timestamp_subsec_nanos() as f64 / 1e9
            });
        }
    }
    None
}

/// Write this app's CSV format: Time,Ch,Dir,ID,Len,Data
pub fn write_csv(path: &str, frames: &[CanFrame]) -> Result<(), String> {
    use std::io::Write;
    let f = std::fs::File::create(path).map_err(|e| format!("创建失败: {e}"))?;
    let mut w = std::io::BufWriter::new(f);
    writeln!(w, "Time,Ch,Dir,ID,Len,Data").map_err(|e| e.to_string())?;
    for fr in frames {
        writeln!(
            w,
            "{:.6},CAN{},{},0x{:X},{},{}",
            fr.t,
            fr.ch,
            if fr.tx { "Tx" } else { "Rx" },
            fr.id,
            fr.data.len(),
            fr.data_hex()
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// CAN FD 数据长度 → DLC 码(0..15: 0..8,12,16,20,24,32,48,64)。
fn fd_dlc_code(len: usize) -> u8 {
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

/// Write Vector ASC. 经典帧写 `d`/远程帧写 `r`，CAN FD 帧写 `CANFD` 行(保留 BRS/数据)。
pub fn write_asc(path: &str, frames: &[CanFrame]) -> Result<(), String> {
    use std::io::Write;
    let f = std::fs::File::create(path).map_err(|e| format!("创建失败: {e}"))?;
    let mut w = std::io::BufWriter::new(f);
    writeln!(w, "date converted by PcanWork").map_err(|e| e.to_string())?;
    writeln!(w, "base hex  timestamps absolute").map_err(|e| e.to_string())?;
    for fr in frames {
        let idtxt = if fr.ext {
            format!("{:X}x", fr.id)
        } else {
            format!("{:X}", fr.id)
        };
        let dir = if fr.tx { "Tx" } else { "Rx" };
        let res = if fr.fd {
            // Vector CANFD 行: <t> CANFD <ch> <dir> <id> <name> <brs> <esi> <dlc> <datalen> <data...>
            writeln!(
                w,
                "{:.6} CANFD {} {} {} - {} 0 {} {} {}",
                fr.t,
                fr.ch,
                dir,
                idtxt,
                if fr.brs { 1 } else { 0 },
                fd_dlc_code(fr.data.len()),
                fr.data.len(),
                fr.data_hex()
            )
        } else if fr.remote {
            // 经典远程帧: 用 r 标记(无数据)
            writeln!(w, "{:.6} {} {:<16}{}   r", fr.t, fr.ch, idtxt, dir)
        } else {
            writeln!(
                w,
                "{:.6} {} {:<16}{}   d {} {}",
                fr.t,
                fr.ch,
                idtxt,
                dir,
                fr.data.len(),
                fr.data_hex()
            )
        };
        res.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Read `in_path`, convert to `fmt`, write to `out_path`; returns the frame count written.
pub fn convert(in_path: &str, out_path: &str, fmt: LogFmt) -> Result<usize, String> {
    let frames = parse_playback_file(in_path)?;
    match fmt {
        LogFmt::Csv => write_csv(out_path, &frames)?,
        LogFmt::Asc => write_asc(out_path, &frames)?,
        LogFmt::Blf => blf::write(out_path, &frames)?,
    }
    Ok(frames.len())
}

/// Batch-convert every CSV/ASC/BLF file in `in_dir` to `fmt`, writing results into `out_dir`
/// (same file stem, new extension). Returns (ok_count, fail_count, per-file log lines).
pub fn convert_dir(in_dir: &str, out_dir: &str, fmt: LogFmt) -> (usize, usize, Vec<String>) {
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut log = Vec::new();
    let rd = match std::fs::read_dir(in_dir) {
        Ok(rd) => rd,
        Err(e) => return (0, 0, vec![format!("打开目录失败: {e}")]),
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "csv" | "asc" | "blf") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let dst = std::path::Path::new(out_dir).join(format!("{stem}.{}", fmt.ext()));
        let dst_name = dst
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        match convert(&path.to_string_lossy(), &dst.to_string_lossy(), fmt) {
            Ok(n) => {
                ok += 1;
                log.push(format!("✓ {name} → {dst_name} ({n} 帧)"));
            }
            Err(e) => {
                fail += 1;
                log.push(format!("✗ {name}: {e}"));
            }
        }
    }
    if ok == 0 && fail == 0 {
        log.push("目录中没有可转换的 CSV/ASC/BLF 文件".to_string());
    }
    (ok, fail, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_fd_and_remote_roundtrip() {
        let frames = vec![
            // 经典数据帧
            CanFrame {
                t: 0.001,
                ch: 1,
                tx: false,
                id: 0x123,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0x11, 0x22],
            },
            // 经典远程帧(无数据)
            CanFrame {
                t: 0.002,
                ch: 1,
                tx: true,
                id: 0x7FF,
                ext: false,
                fd: false,
                brs: false,
                remote: true,
                error: false,
                data: vec![],
            },
            // CAN FD 帧: BRS + 扩展 ID + 16 字节
            CanFrame {
                t: 0.003,
                ch: 2,
                tx: false,
                id: 0x18FF1234,
                ext: true,
                fd: true,
                brs: true,
                remote: false,
                error: false,
                data: (0u8..16).collect(),
            },
        ];
        let p = std::env::temp_dir().join("pcanwork_asc_fd_test.asc");
        let path = p.to_str().unwrap();
        write_asc(path, &frames).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let back = parse_asc_frames(&text);
        let _ = std::fs::remove_file(path);
        assert_eq!(back.len(), 3, "三帧都应读回: {text}");
        // 经典数据帧
        assert!(!back[0].fd && !back[0].remote);
        assert_eq!(back[0].data, vec![0x11, 0x22]);
        // 远程帧: 标志保留, 无数据
        assert!(back[1].remote && back[1].data.is_empty());
        assert_eq!(back[1].id, 0x7FF);
        // FD 帧: fd/brs/ext/数据 全部保留
        assert!(back[2].fd && back[2].brs && back[2].ext);
        assert_eq!(back[2].id, 0x18FF1234);
        assert_eq!(back[2].data, (0u8..16).collect::<Vec<u8>>());
    }

    #[test]
    fn asc_absolute_timestamp_uses_date_header() {
        let asc = "date Fri Jun 26 05:28:02.278 PM 2026\n\
base hex timestamps absolute\n\
// version 7.0.0\n\
0.226000 1  18fa78f5x       Rx   d 8 68 00 00 00 00 00 00 00\n";
        let frames = parse_asc_frames(asc);
        assert_eq!(frames.len(), 1);
        let base = parse_asc_date_epoch("Fri Jun 26 05:28:02.278 PM 2026").unwrap();
        assert!((frames[0].t - (base + 0.226)).abs() < 0.001);
        assert!(frames[0].t > 1_700_000_000.0);
        assert_eq!(frames[0].id, 0x18FA78F5);
        assert!(frames[0].ext);
    }

    #[test]
    fn csv_roundtrip_and_convert() {
        let frames = vec![
            CanFrame {
                t: 0.001,
                ch: 1,
                tx: false,
                id: 0x100,
                ext: false,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0x11, 0x22, 0x33],
            },
            CanFrame {
                t: 0.05,
                ch: 2,
                tx: true,
                id: 0x1ABCDEF,
                ext: true,
                fd: false,
                brs: false,
                remote: false,
                error: false,
                data: vec![0xAA],
            },
        ];
        let dir = std::env::temp_dir();
        let csv = dir.join("pcanwork_conv.csv");
        let asc = dir.join("pcanwork_conv.asc");
        write_csv(&csv.to_string_lossy(), &frames).unwrap();
        // CSV -> read back
        let back = parse_playback_file(&csv.to_string_lossy()).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, 0x100);
        assert_eq!(back[0].data, vec![0x11, 0x22, 0x33]);
        assert_eq!(back[1].ch, 2);
        assert!(back[1].ext);
        // CSV -> ASC conversion, then read back
        let n = convert(&csv.to_string_lossy(), &asc.to_string_lossy(), LogFmt::Asc).unwrap();
        assert_eq!(n, 2);
        let back2 = parse_playback_file(&asc.to_string_lossy()).unwrap();
        assert_eq!(back2.len(), 2);
        assert_eq!(back2[1].id, 0x1ABCDEF);
        assert!(back2[1].tx);
        let _ = std::fs::remove_file(&csv);
        let _ = std::fs::remove_file(&asc);
    }

    #[test]
    fn batch_dir_conversion() {
        let frame = CanFrame {
            t: 0.01,
            ch: 1,
            tx: false,
            id: 0x123,
            ext: false,
            fd: false,
            brs: false,
            remote: false,
            error: false,
            data: vec![1, 2, 3, 4],
        };
        let base = std::env::temp_dir().join("pcanwork_batch_in");
        let outd = std::env::temp_dir().join("pcanwork_batch_out");
        let _ = std::fs::create_dir_all(&base);
        let _ = std::fs::create_dir_all(&outd);
        write_csv(&base.join("a.csv").to_string_lossy(), &[frame.clone()]).unwrap();
        write_asc(&base.join("b.asc").to_string_lossy(), &[frame.clone()]).unwrap();
        // a non-log file should be ignored
        std::fs::write(base.join("note.txt"), "ignore me").unwrap();
        let (ok, fail, _log) = convert_dir(
            &base.to_string_lossy(),
            &outd.to_string_lossy(),
            LogFmt::Blf,
        );
        assert_eq!(ok, 2);
        assert_eq!(fail, 0);
        assert!(outd.join("a.blf").exists());
        assert!(outd.join("b.blf").exists());
        // round-trip one of them
        let back = parse_playback_file(&outd.join("a.blf").to_string_lossy()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, 0x123);
        assert_eq!(back[0].data, vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outd);
    }
}
