//! A deliberately closed expression language: no names beyond the whitelist,
//! no scripting engine, and bounded parser depth and evaluation work.
use crate::GeometryError;

pub(crate) const MAX_BYTES: usize = 256;
const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 24;

#[derive(Debug)]
pub(crate) enum Expr {
    Number(f64),
    U,
    V,
    Neg(Box<Expr>),
    Binary(u8, Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    Pair(&'static str, Box<Expr>, Box<Expr>),
}

fn error(message: &str) -> GeometryError {
    GeometryError::Surface(format!("formula: {message}"))
}

impl Expr {
    pub(crate) fn parse(source: &str) -> Result<Self, GeometryError> {
        if source.is_empty() || source.len() > MAX_BYTES || !source.is_ascii() {
            return Err(error("use 1..256 ASCII bytes"));
        }
        let mut p = Parser {
            bytes: source.as_bytes(),
            at: 0,
            nodes: 0,
        };
        let expression = p.expression(0, 0)?;
        p.space();
        if p.at != p.bytes.len() {
            return Err(error("unexpected trailing input"));
        }
        Ok(expression)
    }

    pub(crate) fn value(&self, u: f64, v: f64) -> Result<f64, GeometryError> {
        // Check intermediate results as well: min(0, sqrt(-1)) must not
        // mask invalid math and turn a failed formula into a flat face.
        let x = match self {
            Self::Number(x) => *x,
            Self::U => u,
            Self::V => v,
            Self::Neg(a) => -a.value(u, v)?,
            Self::Binary(op, a, b) => {
                let (a, b) = (a.value(u, v)?, b.value(u, v)?);
                match op {
                    b'+' => a + b,
                    b'-' => a - b,
                    b'*' => a * b,
                    b'/' => a / b,
                    b'^' => a.powf(b),
                    _ => unreachable!(),
                }
            }
            Self::Unary(name, a) => {
                let a = a.value(u, v)?;
                match *name {
                    "sin" => a.sin(),
                    "cos" => a.cos(),
                    "abs" => a.abs(),
                    "sqrt" => a.sqrt(),
                    _ => unreachable!(),
                }
            }
            Self::Pair(name, a, b) => {
                let (a, b) = (a.value(u, v)?, b.value(u, v)?);
                if *name == "min" {
                    a.min(b)
                } else {
                    a.max(b)
                }
            }
        };
        if !x.is_finite() {
            return Err(error(
                "non-finite result (division by zero, domain error or overflow)",
            ));
        }
        Ok(x)
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: usize,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }
    fn take(&mut self, c: u8) -> bool {
        self.space();
        if self.bytes.get(self.at) == Some(&c) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn node(&mut self, value: Expr) -> Result<Expr, GeometryError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            Err(error("at most 128 expression nodes"))
        } else {
            Ok(value)
        }
    }
    fn expression(&mut self, min_bp: u8, depth: usize) -> Result<Expr, GeometryError> {
        if depth > MAX_DEPTH {
            return Err(error("at most 24 nested levels"));
        }
        self.space();
        let mut left = if self.take(b'-') {
            let a = self.expression(5, depth + 1)?;
            self.node(Expr::Neg(Box::new(a)))?
        } else if self.take(b'+') {
            self.expression(5, depth + 1)?
        } else if self.take(b'(') {
            let a = self.expression(0, depth + 1)?;
            if !self.take(b')') {
                return Err(error("missing closing parenthesis"));
            }
            a
        } else if self
            .bytes
            .get(self.at)
            .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
        {
            let start = self.at;
            while self
                .bytes
                .get(self.at)
                .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
            {
                self.at += 1;
            }
            if self
                .bytes
                .get(self.at)
                .is_some_and(|c| *c == b'e' || *c == b'E')
            {
                self.at += 1;
                if self
                    .bytes
                    .get(self.at)
                    .is_some_and(|c| *c == b'+' || *c == b'-')
                {
                    self.at += 1;
                }
                while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                    self.at += 1;
                }
            }
            let number = std::str::from_utf8(&self.bytes[start..self.at])
                .unwrap()
                .parse::<f64>()
                .map_err(|_| error("invalid numeric literal"))?;
            if !number.is_finite() {
                return Err(error("numeric literal must be finite"));
            }
            self.node(Expr::Number(number))?
        } else {
            let start = self.at;
            while self
                .bytes
                .get(self.at)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            {
                self.at += 1;
            }
            let name = std::str::from_utf8(&self.bytes[start..self.at]).unwrap();
            match name {
                "u" => self.node(Expr::U)?,
                "v" => self.node(Expr::V)?,
                "pi" => self.node(Expr::Number(std::f64::consts::PI))?,
                "e" => self.node(Expr::Number(std::f64::consts::E))?,
                "sin" | "cos" | "abs" | "sqrt" | "min" | "max" => {
                    let function = match name {
                        "sin" => "sin",
                        "cos" => "cos",
                        "abs" => "abs",
                        "sqrt" => "sqrt",
                        "min" => "min",
                        _ => "max",
                    };
                    if !self.take(b'(') {
                        return Err(error("function needs parentheses"));
                    }
                    let a = self.expression(0, depth + 1)?;
                    let expression = if function == "min" || function == "max" {
                        if !self.take(b',') {
                            return Err(error("min/max need two arguments"));
                        }
                        let b = self.expression(0, depth + 1)?;
                        Expr::Pair(function, Box::new(a), Box::new(b))
                    } else {
                        Expr::Unary(function, Box::new(a))
                    };
                    if !self.take(b')') {
                        return Err(error("missing closing function parenthesis"));
                    }
                    self.node(expression)?
                }
                _ => {
                    return Err(error(
                        "expected number, u, v, pi, e, or sin/cos/abs/sqrt/min/max",
                    ))
                }
            }
        };
        loop {
            self.space();
            let Some(&op) = self.bytes.get(self.at) else {
                break;
            };
            let (lbp, rbp) = match op {
                b'+' | b'-' => (1, 2),
                b'*' | b'/' => (3, 4),
                b'^' => (6, 5),
                _ => break,
            };
            if lbp < min_bp {
                break;
            }
            self.at += 1;
            let right = self.expression(rbp, depth + 1)?;
            left = self.node(Expr::Binary(op, Box::new(left), Box::new(right)))?;
        }
        Ok(left)
    }
}
