//! Rendering raw register / coil values into the display strings shown in the
//! data grid. Mirrors the Modbus Poll display formats (16-bit native plus 32/64-bit
//! integers, float and double in the four word/byte orders) and applies optional
//! linear scaling. Each rendered row also carries a numeric value used by the
//! conditional-colour rules and the real-time chart.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order {
    Abcd, // big-endian
    Cdab, // little-endian word swap
    Badc, // big-endian byte swap
    Dcba, // little-endian
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegFormat {
    U16,
    S16,
    Hex16,
    Bin16,
    AsciiHex,
    I32(Order),
    U32(Order),
    F32(Order),
    I64(Order),
    U64(Order),
    F64(Order),
}

impl RegFormat {
    pub fn from_index(i: i32) -> Self {
        use Order::*;
        match i {
            0 => RegFormat::S16,
            1 => RegFormat::U16,
            2 => RegFormat::Hex16,
            3 => RegFormat::Bin16,
            4 => RegFormat::AsciiHex,
            5 => RegFormat::I32(Abcd),
            6 => RegFormat::I32(Cdab),
            7 => RegFormat::I32(Badc),
            8 => RegFormat::I32(Dcba),
            9 => RegFormat::U32(Abcd),
            10 => RegFormat::U32(Cdab),
            11 => RegFormat::U32(Badc),
            12 => RegFormat::U32(Dcba),
            13 => RegFormat::F32(Abcd),
            14 => RegFormat::F32(Cdab),
            15 => RegFormat::F32(Badc),
            16 => RegFormat::F32(Dcba),
            17 => RegFormat::I64(Abcd),
            18 => RegFormat::I64(Cdab),
            19 => RegFormat::I64(Badc),
            20 => RegFormat::I64(Dcba),
            21 => RegFormat::U64(Abcd),
            22 => RegFormat::U64(Cdab),
            23 => RegFormat::U64(Badc),
            24 => RegFormat::U64(Dcba),
            25 => RegFormat::F64(Abcd),
            26 => RegFormat::F64(Cdab),
            27 => RegFormat::F64(Badc),
            28 => RegFormat::F64(Dcba),
            _ => RegFormat::U16,
        }
    }

    pub fn span(self) -> usize {
        match self {
            RegFormat::I32(_) | RegFormat::U32(_) | RegFormat::F32(_) => 2,
            RegFormat::I64(_) | RegFormat::U64(_) | RegFormat::F64(_) => 4,
            _ => 1,
        }
    }

    fn numeric_display(self) -> bool {
        !matches!(
            self,
            RegFormat::Hex16 | RegFormat::Bin16 | RegFormat::AsciiHex
        )
    }
}

/// Linear scaling: Y = m·(X − X1) + Y1, m = (Y2 − Y1) / (X2 − X1).
#[derive(Clone, Copy)]
pub struct Scaling {
    pub enabled: bool,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub decimals: usize,
}

impl Scaling {
    pub fn off() -> Self {
        Scaling {
            enabled: false,
            x1: 0.0,
            y1: 0.0,
            x2: 1.0,
            y2: 1.0,
            decimals: 2,
        }
    }
    pub fn apply(&self, x: f64) -> f64 {
        if !self.enabled || self.x2 == self.x1 {
            return x;
        }
        let m = (self.y2 - self.y1) / (self.x2 - self.x1);
        m * (x - self.x1) + self.y1
    }
}

/// One rendered grid value. `num` is the numeric value (after scaling) used by
/// conditional colours and the chart; `None` for continuation rows.
#[derive(Clone)]
pub struct DisplayRow {
    pub address: i32,
    pub value: String,
    pub raw: String,
    pub num: Option<f64>,
}

fn order_for(fmt: RegFormat) -> Order {
    match fmt {
        RegFormat::I32(o)
        | RegFormat::U32(o)
        | RegFormat::F32(o)
        | RegFormat::I64(o)
        | RegFormat::U64(o)
        | RegFormat::F64(o) => o,
        _ => Order::Abcd,
    }
}

fn canonical_bytes(regs: &[u16]) -> Vec<u8> {
    let mut b = Vec::with_capacity(regs.len() * 2);
    for r in regs {
        b.push((r >> 8) as u8);
        b.push((r & 0xff) as u8);
    }
    b
}

fn ordered(canon: &[u8], order: Order) -> Vec<u8> {
    match order {
        Order::Abcd => canon.to_vec(),
        Order::Dcba => {
            let mut v = canon.to_vec();
            v.reverse();
            v
        }
        Order::Badc => {
            let mut v = canon.to_vec();
            let mut i = 0;
            while i + 1 < v.len() {
                v.swap(i, i + 1);
                i += 2;
            }
            v
        }
        Order::Cdab => {
            let mut v = Vec::with_capacity(canon.len());
            for w in canon.chunks(2).rev() {
                v.extend_from_slice(w);
            }
            v
        }
    }
}

/// (display string, numeric value) for a single 16-bit register before scaling.
fn single_base(v: u16, fmt: RegFormat) -> (String, f64) {
    match fmt {
        RegFormat::U16 => (v.to_string(), v as f64),
        RegFormat::S16 => ((v as i16).to_string(), (v as i16) as f64),
        RegFormat::Hex16 => (format!("0x{v:04X}"), v as f64),
        RegFormat::Bin16 => {
            let s = format!("{v:016b}");
            (
                format!("{} {} {} {}", &s[0..4], &s[4..8], &s[8..12], &s[12..16]),
                v as f64,
            )
        }
        RegFormat::AsciiHex => {
            let hi = (v >> 8) as u8;
            let lo = (v & 0xff) as u8;
            let c = |b: u8| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            };
            (format!("{}{}", c(hi), c(lo)), v as f64)
        }
        _ => (v.to_string(), v as f64),
    }
}

/// (display string, numeric value) for a multi-register value before scaling.
fn multi_base(regs: &[u16], fmt: RegFormat) -> (String, f64) {
    let b = ordered(&canonical_bytes(regs), order_for(fmt));
    match fmt {
        RegFormat::I32(_) => {
            let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            (v.to_string(), v as f64)
        }
        RegFormat::U32(_) => {
            let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            (v.to_string(), v as f64)
        }
        RegFormat::F32(_) => {
            let v = f32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            (format!("{v}"), v as f64)
        }
        RegFormat::I64(_) => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[0..8]);
            let v = i64::from_be_bytes(a);
            (v.to_string(), v as f64)
        }
        RegFormat::U64(_) => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[0..8]);
            let v = u64::from_be_bytes(a);
            (v.to_string(), v as f64)
        }
        RegFormat::F64(_) => {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[0..8]);
            let v = f64::from_be_bytes(a);
            (format!("{v}"), v)
        }
        _ => single_base(regs[0], fmt),
    }
}

fn finalize(base_str: String, base_num: f64, fmt: RegFormat, scaling: &Scaling) -> (String, f64) {
    // 非数值显示格式(Hex16/Bin16/AsciiHex)不缩放 num，使颜色/标签判定与所见的原始 hex/bin 一致。
    if scaling.enabled && fmt.numeric_display() {
        let num = scaling.apply(base_num);
        (format!("{:.*}", scaling.decimals, num), num)
    } else {
        (base_str, base_num)
    }
}

/// Render a window of registers, applying the chosen format, word order and scaling.
pub fn render_registers(
    start: u16,
    values: &[u16],
    default_fmt: RegFormat,
    overrides: &std::collections::HashMap<u16, RegFormat>,
    scaling: &Scaling,
) -> Vec<DisplayRow> {
    let mut rows = Vec::with_capacity(values.len());
    let mut i = 0usize;
    while i < values.len() {
        let addr = start as i32 + i as i32;
        // Per-register format override falls back to the window default.
        let fmt = overrides
            .get(&((start as usize + i) as u16))
            .copied()
            .unwrap_or(default_fmt);
        let span = fmt.span();

        if span == 1 || values.len() - i < span {
            // 16-bit (or a dangling register too short for its span).
            let single_fmt = if span == 1 { fmt } else { RegFormat::U16 };
            let (bs, bn) = single_base(values[i], single_fmt);
            let (value, num) = finalize(bs, bn, single_fmt, scaling);
            rows.push(DisplayRow {
                address: addr,
                value,
                raw: format!("0x{:04X}", values[i]),
                num: Some(num),
            });
            i += 1;
            continue;
        }

        let group = &values[i..i + span];
        let (bs, bn) = multi_base(group, fmt);
        let (value, num) = finalize(bs, bn, fmt, scaling);
        rows.push(DisplayRow {
            address: addr,
            value,
            raw: format!("0x{:04X}", values[i]),
            num: Some(num),
        });
        for k in 1..span {
            rows.push(DisplayRow {
                address: addr + k as i32,
                value: "·".into(),
                raw: format!("0x{:04X}", values[i + k]),
                num: None,
            });
        }
        i += span;
    }
    rows
}

/// Render a window of coils / discrete inputs.
pub fn render_bits(start: u16, values: &[bool]) -> Vec<DisplayRow> {
    values
        .iter()
        .enumerate()
        .map(|(i, &b)| DisplayRow {
            address: start as i32 + i as i32,
            value: if b { "1" } else { "0" }.into(),
            raw: if b { "ON" } else { "OFF" }.into(),
            num: Some(if b { 1.0 } else { 0.0 }),
        })
        .collect()
}

/// Encode a numeric value into register words for a multi-register format,
/// honouring the byte/word order. Returns `None` for 16-bit (non-multi) formats.
/// 生产路径已改用 encode_typed(精确整数+范围校验)；此函数仅保留供测试校验字节序逻辑。
#[cfg(test)]
pub fn encode_value(value: f64, fmt: RegFormat) -> Option<Vec<u16>> {
    let be: Vec<u8> = match fmt {
        RegFormat::I32(_) => (value as i32).to_be_bytes().to_vec(),
        RegFormat::U32(_) => ((value as i64) as u32).to_be_bytes().to_vec(),
        RegFormat::F32(_) => (value as f32).to_be_bytes().to_vec(),
        RegFormat::I64(_) => (value as i64).to_be_bytes().to_vec(),
        RegFormat::U64(_) => ((value as i128) as u64).to_be_bytes().to_vec(),
        RegFormat::F64(_) => value.to_be_bytes().to_vec(),
        _ => return None,
    };
    // The four orders are involutions, so applying `ordered` maps the value's
    // big-endian bytes to the canonical register byte layout.
    let canon = ordered(&be, order_for(fmt));
    Some(
        canon
            .chunks(2)
            .map(|c| ((c[0] as u16) << 8) | (c[1] as u16))
            .collect(),
    )
}

/// 把用户输入编码为多寄存器格式的寄存器字。整数格式按精确整数解析并范围校验
/// (不经 f64 中转，避免 >2^53 精度丢失、u64 高半区被拒、超范围静默饱和)。
/// 非多寄存器(16 位)格式返回 Err。
pub fn encode_typed(text: &str, fmt: RegFormat) -> Result<Vec<u16>, String> {
    let t = text.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"));
    let parse_int = |lo: i128, hi: i128| -> Result<i128, String> {
        let v = if let Some(h) = hex {
            i128::from_str_radix(h, 16).map_err(|_| "invalid number".to_string())?
        } else if let Ok(i) = t.parse::<i128>() {
            i
        } else {
            // 容忍 "100.0" 这类整值浮点写法
            let f = t.parse::<f64>().map_err(|_| "invalid number".to_string())?;
            if !f.is_finite() || f.fract() != 0.0 {
                return Err("value must be an integer".to_string());
            }
            f as i128
        };
        if v < lo || v > hi {
            return Err("value out of range".to_string());
        }
        Ok(v)
    };
    let be: Vec<u8> = match fmt {
        RegFormat::I32(_) => (parse_int(i32::MIN as i128, i32::MAX as i128)? as i32)
            .to_be_bytes()
            .to_vec(),
        RegFormat::U32(_) => (parse_int(0, u32::MAX as i128)? as u32)
            .to_be_bytes()
            .to_vec(),
        RegFormat::I64(_) => (parse_int(i64::MIN as i128, i64::MAX as i128)? as i64)
            .to_be_bytes()
            .to_vec(),
        RegFormat::U64(_) => (parse_int(0, u64::MAX as i128)? as u64)
            .to_be_bytes()
            .to_vec(),
        RegFormat::F32(_) => t
            .parse::<f32>()
            .map_err(|_| "invalid number".to_string())?
            .to_be_bytes()
            .to_vec(),
        RegFormat::F64(_) => t
            .parse::<f64>()
            .map_err(|_| "invalid number".to_string())?
            .to_be_bytes()
            .to_vec(),
        _ => return Err("unsupported type".to_string()),
    };
    let canon = ordered(&be, order_for(fmt));
    Ok(canon
        .chunks(2)
        .map(|c| ((c[0] as u16) << 8) | (c[1] as u16))
        .collect())
}

pub fn parse_bit(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

pub fn parse_word(text: &str) -> Option<u16> {
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).ok();
    }
    if let Ok(v) = t.parse::<u16>() {
        return Some(v);
    }
    t.parse::<i16>().ok().map(|v| v as u16)
}

pub fn parse_word_list(text: &str) -> Option<Vec<u16>> {
    let parts: Vec<&str> = text
        .split(|c| c == ',' || c == ' ' || c == ';')
        .filter(|s| !s.trim().is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.iter().map(|p| parse_word(p)).collect()
}

pub fn parse_bit_list(text: &str) -> Option<Vec<bool>> {
    let parts: Vec<&str> = text
        .split(|c| c == ',' || c == ' ' || c == ';')
        .filter(|s| !s.trim().is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.iter().map(|p| parse_bit(p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_orders_32() {
        let regs = [0x1122u16, 0x3344u16];
        assert_eq!(
            multi_base(&regs, RegFormat::U32(Order::Abcd)).1 as u64,
            0x11223344
        );
        assert_eq!(
            multi_base(&regs, RegFormat::U32(Order::Dcba)).1 as u64,
            u32::from_be_bytes([0x44, 0x33, 0x22, 0x11]) as u64
        );
        assert_eq!(
            multi_base(&regs, RegFormat::U32(Order::Cdab)).1 as u64,
            u32::from_be_bytes([0x33, 0x44, 0x11, 0x22]) as u64
        );
        assert_eq!(
            multi_base(&regs, RegFormat::U32(Order::Badc)).1 as u64,
            u32::from_be_bytes([0x22, 0x11, 0x44, 0x33]) as u64
        );
    }

    #[test]
    fn float_abcd() {
        let (s, _) = multi_base(&[0x4049, 0x0FDB], RegFormat::F32(Order::Abcd));
        assert!(s.starts_with("3.14"), "got {s}");
    }

    #[test]
    fn encode_decode_roundtrip() {
        // float pi encodes to registers and decodes back, for every byte order.
        for o in [Order::Abcd, Order::Cdab, Order::Badc, Order::Dcba] {
            let regs = encode_value(3.14159_f64, RegFormat::F32(o)).unwrap();
            assert_eq!(regs.len(), 2);
            let (_s, num) = multi_base(&regs, RegFormat::F32(o));
            assert!((num - 3.14159).abs() < 1e-4, "order {o:?} -> {num}");
        }
        // 32-bit int
        let regs = encode_value(-123456.0, RegFormat::I32(Order::Cdab)).unwrap();
        let (_s, num) = multi_base(&regs, RegFormat::I32(Order::Cdab));
        assert_eq!(num, -123456.0);
        // 16-bit returns None
        assert!(encode_value(5.0, RegFormat::U16).is_none());
    }

    #[test]
    fn scaling_linear() {
        // raw 0..65535 -> 0..100
        let sc = Scaling {
            enabled: true,
            x1: 0.0,
            y1: 0.0,
            x2: 65535.0,
            y2: 100.0,
            decimals: 1,
        };
        let ov = std::collections::HashMap::new();
        let rows = render_registers(0, &[32768], RegFormat::U16, &ov, &sc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "50.0");
        assert!((rows[0].num.unwrap() - 50.0).abs() < 0.01);
    }
}
