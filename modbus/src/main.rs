// Hide the console window on Windows.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

slint::include_modules!();

mod backend;
mod expr;
mod format;
mod protocol;
mod tls;

use backend::{
    Cmd, DerivedCh, LogCfg, MasterCfg, ScanCfg, SimCfg, SimMode, SlaveCfg, SlaveScanCfg, UiCfg,
    WriteFunc, WriteItem, WriteReq,
};
use slint::VecModel;
use format::{Order, RegFormat, Scaling};
use protocol::{Area, ColorRules, CmpOp, Transport, ValueNames};
use slint::Model;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

fn baud_from_index(i: i32) -> u32 {
    match i {
        1 => 19200,
        2 => 38400,
        3 => 57600,
        4 => 115200,
        _ => 9600,
    }
}

fn databits_from_index(i: i32) -> u8 {
    match i {
        1 => 7,
        2 => 6,
        3 => 5,
        _ => 8,
    }
}

// Write-dialog data type + byte order → multi-register encode format (None = 16-bit).
fn parse_derived(text: &str) -> Vec<DerivedCh> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (name, formula) = line.split_once('=')?;
            let (name, formula) = (name.trim().to_string(), formula.trim().to_string());
            if name.is_empty() || formula.is_empty() {
                return None;
            }
            Some(DerivedCh { name, formula })
        })
        .collect()
}

/// Collect WriteItems from the Write List grid model. Addresses are derived from
/// the Start address plus row index (mirrors the read definition). Value accepts 0x.
fn wl_items(rows: &slint::ModelRc<WriteRow>, start: i32) -> Vec<WriteItem> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let v = r.value.trim();
            let value = v
                .strip_prefix("0x")
                .or_else(|| v.strip_prefix("0X"))
                .and_then(|h| i64::from_str_radix(h, 16).ok())
                .or_else(|| v.parse::<i64>().ok())?;
            let address = start + i as i32;
            if !(0..=65535).contains(&value) || !(0..=65535).contains(&address) {
                return None;
            }
            Some(WriteItem { address: address as u16, value: value as u16, inc: r.auto })
        })
        .collect()
}

fn write_encode_fmt(t: i32, order: i32) -> Option<RegFormat> {
    let o = match order {
        1 => Order::Cdab,
        2 => Order::Badc,
        3 => Order::Dcba,
        _ => Order::Abcd,
    };
    match t {
        1 => Some(RegFormat::I32(o)),
        2 => Some(RegFormat::U32(o)),
        3 => Some(RegFormat::F32(o)),
        4 => Some(RegFormat::I64(o)),
        5 => Some(RegFormat::U64(o)),
        6 => Some(RegFormat::F64(o)),
        _ => None,
    }
}

fn stopbits_from_index(i: i32) -> u8 {
    if i == 1 {
        2
    } else {
        1
    }
}

fn color_argb(i: i32) -> u32 {
    match i {
        1 => 0xFFE06C75, // red
        2 => 0xFF98C379, // green
        3 => 0xFFE5C07B, // yellow
        4 => 0xFF61AFEF, // blue
        5 => 0xFFD19A66, // orange
        6 => 0xFF56B6C2, // cyan
        7 => 0xFFC678DD, // magenta
        _ => 0,          // default theme colour
    }
}

fn parse_num(s: &str, d: f64) -> f64 {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).map(|v| v as f64).unwrap_or(d);
    }
    t.parse().unwrap_or(d)
}

fn master_transport(a: &AppWindow) -> Transport {
    if a.get_m_transport() == 0 {
        Transport::Tcp {
            host: a.get_m_host().to_string(),
            port: a.get_m_port() as u16,
        }
    } else {
        Transport::Rtu {
            path: a.get_m_serial().to_string(),
            baud: baud_from_index(a.get_m_baud_index()),
            data_bits: databits_from_index(a.get_m_databits_index()),
            parity: a.get_m_parity() as u8,
            stop_bits: stopbits_from_index(a.get_m_stopbits()),
        }
    }
}

fn slave_transport(a: &AppWindow) -> Transport {
    if a.get_s_transport() == 0 {
        Transport::Tcp {
            host: a.get_s_host().to_string(),
            port: a.get_s_port() as u16,
        }
    } else {
        Transport::Rtu {
            path: a.get_s_serial().to_string(),
            baud: baud_from_index(a.get_s_baud_index()),
            data_bits: databits_from_index(a.get_s_databits_index()),
            parity: a.get_s_parity() as u8,
            stop_bits: stopbits_from_index(a.get_s_stopbits()),
        }
    }
}

/// The unified master Function combo (0-7): 0-3 are read FCs, 4-7 are write FCs.
/// Map an index to its data area.
fn func_area(i: i32) -> Area {
    match i {
        0 | 4 | 6 => Area::Coils,            // FC01 read / FC05 write single / FC15 write multi
        1 => Area::DiscreteInputs,           // FC02 read
        3 => Area::InputRegisters,           // FC04 read
        _ => Area::HoldingRegisters,         // FC03 read / FC06 / FC16
    }
}

/// Whether the selected function is a WRITE code (index >= 4).
fn func_is_write(i: i32) -> bool {
    i >= 4
}

/// Human label for a master function-code index (mirrors the Slint `fc-label`).
fn fc_label(i: i32) -> &'static str {
    match i {
        0 => "FC 01 · Read Coils",
        1 => "FC 02 · Read Discrete Inputs",
        2 => "FC 03 · Read Holding Registers",
        3 => "FC 04 · Read Input Registers",
        4 => "FC 05 · Write Single Coil",
        5 => "FC 06 · Write Single Register",
        6 => "FC 15 · Write Multiple Coils",
        _ => "FC 16 · Write Multiple Registers",
    }
}

/// Map a write-function index (4-7) to the backend WriteFunc.
/// 纯十六进制(可带 0x)→ u16, 用于 FC22 的 AND/OR 掩码。
fn parse_hex_u16(s: &str) -> Option<u16> {
    let t = s.trim();
    let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u16::from_str_radix(h, 16).ok()
}
/// 0x.. 按十六进制, 否则按十进制 → u16, 用于 FC23 写入值。
fn parse_u16_field(s: &str) -> Option<u16> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16).ok(),
        None => t.parse::<u16>().ok(),
    }
}

fn func_write(i: i32) -> WriteFunc {
    match i {
        4 => WriteFunc::SingleCoil,  // FC05
        6 => WriteFunc::MultiCoils,  // FC15
        7 => WriteFunc::MultiRegs,   // FC16
        _ => WriteFunc::SingleReg,   // FC06
    }
}

fn master_cfg(a: &AppWindow) -> MasterCfg {
    let func = a.get_m_function();
    MasterCfg {
        transport: master_transport(a),
        slave_id: a.get_m_slave_id() as u8,
        area: func_area(func),
        address: a.get_m_address() as u16,
        quantity: a.get_m_quantity() as u16,
        scan_ms: a.get_m_scanrate() as u64,
        format: RegFormat::from_index(a.get_m_format()),
        scaling: scaling_cfg(a),
        colors: colors_cfg(a),
        value_names: value_names_cfg(a),
        poll: !func_is_write(func),
        timeout_ms: a.get_m_timeout() as u64,
        reconnect: a.get_m_reconnect(),
        reconnect_ms: a.get_m_reconnect_ms() as u64,
        tls: a.get_m_tls().then(|| tls::TlsClientCfg {
            ca_file: a.get_m_tls_ca().to_string(),
            skip_verify: a.get_m_tls_skip(),
            domain: a.get_m_tls_domain().to_string(),
            client_cert: a.get_m_tls_cert().to_string(),
            client_key: a.get_m_tls_key().to_string(),
            cipher: a.get_m_tls_cipher(),
            version: a.get_m_tls_version(),
        }),
    }
}

fn read_uicfg(a: &AppWindow) -> UiCfg {
    UiCfg {
        transport: a.get_m_transport(),
        host: a.get_m_host().to_string(),
        port: a.get_m_port(),
        serial: a.get_m_serial().to_string(),
        baud_index: a.get_m_baud_index(),
        databits_index: a.get_m_databits_index(),
        parity: a.get_m_parity(),
        stopbits: a.get_m_stopbits(),
        slave_id: a.get_m_slave_id(),
        function: a.get_m_function(),
        address: a.get_m_address(),
        quantity: a.get_m_quantity(),
        scanrate: a.get_m_scanrate(),
        format: a.get_m_format(),
        scl_enabled: a.get_scl_enabled(),
        scl_x1: a.get_scl_x1().to_string(),
        scl_y1: a.get_scl_y1().to_string(),
        scl_x2: a.get_scl_x2().to_string(),
        scl_y2: a.get_scl_y2().to_string(),
        scl_decimals: a.get_scl_decimals(),
        col_normal: a.get_col_normal(),
        col_op1: a.get_col_op1(),
        col_v1: a.get_col_v1().to_string(),
        col_c1: a.get_col_c1(),
        col_op2: a.get_col_op2(),
        col_v2: a.get_col_v2().to_string(),
        col_c2: a.get_col_c2(),
        vn_enabled: a.get_vn_enabled(),
        vn_text: a.get_vn_text().to_string(),
    }
}

fn slave_cfg(a: &AppWindow) -> SlaveCfg {
    SlaveCfg {
        transport: slave_transport(a),
        unit_id: a.get_s_unit_id() as u8,
        ignore_unit_id: a.get_s_ignore_unit_id(),
        area: Area::from_index(a.get_s_area()),
        address: a.get_s_address() as u16,
        quantity: a.get_s_quantity() as u16,
        format: RegFormat::from_index(a.get_s_format()),
        tls: a.get_s_tls().then(|| tls::TlsServerCfg {
            cert_file: a.get_s_tls_cert().to_string(),
            key_file: a.get_s_tls_key().to_string(),
            require_client_cert: a.get_s_tls_require(),
            client_ca: a.get_s_tls_cacert().to_string(),
            cipher: a.get_s_tls_cipher(),
            version: a.get_s_tls_version(),
        }),
    }
}

fn sim_cfg(a: &AppWindow) -> SimCfg {
    SimCfg {
        mode: SimMode::from_index(a.get_sim_mode()),
        area: Area::from_index(a.get_s_area()),
        address: a.get_s_address() as u16,
        quantity: a.get_s_quantity() as u16,
        step: a.get_sim_step().max(1) as u16,
        min: a.get_sim_min() as i64,
        max: a.get_sim_max() as i64,
        interval_ms: a.get_sim_interval().max(50) as u64,
        target: if a.get_sim_one() { a.get_sim_target() } else { -1 },
    }
}

fn scaling_cfg(a: &AppWindow) -> Scaling {
    Scaling {
        enabled: a.get_scl_enabled(),
        x1: parse_num(&a.get_scl_x1(), 0.0),
        y1: parse_num(&a.get_scl_y1(), 0.0),
        x2: parse_num(&a.get_scl_x2(), 1.0),
        y2: parse_num(&a.get_scl_y2(), 1.0),
        decimals: a.get_scl_decimals().max(0) as usize,
    }
}

fn value_names_cfg(a: &AppWindow) -> ValueNames {
    ValueNames {
        enabled: a.get_vn_enabled(),
        map: ValueNames::parse(&a.get_vn_text()),
    }
}

fn colors_cfg(a: &AppWindow) -> ColorRules {
    ColorRules {
        normal: color_argb(a.get_col_normal()),
        op1: CmpOp::from_index(a.get_col_op1()),
        v1: parse_num(&a.get_col_v1(), 0.0),
        c1: color_argb(a.get_col_c1()),
        op2: CmpOp::from_index(a.get_col_op2()),
        v2: parse_num(&a.get_col_v2(), 0.0),
        c2: color_argb(a.get_col_c2()),
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Parse one CSV line into fields, honouring double-quoted fields with `""` escapes.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            }
        }
    }
    out.push(cur);
    out
}

fn is_yes(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "yes" | "y" | "true" | "1" | "on")
}

fn serialize_workspace(a: &AppWindow) -> String {
    use std::fmt::Write;
    let mut s = String::from("modbus-tools-workspace v1\n");
    macro_rules! kv {
        ($k:expr, $v:expr) => {{
            let _ = writeln!(s, "{}={}", $k, $v);
        }};
    }
    kv!("active_mode", a.get_active_mode());
    kv!("m_transport", a.get_m_transport());
    kv!("m_host", a.get_m_host());
    kv!("m_port", a.get_m_port());
    kv!("m_serial", a.get_m_serial());
    kv!("m_baud_index", a.get_m_baud_index());
    kv!("m_databits_index", a.get_m_databits_index());
    kv!("m_parity", a.get_m_parity());
    kv!("m_stopbits", a.get_m_stopbits());
    kv!("m_slave_id", a.get_m_slave_id());
    kv!("m_function", a.get_m_function());
    kv!("m_address", a.get_m_address());
    kv!("m_quantity", a.get_m_quantity());
    kv!("m_scanrate", a.get_m_scanrate());
    kv!("m_format", a.get_m_format());
    kv!("m_timeout", a.get_m_timeout());
    kv!("m_reconnect", a.get_m_reconnect());
    kv!("m_reconnect_ms", a.get_m_reconnect_ms());
    kv!("scl_enabled", a.get_scl_enabled());
    kv!("scl_x1", a.get_scl_x1());
    kv!("scl_y1", a.get_scl_y1());
    kv!("scl_x2", a.get_scl_x2());
    kv!("scl_y2", a.get_scl_y2());
    kv!("scl_decimals", a.get_scl_decimals());
    kv!("col_normal", a.get_col_normal());
    kv!("col_op1", a.get_col_op1());
    kv!("col_v1", a.get_col_v1());
    kv!("col_c1", a.get_col_c1());
    kv!("col_op2", a.get_col_op2());
    kv!("col_v2", a.get_col_v2());
    kv!("col_c2", a.get_col_c2());
    kv!("vn_enabled", a.get_vn_enabled());
    kv!("vn_text", a.get_vn_text().replace('\n', "\\n"));
    kv!("s_transport", a.get_s_transport());
    kv!("s_host", a.get_s_host());
    kv!("s_port", a.get_s_port());
    kv!("s_serial", a.get_s_serial());
    kv!("s_baud_index", a.get_s_baud_index());
    kv!("s_databits_index", a.get_s_databits_index());
    kv!("s_parity", a.get_s_parity());
    kv!("s_stopbits", a.get_s_stopbits());
    kv!("s_unit_id", a.get_s_unit_id());
    kv!("s_area", a.get_s_area());
    kv!("s_address", a.get_s_address());
    kv!("s_quantity", a.get_s_quantity());
    kv!("s_format", a.get_s_format());
    s
}

fn apply_workspace(a: &AppWindow, content: &str) {
    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    let geti = |k: &str, d: i32| {
        map.get(k)
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(d)
    };
    let getb = |k: &str, d: bool| map.get(k).map(|s| s.trim() == "true").unwrap_or(d);
    let sets = |set: &dyn Fn(slint::SharedString), k: &str| {
        if let Some(v) = map.get(k) {
            set(v.trim().into());
        }
    };

    a.set_active_mode(geti("active_mode", a.get_active_mode()));
    a.set_m_transport(geti("m_transport", 0));
    sets(&|v| a.set_m_host(v), "m_host");
    a.set_m_port(geti("m_port", 502));
    sets(&|v| a.set_m_serial(v), "m_serial");
    a.set_m_baud_index(geti("m_baud_index", 0));
    a.set_m_databits_index(geti("m_databits_index", 0));
    a.set_m_parity(geti("m_parity", 0));
    a.set_m_stopbits(geti("m_stopbits", 0));
    a.set_m_slave_id(geti("m_slave_id", 1));
    a.set_m_function(geti("m_function", 2));
    a.set_m_address(geti("m_address", 0));
    a.set_m_quantity(geti("m_quantity", 10));
    a.set_m_scanrate(geti("m_scanrate", 1000));
    a.set_m_format(geti("m_format", 1));
    a.set_m_timeout(geti("m_timeout", 1000));
    a.set_m_reconnect(getb("m_reconnect", false));
    a.set_m_reconnect_ms(geti("m_reconnect_ms", 2000));
    a.set_scl_enabled(getb("scl_enabled", false));
    sets(&|v| a.set_scl_x1(v), "scl_x1");
    sets(&|v| a.set_scl_y1(v), "scl_y1");
    sets(&|v| a.set_scl_x2(v), "scl_x2");
    sets(&|v| a.set_scl_y2(v), "scl_y2");
    a.set_scl_decimals(geti("scl_decimals", 2));
    a.set_col_normal(geti("col_normal", 0));
    a.set_col_op1(geti("col_op1", 0));
    sets(&|v| a.set_col_v1(v), "col_v1");
    a.set_col_c1(geti("col_c1", 1));
    a.set_col_op2(geti("col_op2", 0));
    sets(&|v| a.set_col_v2(v), "col_v2");
    a.set_col_c2(geti("col_c2", 2));
    a.set_vn_enabled(getb("vn_enabled", false));
    if let Some(v) = map.get("vn_text") {
        a.set_vn_text(v.trim().replace("\\n", "\n").into());
    }
    a.set_s_transport(geti("s_transport", 0));
    sets(&|v| a.set_s_host(v), "s_host");
    a.set_s_port(geti("s_port", 502));
    sets(&|v| a.set_s_serial(v), "s_serial");
    a.set_s_baud_index(geti("s_baud_index", 0));
    a.set_s_databits_index(geti("s_databits_index", 0));
    a.set_s_parity(geti("s_parity", 0));
    a.set_s_stopbits(geti("s_stopbits", 0));
    a.set_s_unit_id(geti("s_unit_id", 1));
    a.set_s_area(geti("s_area", 2));
    a.set_s_address(geti("s_address", 0));
    a.set_s_quantity(geti("s_quantity", 10));
    a.set_s_format(geti("s_format", 1));
}

fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    let tx = backend::start_backend(app.as_weak());

    macro_rules! on_cmd {
        ($setter:ident, $build:expr) => {{
            let weak = app.as_weak();
            let tx = tx.clone();
            app.$setter(move || {
                if let Some(a) = weak.upgrade() {
                    let _ = tx.send($build(&a));
                }
            });
        }};
    }

    // ---- Master ----
    on_cmd!(on_master_connect, |a: &AppWindow| Cmd::MasterConnect(master_cfg(a)));
    {
        let tx = tx.clone();
        app.on_master_disconnect(move || {
            let _ = tx.send(Cmd::MasterDisconnect);
        });
    }
    on_cmd!(on_master_write, |a: &AppWindow| Cmd::MasterWrite(WriteReq {
        func: WriteFunc::from_index(a.get_wd_func()),
        address: a.get_wd_address() as u16,
        text: a.get_wd_text().to_string(),
        encode: write_encode_fmt(a.get_wd_type(), a.get_wd_order()),
    }));
    on_cmd!(on_master_format_changed, |a: &AppWindow| Cmd::MasterFormat(
        RegFormat::from_index(a.get_m_format())
    ));
    on_cmd!(on_master_scaling, |a: &AppWindow| Cmd::MasterScaling(scaling_cfg(a)));
    on_cmd!(on_master_colors, |a: &AppWindow| Cmd::MasterColors(colors_cfg(a)));
    on_cmd!(on_master_cell_format, |a: &AppWindow| Cmd::MasterCellFormat {
        address: a.get_cf_addr() as u16,
        format: Some(RegFormat::from_index(a.get_cf_fmt())),
    });
    on_cmd!(on_master_cell_format_clear, |a: &AppWindow| Cmd::MasterCellFormat {
        address: a.get_cf_addr() as u16,
        format: None,
    });
    on_cmd!(on_master_derived, |a: &AppWindow| Cmd::MasterDerived(parse_derived(&a.get_dv_text())));
    on_cmd!(on_master_write_once, |a: &AppWindow| Cmd::MasterWriteOnce {
        func: func_write(a.get_m_function()),
        items: wl_items(&a.get_wl_rows(), a.get_m_address()),
    });
    {
        // Auto-write: 前置校验(已连接 + 写入列表有有效行)通过后才真正下发并把 UI 置为运行
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_auto_write(move || {
            let Some(a) = weak.upgrade() else { return };
            if !a.get_m_connected() {
                a.set_m_status("自动写入: 未连接".into());
                return;
            }
            let items = wl_items(&a.get_wl_rows(), a.get_m_address());
            if items.is_empty() {
                a.set_m_status("自动写入: 写入列表无有效行".into());
                return;
            }
            let _ = tx.send(Cmd::MasterAutoWrite {
                func: func_write(a.get_m_function()),
                items,
                interval_ms: a.get_wl_interval().max(50) as u64,
            });
            a.set_wl_active(true);
        });
    }
    on_cmd!(on_master_auto_write_stop, |_a: &AppWindow| Cmd::MasterAutoWriteStop);
    // ---- FC22 Mask Write / FC23 Read-Write Multiple ----
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_mask_write(move || {
            let Some(a) = weak.upgrade() else { return };
            match (parse_hex_u16(a.get_mw_and().as_str()), parse_hex_u16(a.get_mw_or().as_str())) {
                (Some(and_mask), Some(or_mask)) => {
                    let _ = tx.send(Cmd::MasterMaskWrite { address: a.get_mw_addr() as u16, and_mask, or_mask });
                }
                _ => a.set_m_status("FC22: AND/OR 掩码需为十六进制(0..FFFF)".into()),
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_read_write(move || {
            let Some(a) = weak.upgrade() else { return };
            let vals: Vec<u16> = a
                .get_rw_write_vals()
                .split_whitespace()
                .filter_map(parse_u16_field)
                .collect();
            if vals.is_empty() {
                a.set_m_status("FC23: 写入值为空或非法(空格分隔, 十进制或 0x..)".into());
                return;
            }
            let _ = tx.send(Cmd::MasterReadWrite {
                read_addr: a.get_rw_read_addr() as u16,
                read_qty: a.get_rw_read_qty() as u16,
                write_addr: a.get_rw_write_addr() as u16,
                write_values: vals,
            });
        });
    }
    // ---- Write List grid model (UI-owned; rows derived from Start + Quantity) ----
    {
        let wl_model: Rc<VecModel<WriteRow>> = Rc::new(VecModel::from(vec![
            WriteRow { value: "0".into(), auto: false },
            WriteRow { value: "0".into(), auto: false },
            WriteRow { value: "0".into(), auto: false },
            WriteRow { value: "0".into(), auto: false },
        ]));
        app.set_wl_rows(wl_model.clone().into());
        {
            // Quantity changed → grow/shrink the row model, preserving existing values.
            let m = wl_model.clone();
            app.on_wl_resize(move |n| {
                let n = n.clamp(1, 100) as usize;
                while m.row_count() > n {
                    m.remove(m.row_count() - 1);
                }
                while m.row_count() < n {
                    m.push(WriteRow { value: "0".into(), auto: false });
                }
            });
        }
        {
            let m = wl_model.clone();
            app.on_wl_set_value(move |i, t| {
                let i = i as usize;
                if let Some(mut r) = m.row_data(i) {
                    r.value = t;
                    m.set_row_data(i, r);
                }
            });
        }
        {
            let m = wl_model.clone();
            app.on_wl_set_auto(move |i, b| {
                let i = i as usize;
                if let Some(mut r) = m.row_data(i) {
                    r.auto = b;
                    m.set_row_data(i, r);
                }
            });
        }
    }
    {
        let tx = tx.clone();
        app.on_master_chart_axis(move |addr, axis| {
            let _ = tx.send(Cmd::MasterChartAxis { addr: addr as u16, right: axis == 1 });
        });
    }
    {
        let tx = tx.clone();
        app.on_master_chart_export(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("CSV", &["csv"])
                .set_file_name("chart.csv")
                .save_file()
            {
                let _ = tx.send(Cmd::MasterChartExport(path.to_string_lossy().to_string()));
            }
        });
    }
    {
        let tx = tx.clone();
        app.on_master_name(move |addr, name: slint::SharedString| {
            let _ = tx.send(Cmd::MasterName {
                address: addr as u16,
                name: name.to_string(),
            });
        });
    }
    on_cmd!(on_master_scan_address, |a: &AppWindow| Cmd::ScanAddress(ScanCfg {
        transport: master_transport(a),
        slave_id: a.get_m_slave_id() as u8,
        area: Area::from_index(a.get_sc_func()),
        start: a.get_sc_start() as u16,
        count: a.get_sc_count() as u16,
    }));
    on_cmd!(on_master_scan_slave, |a: &AppWindow| Cmd::ScanSlave(SlaveScanCfg {
        transport: master_transport(a),
        area: Area::from_index(a.get_sc_func()),
        address: a.get_sc_addr() as u16,
        from_id: a.get_sc_fromid() as u8,
        to_id: a.get_sc_toid() as u8,
    }));
    {
        let tx = tx.clone();
        app.on_master_scan_stop(move || {
            let _ = tx.send(Cmd::ScanStop);
        });
    }

    // ---- Poll windows (MDI) ----
    // Floating monitor windows, keyed by poll-window id (UI thread only).
    let floats: Rc<RefCell<HashMap<u32, PollFloat>>> = Rc::new(RefCell::new(HashMap::new()));
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        let floats = floats.clone();
        app.on_float_window(move || {
            let Some(a) = weak.upgrade() else { return };
            let id = a.get_active_win() as u32;
            let func = a.get_m_function();
            // Re-show an existing float for this window.
            if let Some(pf) = floats.borrow().get(&id) {
                pf.set_fc_text(fc_label(func).into());
                pf.set_is_write(func_is_write(func));
                let _ = pf.show();
                let _ = tx.send(Cmd::SetFloat { id, float: Some(pf.as_weak()) });
                return;
            }
            let Ok(pf) = PollFloat::new() else { return };
            pf.set_ptitle(format!("Poll {id}").into());
            pf.set_fc_text(fc_label(func).into());
            pf.set_is_write(func_is_write(func));
            {
                let tx2 = tx.clone();
                pf.on_chart_axis(move |addr, axis| {
                    let _ = tx2.send(Cmd::MasterChartAxis { addr: addr as u16, right: axis == 1 });
                });
            }
            {
                let tx2 = tx.clone();
                pf.window().on_close_requested(move || {
                    let _ = tx2.send(Cmd::SetFloat { id, float: None });
                    slint::CloseRequestResponse::HideWindow
                });
            }
            let _ = pf.show();
            let w = pf.as_weak();
            floats.borrow_mut().insert(id, pf);
            let _ = tx.send(Cmd::SetFloat { id, float: Some(w) });
        });
    }
    // Read/write definition change: send the cmd AND refresh the active window's
    // float function-code indicator if one is open.
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        let floats = floats.clone();
        app.on_master_readdef_changed(move || {
            let Some(a) = weak.upgrade() else { return };
            let func = a.get_m_function();
            let _ = tx.send(Cmd::MasterReadDef {
                area: func_area(func),
                address: a.get_m_address() as u16,
                quantity: a.get_m_quantity() as u16,
                scan_ms: a.get_m_scanrate() as u64,
                poll: !func_is_write(func),
            });
            if let Some(pf) = floats.borrow().get(&(a.get_active_win() as u32)) {
                pf.set_fc_text(fc_label(func).into());
                pf.set_is_write(func_is_write(func));
            }
        });
    }
    // ---- Slave view-window tabs (MDI, mirroring the client) + floating views ----
    {
        let s_windows: Rc<VecModel<SlaveWinTab>> = Rc::new(VecModel::from(vec![
            SlaveWinTab { id: 1, title: "View 1".into(), area: 2, address: 0, quantity: 10, format: 1 },
        ]));
        app.set_s_windows(s_windows.clone().into());
        let s_next = Rc::new(std::cell::Cell::new(2i32));

        // helper: load a tab's def into the active s-* props and re-render
        fn load_tab(a: &AppWindow, tx: &tokio::sync::mpsc::UnboundedSender<Cmd>, t: &SlaveWinTab) {
            a.set_s_area(t.area);
            a.set_s_address(t.address);
            a.set_s_quantity(t.quantity);
            a.set_s_format(t.format);
            a.set_s_active(t.id);
            let _ = tx.send(Cmd::SlaveView {
                area: Area::from_index(t.area),
                address: t.address as u16,
                quantity: t.quantity as u16,
                format: RegFormat::from_index(t.format),
            });
        }

        // + New: snapshot the current view into a new tab and activate it.
        {
            let weak = app.as_weak();
            let tx = tx.clone();
            let m = s_windows.clone();
            let s_next = s_next.clone();
            app.on_new_slave_window(move || {
                let Some(a) = weak.upgrade() else { return };
                let id = s_next.get();
                s_next.set(id + 1);
                let t = SlaveWinTab {
                    id,
                    title: format!("View {id}").into(),
                    area: a.get_s_area(),
                    address: a.get_s_address(),
                    quantity: a.get_s_quantity(),
                    format: a.get_s_format(),
                };
                m.push(t.clone());
                // 切到新视图并把视图定义下发后台(否则后台 active view 没切, 仍按旧视图)
                load_tab(&a, &tx, &t);
            });
        }
        // Select a tab: load its definition.
        {
            let weak = app.as_weak();
            let tx = tx.clone();
            let m = s_windows.clone();
            app.on_select_slave_window(move |id| {
                let Some(a) = weak.upgrade() else { return };
                for i in 0..m.row_count() {
                    if let Some(t) = m.row_data(i) {
                        if t.id == id {
                            load_tab(&a, &tx, &t);
                            break;
                        }
                    }
                }
            });
        }
        // Close a tab (keep at least one); if it was active, switch to the first.
        {
            let weak = app.as_weak();
            let tx = tx.clone();
            let m = s_windows.clone();
            app.on_close_slave_window(move |id| {
                let Some(a) = weak.upgrade() else { return };
                if m.row_count() <= 1 {
                    return;
                }
                if let Some(idx) = (0..m.row_count()).find(|&i| m.row_data(i).map_or(false, |t| t.id == id)) {
                    m.remove(idx);
                }
                if a.get_s_active() == id {
                    if let Some(t) = m.row_data(0) {
                        load_tab(&a, &tx, &t);
                    }
                }
            });
        }
        // View definition edited: send the cmd AND persist into the active tab.
        {
            let weak = app.as_weak();
            let tx = tx.clone();
            let m = s_windows.clone();
            app.on_slave_view_changed(move || {
                let Some(a) = weak.upgrade() else { return };
                let _ = tx.send(Cmd::SlaveView {
                    area: Area::from_index(a.get_s_area()),
                    address: a.get_s_address() as u16,
                    quantity: a.get_s_quantity() as u16,
                    format: RegFormat::from_index(a.get_s_format()),
                });
                let active = a.get_s_active();
                for i in 0..m.row_count() {
                    if let Some(mut t) = m.row_data(i) {
                        if t.id == active {
                            t.area = a.get_s_area();
                            t.address = a.get_s_address();
                            t.quantity = a.get_s_quantity();
                            t.format = a.get_s_format();
                            m.set_row_data(i, t);
                            break;
                        }
                    }
                }
            });
        }
        // Float: pop the active view into a floating SlaveMonitor window.
        {
            let weak = app.as_weak();
            let tx = tx.clone();
            let monitors: Rc<RefCell<HashMap<u32, SlaveMonitor>>> = Rc::new(RefCell::new(HashMap::new()));
            let mon_next = Rc::new(std::cell::Cell::new(1u32));
            app.on_float_slave_window(move || {
                let Some(a) = weak.upgrade() else { return };
                let Ok(mon) = SlaveMonitor::new() else { return };
                let id = mon_next.get();
                mon_next.set(id + 1);
                mon.set_mtitle(format!("View @{} ×{}", a.get_s_address(), a.get_s_quantity()).into());
                mon.set_mon_area(a.get_s_area());
                mon.set_mon_address(a.get_s_address());
                mon.set_mon_quantity(a.get_s_quantity());
                mon.set_mon_format(a.get_s_format());
                let _ = tx.send(Cmd::SlaveAddMonitor {
                    id,
                    weak: mon.as_weak(),
                    area: Area::from_index(mon.get_mon_area()),
                    address: mon.get_mon_address() as u16,
                    quantity: mon.get_mon_quantity() as u16,
                    format: RegFormat::from_index(mon.get_mon_format()),
                });
                {
                    let tx = tx.clone();
                    let w = mon.as_weak();
                    mon.on_view_changed(move || {
                        if let Some(m) = w.upgrade() {
                            let _ = tx.send(Cmd::SlaveMonitorView {
                                id,
                                area: Area::from_index(m.get_mon_area()),
                                address: m.get_mon_address() as u16,
                                quantity: m.get_mon_quantity() as u16,
                                format: RegFormat::from_index(m.get_mon_format()),
                            });
                        }
                    });
                }
                {
                    let tx = tx.clone();
                    mon.on_edit(move |addr, txt: slint::SharedString| {
                        let _ = tx.send(Cmd::SlaveMonitorEdit { id, address: addr as u16, text: txt.to_string() });
                    });
                }
                {
                    let tx = tx.clone();
                    let monitors = monitors.clone();
                    mon.window().on_close_requested(move || {
                        let _ = tx.send(Cmd::SlaveRemoveMonitor { id });
                        monitors.borrow_mut().remove(&id);
                        slint::CloseRequestResponse::HideWindow
                    });
                }
                let _ = mon.show();
                monitors.borrow_mut().insert(id, mon);
            });
        }
    }
    // ---- TLS certificate/key file pickers ----
    macro_rules! pick_pem {
        ($cb:ident, $set:ident) => {{
            let weak = app.as_weak();
            app.$cb(move || {
                if let Some(a) = weak.upgrade() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("PEM", &["pem", "crt", "cer", "key"])
                        .pick_file()
                    {
                        a.$set(p.to_string_lossy().to_string().into());
                    }
                }
            });
        }};
    }
    pick_pem!(on_pick_m_tls_ca, set_m_tls_ca);
    pick_pem!(on_pick_m_tls_cert, set_m_tls_cert);
    pick_pem!(on_pick_m_tls_key, set_m_tls_key);
    pick_pem!(on_pick_s_tls_cert, set_s_tls_cert);
    pick_pem!(on_pick_s_tls_key, set_s_tls_key);
    pick_pem!(on_pick_s_tls_cacert, set_s_tls_cacert);

    on_cmd!(on_new_window, |a: &AppWindow| Cmd::NewWindow(read_uicfg(a)));
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_select_window(move |id| {
            if let Some(a) = weak.upgrade() {
                let _ = tx.send(Cmd::SelectWindow {
                    id: id as u32,
                    current: read_uicfg(&a),
                });
            }
        });
    }
    {
        let tx = tx.clone();
        let floats = floats.clone();
        app.on_close_window(move |id| {
            let uid = id as u32;
            if let Some(pf) = floats.borrow_mut().remove(&uid) {
                let _ = pf.hide();
            }
            let _ = tx.send(Cmd::CloseWindow { id: uid });
        });
    }

    // ---- Data logging ----
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_start_log(move || {
            if let Some(a) = weak.upgrade() {
                // 未连接主站时记录无意义, 提示并退出(不切到 Logging 状态)
                if !a.get_m_connected() {
                    a.set_m_status("记录: 请先连接主站".into());
                    return;
                }
                let tab = a.get_log_delim() == 1;
                let default = if tab { "log.txt" } else { "log.csv" };
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Log file", &["csv", "txt"])
                    .set_file_name(default)
                    .save_file()
                {
                    // 预探测文件能否创建; 失败则提示并退出, 不假装在记录
                    if let Err(e) = std::fs::File::create(&path) {
                        a.set_m_status(format!("记录: 文件打开失败 — {e}").into());
                        return;
                    }
                    let cfg = LogCfg {
                        path: path.to_string_lossy().to_string(),
                        each_read: a.get_log_rate_mode() == 0,
                        period_s: a.get_log_period().max(1) as u32,
                        delimiter: if tab { '\t' } else { ',' },
                        on_change: a.get_log_onchange(),
                        timestamp: a.get_log_timestamp(),
                    };
                    let _ = tx.send(Cmd::MasterStartLog(cfg));
                    a.set_log_active(true);
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_stop_log(move || {
            let _ = tx.send(Cmd::MasterStopLog);
            if let Some(a) = weak.upgrade() {
                a.set_log_active(false);
            }
        });
    }

    // ---- Value names ----
    on_cmd!(on_master_value_names, |a: &AppWindow| Cmd::MasterValueNames(value_names_cfg(a)));
    {
        let weak = app.as_weak();
        app.on_vn_import(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().add_filter("Text", &["txt"]).pick_file() {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        a.set_vn_text(content.into());
                    }
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_vn_export(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Text", &["txt"])
                    .set_file_name("value-names.txt")
                    .save_file()
                {
                    let _ = std::fs::write(path, a.get_vn_text().as_str());
                }
            }
        });
    }

    // ---- Workspace / CSV (UI-side file I/O) ----
    {
        let weak = app.as_weak();
        app.on_save_workspace(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Modbus workspace", &["mbw"])
                    .set_file_name("workspace.mbw")
                    .save_file()
                {
                    let _ = std::fs::write(path, serialize_workspace(&a));
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_open_workspace(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Modbus workspace", &["mbw"])
                    .pick_file()
                {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        apply_workspace(&a, &content);
                        // Push display settings to a live master, if any.
                        let _ = tx.send(Cmd::MasterScaling(scaling_cfg(&a)));
                        let _ = tx.send(Cmd::MasterColors(colors_cfg(&a)));
                        let _ = tx.send(Cmd::MasterValueNames(value_names_cfg(&a)));
                        // 补发关键配置, 否则后台仍按旧地址/数量/格式/视图轮询(界面新、通信旧)
                        let func = a.get_m_function();
                        let _ = tx.send(Cmd::MasterReadDef {
                            area: func_area(func),
                            address: a.get_m_address() as u16,
                            quantity: a.get_m_quantity() as u16,
                            scan_ms: a.get_m_scanrate() as u64,
                            poll: !func_is_write(func),
                        });
                        let _ = tx.send(Cmd::MasterFormat(RegFormat::from_index(a.get_m_format())));
                        let _ = tx.send(Cmd::SlaveView {
                            area: Area::from_index(a.get_s_area()),
                            address: a.get_s_address() as u16,
                            quantity: a.get_s_quantity() as u16,
                            format: RegFormat::from_index(a.get_s_format()),
                        });
                    }
                }
            }
        });
    }
    // Export the active grid as a round-trippable register template
    // (Import,Address,Name,Value — Import=yes marks rows to load back).
    {
        let weak = app.as_weak();
        app.on_export_csv(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("CSV", &["csv"])
                    .set_file_name("registers.csv")
                    .save_file()
                {
                    let rows = if a.get_active_mode() == 0 {
                        a.get_m_rows()
                    } else {
                        a.get_s_rows()
                    };
                    let names = if a.get_active_mode() == 0 {
                        a.get_m_names()
                    } else {
                        a.get_s_names()
                    };
                    let mut s = String::from("Import,Address,Name,Value\n");
                    for (i, r) in rows.iter().enumerate() {
                        let name = if i < names.row_count() {
                            names.row_data(i).unwrap_or_default().to_string()
                        } else {
                            r.name.to_string()
                        };
                        s.push_str(&format!(
                            "yes,{},{},{}\n",
                            r.address,
                            csv_escape(&name),
                            csv_escape(&r.value)
                        ));
                    }
                    let _ = std::fs::write(path, s);
                }
            }
        });
    }
    // Import a register template: rows flagged Import=yes set the name (and, in
    // slave mode, the value) for that address on the active window/server.
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_import_csv(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).pick_file() {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        let slave = a.get_active_mode() == 1;
                        let mut count = 0;
                        for line in content.lines() {
                            if line.trim().is_empty() {
                                continue;
                            }
                            let f = parse_csv_line(line);
                            if f.len() < 3 || !is_yes(&f[0]) {
                                continue; // header / not-selected rows
                            }
                            let addr = match f[1].trim().parse::<u16>() {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let name = f[2].trim().to_string();
                            if slave {
                                let _ = tx.send(Cmd::SlaveName { address: addr, name });
                                if let Some(val) = f.get(3) {
                                    if !val.trim().is_empty() {
                                        let _ = tx.send(Cmd::SlaveEdit {
                                            address: addr,
                                            text: val.trim().to_string(),
                                        });
                                    }
                                }
                            } else {
                                let _ = tx.send(Cmd::MasterName { address: addr, name });
                            }
                            count += 1;
                        }
                        if slave {
                            a.set_s_status(format!("Imported {count} register(s)").into());
                        } else {
                            a.set_m_status(format!("Imported {count} register name(s)").into());
                        }
                    }
                }
            }
        });
    }

    // ---- Slave ----
    on_cmd!(on_slave_start, |a: &AppWindow| Cmd::SlaveStart(slave_cfg(a)));
    {
        let tx = tx.clone();
        app.on_slave_stop(move || {
            let _ = tx.send(Cmd::SlaveStop);
        });
    }
    {
        let tx = tx.clone();
        app.on_slave_edit(move |addr, text: slint::SharedString| {
            let _ = tx.send(Cmd::SlaveEdit {
                address: addr as u16,
                text: text.to_string(),
            });
        });
    }
    {
        let tx = tx.clone();
        app.on_slave_name(move |addr, name: slint::SharedString| {
            let _ = tx.send(Cmd::SlaveName {
                address: addr as u16,
                name: name.to_string(),
            });
        });
    }
    on_cmd!(on_slave_scaling, |a: &AppWindow| Cmd::SlaveScaling(scaling_cfg(a)));
    on_cmd!(on_slave_colors, |a: &AppWindow| Cmd::SlaveColors(colors_cfg(a)));
    on_cmd!(on_slave_value_names, |a: &AppWindow| Cmd::SlaveValueNames(value_names_cfg(a)));
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_slave_sim_start(move || {
            if let Some(a) = weak.upgrade() {
                // 从机服务器没启动时, 后台会忽略 SimStart, 不能假装在模拟
                if !a.get_s_running() {
                    a.set_s_status("模拟: 请先启动从机服务器".into());
                    return;
                }
                let _ = tx.send(Cmd::SlaveSimStart(sim_cfg(&a)));
                a.set_sim_active(true);
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_slave_sim_stop(move || {
            let _ = tx.send(Cmd::SlaveSimStop);
            if let Some(a) = weak.upgrade() {
                a.set_sim_active(false);
            }
        });
    }

    // ---- Grid context actions (copy to clipboard / delete name / jump to chart) ----
    {
        let weak = app.as_weak();
        app.on_master_copy(move |addr| {
            if let Some(a) = weak.upgrade() {
                let addr = addr as u16;
                if let Some(r) = a.get_m_rows().iter().find(|r| r.address as u16 == addr) {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(r.value.to_string());
                    }
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_master_delete(move |addr| {
            if let Some(a) = weak.upgrade() {
                let _ = tx.send(Cmd::MasterName { address: addr as u16, name: String::new() });
                a.set_m_selected_row(-1);
            }
        });
    }
    app.on_master_chart(|_| {}); // tab switch handled in Slint callback
    {
        let weak = app.as_weak();
        app.on_slave_copy(move |addr| {
            if let Some(a) = weak.upgrade() {
                let addr = addr as u16;
                if let Some(r) = a.get_s_rows().iter().find(|r| r.address as u16 == addr) {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(r.value.to_string());
                    }
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_slave_delete(move |addr| {
            if let Some(a) = weak.upgrade() {
                let _ = tx.send(Cmd::SlaveName { address: addr as u16, name: String::new() });
                a.set_s_selected_row(-1);
            }
        });
    }
    app.on_slave_chart(|_| {}); // tab switch handled in Slint callback
    {
        let tx = tx.clone();
        app.on_slave_autoinc(move |addr| {
            let _ = tx.send(Cmd::SlaveAutoInc { address: addr as u16 });
        });
    }

    // ---- Keyboard shortcuts ----
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_key_new_window(move || {
            if let Some(a) = weak.upgrade() {
                let _ = tx.send(Cmd::NewWindow(read_uicfg(&a)));
            }
        });
    }
    {
        let weak = app.as_weak();
        app.on_key_save(move || {
            if let Some(a) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Modbus workspace", &["mbw"])
                    .set_file_name("workspace.mbw")
                    .save_file()
                {
                    let _ = std::fs::write(path, serialize_workspace(&a));
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_key_reconnect(move || {
            if let Some(a) = weak.upgrade() {
                if a.get_m_connected() {
                    let _ = tx.send(Cmd::MasterDisconnect);
                } else {
                    let _ = tx.send(Cmd::MasterConnect(master_cfg(&a)));
                }
            }
        });
    }
    {
        let weak = app.as_weak();
        let tx = tx.clone();
        app.on_key_delete_selected(move || {
            if let Some(a) = weak.upgrade() {
                if a.get_active_mode() == 0 {
                    let sel = a.get_m_selected_row();
                    if sel >= 0 {
                        if let Some(r) = a.get_m_rows().row_data(sel as usize) {
                            let _ = tx.send(Cmd::MasterName { address: r.address as u16, name: String::new() });
                        }
                        a.set_m_selected_row(-1);
                    }
                } else {
                    let sel = a.get_s_selected_row();
                    if sel >= 0 {
                        if let Some(r) = a.get_s_rows().row_data(sel as usize) {
                            let _ = tx.send(Cmd::SlaveName { address: r.address as u16, name: String::new() });
                        }
                        a.set_s_selected_row(-1);
                    }
                }
            }
        });
    }

    // ---- Connection info popup builder ----
    {
        let weak = app.as_weak();
        app.on_show_conn_info(move || {
            if let Some(a) = weak.upgrade() {
                let info = if a.get_active_mode() == 0 {
                    let transport = if a.get_m_transport() == 0 {
                        format!("Modbus TCP  {}:{}", a.get_m_host(), a.get_m_port())
                    } else {
                        format!("Modbus RTU  {}  baud={}", a.get_m_serial(),
                            baud_from_index(a.get_m_baud_index()))
                    };
                    format!(
                        "Mode:        {}\n\
                         Slave ID:    {}\n\
                         Function:    {}\n\
                         Address:     {}\n\
                         Quantity:    {}\n\
                         Scan rate:   {} ms\n\
                         Connected:   {}\n\
                         Tx / Rx / Err: {} / {} / {}",
                        transport,
                        a.get_m_slave_id(),
                        match a.get_m_function() {
                            0 => "0x Coils", 1 => "1x Discrete Inputs",
                            2 => "4x Holding Registers", _ => "3x Input Registers",
                        },
                        a.get_m_address(),
                        a.get_m_quantity(),
                        a.get_m_scanrate(),
                        if a.get_m_connected() { "Yes" } else { "No" },
                        a.get_m_tx(), a.get_m_rx(), a.get_m_err()
                    )
                } else {
                    let transport = if a.get_s_transport() == 0 {
                        format!("Modbus TCP  {}:{}", a.get_s_host(), a.get_s_port())
                    } else {
                        format!("Modbus RTU  {}  baud={}", a.get_s_serial(),
                            baud_from_index(a.get_s_baud_index()))
                    };
                    format!(
                        "Mode:        {}\n\
                         Unit ID:     {}{}\n\
                         Table:       {}\n\
                         Address:     {}\n\
                         Quantity:    {}\n\
                         Running:     {}\n\
                         Requests:    {}",
                        transport,
                        a.get_s_unit_id(),
                        if a.get_s_ignore_unit_id() { "  (ignore)" } else { "" },
                        match a.get_s_area() {
                            0 => "0x Coils", 1 => "1x Discrete Inputs",
                            2 => "4x Holding Registers", _ => "3x Input Registers",
                        },
                        a.get_s_address(),
                        a.get_s_quantity(),
                        if a.get_s_running() { "Yes" } else { "No" },
                        a.get_s_requests()
                    )
                };
                a.set_conn_info_text(info.into());
            }
        });
    }

    // Closing the main window closes any floating monitor windows and quits.
    {
        let floats = floats.clone();
        app.window().on_close_requested(move || {
            floats.borrow_mut().clear();
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 标题栏锁品牌深蓝(与 pcanwork/serial 统一), 低频定时器兼顾后开的子窗口
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

    app.run()
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
