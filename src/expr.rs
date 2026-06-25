//! 表达式派生信号求值器。
//!
//! 支持 `+ - * / %`、位运算 `& | ^ << >>`、括号、一元 `+/-`、十进制/`0x` 十六进制数,
//! 以及**按名字引用 DBC 信号**(标识符,如 `Voltage`、`Cell_Temp_1`)。求值时标识符
//! 从传入的「信号名 -> 最新物理值」表里取;未出现过的信号取 0(尚无帧时不报错,
//! 等帧到了曲线自然跟上)。
//!
//! 改编自 modbus 工具的 expr.rs,把寄存器引用 `rN` 换成了任意标识符。

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char), // + - * / % & | ^ ( )
    Shl,
    Shr,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            // 标识符(信号名): 字母/下划线开头, 之后可含字母/数字/下划线
            let start = i;
            while i < b.len() {
                let ch = b[i] as char;
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(Tok::Ident(s[start..i].to_string()));
        } else if c.is_ascii_digit() || c == '.' {
            if c == '0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                let start = i + 2;
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                let v = i64::from_str_radix(&s[start..i], 16).map_err(|_| "十六进制数无效".to_string())?;
                out.push(Tok::Num(v as f64));
            } else {
                let start = i;
                while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                let v: f64 = s[start..i].parse().map_err(|_| "数字无效".to_string())?;
                out.push(Tok::Num(v));
            }
        } else if c == '<' && i + 1 < b.len() && b[i + 1] == b'<' {
            out.push(Tok::Shl);
            i += 2;
        } else if c == '>' && i + 1 < b.len() && b[i + 1] == b'>' {
            out.push(Tok::Shr);
            i += 2;
        } else if "+-*/%&|^()".contains(c) {
            out.push(Tok::Op(c));
            i += 1;
        } else {
            return Err(format!("无法识别的字符 '{c}'"));
        }
    }
    Ok(out)
}

/// 递归下降深度上限,防止深层嵌套把线程栈撑爆。
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    t: Vec<Tok>,
    pos: usize,
    vars: &'a HashMap<String, f64>,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.pos)
    }
    fn eat(&mut self) -> Option<Tok> {
        let v = self.t.get(self.pos).cloned();
        self.pos += 1;
        v
    }
    // | (最低) -> ^ -> & -> 移位 -> 加减 -> 乘除 -> 一元 -> 原子
    fn or_expr(&mut self) -> Result<f64, String> {
        let mut v = self.xor_expr()?;
        while matches!(self.peek(), Some(Tok::Op('|'))) {
            self.eat();
            let r = self.xor_expr()?;
            v = ((v as i64) | (r as i64)) as f64;
        }
        Ok(v)
    }
    fn xor_expr(&mut self) -> Result<f64, String> {
        let mut v = self.and_expr()?;
        while matches!(self.peek(), Some(Tok::Op('^'))) {
            self.eat();
            let r = self.and_expr()?;
            v = ((v as i64) ^ (r as i64)) as f64;
        }
        Ok(v)
    }
    fn and_expr(&mut self) -> Result<f64, String> {
        let mut v = self.shift_expr()?;
        while matches!(self.peek(), Some(Tok::Op('&'))) {
            self.eat();
            let r = self.shift_expr()?;
            v = ((v as i64) & (r as i64)) as f64;
        }
        Ok(v)
    }
    fn shift_expr(&mut self) -> Result<f64, String> {
        let mut v = self.add_expr()?;
        loop {
            match self.peek() {
                Some(Tok::Shl) => {
                    self.eat();
                    let s = self.add_expr()? as i64;
                    if !(0..64).contains(&s) {
                        return Err("移位位数必须在 0..63".into());
                    }
                    v = ((v as i64) << s) as f64;
                }
                Some(Tok::Shr) => {
                    self.eat();
                    let s = self.add_expr()? as i64;
                    if !(0..64).contains(&s) {
                        return Err("移位位数必须在 0..63".into());
                    }
                    v = ((v as i64) >> s) as f64;
                }
                _ => break,
            }
        }
        Ok(v)
    }
    fn add_expr(&mut self) -> Result<f64, String> {
        let mut v = self.mul_expr()?;
        loop {
            match self.peek() {
                Some(Tok::Op('+')) => { self.eat(); v += self.mul_expr()?; }
                Some(Tok::Op('-')) => { self.eat(); v -= self.mul_expr()?; }
                _ => break,
            }
        }
        Ok(v)
    }
    fn mul_expr(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            match self.peek() {
                Some(Tok::Op('*')) => { self.eat(); v *= self.unary()?; }
                Some(Tok::Op('/')) => { self.eat(); let r = self.unary()?; v /= r; }
                Some(Tok::Op('%')) => { self.eat(); let r = self.unary()?; v %= r; }
                _ => break,
            }
        }
        Ok(v)
    }
    fn unary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Tok::Op('-')) => {
                self.eat();
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("表达式嵌套过深".into());
                }
                let r = self.unary();
                self.depth -= 1;
                Ok(-r?)
            }
            Some(Tok::Op('+')) => {
                self.eat();
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("表达式嵌套过深".into());
                }
                let r = self.unary();
                self.depth -= 1;
                r
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<f64, String> {
        match self.eat() {
            Some(Tok::Num(n)) => Ok(n),
            // 未知信号(尚无帧)取 0, 不报错
            Some(Tok::Ident(name)) => Ok(self.vars.get(&name).copied().unwrap_or(0.0)),
            Some(Tok::Op('(')) => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err("表达式嵌套过深".into());
                }
                let v = self.or_expr();
                self.depth -= 1;
                let v = v?;
                match self.eat() {
                    Some(Tok::Op(')')) => Ok(v),
                    _ => Err("缺少右括号 ')'".into()),
                }
            }
            other => Err(format!("意外的记号 {other:?}")),
        }
    }
}

/// 用「信号名 -> 最新值」表求值表达式。未出现过的信号当 0。
pub fn eval(formula: &str, vars: &HashMap<String, f64>) -> Result<f64, String> {
    let t = tokenize(formula)?;
    if t.is_empty() {
        return Err("空表达式".into());
    }
    let mut p = Parser { t, pos: 0, vars, depth: 0 };
    let v = p.or_expr()?;
    if p.pos != p.t.len() {
        return Err("表达式末尾有多余内容".into());
    }
    Ok(v)
}

/// 校验语法(把所有信号当 0),返回表达式引用到的所有信号名(去重、保持出现顺序)。
/// 语法错误时返回 Err。
pub fn refs(formula: &str) -> Result<Vec<String>, String> {
    let t = tokenize(formula)?;
    if t.is_empty() {
        return Err("空表达式".into());
    }
    // 先做一次语法校验(空变量表 -> 全 0)
    let empty = HashMap::new();
    let mut p = Parser { t: t.clone(), pos: 0, vars: &empty, depth: 0 };
    let _ = p.or_expr()?;
    if p.pos != p.t.len() {
        return Err("表达式末尾有多余内容".into());
    }
    let mut names = Vec::new();
    for tok in &t {
        if let Tok::Ident(n) = tok
            && !names.contains(n) {
            names.push(n.clone());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn arithmetic_and_signals() {
        let v = vars(&[("Voltage", 12.0), ("Current", 5.0)]);
        assert_eq!(eval("Voltage * Current", &v).unwrap(), 60.0);
        assert_eq!(eval("Voltage * Current / 1000", &v).unwrap(), 0.06);
        assert_eq!(eval("(Voltage + 8) * 2", &v).unwrap(), 40.0);
        assert_eq!(eval("-Current + 1", &v).unwrap(), -4.0);
        assert_eq!(eval("1 << 4", &v).unwrap(), 16.0);
        // 未知信号当 0
        assert_eq!(eval("Voltage + Missing", &v).unwrap(), 12.0);
        assert!(eval("Voltage +", &v).is_err());
        assert!(eval("Voltage @ Current", &v).is_err());
    }

    #[test]
    fn refs_collects_signal_names() {
        let r = refs("Voltage * Current / 1000 + Voltage").unwrap();
        assert_eq!(r, vec!["Voltage".to_string(), "Current".to_string()]);
        assert!(refs("1 + )").is_err());
    }
}
