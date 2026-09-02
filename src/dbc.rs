//! DBC 加载与信号解码。位提取实现经典 Intel(小端) / Motorola(大端 sawtooth) 两种字节序。

use can_dbc::{
    ByteOrder, Dbc, MessageId, MultiplexIndicator, NumericValue, SignalExtendedValueType, ValueType,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DbcDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbcDiagnostic {
    pub severity: DbcDiagnosticSeverity,
    pub code: String,
    pub message_id: u32,
    pub extended: bool,
    pub message_name: String,
    pub signal_name: String,
    pub title_zh: String,
    pub title_en: String,
    pub detail_zh: String,
    pub detail_en: String,
}

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

impl SignalDef {
    pub fn fits_in_bytes(&self, byte_len: u64) -> bool {
        if self.size == 0 || self.size > 64 || byte_len == 0 {
            return false;
        }
        if self.little_endian {
            return self
                .start_bit
                .checked_add(self.size - 1)
                .is_some_and(|last| last < byte_len.saturating_mul(8));
        }
        let mut bit = self.start_bit as i128;
        let limit = byte_len.saturating_mul(8) as i128;
        for _ in 0..self.size {
            if bit < 0 || bit >= limit {
                return false;
            }
            bit = if bit % 8 == 0 { bit + 15 } else { bit - 1 };
        }
        true
    }

    /// Physical range that can actually be represented by the signal bits,
    /// intersected with a valid DBC-declared range. Some field DBCs contain
    /// copied ranges (for example an 8-bit, -40 offset signal declared 0..65535);
    /// exposing those unchecked values makes the editor accept data the encoder
    /// must later reject.
    pub fn effective_physical_range(&self) -> Option<(f64, f64)> {
        if self.float_bits != 0 {
            return (self.max > self.min).then_some((self.min, self.max));
        }
        if self.size == 0 || self.size > 64 || !self.factor.is_finite() || !self.offset.is_finite()
        {
            return None;
        }
        let (mut min, mut max) = self.representable_physical_range()?;
        if self.max > self.min {
            min = min.max(self.min);
            max = max.min(self.max);
        }
        (max >= min).then_some((min, max))
    }

    pub fn representable_physical_range(&self) -> Option<(f64, f64)> {
        if self.float_bits != 0 {
            return None;
        }
        if self.size == 0 || self.size > 64 || !self.factor.is_finite() || !self.offset.is_finite()
        {
            return None;
        }
        let (raw_min, raw_max) = if self.signed {
            let limit = 2f64.powi(self.size.saturating_sub(1) as i32);
            (-limit, limit - 1.0)
        } else {
            (0.0, 2f64.powi(self.size as i32) - 1.0)
        };
        let first = raw_min * self.factor + self.offset;
        let second = raw_max * self.factor + self.offset;
        Some((first.min(second), first.max(second)))
    }
}

#[derive(Clone)]
pub struct MessageDef {
    pub id: u32, // 不含扩展标志
    pub extended: bool,
    pub name: String,
    pub size: u64, // 报文字节数
    pub signals: Vec<SignalDef>,
}

#[derive(Clone)]
pub struct DbcDb {
    pub file_name: String,
    by_id: HashMap<(u32, bool), MessageDef>,
    enums: HashMap<(u32, bool, String), Vec<(i64, String)>>,
}

/// 单个信号解码结果。
#[derive(Clone)]
pub struct Decoded {
    pub name: String,
    /// Legacy signed view. Unsigned values above i64::MAX are saturated; use
    /// raw_unsigned/raw_text when exact unsigned semantics matter.
    pub raw: i64,
    pub raw_unsigned: Option<u64>,
    pub raw_text: String,
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
    /// 非复用信号以及当前 MUX 选中分支为 true。
    pub mux_active: bool,
    pub mux_value: Option<u64>,
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
        let mut enums: HashMap<(u32, bool, String), Vec<(i64, String)>> = HashMap::new();
        for m in &dbc.messages {
            let (id, extended) = match m.id {
                MessageId::Standard(v) => (v as u32, false),
                MessageId::Extended(v) => (v, true),
            };
            for s in &m.signals {
                if let Some(vds) = dbc.value_descriptions_for_signal(m.id, &s.name) {
                    let v: Vec<(i64, String)> =
                        vds.iter().map(|d| (d.id, d.description.clone())).collect();
                    if !v.is_empty() {
                        enums.insert((id, extended, s.name.clone()), v);
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
            if by_id.contains_key(&(id, extended)) {
                return Err(format!(
                    "DBC 存在重复报文定义: 0x{id:X} ext={extended} ({})",
                    m.name
                ));
            }
            by_id.insert(
                (id, extended),
                MessageDef {
                    id,
                    extended,
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

    pub fn message_name_ext(&self, id: u32, extended: bool) -> Option<&str> {
        self.by_id.get(&(id, extended)).map(|m| m.name.as_str())
    }

    pub fn messages(&self) -> impl Iterator<Item = &MessageDef> {
        self.by_id.values()
    }

    /// Product-grade static diagnostics. The encoder remains authoritative and
    /// rejects unsafe values; this report explains problems before transmission.
    pub fn diagnostics(&self) -> Vec<DbcDiagnostic> {
        let mut result = Vec::new();
        {
            let mut add = |severity: DbcDiagnosticSeverity,
                           code: &str,
                           message: &MessageDef,
                           signal_name: &str,
                           title_zh: String,
                           title_en: String,
                           detail_zh: String,
                           detail_en: String| {
                result.push(DbcDiagnostic {
                    severity,
                    code: code.to_string(),
                    message_id: message.id,
                    extended: message.extended,
                    message_name: message.name.clone(),
                    signal_name: signal_name.to_string(),
                    title_zh,
                    title_en,
                    detail_zh,
                    detail_en,
                });
            };

            let mut ids = HashSet::new();
            for message in self.by_id.values() {
                if self.by_id.contains_key(&(message.id, !message.extended))
                    && ids.insert(message.id)
                {
                    add(
                        DbcDiagnosticSeverity::Warning,
                        "DBC-ID-FORMAT-CONFLICT",
                        message,
                        "",
                        "同一数值 ID 同时定义为标准帧和扩展帧".into(),
                        "Numeric ID is defined as both standard and extended".into(),
                        format!(
                            "0x{:X} 同时存在标准帧与扩展帧定义。绑定、脚本和过滤器必须显式携带帧格式。",
                            message.id
                        ),
                        format!(
                            "0x{:X} has both standard and extended definitions. Bindings, scripts and filters must carry the frame format explicitly.",
                            message.id
                        ),
                    );
                }
            }

            for message in self.by_id.values() {
                if message.size == 0 || message.size > 64 {
                    add(
                        DbcDiagnosticSeverity::Error,
                        "DBC-MSG-DLC",
                        message,
                        "",
                        "报文长度无效".into(),
                        "Invalid message length".into(),
                        format!("DLC={}，有效范围是 1..64 字节。", message.size),
                        format!(
                            "DLC={} is outside the valid 1..64-byte range.",
                            message.size
                        ),
                    );
                } else if message.size > 8 {
                    add(
                        DbcDiagnosticSeverity::Info,
                        "DBC-MSG-REQUIRES-FD",
                        message,
                        "",
                        "报文长度要求 CAN FD".into(),
                        "Message length requires CAN FD".into(),
                        format!("DLC={} 超过经典 CAN 的 8 字节上限。", message.size),
                        format!(
                            "DLC={} exceeds the 8-byte Classical CAN limit.",
                            message.size
                        ),
                    );
                }

                let mut names = HashSet::new();
                for signal in &message.signals {
                    if !names.insert(signal.name.as_str()) {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-DUPLICATE",
                            message,
                            &signal.name,
                            "报文内信号名称重复".into(),
                            "Duplicate signal name in message".into(),
                            format!("信号“{}”定义了多次，编码目标不明确。", signal.name),
                            format!(
                                "Signal '{}' is defined more than once; the encoding target is ambiguous.",
                                signal.name
                            ),
                        );
                    }
                }

                let multiplexors: Vec<&SignalDef> = message
                    .signals
                    .iter()
                    .filter(|signal| signal.is_multiplexor)
                    .collect();
                if multiplexors.len() > 1 {
                    add(
                        DbcDiagnosticSeverity::Error,
                        "DBC-MUX-MULTIPLE",
                        message,
                        "",
                        "同一报文存在多个复用开关".into(),
                        "Multiple multiplexors in one message".into(),
                        format!(
                            "检测到 {} 个复用开关；当前编码模型要求每个报文最多一个。",
                            multiplexors.len()
                        ),
                        format!(
                            "Found {} multiplexors; the encoder requires at most one per message.",
                            multiplexors.len()
                        ),
                    );
                }

                for signal in &message.signals {
                    if signal.size == 0 || signal.size > 64 {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-SIZE",
                            message,
                            &signal.name,
                            "信号位宽无效".into(),
                            "Invalid signal bit length".into(),
                            format!("位宽={}，有效范围是 1..64。", signal.size),
                            format!(
                                "Bit length={} is outside the valid 1..64 range.",
                                signal.size
                            ),
                        );
                    } else if !signal.fits_in_bytes(message.size) {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-DLC-OVERFLOW",
                            message,
                            &signal.name,
                            "信号位布局超出报文长度".into(),
                            "Signal layout exceeds message length".into(),
                            format!(
                                "起始位={}、位宽={}、字节序={}，无法放入 DLC={}。该信号禁止编码。",
                                signal.start_bit,
                                signal.size,
                                if signal.little_endian {
                                    "Intel"
                                } else {
                                    "Motorola"
                                },
                                message.size
                            ),
                            format!(
                                "Start bit={}, length={}, order={} does not fit DLC={}. Encoding this signal is blocked.",
                                signal.start_bit,
                                signal.size,
                                if signal.little_endian {
                                    "Intel"
                                } else {
                                    "Motorola"
                                },
                                message.size
                            ),
                        );
                    }

                    if !signal.factor.is_finite() || signal.factor == 0.0 {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-FACTOR",
                            message,
                            &signal.name,
                            "信号比例因子无效".into(),
                            "Invalid signal factor".into(),
                            format!("factor={}，无法进行可靠的物理值换算。", signal.factor),
                            format!(
                                "factor={} cannot produce a reliable physical conversion.",
                                signal.factor
                            ),
                        );
                    }
                    if !signal.offset.is_finite() {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-OFFSET",
                            message,
                            &signal.name,
                            "信号偏移量无效".into(),
                            "Invalid signal offset".into(),
                            format!("offset={} 不是有限数值。", signal.offset),
                            format!("offset={} is not finite.", signal.offset),
                        );
                    }
                    if signal.max < signal.min {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-SIG-RANGE-ORDER",
                            message,
                            &signal.name,
                            "信号范围上下限颠倒".into(),
                            "Signal range is reversed".into(),
                            format!(
                                "声明范围 [{}, {}]，最大值小于最小值。",
                                signal.min, signal.max
                            ),
                            format!(
                                "Declared range [{}, {}] has max below min.",
                                signal.min, signal.max
                            ),
                        );
                    } else if signal.max > signal.min
                        && let Some((raw_min, raw_max)) = signal.representable_physical_range()
                        && (signal.min < raw_min || signal.max > raw_max)
                    {
                        add(
                            DbcDiagnosticSeverity::Warning,
                            "DBC-SIG-RANGE-WIDTH",
                            message,
                            &signal.name,
                            "声明范围超出信号位宽".into(),
                            "Declared range exceeds signal bit width".into(),
                            format!(
                                "声明 [{}, {}]，根据位宽、符号、factor/offset 实际可表示 [{}, {}]；编辑器将使用交集。",
                                signal.min, signal.max, raw_min, raw_max
                            ),
                            format!(
                                "Declared [{}, {}], but width/sign/factor/offset can represent [{}, {}]. Editors use the intersection.",
                                signal.min, signal.max, raw_min, raw_max
                            ),
                        );
                    }

                    if signal.mux_value.is_some() && multiplexors.is_empty() {
                        add(
                        DbcDiagnosticSeverity::Error,
                        "DBC-MUX-MISSING",
                        message,
                        &signal.name,
                        "复用分支缺少复用开关".into(),
                        "Multiplexed branch has no multiplexor".into(),
                        "信号声明了 m<n> 分支，但报文中没有 M 复用开关。".into(),
                        "The signal declares an m<n> branch, but the message has no M multiplexor."
                            .into(),
                    );
                    }
                    if signal.is_multiplexor && signal.mux_value.is_some() {
                        add(
                            DbcDiagnosticSeverity::Error,
                            "DBC-MUX-ROLE",
                            message,
                            &signal.name,
                            "信号同时声明为复用开关和分支".into(),
                            "Signal is both multiplexor and branch".into(),
                            "M 与 m<n> 角色冲突。".into(),
                            "The M and m<n> roles conflict.".into(),
                        );
                    }
                    if let (Some(branch), Some(mux)) = (signal.mux_value, multiplexors.first()) {
                        let mux_max = if mux.size >= 64 {
                            u64::MAX
                        } else {
                            (1u64 << mux.size) - 1
                        };
                        if branch > mux_max {
                            add(
                                DbcDiagnosticSeverity::Error,
                                "DBC-MUX-VALUE",
                                message,
                                &signal.name,
                                "复用分支值超出开关位宽".into(),
                                "Multiplex value exceeds selector width".into(),
                                format!(
                                    "分支值 {}，但 {} 位复用开关最大为 {}。",
                                    branch, mux.size, mux_max
                                ),
                                format!(
                                    "Branch value {} exceeds the {}-bit selector maximum {}.",
                                    branch, mux.size, mux_max
                                ),
                            );
                        }
                    }
                }

                for left_index in 0..message.signals.len() {
                    let left = &message.signals[left_index];
                    if !left.fits_in_bytes(message.size) {
                        continue;
                    }
                    let left_bits: HashSet<u64> = signal_bit_positions(left).into_iter().collect();
                    for right in message.signals.iter().skip(left_index + 1) {
                        if !right.fits_in_bytes(message.size) {
                            continue;
                        }
                        let exclusive_mux_branches = matches!(
                            (left.mux_value, right.mux_value),
                            (Some(left_mux), Some(right_mux)) if left_mux != right_mux
                        );
                        if exclusive_mux_branches {
                            continue;
                        }
                        if signal_bit_positions(right)
                            .into_iter()
                            .any(|bit| left_bits.contains(&bit))
                        {
                            add(
                                DbcDiagnosticSeverity::Error,
                                "DBC-SIG-OVERLAP",
                                message,
                                &right.name,
                                "信号位域重叠".into(),
                                "Signal bit fields overlap".into(),
                                format!(
                                    "“{}”与“{}”占用了相同位；编码其中一个会破坏另一个。",
                                    left.name, right.name
                                ),
                                format!(
                                    "'{}' and '{}' occupy the same bits; encoding one corrupts the other.",
                                    left.name, right.name
                                ),
                            );
                        }
                    }
                }
            }
        }
        result.sort_by(|left, right| {
            right
                .severity
                .cmp(&left.severity)
                .then_with(|| left.message_id.cmp(&right.message_id))
                .then_with(|| left.signal_name.cmp(&right.signal_name))
                .then_with(|| left.code.cmp(&right.code))
        });
        result
    }

    pub fn message(&self, id: u32) -> Option<&MessageDef> {
        self.by_id
            .get(&(id, false))
            .or_else(|| self.by_id.get(&(id, true)))
    }

    pub fn message_ext(&self, id: u32, extended: bool) -> Option<&MessageDef> {
        self.by_id.get(&(id, extended))
    }

    /// 把信号物理值编码成报文字节。values 缺省的信号按 0 处理。
    #[cfg(test)]
    pub fn encode(&self, id: u32, values: &HashMap<String, f64>) -> Option<Vec<u8>> {
        let extended = !self.by_id.contains_key(&(id, false));
        self.encode_ext(id, extended, values)
    }

    pub fn encode_ext(
        &self,
        id: u32,
        extended: bool,
        values: &HashMap<String, f64>,
    ) -> Option<Vec<u8>> {
        self.encode_checked_ext(id, extended, values).ok()
    }

    pub fn encode_checked_ext(
        &self,
        id: u32,
        extended: bool,
        values: &HashMap<String, f64>,
    ) -> Result<Vec<u8>, String> {
        let m = self
            .message_ext(id, extended)
            .ok_or_else(|| format!("DBC message 0x{id:X} ext={extended} not found"))?;
        if let Some(unknown) = values
            .keys()
            .find(|name| !m.signals.iter().any(|signal| signal.name == name.as_str()))
        {
            return Err(format!("DBC signal {unknown} not found in 0x{id:X}"));
        }
        let len = (m.size as usize).clamp(1, 64);
        let mut data = vec![0u8; len];
        let mux_raw = m
            .signals
            .iter()
            .find(|signal| signal.is_multiplexor)
            .filter(|signal| signal.fits_in_bytes(m.size))
            .map(|signal| {
                let physical = values.get(&signal.name).copied().unwrap_or(0.0);
                encode_signal_raw(signal, physical)
            })
            .transpose()?;
        for s in &m.signals {
            if !s.fits_in_bytes(m.size) {
                if values.contains_key(&s.name) {
                    return Err(format!(
                        "signal {} bit layout exceeds message 0x{id:X} DLC {}",
                        s.name, m.size
                    ));
                }
                continue;
            }
            if let Some(branch) = s.mux_value
                && mux_raw != Some(branch)
            {
                continue;
            }
            let phys = values.get(&s.name).copied().unwrap_or(0.0);
            let raw = encode_signal_raw(s, phys)?;
            insert(&mut data, s.start_bit, s.size, s.little_endian, raw);
        }
        Ok(data)
    }

    /// Update one signal in an existing frame while preserving all other signal
    /// bits. This is used by simulation controls that share one CAN message.
    pub fn encode_signal_into_ext(
        &self,
        id: u32,
        extended: bool,
        base: &[u8],
        signal_name: &str,
        physical: f64,
    ) -> Result<Vec<u8>, String> {
        let message = self
            .message_ext(id, extended)
            .ok_or_else(|| format!("DBC message 0x{id:X} ext={extended} not found"))?;
        let len = (message.size as usize).clamp(1, 64);
        let mut data = vec![0u8; len];
        let copy_len = base.len().min(len);
        data[..copy_len].copy_from_slice(&base[..copy_len]);
        let signal = message
            .signals
            .iter()
            .find(|signal| signal.name == signal_name)
            .ok_or_else(|| format!("DBC signal {signal_name} not found in 0x{id:X}"))?;
        if !signal.fits_in_bytes(message.size) {
            return Err(format!(
                "signal {signal_name} bit layout exceeds message 0x{id:X} DLC {}",
                message.size
            ));
        }
        let raw = encode_signal_raw(signal, physical)?;
        insert(
            &mut data,
            signal.start_bit,
            signal.size,
            signal.little_endian,
            raw,
        );
        Ok(data)
    }

    /// 解码给定 ID 的全部信号。无匹配返回空。
    pub fn decode(&self, id: u32, data: &[u8]) -> Vec<Decoded> {
        let extended = !self.by_id.contains_key(&(id, false));
        self.decode_ext(id, extended, data)
    }

    pub fn decode_ext(&self, id: u32, extended: bool, data: &[u8]) -> Vec<Decoded> {
        self.decode_checked_ext(id, extended, data)
            .unwrap_or_default()
    }

    pub fn decode_checked_ext(
        &self,
        id: u32,
        extended: bool,
        data: &[u8],
    ) -> Result<Vec<Decoded>, String> {
        self.decode_checked_ext_impl(id, extended, data, false)
    }

    /// 返回报文的全部信号定义。未选中的 MUX 分支仅供列表展示，
    /// `mux_active` 为 false，不应将当前重叠位解码值当作有效值。
    pub fn decode_all_ext(&self, id: u32, extended: bool, data: &[u8]) -> Vec<Decoded> {
        self.decode_checked_ext_impl(id, extended, data, true)
            .unwrap_or_default()
    }

    fn decode_checked_ext_impl(
        &self,
        id: u32,
        extended: bool,
        data: &[u8],
        include_inactive_mux: bool,
    ) -> Result<Vec<Decoded>, String> {
        let m = self
            .message_ext(id, extended)
            .ok_or_else(|| format!("DBC message 0x{id:X} ext={extended} not found"))?;
        let required = (m.size as usize).min(64);
        if data.len() < required {
            return Err(format!(
                "truncated DBC frame 0x{id:X}: got {} bytes, need {required}",
                data.len()
            ));
        }
        // 复用开关的原始值（用于筛选有效的复用信号）
        let mux_raw: Option<u64> = m
            .signals
            .iter()
            .find(|s| s.is_multiplexor && s.fits_in_bytes(m.size))
            .map(|s| extract_bits(data, s.start_bit, s.size, s.little_endian));
        Ok(m.signals
            .iter()
            .filter(|s| s.fits_in_bytes(m.size))
            .filter(|s| {
                include_inactive_mux
                    || match s.mux_value {
                        Some(n) => mux_raw == Some(n),
                        None => true,
                    }
            })
            .map(|s| {
                let mux_active = s.mux_value.is_none_or(|n| mux_raw == Some(n));
                let raw_bits = extract_bits(data, s.start_bit, s.size, s.little_endian);
                let signed_raw = sign_extend(raw_bits, s.size);
                let raw_unsigned = (!s.signed).then_some(raw_bits);
                let raw = if s.signed {
                    signed_raw
                } else {
                    i64::try_from(raw_bits).unwrap_or(i64::MAX)
                };
                let raw_text = if s.signed {
                    signed_raw.to_string()
                } else {
                    raw_bits.to_string()
                };
                // 浮点信号: 把位模式重解释为 f32/f64, 而非整数缩放
                let physical = match s.float_bits {
                    32 => f32::from_bits(raw_bits as u32) as f64 * s.factor + s.offset,
                    64 => f64::from_bits(raw_bits) * s.factor + s.offset,
                    _ if s.signed => signed_raw as f64 * s.factor + s.offset,
                    _ => raw_bits as f64 * s.factor + s.offset,
                };
                let out = s.max > s.min && (physical < s.min || physical > s.max);
                let enum_txt = self
                    .enums
                    .get(&(id, extended, s.name.clone()))
                    .and_then(|v| {
                        v.iter()
                            .find(|(val, _)| *val == signed_raw)
                            .map(|(_, d)| d.clone())
                    })
                    .unwrap_or_default();
                Decoded {
                    name: s.name.clone(),
                    raw,
                    raw_unsigned,
                    raw_text,
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
                    mux_active,
                    mux_value: s.mux_value,
                }
            })
            .collect())
    }
}

fn encode_signal_raw(signal: &SignalDef, physical: f64) -> Result<u64, String> {
    if signal.size == 0 || signal.size > 64 {
        return Err(format!(
            "signal {} has invalid bit size {}",
            signal.name, signal.size
        ));
    }
    if !physical.is_finite() || !signal.factor.is_finite() || signal.factor == 0.0 {
        return Err(format!(
            "signal {} requires a finite value and non-zero factor",
            signal.name
        ));
    }
    let normalized = (physical - signal.offset) / signal.factor;
    if !normalized.is_finite() {
        return Err(format!(
            "signal {} normalized value is not finite",
            signal.name
        ));
    }
    match signal.float_bits {
        32 => Ok((normalized as f32).to_bits() as u64),
        64 => Ok(normalized.to_bits()),
        _ if signal.signed => {
            let rounded = normalized.round();
            let (minimum, maximum) = if signal.size == 64 {
                (i64::MIN as f64, i64::MAX as f64)
            } else {
                let limit = 1i128 << (signal.size - 1);
                (-(limit as f64), (limit - 1) as f64)
            };
            if rounded < minimum || rounded > maximum {
                return Err(format!(
                    "signal {} signed value is out of range",
                    signal.name
                ));
            }
            let raw = rounded as i64 as u64;
            Ok(if signal.size == 64 {
                raw
            } else {
                raw & ((1u64 << signal.size) - 1)
            })
        }
        _ => {
            let rounded = normalized.round();
            let maximum = if signal.size == 64 {
                u64::MAX as f64
            } else {
                ((1u128 << signal.size) - 1) as f64
            };
            if rounded < 0.0 || rounded > maximum {
                return Err(format!(
                    "signal {} unsigned value is out of range",
                    signal.name
                ));
            }
            Ok(rounded as u64)
        }
    }
}

fn signal_bit_positions(signal: &SignalDef) -> Vec<u64> {
    if signal.size == 0 || signal.size > 64 {
        return Vec::new();
    }
    if signal.little_endian {
        return (0..signal.size)
            .filter_map(|offset| signal.start_bit.checked_add(offset))
            .collect();
    }
    let mut positions = Vec::with_capacity(signal.size as usize);
    let mut bit = signal.start_bit as i128;
    for _ in 0..signal.size {
        if bit < 0 || bit > u64::MAX as i128 {
            break;
        }
        positions.push(bit as u64);
        bit = if bit % 8 == 0 { bit + 15 } else { bit - 1 };
    }
    positions
}

fn sign_extend(raw: u64, size: u64) -> i64 {
    let size = size.min(64) as u32;
    if size == 0 || size == 64 {
        return raw as i64;
    }
    let sign = 1u64 << (size - 1);
    if raw & sign == 0 {
        raw as i64
    } else {
        (raw | !((1u64 << size) - 1)) as i64
    }
}

/// 提取位域，保留完整的无符号 64 位位模式。
fn extract_bits(data: &[u8], start_bit: u64, size: u64, little_endian: bool) -> u64 {
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

    val
}

/// 写位域（extract_bits 的逆操作）。
fn insert(data: &mut [u8], start_bit: u64, size: u64, little_endian: bool, raw: u64) {
    let size = size.min(64) as u32;
    if size == 0 {
        return;
    }
    // 取低 size 位
    let val = if size >= 64 {
        raw
    } else {
        raw & ((1u64 << size) - 1)
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
        vals.insert("F".to_string(), std::f64::consts::PI);
        let data = db.encode(512, &vals).unwrap();
        let dec = db.decode(512, &data);
        let f = dec.iter().find(|d| d.name == "F").unwrap();
        // f32 精度: 误差应远小于 1e-4
        assert!(
            (f.physical - std::f64::consts::PI).abs() < 1e-4,
            "got {}",
            f.physical
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn float64_roundtrip() {
        let txt = "VERSION \"\"\nBO_ 513 DMsg: 8 ECU\n SG_ D : 0|64@1+ (1,0) [0|0] \"\" Vector__XXX\nSIG_VALTYPE_ 513 D : 2;\n";
        let p = std::env::temp_dir().join("pcanwork_f64_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("D".to_string(), std::f64::consts::E);
        let data = db.encode(513, &vals).unwrap();
        let dec = db.decode(513, &data);
        let d = dec.iter().find(|x| x.name == "D").unwrap();
        assert!(
            (d.physical - std::f64::consts::E).abs() < 1e-9,
            "got {}",
            d.physical
        );
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
        assert!(
            d0.iter()
                .any(|d| d.name == "A" && (d.physical - 42.0).abs() < 1e-9)
        );
        assert!(!d0.iter().any(|d| d.name == "B"));

        // Mux=1 → 只出 Mux + B=99, 无 A
        let d1 = db.decode(768, &[1, 99, 0, 0, 0, 0, 0, 0]);
        assert!(
            d1.iter()
                .any(|d| d.name == "B" && (d.physical - 99.0).abs() < 1e-9)
        );
        assert!(!d1.iter().any(|d| d.name == "A"));

        // 列表展开使用全分支解码：两个分支均返回，但只有 MUX=1 的 B 有效。
        let all = db.decode_all_ext(768, false, &[1, 99, 0, 0, 0, 0, 0, 0]);
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|d| d.name == "Mux" && d.mux_active));
        assert!(
            all.iter()
                .any(|d| d.name == "A" && !d.mux_active && d.mux_value == Some(0))
        );
        assert!(
            all.iter()
                .any(|d| d.name == "B" && d.mux_active && d.mux_value == Some(1))
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn multiplexed_encode_only_writes_the_selected_branch() {
        let txt = "VERSION \"\"\nBO_ 768 MuxMsg: 2 ECU\n SG_ Mux M : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ A m0 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B m1 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_mux_encode_test.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("Mux".to_string(), 1.0);
        vals.insert("A".to_string(), 42.0);
        vals.insert("B".to_string(), 99.0);

        let data = db.encode_checked_ext(768, false, &vals).unwrap();
        assert_eq!(data, vec![1, 99]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn standard_and_extended_messages_with_the_same_id_are_distinct() {
        let extended_id = (1u32 << 31) | 0x123;
        let txt = format!(
            "VERSION \"\"\nBO_ 291 StandardMsg: 1 ECU\n SG_ StandardValue : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\nBO_ {extended_id} ExtendedMsg: 1 ECU\n SG_ ExtendedValue : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n"
        );
        let p = std::env::temp_dir().join("pcanwork_std_ext_same_id.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();

        assert_eq!(db.message_name_ext(0x123, false), Some("StandardMsg"));
        assert_eq!(db.message_name_ext(0x123, true), Some("ExtendedMsg"));
        assert_eq!(db.decode_ext(0x123, false, &[7])[0].name, "StandardValue");
        assert_eq!(db.decode_ext(0x123, true, &[8])[0].name, "ExtendedValue");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unsigned_64_bit_maximum_is_preserved_exactly() {
        let txt = "VERSION \"\"\nBO_ 1024 U64Msg: 8 ECU\n SG_ U64 : 0|64@1+ (1,0) [0|0] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_u64_max.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();

        let decoded = db.decode_checked_ext(1024, false, &[0xFF; 8]).unwrap();
        assert_eq!(decoded[0].raw_unsigned, Some(u64::MAX));
        assert_eq!(decoded[0].raw_text, u64::MAX.to_string());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn short_frame_is_rejected_instead_of_zero_filled() {
        let txt = "VERSION \"\"\nBO_ 1025 ShortMsg: 8 ECU\n SG_ Tail : 56|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_short_frame.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();

        let error = db
            .decode_checked_ext(1025, false, &[0x11, 0x22])
            .err()
            .expect("short frame must fail");
        assert!(error.contains("truncated"));
        assert!(db.decode_ext(1025, false, &[0x11, 0x22]).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn encode_rejects_unsigned_value_outside_signal_width() {
        let txt = "VERSION \"\"\nBO_ 1026 RangeMsg: 1 ECU\n SG_ Byte : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_encode_range.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let mut vals = HashMap::new();
        vals.insert("Byte".to_string(), 300.0);

        assert!(db.encode_checked_ext(1026, false, &vals).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn effective_range_intersects_malformed_dbc_range_with_bit_width() {
        let signal = SignalDef {
            name: "Temperature".into(),
            start_bit: 0,
            size: 8,
            little_endian: true,
            signed: false,
            factor: 1.0,
            offset: -40.0,
            min: 0.0,
            max: 65535.0,
            unit: "degC".into(),
            float_bits: 0,
            is_multiplexor: false,
            mux_value: None,
        };
        assert_eq!(signal.effective_physical_range(), Some((0.0, 215.0)));
    }

    #[test]
    fn patching_one_signal_preserves_other_signal_bits() {
        let txt = "VERSION \"\"\nBO_ 256 Pair: 2 ECU\n SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_signal_patch.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let first = db
            .encode_signal_into_ext(256, false, &[], "A", 17.0)
            .unwrap();
        let second = db
            .encode_signal_into_ext(256, false, &first, "B", 29.0)
            .unwrap();
        assert_eq!(second, vec![17, 29]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn encode_rejects_unknown_signal_name() {
        let txt =
            "VERSION \"\"\nBO_ 256 Known: 1 ECU\n SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_unknown_signal.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let values = HashMap::from([("Typo".to_string(), 1.0)]);
        assert!(db.encode_checked_ext(256, false, &values).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn signal_beyond_message_dlc_is_never_silently_encoded_or_decoded() {
        let txt = "VERSION \"\"\nBO_ 256 BadLayout: 8 ECU\n SG_ Good : 39|16@0+ (1,0) [0|65535] \"\" Vector__XXX\n SG_ Outside : 71|16@0+ (1,0) [0|65535] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_signal_outside_dlc.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let good = HashMap::from([("Good".to_string(), 1234.0)]);
        assert!(db.encode_checked_ext(256, false, &good).is_ok());
        let outside = HashMap::from([("Outside".to_string(), 1.0)]);
        assert!(db.encode_checked_ext(256, false, &outside).is_err());
        let decoded = db.decode_checked_ext(256, false, &[0; 8]).unwrap();
        assert!(decoded.iter().any(|signal| signal.name == "Good"));
        assert!(!decoded.iter().any(|signal| signal.name == "Outside"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn diagnostics_find_layout_range_factor_overlap_and_mux_errors() {
        let txt = "VERSION \"\"\nBO_ 256 Problems: 8 ECU\n SG_ Selector M : 0|1@1+ (1,0) [0|1] \"\" Vector__XXX\n SG_ A : 8|8@1+ (1,-40) [0|65535] \"\" Vector__XXX\n SG_ B : 8|8@1+ (0,0) [10|0] \"\" Vector__XXX\n SG_ MissingMux m3 : 71|16@0+ (1,0) [0|65535] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_dbc_diagnostics.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        let codes: HashSet<String> = db.diagnostics().into_iter().map(|item| item.code).collect();
        assert!(codes.contains("DBC-SIG-DLC-OVERFLOW"));
        assert!(codes.contains("DBC-SIG-RANGE-WIDTH"));
        assert!(codes.contains("DBC-SIG-FACTOR"));
        assert!(codes.contains("DBC-SIG-RANGE-ORDER"));
        assert!(codes.contains("DBC-SIG-OVERLAP"));
        assert!(codes.contains("DBC-MUX-VALUE"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn diagnostics_allow_bit_overlap_between_exclusive_mux_branches() {
        let txt = "VERSION \"\"\nBO_ 768 MuxMsg: 2 ECU\n SG_ Mux M : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ A m0 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n SG_ B m1 : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        let p = std::env::temp_dir().join("pcanwork_dbc_mux_overlap.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        assert!(
            !db.diagnostics()
                .iter()
                .any(|item| item.code == "DBC-SIG-OVERLAP")
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn diagnostics_warn_when_standard_and_extended_ids_share_numeric_value() {
        let ext = (1u32 << 31) | 0x123;
        let txt = format!(
            "VERSION \"\"\nBO_ 291 Std: 1 ECU\n SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\nBO_ {ext} Ext: 1 ECU\n SG_ B : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n"
        );
        let p = std::env::temp_dir().join("pcanwork_dbc_id_format_conflict.dbc");
        std::fs::write(&p, txt).unwrap();
        let db = DbcDb::load(&p.to_string_lossy()).unwrap();
        assert_eq!(
            db.diagnostics()
                .iter()
                .filter(|item| item.code == "DBC-ID-FORMAT-CONFLICT")
                .count(),
            1
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    #[ignore = "external DBC product gate; set PCANWORK_TEST_DBC"]
    fn external_dbc_diagnostics_report() {
        let path = std::env::var("PCANWORK_TEST_DBC").expect("PCANWORK_TEST_DBC is required");
        let db = DbcDb::load(&path).expect("external DBC must load");
        let diagnostics = db.diagnostics();
        let errors = diagnostics
            .iter()
            .filter(|item| item.severity == DbcDiagnosticSeverity::Error)
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|item| item.severity == DbcDiagnosticSeverity::Warning)
            .count();
        let infos = diagnostics.len().saturating_sub(errors + warnings);
        println!(
            "DBC_DIAGNOSTICS file={} errors={} warnings={} infos={}",
            db.file_name, errors, warnings, infos
        );
        for item in diagnostics {
            println!(
                "{:?} {} 0x{:X} {} {}: {}",
                item.severity,
                item.code,
                item.message_id,
                item.message_name,
                item.signal_name,
                item.detail_zh
            );
        }
        if std::env::var_os("PCANWORK_EXPECT_CLEAN_DBC").is_some() {
            assert_eq!(errors, 0, "DBC contains blocking diagnostics");
        }
    }
}
