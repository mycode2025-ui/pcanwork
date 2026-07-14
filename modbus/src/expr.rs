//! A tiny expression evaluator for derived channels.
//!
//! Supports `+ - * / %`, bitwise `& | ^ << >>`, parentheses, unary minus,
//! decimal / `0x` hex numbers, and register references `r0`, `r1`, … (the values
//! of the currently polled register window, `r0` = first register).

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Reg(usize),
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
        } else if c == 'r' || c == 'R' {
            // register reference rN
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return Err("expected register index after 'r'".into());
            }
            let idx: usize = s[start..i]
                .parse()
                .map_err(|_| "bad register index".to_string())?;
            out.push(Tok::Reg(idx));
        } else if c.is_ascii_digit() || c == '.' {
            // number (hex or decimal float)
            if c == '0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                let start = i + 2;
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                let v = i64::from_str_radix(&s[start..i], 16).map_err(|_| "bad hex".to_string())?;
                out.push(Tok::Num(v as f64));
            } else {
                let start = i;
                while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                let v: f64 = s[start..i].parse().map_err(|_| "bad number".to_string())?;
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
            return Err(format!("unexpected character '{c}'"));
        }
    }
    Ok(out)
}

/// 递归下降深度上限：防止深层嵌套 (((…))) / ----x 把线程栈撑爆(栈溢出无法 catch)。
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    t: Vec<Tok>,
    pos: usize,
    regs: &'a [u16],
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.pos)
    }
    fn eat(&mut self) -> Option<Tok> {
        let v = self.t.get(self.pos).cloned();
        self.pos += 1;
        v
    }

    // | (lowest) -> ^ -> & -> shift -> add -> mul -> unary -> primary
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
                Some(Tok::Op('+')) => {
                    self.eat();
                    v += self.mul_expr()?;
                }
                Some(Tok::Op('-')) => {
                    self.eat();
                    v -= self.mul_expr()?;
                }
                _ => break,
            }
        }
        Ok(v)
    }
    fn mul_expr(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            match self.peek() {
                Some(Tok::Op('*')) => {
                    self.eat();
                    v *= self.unary()?;
                }
                Some(Tok::Op('/')) => {
                    self.eat();
                    let r = self.unary()?;
                    v /= r;
                }
                Some(Tok::Op('%')) => {
                    self.eat();
                    let r = self.unary()?;
                    v %= r;
                }
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
            Some(Tok::Reg(i)) => Ok(*self.regs.get(i).unwrap_or(&0) as f64),
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
                    _ => Err("expected ')'".into()),
                }
            }
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

pub fn eval_formula(expr: &str, regs: &[u16]) -> Result<f64, String> {
    let t = tokenize(expr)?;
    if t.is_empty() {
        return Err("empty formula".into());
    }
    let mut p = Parser {
        t,
        pos: 0,
        regs,
        depth: 0,
    };
    let v = p.or_expr()?;
    if p.pos != p.t.len() {
        return Err("unexpected trailing input".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_and_registers() {
        let regs = [0x1234u16, 0x00FFu16, 10u16];
        assert_eq!(eval_formula("r0", &regs).unwrap(), 4660.0);
        assert_eq!(
            eval_formula("r0*256 + r1", &regs).unwrap(),
            4660.0 * 256.0 + 255.0
        );
        assert_eq!(eval_formula("(r1 & 0x0F)", &regs).unwrap(), 15.0);
        assert_eq!(eval_formula("r2 * 0.1", &regs).unwrap(), 1.0);
        assert_eq!(eval_formula("-r2 + 5", &regs).unwrap(), -5.0);
        assert_eq!(eval_formula("1 << 4", &regs).unwrap(), 16.0);
        assert_eq!(eval_formula("(2 + 3) * 4", &regs).unwrap(), 20.0);
        assert!(eval_formula("r0 +", &regs).is_err());
    }
}
