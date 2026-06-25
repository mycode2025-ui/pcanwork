//! DBC 加载与信号解码。位提取实现经典 Intel(小端) / Motorola(大端 sawtooth) 两种字节序。

use can_dbc::{
    ByteOrder, Dbc, MessageId, MultiplexIndicator, NumericValue, SignalExtendedValueType, ValueType,
};
use std::collections::HashMap;

#[derive(Clone)]
pub struct SignalDef {
    pub name: String,
    pub start_bit: u64,
    pub size: u64,
    pub little_endian: bool,
    pub signed: bool,
    pub factor: f64,
    pub offset: f64,
    pub min: f64,
    pub max: f64,
    pub unit: String,
    /// 浮点位宽: 0=整数, 32=IEEE float, 64=IEEE double
    pub float_bits: u8,
    /// 是否为复用开关(multiplexor, 'M')
    pub is_multiplexor: bool,
    /// 复用信号: Some(n)=仅当复用开关值==n 时有效('m<n>'); None=非复用
    pub mux_value: Option<u64>,
}

#[derive(Clone)]
pub struct MessageDef {
    pub id: u32, // 不含扩展标志
    pub name: String,
    pub size: u64, // 报文字节数
    pub signals: Vec<SignalDef>,
}

#[derive(Clone)]
pub struct DbcDb {
    pub file_name: String,
    by_id: HashMap<u32, MessageDef>,
    enums: HashMap<(u32, String), Vec<(i64, String)>>, // (id, signal) -> 枚举值表
}

/// 单个信号解码结果。
pub struct Decoded {
    pub name: String,
    pub raw: i64,
    pub physical: f64,
    pub unit: String,
    pub min: f64,
    pub max: f64,
    pub start_bit: u64,
    pub size: u64,
    pub little_endian: bool,
    pub signed: bool,
    pub factor: f64,
    pub offset: f64,
    pub out_of_range: bool,
    pub enum_txt: String,
}

fn num(v: &NumericValue) -> f64 {
    match v {
        NumericValue::Uint(x) => *x as f64,
        NumericValue::Int(x) => *x as f64,
        NumericValue::Double(x) => *x,
    }
}

impl DbcDb {
    pub fn load(path: &str) -> Result<DbcDb, String> {
        let raw = std::fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
        // DBC 常见 latin-1/cp1252 编码，先尽量按 UTF-8，失败再退到有损转换。
        let text = String::from_utf8(raw.clone())
            .unwrap_or_else(|_| raw.iter().map(|&b| b as char).collect());
        let dbc = Dbc::try_from(text.as_str()).map_err(|e| format!("DBC 解析失败: {e:?}"))?;

        let mut by_id = HashMap::new();
        let mut enums: HashMap<(u32, String), Vec<(i64, String)>> = HashMap::new();
        for m in &dbc.messages {
            let id = match m.id {
                MessageId::Standard(v) => v as u32,
                MessageId::Extended(v) => v,
            };
            for s in &m.signals {
                if let Some(vds) = dbc.value_descriptions_for_signal(m.id, &s.name) {
                    let v: Vec<(i64, String)> =
                        vds.iter().map(|d| (d.id, d.description.clone())).collect();
                    if !v.is_empty() {
                        enums.insert((id, s.name.clone()), v);
                    }
                }
            }
            let signals = m
                .signals
                .iter()
                .map(|s| {
                    // 浮点类型来自 SIG_VALTYPE_ 扩展表；仅当位宽确为 32/64 才按浮点重解释，
                    // 否则(畸形 DBC 把浮点标在非 32/64 位信号上)退回整数缩放，避免 from_bits 出垃圾值。
                    let float_bits = match dbc.extended_value_type_for_signal(m.id, &s.name) {
                        Some(SignalExtendedValueType::IEEEfloat32Bit) if s.size == 32 => 32,
                        Some(SignalExtendedValueType::IEEEdouble64bit) if s.size == 64 => 64,
                        _ => 0,
                    };
                    // 复用指示
                    let (is_multiplexor, mux_value) = match s.multiplexer_indicator {
                        MultiplexIndicator::Multiplexor => (true, None),
                        MultiplexIndicator::MultiplexedSignal(n) => (false, Some(n)),
                        MultiplexIndicator::MultiplexorAndMultiplexedSignal(n) => (true, Some(n)),
                        MultiplexIndicator::Plain => (false, None),
                    };
                    SignalDef {
                        name: s.name.clone(),
                        start_bit: s.start_bit,
                        size: s.size,
                        little_endian: matches!(s.byte_order, ByteOrder::LittleEndian),
                        signed: matches!(s.value_type, ValueType::Signed),
                        factor: s.factor,
                        offset: s.offset,
                        min: num(&s.min),
                        max: num(&s.max),
                        unit: s.unit.clone(),
                        float_bits,
                        is_multiplexor,
                        mux_value,
                    }
                })
                .collect();
            by_id.insert(
                id,
                MessageDef {
                    id,
                    name: m.name.clone(),
                    size: m.size,
                    signals,
                },
            );
        }

        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());

        Ok(DbcDb {
            file_name,
            by_id,
            enums,
        })
    }

    pub fn message_name(&self, id: u32) -> Option<&str> {
        self.by_id.get(&id).map(|m| m.name.as_str())
    }

    pub fn messages(&self) -> impl Iterator<Item = &MessageDef> {
        self.by_id.values()
    }

    pub fn message(&self, id: u32) -> Option<&MessageDef> {
        self.by_id.get(&id)
    }

    /// 报文字节长度（取定义长度与信号跨度的较大者，至少 1）。
    pub fn message_len(&self, id: u32) -> usize {
        let Some(m) = self.by_id.get(&id) else { return 0 };
        let span = m
            .signals
            .iter()
            .map(|s| (s.start_bit + s.size).div_ceil(8) as usize)
            .max()
            .unwrap_or(0);
        (m.size as usize).max(span).max(1)
    }

    /// 把信号物理值编码成报文字节。values 缺省的信号按 0 处理。
    pub fn encode(&self, id: u32, values: &HashMap<String, f64>) -> Option<Vec<u8>> {
        let m = self.by_id.get(&id)?;
        let len = self.message_len(id);
        let mut data = vec![0u8; len];
        for s in &m.signals {
            let phys = values.get(&s.name).copied().unwrap_or(0.0);
            let raw = match s.float_bits {
                32 => (((phys - s.offset) / s.factor) as f32).to_bits() as i64,
                64 => f64::to_bits((phys - s.offset) / s.factor) as i64,
                _ => ((phys - s.offset) / s.factor).round() as i64,
            };
            insert(&mut data, s.start_bit, s.size, s.little_endian, raw);
        }
        Some(data)
    }

    /// 解码给定 ID 的全部信号。无匹配返回空。
    pub fn decode(&self, id: u32, data: &[u8]) -> Vec<Decoded> {
        let Some(m) = self.by_id.get(&id) else {
            return Vec::new();
        };
        // 复用开关的原始值（用于筛选有效的复用信号）
        let mux_raw: Option<i64> = m
            .signals
            .iter()
            .find(|s| s.is_multiplexor)
            .map(|s| extract(data, s.start_bit, s.size, s.little_endian, false));
        m.signals
            .iter()
            // 复用信号: 仅当复用开关值匹配时才解码; 非复用信号始终保留
            .filter(|s| match s.mux_value {
                Some(n) => mux_raw == Some(n as i64),
                None => true,
            })
            .map(|s| {
                let raw = extract(data, s.start_bit, s.size, s.little_endian, s.signed);
                // 浮点信号: 把位模式重解释为 f32/f64, 而非整数缩放
                let physical = match s.float_bits {
                    32 => {
                        let bits =
                            extract(data, s.start_bit, s.size, s.little_endian, false) as u32;
                        f32::from_bits(bits) as f64 * s.factor + s.offset
                    }
                    64 => {
                        let bits =
                            extract(data, s.start_bit, s.size, s.little_endian, false) as u64;
                        f64::from_bits(bits) * s.factor + s.offset
                    }
                    _ => raw as f64 * s.factor + s.offset,
                };
                let out = s.max > s.min && (physical < s.min || physical > s.max);
                let enum_txt = self
                    .enums
                    .get(&(id, s.name.clone()))
                    .and_then(|v| {
                        v.iter()
                            .find(|(val, _)| *val == raw)
                            .map(|(_, d)| d.clone())
                    })
                    .unwrap_or_default();
                Decoded {
                    name: s.name.clone(),
                    raw,
                    physical,
                    unit: s.unit.clone(),
                    min: s.min,
                    max: s.max,
                    start_bit: s.start_bit,
                    size: s.size,
                    little_endian: s.little_endian,
                    signed: s.signed,
                    factor: s.factor,
                    offset: s.offset,
                    out_of_range: out,
                    enum_txt,
                }
            })
            .collect()
    }
}

/// 提取位域。返回带符号扩展后的原始值。
fn extract(data: &[u8], start_bit: u64, size: u64, little_endian: bool, signed: bool) -> i64 {
    let size = size.min(64) as u32;
    if size == 0 {
        return 0;
    }
    let bit_at = |idx: u64| -> u64 {
        let byte = (idx / 8) as usize;
        let pos = (idx % 8) as u32;
        if byte < data.len() {
            ((data[byte] >> pos) & 1) as u64
        } else {
            0
        }
    };

    let mut val: u64 = 0;
    if little_endian {
        // Intel：从 start_bit 起，第 i 位放到结果第 i 位。
        for i in 0..size as u64 {
            val |= bit_at(start_bit + i) << i;
        }
    } else {
        // Motorola：start_bit 为 MSB，sawtooth 编号。
        let mut bit = start_bit as i64;
        for _ in 0..size {
            let b = bit_at(bit as u64);
            val = (val << 1) | b;
            if bit % 8 == 0 {
                bit += 15;
            } else {
                bit -= 1;
            }
        }
    }

    if signed && size < 64 {
        let sign = 1u64 << (size - 1);
        if val & sign != 0 {
            // 负数：高位补 1
            let mask = !((1u64 << size) - 1);
            return (val | mask) as i64;
        }
    }
    val as i64
}

/// 写位域（extract 的逆操作）。
fn insert(data: &mut [u8], start_bit: u64, size: u64, little_endian: bool, raw: i64) {
    let size = size.min(64) as u32;
    if size == 0 {
        return;
    }
    // 取低 size 位
    let val: u64 = if size >= 64 {
        raw as u64
    } else {
        (raw as u64) & ((1u64 << size) - 1)
    };
    let mut set_bit = |idx: u64, bit: u64| {
        let byte = (idx / 8) as usize;
        let pos = (idx % 8) as u32;
        if byte < data.len() {
            if bit != 0 {
                data[byte] |= 1 << pos;
            } else {
                data[byte] &= !(1 << pos);
            }
        }
    };
    if little_endian {
        for i in 0..size as u64 {
            set_bit(start_bit + i, (val >> i) & 1);
        }
    } else {
        let mut bit = start_bit as i64;
        for k in 0..size {
            let b = (val >> (size - 1 - k)) & 1;
            set_bit(bit as u64, b);
            if bit % 8 == 0 {
                bit += 15;
            } else {
                bit -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        // 构造一个内存 DBC
        let txt = "VERSION \"\"\nBO_ 256 Test: 8 ECU\n SG_ A : 0|16@1+ (0.1,0) [0|100] \"%\" Vector__XXX\n SG_ B : 16|16@1- (0.1,0) [-500|500] \"A\" Vector__XXX\n SG_ C : 32|4@1+ (1,0) [0|3] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_enc_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("A".to_string(), 80.0);
        vals.insert("B".to_string(), -12.0);
        vals.insert("C".to_string(), 2.0);
        let data = db.encode(256, &vals).unwrap();
        let dec = db.decode(256, &data);
        let get = |n: &str| dec.iter().find(|d| d.name == n).unwrap().physical;
        assert!((get("A") - 80.0).abs() < 1e-6);
        assert!((get("B") - (-12.0)).abs() < 1e-6);
        assert!((get("C") - 2.0).abs() < 1e-6);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn float32_roundtrip() {
        let txt = "VERSION \"\"\nBO_ 512 FMsg: 8 ECU\n SG_ F : 0|32@1+ (1,0) [0|0] \"V\" Vector__XXX\nSIG_VALTYPE_ 512 F : 1;\n";
        let p = std::env::temp_dir().join("pcanwork_f32_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("F".to_string(), 3.14_f64);
        let data = db.encode(512, &vals).unwrap();
        let dec = db.decode(512, &data);
        let f = dec.iter().find(|d| d.name == "F").unwrap();
        // f32 精度: 误差应远小于 1e-4
        assert!((f.physical - 3.14).abs() < 1e-4, "got {}", f.physical);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn float64_roundtrip() {
        let txt = "VERSION \"\"\nBO_ 513 DMsg: 8 ECU\n SG_ D : 0|64@1+ (1,0) [0|0] \"\" Vector__XXX\nSIG_VALTYPE_ 513 D : 2;\n";
        let p = std::env::temp_dir().join("pcanwork_f64_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("D".to_string(), 2.718281828_f64);
        let data = db.encode(513, &vals).unwrap();
        let dec = db.decode(513, &data);
        let d = dec.iter().find(|x| x.name == "D").unwrap();
        assert!((d.physical - 2.718281828).abs() < 1e-9, "got {}", d.physical);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn multiplexed_decode() {
        // Mux 为开关; A 仅 Mux==0 有效, B 仅 Mux==1 有效 (A/B 共用 bit 8)
        let txt = "VERSION \"\"\nBO_ 768 MuxMsg: 8 ECU\n SG_ Mux M : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ A m0 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B m1 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_mux_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();

        // Mux=0 → 只出 Mux + A=42, 无 B
        let d0 = db.decode(768, &[0, 42, 0, 0, 0, 0, 0, 0]);
        assert!(d0.iter().any(|d| d.name == "Mux"));
        assert!(d0.iter().any(|d| d.name == "A" && (d.physical - 42.0).abs() < 1e-9));
        assert!(!d0.iter().any(|d| d.name == "B"));

        // Mux=1 → 只出 Mux + B=99, 无 A
        let d1 = db.decode(768, &[1, 99, 0, 0, 0, 0, 0, 0]);
        assert!(d1.iter().any(|d| d.name == "B" && (d.physical - 99.0).abs() < 1e-9));
        assert!(!d1.iter().any(|d| d.name == "A"));
        let _ = std::fs::remove_file(&p);
    }
}
