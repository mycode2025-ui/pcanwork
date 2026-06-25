//! Core Modbus domain types shared by the master (poll) and slave (simulator)
//! engines: register areas, transport configuration and the slave data store.

/// The four standard Modbus data tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Area {
    Coils,            // 0xxxx — read/write bits
    DiscreteInputs,   // 1xxxx — read-only bits
    HoldingRegisters, // 4xxxx — read/write 16-bit words
    InputRegisters,   // 3xxxx — read-only 16-bit words
}

impl Area {
    pub fn from_index(i: i32) -> Self {
        match i {
            0 => Area::Coils,
            1 => Area::DiscreteInputs,
            2 => Area::HoldingRegisters,
            3 => Area::InputRegisters,
            _ => Area::HoldingRegisters,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Area::Coils => "Coils (0x)",
            Area::DiscreteInputs => "Discrete Inputs (1x)",
            Area::HoldingRegisters => "Holding Registers (4x)",
            Area::InputRegisters => "Input Registers (3x)",
        }
    }

    /// PLC (Modicon) base-1 address prefix digit for this area.
    pub fn plc_prefix(self) -> char {
        match self {
            Area::Coils => '0',
            Area::DiscreteInputs => '1',
            Area::InputRegisters => '3',
            Area::HoldingRegisters => '4',
        }
    }

    /// 6-digit PLC base-1 address, e.g. holding register 0 → "400001".
    pub fn plc_addr(self, offset: u16) -> String {
        format!("{}{:05}", self.plc_prefix(), offset as u32 + 1)
    }
}

/// Physical/link layer used to reach the peer.
#[derive(Clone, Debug)]
pub enum Transport {
    Tcp {
        host: String,
        port: u16,
    },
    Rtu {
        path: String,
        baud: u32,
        data_bits: u8,
        parity: u8, // 0 = none, 1 = even, 2 = odd
        stop_bits: u8,
    },
}

impl Transport {
    pub fn describe(&self) -> String {
        match self {
            Transport::Tcp { host, port } => format!("TCP {host}:{port}"),
            Transport::Rtu {
                path,
                baud,
                data_bits,
                parity,
                stop_bits,
            } => {
                let p = match parity {
                    1 => 'E',
                    2 => 'O',
                    _ => 'N',
                };
                format!("RTU {path} {baud} {data_bits}{p}{stop_bits}")
            }
        }
    }
}

/// Comparison operator for a conditional-colour rule.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    None,
    Eq,
    Gt,
    Lt,
    Ge,
    Le,
    And, // bitwise AND mask (non-zero → match)
}

impl CmpOp {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => CmpOp::Eq,
            2 => CmpOp::Gt,
            3 => CmpOp::Lt,
            4 => CmpOp::Ge,
            5 => CmpOp::Le,
            6 => CmpOp::And,
            _ => CmpOp::None,
        }
    }
    pub fn test(self, val: f64, cmp: f64) -> bool {
        match self {
            CmpOp::None => false,
            CmpOp::Eq => val == cmp,
            CmpOp::Gt => val > cmp,
            CmpOp::Lt => val < cmp,
            CmpOp::Ge => val >= cmp,
            CmpOp::Le => val <= cmp,
            CmpOp::And => ((val as i64) & (cmp as i64)) != 0,
        }
    }
}

/// Conditional-colour rules applied to the data grid. Colours are ARGB; a value
/// of 0 means "use the default theme colour". Rule 1 takes precedence over rule 2.
#[derive(Clone, Copy)]
pub struct ColorRules {
    pub normal: u32,
    pub op1: CmpOp,
    pub v1: f64,
    pub c1: u32,
    pub op2: CmpOp,
    pub v2: f64,
    pub c2: u32,
}

impl ColorRules {
    pub fn off() -> Self {
        ColorRules {
            normal: 0,
            op1: CmpOp::None,
            v1: 0.0,
            c1: 0,
            op2: CmpOp::None,
            v2: 0.0,
            c2: 0,
        }
    }
    /// Returns the ARGB colour for a value (0 = default).
    pub fn eval(&self, num: f64) -> u32 {
        if self.op1.test(num, self.v1) {
            self.c1
        } else if self.op2.test(num, self.v2) {
            self.c2
        } else {
            self.normal
        }
    }
}

/// Maps register values to descriptive labels (e.g. `0=Off`, `1=Running`).
/// When enabled, the grid shows the label for a value if one exists, otherwise
/// the numeric value. Loaded from / saved to a `value=name` text file.
#[derive(Clone, Default)]
pub struct ValueNames {
    pub enabled: bool,
    pub map: std::collections::HashMap<i64, String>,
}

impl ValueNames {
    pub fn off() -> Self {
        ValueNames { enabled: false, map: std::collections::HashMap::new() }
    }

    /// Returns the label for an (integer-valued) number, if mapped and enabled.
    pub fn lookup(&self, num: f64) -> Option<&str> {
        if !self.enabled || num.fract().abs() > 1e-9 {
            return None;
        }
        self.map.get(&(num as i64)).map(|s| s.as_str())
    }

    /// Parse `value=name` lines (decimal or `0x` hex keys; `#`/`//` comments).
    pub fn parse(text: &str) -> std::collections::HashMap<i64, String> {
        let mut m = std::collections::HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let key = if let Some(h) = k.strip_prefix("0x").or_else(|| k.strip_prefix("0X")) {
                    i64::from_str_radix(h, 16).ok()
                } else {
                    k.parse::<i64>().ok()
                };
                if let Some(key) = key {
                    m.insert(key, v.trim().to_string());
                }
            }
        }
        m
    }

    /// Serialise to sorted `value=name` lines.
    #[allow(dead_code)]
    pub fn to_text(&self) -> String {
        let mut keys: Vec<i64> = self.map.keys().copied().collect();
        keys.sort_unstable();
        keys.iter()
            .map(|k| format!("{}={}", k, self.map[k]))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod value_names_tests {
    use super::ValueNames;

    #[test]
    fn parse_lookup_roundtrip() {
        let text = "# states\n0=Off\n1 = Running\n0x02=Fault\n//comment\n";
        let map = ValueNames::parse(text);
        assert_eq!(map.len(), 3);
        let vn = ValueNames { enabled: true, map };
        assert_eq!(vn.lookup(0.0), Some("Off"));
        assert_eq!(vn.lookup(1.0), Some("Running"));
        assert_eq!(vn.lookup(2.0), Some("Fault")); // 0x02
        assert_eq!(vn.lookup(3.0), None); // unmapped
        assert_eq!(vn.lookup(1.5), None); // non-integer

        let disabled = ValueNames { enabled: false, ..vn.clone() };
        assert_eq!(disabled.lookup(0.0), None);

        // sorted serialisation
        assert_eq!(vn.to_text(), "0=Off\n1=Running\n2=Fault");
    }
}

/// Backing memory for the slave simulator. The full 16-bit address space of each
/// table is pre-allocated so any legal address can be served or edited.
pub struct DataStore {
    pub coils: Vec<bool>,
    pub discrete_inputs: Vec<bool>,
    pub holding: Vec<u16>,
    pub input: Vec<u16>,
}

impl DataStore {
    pub fn new() -> Self {
        Self {
            coils: vec![false; 65536],
            discrete_inputs: vec![false; 65536],
            holding: vec![0; 65536],
            input: vec![0; 65536],
        }
    }
}
