//! Minimal expression language for lengths / ranges / selectors.
//!
//! Recursive-descent parser. Literals are decimal or `0x` hex integers.
//! Paths are dotted identifiers (`a.b[0].c` == `a.b.0.c`).
//! Builtins: `$pos` (current cursor), `$eof` (input length);
//! function `len(path)` resolves to a byte/element length through the Resolver.
//! Operators (low to high precedence):
//!   `||`, `&&`, comparisons/`== != < <= > >=`, `|`, `^`, `&`, shifts,
//!   `+ -`, `* / %`, unary `- ! ~`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    UnknownPath(String),
    BadExpression(String),
    BadArgument(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnknownPath(p) => write!(f, "unknown field reference: {p}"),
            EvalError::BadExpression(m) => write!(f, "bad expression: {m}"),
            EvalError::BadArgument(m) => write!(f, "bad argument: {m}"),
        }
    }
}

pub trait Resolver {
    fn resolve(&self, path: &str) -> Result<i128, EvalError>;
    fn byte_len(&self, _path: &str) -> Result<i128, EvalError> {
        Err(EvalError::BadArgument(format!("len() unsupported for {_path}")))
    }
}

impl Resolver for std::collections::HashMap<String, i128> {
    fn resolve(&self, path: &str) -> Result<i128, EvalError> {
        self.get(path).copied().ok_or_else(|| EvalError::UnknownPath(path.to_string()))
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser { b: s.as_bytes(), i: 0 }
    }
    fn skip(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&mut self) -> Option<char> {
        self.skip();
        self.b.get(self.i).map(|c| *c as char)
    }
    fn eat(&mut self, s: &str) -> bool {
        self.skip();
        let fits = self.b[self.i..].starts_with(s.as_bytes());
        // Single-char operators must not swallow the first char of a two-char op.
        let boundary_ok = if s.len() == 1 {
            match s.as_bytes()[0] {
                b'&' => self.b.get(self.i + 1) != Some(&b'&'),
                b'|' => self.b.get(self.i + 1) != Some(&b'|'),
                b'<' => !matches!(self.b.get(self.i + 1), Some(b'<') | Some(b'=')),
                b'>' => !matches!(self.b.get(self.i + 1), Some(b'>') | Some(b'=')),
                b'!' => self.b.get(self.i + 1) != Some(&b'='),
                _ => true,
            }
        } else {
            true
        };
        if fits && boundary_ok {
            self.i += s.len();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, c: char) -> Result<(), EvalError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(EvalError::BadExpression(format!("expected {c}")))
        }
    }

    fn parse<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        let v = self.parse_or(r)?;
        self.skip();
        if self.i != self.b.len() {
            return Err(EvalError::BadExpression(format!("trailing input at {}", self.i)));
        }
        Ok(v)
    }

    fn binary<R: Resolver>(
        &mut self,
        r: &R,
        ops: &[&str],
        next: fn(&mut Self, &R) -> Result<i128, EvalError>,
    ) -> Result<i128, EvalError> {
        let mut left = next(self, r)?;
        loop {
            let mut matched = None;
            for op in ops {
                if self.eat(op) {
                    matched = Some(*op);
                    break;
                }
            }
            match matched {
                None => return Ok(left),
                Some(op) => {
                    let right = next(self, r)?;
                    left = apply_op(op, left, right)?;
                }
            }
        }
    }

    fn parse_or<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["||"], Self::parse_and)
    }
    fn parse_and<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["&&"], Self::parse_eq)
    }
    fn parse_eq<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["==", "!="], Self::parse_cmp)
    }
    fn parse_cmp<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["<=", ">=", "<", ">"], Self::parse_bor)
    }
    fn parse_bor<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["|"], Self::parse_bxor)
    }
    fn parse_bxor<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["^"], Self::parse_band)
    }
    fn parse_band<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["&"], Self::parse_shift)
    }
    fn parse_shift<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["<<", ">>"], Self::parse_add)
    }
    fn parse_add<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["+", "-"], Self::parse_mul)
    }
    fn parse_mul<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.binary(r, &["*", "/", "%"], Self::parse_unary)
    }

    fn parse_unary<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        if self.eat("-") {
            return Ok(self.parse_unary(r)?.wrapping_neg());
        }
        if self.eat("!") {
            return Ok((self.parse_unary(r)? == 0) as i128);
        }
        if self.eat("~") {
            return Ok(!self.parse_unary(r)?);
        }
        self.parse_atom(r)
    }

    fn read_path(&mut self) -> Result<String, EvalError> {
        self.skip();
        let start = self.i;
        let mut path = String::new();
        let ok_head = |c: char| c.is_ascii_alphabetic() || c == '_' || c == '$';
        let ok_tail = |c: char| c.is_ascii_alphanumeric() || c == '_';
        if self.i >= self.b.len() || !ok_head(self.b[self.i] as char) {
            return Err(EvalError::BadExpression("identifier expected".into()));
        }
        path.push(self.b[self.i] as char);
        self.i += 1;
        while self.i < self.b.len() {
            let c = self.b[self.i] as char;
            if ok_tail(c) {
                path.push(c);
                self.i += 1;
            } else if c == '.' {
                path.push('.');
                self.i += 1;
                while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_alphanumeric()
                    || (self.i < self.b.len() && self.b[self.i] as char == '_')
                {
                    path.push(self.b[self.i] as char);
                    self.i += 1;
                }
            } else if c == '[' {
                self.i += 1;
                self.skip();
                let nstart = self.i;
                while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_digit() {
                    self.i += 1;
                }
                if nstart == self.i {
                    return Err(EvalError::BadExpression("index expected".into()));
                }
                path.push('.');
                path.push_str(&String::from_utf8_lossy(&self.b[nstart..self.i]));
                self.expect(']')?;
            } else {
                break;
            }
        }
        let _ = start;
        Ok(path)
    }

    fn parse_atom<R: Resolver>(&mut self, r: &R) -> Result<i128, EvalError> {
        self.skip();
        if self.i >= self.b.len() {
            return Err(EvalError::BadExpression("unexpected end of expression".into()));
        }
        if self.eat("(") {
            let v = self.parse_or(r)?;
            self.expect(')')?;
            return Ok(v);
        }
        if self.b[self.i..].starts_with(b"len")
            && self.b.get(self.i + 3).map(|c| (*c as char).is_whitespace() || *c == b'(').unwrap_or(false)
        {
            self.i += 3;
            self.expect('(')?;
            let p = self.read_path()?;
            self.expect(')')?;
            return r.byte_len(&p).map_err(|_| EvalError::UnknownPath(format!("len({p})")));
        }
        let c = self.b[self.i] as char;
        if c.is_ascii_digit() {
            let start = self.i;
            if c == '0' && self.b.get(self.i + 1) == Some(&b'x') {
                self.i += 2;
                let hs = self.i;
                while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_hexdigit() {
                    self.i += 1;
                }
                return i128::from_str_radix(&String::from_utf8_lossy(&self.b[hs..self.i]), 16)
                    .map_err(|_| EvalError::BadExpression("bad hex literal".into()));
            }
            while self.i < self.b.len() && (self.b[self.i] as char).is_ascii_digit() {
                self.i += 1;
            }
            return String::from_utf8_lossy(&self.b[start..self.i])
                .parse()
                .map_err(|_| EvalError::BadExpression("bad number".into()));
        }
        let p = self.read_path()?;
        r.resolve(&p)
    }
}

fn apply_op(op: &str, a: i128, b: i128) -> Result<i128, EvalError> {
    Ok(match op {
        "||" => ((a != 0) || (b != 0)) as i128,
        "&&" => ((a != 0) && (b != 0)) as i128,
        "==" => (a == b) as i128,
        "!=" => (a != b) as i128,
        "<=" => (a <= b) as i128,
        ">=" => (a >= b) as i128,
        "<" => (a < b) as i128,
        ">" => (a > b) as i128,
        "|" => a | b,
        "^" => a ^ b,
        "&" => a & b,
        "<<" => a.wrapping_shl(b.clamp(0, 127) as u32),
        ">>" => a.wrapping_shr(b.clamp(0, 127) as u32),
        "+" => a.wrapping_add(b),
        "-" => a.wrapping_sub(b),
        "*" => a.wrapping_mul(b),
        "/" => {
            if b == 0 {
                return Err(EvalError::BadArgument("division by zero".into()));
            }
            a.wrapping_div(b)
        }
        "%" => {
            if b == 0 {
                return Err(EvalError::BadArgument("modulo by zero".into()));
            }
            a.wrapping_rem(b)
        }
        _ => return Err(EvalError::BadExpression(format!("unknown op {op}"))),
    })
}

pub fn eval<R: Resolver>(expr: &str, r: &R) -> Result<i128, EvalError> {
    if expr.trim().is_empty() {
        return Err(EvalError::BadExpression("empty expression".into()));
    }
    Parser::new(expr).parse(r)
}

pub fn eval_opt<R: Resolver>(expr: &str, r: &R) -> Result<Option<i128>, EvalError> {
    if expr.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(eval(expr, r)?))
}
