//! A small formula language for computed properties.
//!
//! ```text
//! prop("Price") * prop("Qty")
//! if(prop("Done"), "finished", concat("due ", formatDate(prop("Due"), "%b %d")))
//! ```
//!
//! Grammar, lowest to highest precedence: `or`, `and`, `not`, comparisons
//! (`== != < <= > >=`), `+ -`, `* / %`, unary `-`, then literals (`1.5`, `"text"` with `\"`, `\\`,
//! `\n` escapes, `true`, `false`), parentheses and function calls. Names are case-insensitive.
//!
//! | Function | Result |
//! |----------|--------|
//! | `prop("Name")` | the value of another property of the row (the name must be a literal) |
//! | `if(c, a, b)` | `a` when `c` is true, else `b` (only the chosen branch is evaluated) |
//! | `concat(...)` | the arguments as text, joined |
//! | `length(x)` | characters of text, or items of a list |
//! | `lower(x)`, `upper(x)` | case conversion |
//! | `round(x[, digits])`, `abs(x)` | rounding (half away from zero) and absolute value |
//! | `min(...)`, `max(...)` | smallest and largest number (list arguments are flattened) |
//! | `now()`, `today()` | the `now` passed to [`Formula::eval`], as date-time and as date |
//! | `dateAdd(d, n, unit)` | `d` plus `n` of `"days"`, `"weeks"`, `"months"` or `"years"` |
//! | `dateBetween(a, b, unit)` | whole units from `b` to `a` (negative when `a` is earlier); units as above plus `"hours"`, `"minutes"` |
//! | `formatDate(d, fmt)` | `d` formatted with a strftime pattern such as `%Y-%m-%d` |
//! | `contains(text, sub)` | whether the text contains `sub` (or the list contains the item) |
//! | `empty(x)` | whether `x` is empty (see [`Value::is_empty`]) |
//!
//! `+` concatenates when either operand is text; otherwise it adds numbers. Comparing text is
//! exact, numbers and dates compare by value.
//!
//! # Errors and cycles
//!
//! Syntax, unknown functions and wrong argument counts are reported by [`Formula::parse`]. At
//! evaluation time any error (missing operand, wrong type, division by zero, bad date) makes the
//! whole formula evaluate to [`Value::Null`]; evaluation never panics.
//!
//! A formula may read another formula property through `prop(...)` only when the dependency graph
//! is acyclic. The evaluator does not recurse into other formulas: the caller computes them in the
//! order given by [`formula_order`] and answers `prop()` with the already-computed value.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::{Days, Months, NaiveDateTime, TimeDelta};
use thiserror::Error;

use super::property::{PropertyDef, PropertyType};
use super::value::Value;

/// Maximum nesting depth of parentheses, calls and unary operators.
const MAX_DEPTH: usize = 64;

/// Errors from parsing formulas or ordering formula properties.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FormulaError {
    /// Malformed formula text.
    #[error("syntax error at position {pos}: {message}")]
    Syntax {
        /// Character offset in the formula.
        pos: usize,
        /// What was wrong.
        message: String,
    },
    /// A call to a function that does not exist.
    #[error("unknown function `{0}`")]
    UnknownFunction(String),
    /// A call with the wrong number of arguments.
    #[error("`{name}` takes {expected} argument(s), got {got}")]
    Arity {
        /// Function name.
        name: String,
        /// Human readable expectation, such as `2 or 3`.
        expected: String,
        /// Arguments supplied.
        got: usize,
    },
    /// Formula properties depend on each other in a loop.
    #[error("formula cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    /// A formula property failed to parse.
    #[error("property `{name}`: {source}")]
    Property {
        /// Property name.
        name: String,
        /// The underlying error.
        source: Box<FormulaError>,
    },
}

/// Supplies property values to a formula.
pub trait FormulaContext {
    /// The value of the named property of the current row (`Null` when unknown).
    fn prop(&self, name: &str) -> Value;
}

impl FormulaContext for HashMap<String, Value> {
    fn prop(&self, name: &str) -> Value {
        self.get(name).cloned().unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    If,
    Concat,
    Length,
    Lower,
    Upper,
    Round,
    Abs,
    Min,
    Max,
    Now,
    Today,
    DateAdd,
    DateBetween,
    FormatDate,
    Contains,
    Empty,
}

impl Func {
    /// Function, canonical name, minimum and maximum argument count.
    fn lookup(name: &str) -> Option<(Func, usize, usize)> {
        Some(match name.to_ascii_lowercase().as_str() {
            "if" => (Func::If, 3, 3),
            "concat" => (Func::Concat, 0, usize::MAX),
            "length" => (Func::Length, 1, 1),
            "lower" => (Func::Lower, 1, 1),
            "upper" => (Func::Upper, 1, 1),
            "round" => (Func::Round, 1, 2),
            "abs" => (Func::Abs, 1, 1),
            "min" => (Func::Min, 1, usize::MAX),
            "max" => (Func::Max, 1, usize::MAX),
            "now" => (Func::Now, 0, 0),
            "today" => (Func::Today, 0, 0),
            "dateadd" => (Func::DateAdd, 3, 3),
            "datebetween" => (Func::DateBetween, 3, 3),
            "formatdate" => (Func::FormatDate, 2, 2),
            "contains" => (Func::Contains, 2, 2),
            "empty" => (Func::Empty, 1, 1),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Prop(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
}

/// A parsed formula. Parse once, evaluate per row.
#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    source: String,
    expr: Expr,
}

impl Formula {
    /// Parse formula text.
    pub fn parse(source: &str) -> Result<Formula, FormulaError> {
        let tokens = lex(source)?;
        let mut p = Parser {
            tokens,
            at: 0,
            depth: 0,
        };
        let expr = p.parse_or()?;
        if let Some((pos, tok)) = p.tokens.get(p.at) {
            return Err(syntax(*pos, format!("unexpected {}", tok.describe())));
        }
        Ok(Formula {
            source: source.to_string(),
            expr,
        })
    }

    /// The original text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Names of the properties read through `prop("...")`, in first-use order, without repeats.
    pub fn dependencies(&self) -> Vec<String> {
        fn walk(e: &Expr, out: &mut Vec<String>) {
            match e {
                Expr::Prop(n) => {
                    if !out.contains(n) {
                        out.push(n.clone());
                    }
                }
                Expr::Neg(a) | Expr::Not(a) => walk(a, out),
                Expr::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Expr::Call(_, args) => args.iter().for_each(|a| walk(a, out)),
                Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) => {}
            }
        }
        let mut out = Vec::new();
        walk(&self.expr, &mut out);
        out
    }

    /// Evaluate against a row. Any evaluation error yields [`Value::Null`].
    pub fn eval(&self, ctx: &dyn FormulaContext, now: NaiveDateTime) -> Value {
        eval(&self.expr, ctx, now).unwrap_or(Value::Null)
    }
}

/// Names of the formula properties in `defs`, ordered so that every formula comes after the
/// formula properties it reads. Ties keep definition order.
///
/// # Errors
/// [`FormulaError::Property`] wrapping a parse error, or [`FormulaError::Cycle`] listing the
/// property names around a loop (a formula reading itself is a cycle).
pub fn formula_order(defs: &[PropertyDef]) -> Result<Vec<String>, FormulaError> {
    let mut deps: Vec<(&str, Vec<String>)> = Vec::new();
    for def in defs {
        if let PropertyType::Formula { expression } = &def.kind {
            let f = Formula::parse(expression).map_err(|e| FormulaError::Property {
                name: def.name.clone(),
                source: Box::new(e),
            })?;
            deps.push((def.name.as_str(), f.dependencies()));
        }
    }
    // 0 = unvisited, 1 = on the current path, 2 = done
    let mut state = vec![0u8; deps.len()];
    let mut order = Vec::with_capacity(deps.len());
    let mut path: Vec<usize> = Vec::new();

    fn visit(
        i: usize,
        deps: &[(&str, Vec<String>)],
        state: &mut [u8],
        path: &mut Vec<usize>,
        order: &mut Vec<String>,
    ) -> Result<(), FormulaError> {
        state[i] = 1;
        path.push(i);
        for dep in &deps[i].1 {
            if let Some(j) = deps.iter().position(|(n, _)| n == dep) {
                match state[j] {
                    1 => {
                        let start = path.iter().position(|p| *p == j).unwrap_or(0);
                        let mut cycle: Vec<String> = path[start..]
                            .iter()
                            .map(|p| deps[*p].0.to_string())
                            .collect();
                        cycle.push(deps[j].0.to_string());
                        return Err(FormulaError::Cycle(cycle));
                    }
                    0 => visit(j, deps, state, path, order)?,
                    _ => {}
                }
            }
        }
        path.pop();
        state[i] = 2;
        order.push(deps[i].0.to_string());
        Ok(())
    }

    for i in 0..deps.len() {
        if state[i] == 0 {
            visit(i, &deps, &mut state, &mut path, &mut order)?;
        }
    }
    Ok(order)
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Num(n) => format!("number {n}"),
            Tok::Str(s) => format!("string \"{s}\""),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Plus => "`+`".into(),
            Tok::Minus => "`-`".into(),
            Tok::Star => "`*`".into(),
            Tok::Slash => "`/`".into(),
            Tok::Percent => "`%`".into(),
            Tok::EqEq => "`==`".into(),
            Tok::Ne => "`!=`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Le => "`<=`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Ge => "`>=`".into(),
        }
    }
}

fn syntax(pos: usize, message: impl Into<String>) -> FormulaError {
    FormulaError::Syntax {
        pos,
        message: message.into(),
    }
}

fn lex(src: &str) -> Result<Vec<(usize, Tok)>, FormulaError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        let tok = match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            ',' => Tok::Comma,
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '*' => Tok::Star,
            '/' => Tok::Slash,
            '%' => Tok::Percent,
            '=' if next == Some('=') => {
                i += 1;
                Tok::EqEq
            }
            '!' if next == Some('=') => {
                i += 1;
                Tok::Ne
            }
            '<' if next == Some('=') => {
                i += 1;
                Tok::Le
            }
            '<' => Tok::Lt,
            '>' if next == Some('=') => {
                i += 1;
                Tok::Ge
            }
            '>' => Tok::Gt,
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(syntax(start, "unterminated string")),
                        Some('"') => break,
                        Some('\\') => {
                            i += 1;
                            match chars.get(i) {
                                Some('n') => s.push('\n'),
                                Some('t') => s.push('\t'),
                                Some(&other) => s.push(other),
                                None => return Err(syntax(start, "unterminated string")),
                            }
                        }
                        Some(&ch) => s.push(ch),
                    }
                    i += 1;
                }
                Tok::Str(s)
            }
            c if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let mut j = i;
                while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                let n = text
                    .parse::<f64>()
                    .map_err(|_| syntax(start, format!("invalid number `{text}`")))?;
                i = j - 1;
                Tok::Num(n)
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                i = j - 1;
                Tok::Ident(text)
            }
            other => return Err(syntax(start, format!("unexpected character `{other}`"))),
        };
        out.push((start, tok));
        i += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------- parser

struct Parser {
    tokens: Vec<(usize, Tok)>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|(_, t)| t)
    }

    fn pos(&self) -> usize {
        self.tokens
            .get(self.at)
            .map_or_else(|| self.tokens.last().map_or(0, |(p, _)| p + 1), |(p, _)| *p)
    }

    fn eat_keyword(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(word)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Result<(), FormulaError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax(self.pos(), "expression nested too deeply"));
        }
        Ok(())
    }

    fn parse_or(&mut self) -> Result<Expr, FormulaError> {
        self.enter()?;
        let mut left = self.parse_and()?;
        while self.eat_keyword("or") {
            let right = self.parse_and()?;
            left = Expr::Bin(BinOp::Or, Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.parse_not()?;
        while self.eat_keyword("and") {
            let right = self.parse_not()?;
            left = Expr::Bin(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, FormulaError> {
        if self.eat_keyword("not") {
            self.enter()?;
            let inner = self.parse_not()?;
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.parse_add()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_add(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.parse_mul()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_mul(&mut self) -> Result<Expr, FormulaError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::Percent) => BinOp::Rem,
                _ => return Ok(left),
            };
            self.at += 1;
            let right = self.parse_unary()?;
            left = Expr::Bin(op, Box::new(left), Box::new(right));
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, FormulaError> {
        if matches!(self.peek(), Some(Tok::Minus)) {
            self.at += 1;
            self.enter()?;
            let inner = self.parse_unary()?;
            self.depth -= 1;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, FormulaError> {
        let pos = self.pos();
        let Some((_, tok)) = self.tokens.get(self.at).cloned() else {
            return Err(syntax(pos, "unexpected end of formula"));
        };
        self.at += 1;
        match tok {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::LParen => {
                let inner = self.parse_or()?;
                if matches!(self.peek(), Some(Tok::RParen)) {
                    self.at += 1;
                    Ok(inner)
                } else {
                    Err(syntax(self.pos(), "expected `)`"))
                }
            }
            Tok::Ident(name) => {
                if matches!(self.peek(), Some(Tok::LParen)) {
                    self.at += 1;
                    self.parse_call(&name, pos)
                } else if name.eq_ignore_ascii_case("true") {
                    Ok(Expr::Bool(true))
                } else if name.eq_ignore_ascii_case("false") {
                    Ok(Expr::Bool(false))
                } else {
                    Err(syntax(
                        pos,
                        format!("unexpected `{name}` (properties are written prop(\"{name}\"))"),
                    ))
                }
            }
            other => Err(syntax(pos, format!("unexpected {}", other.describe()))),
        }
    }

    fn parse_call(&mut self, name: &str, pos: usize) -> Result<Expr, FormulaError> {
        let mut args = Vec::new();
        if matches!(self.peek(), Some(Tok::RParen)) {
            self.at += 1;
        } else {
            loop {
                args.push(self.parse_or()?);
                match self.peek() {
                    Some(Tok::Comma) => self.at += 1,
                    Some(Tok::RParen) => {
                        self.at += 1;
                        break;
                    }
                    _ => return Err(syntax(self.pos(), "expected `,` or `)`")),
                }
            }
        }
        if name.eq_ignore_ascii_case("prop") {
            return match args.as_slice() {
                [Expr::Str(s)] => Ok(Expr::Prop(s.clone())),
                [_] => Err(syntax(
                    pos,
                    "prop() takes a literal property name in quotes",
                )),
                _ => Err(FormulaError::Arity {
                    name: "prop".into(),
                    expected: "1".into(),
                    got: args.len(),
                }),
            };
        }
        let (func, min, max) =
            Func::lookup(name).ok_or_else(|| FormulaError::UnknownFunction(name.to_string()))?;
        if args.len() < min || args.len() > max {
            let expected = if max == usize::MAX {
                format!("at least {min}")
            } else if min == max {
                min.to_string()
            } else {
                format!("{min} to {max}")
            };
            return Err(FormulaError::Arity {
                name: name.to_string(),
                expected,
                got: args.len(),
            });
        }
        Ok(Expr::Call(func, args))
    }
}

// ---------------------------------------------------------------- evaluation

/// `None` means an evaluation error.
fn eval(e: &Expr, ctx: &dyn FormulaContext, now: NaiveDateTime) -> Option<Value> {
    match e {
        Expr::Num(n) => Some(Value::Number(*n)),
        Expr::Str(s) => Some(Value::Text(s.clone())),
        Expr::Bool(b) => Some(Value::Bool(*b)),
        Expr::Prop(name) => Some(ctx.prop(name)),
        Expr::Neg(a) => {
            let n = eval(a, ctx, now)?.as_number()?;
            finite(-n)
        }
        Expr::Not(a) => Some(Value::Bool(!truthy(&eval(a, ctx, now)?)?)),
        Expr::Bin(BinOp::And, a, b) => {
            if !truthy(&eval(a, ctx, now)?)? {
                return Some(Value::Bool(false));
            }
            Some(Value::Bool(truthy(&eval(b, ctx, now)?)?))
        }
        Expr::Bin(BinOp::Or, a, b) => {
            if truthy(&eval(a, ctx, now)?)? {
                return Some(Value::Bool(true));
            }
            Some(Value::Bool(truthy(&eval(b, ctx, now)?)?))
        }
        Expr::Bin(op, a, b) => binary(*op, eval(a, ctx, now)?, eval(b, ctx, now)?),
        Expr::Call(f, args) => call(*f, args, ctx, now),
    }
}

fn finite(n: f64) -> Option<Value> {
    n.is_finite().then_some(Value::Number(n))
}

fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Null => Some(false),
        Value::List(l) => Some(!l.is_empty()),
        Value::Date(_) | Value::DateTime(_) => Some(true),
        other => other.as_bool(),
    }
}

fn exact_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| exact_eq(p, q))
        }
        (a, b) if matches!(a, Value::Date(_) | Value::DateTime(_)) => {
            matches!(b, Value::Date(_) | Value::DateTime(_)) && a.as_datetime() == b.as_datetime()
        }
        _ => false,
    }
}

fn binary(op: BinOp, a: Value, b: Value) -> Option<Value> {
    match op {
        BinOp::Add if matches!(a, Value::Text(_)) || matches!(b, Value::Text(_)) => {
            Some(Value::Text(format!("{a}{b}")))
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
            let (x, y) = (a.as_number()?, b.as_number()?);
            match op {
                BinOp::Add => finite(x + y),
                BinOp::Sub => finite(x - y),
                BinOp::Mul => finite(x * y),
                BinOp::Div if y == 0.0 => None,
                BinOp::Div => finite(x / y),
                _ if y == 0.0 => None,
                _ => finite(x % y),
            }
        }
        BinOp::Eq => Some(Value::Bool(exact_eq(&a, &b))),
        BinOp::Ne => Some(Value::Bool(!exact_eq(&a, &b))),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            // Text only orders against text; numbers and dates are not coerced from text here.
            let ord = match (&a, &b) {
                (Value::Text(x), Value::Text(y)) => x.cmp(y),
                (Value::Text(_), _) | (_, Value::Text(_)) => return None,
                _ => a.loose_cmp(&b)?,
            };
            Some(Value::Bool(match op {
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }))
        }
        BinOp::And | BinOp::Or => None,
    }
}

fn call(f: Func, args: &[Expr], ctx: &dyn FormulaContext, now: NaiveDateTime) -> Option<Value> {
    if f == Func::If {
        let cond = truthy(&eval(&args[0], ctx, now)?)?;
        return eval(&args[usize::from(!cond) + 1], ctx, now);
    }
    let vals = args
        .iter()
        .map(|a| eval(a, ctx, now))
        .collect::<Option<Vec<Value>>>()?;
    match f {
        Func::If => None,
        Func::Concat => Some(Value::Text(vals.iter().map(Value::as_text).collect())),
        Func::Length => match &vals[0] {
            Value::Text(s) => Some(Value::Number(s.chars().count() as f64)),
            Value::List(l) => Some(Value::Number(l.len() as f64)),
            Value::Null => Some(Value::Number(0.0)),
            _ => None,
        },
        Func::Lower => text_arg(&vals[0]).map(|s| Value::Text(s.to_lowercase())),
        Func::Upper => text_arg(&vals[0]).map(|s| Value::Text(s.to_uppercase())),
        Func::Round => {
            let x = vals[0].as_number()?;
            let digits = match vals.get(1) {
                Some(d) => d.as_number()?.trunc().clamp(-15.0, 15.0) as i32,
                None => 0,
            };
            let scale = 10f64.powi(digits);
            finite((x * scale).round() / scale)
        }
        Func::Abs => finite(vals[0].as_number()?.abs()),
        Func::Min | Func::Max => {
            let mut nums = Vec::new();
            for v in &vals {
                match v {
                    Value::List(items) => {
                        for item in items {
                            nums.push(item.as_number()?);
                        }
                    }
                    other => nums.push(other.as_number()?),
                }
            }
            let pick = if f == Func::Min { f64::min } else { f64::max };
            finite(nums.into_iter().reduce(pick)?)
        }
        Func::Now => Some(Value::DateTime(now)),
        Func::Today => Some(Value::Date(now.date())),
        Func::DateAdd => date_add(&vals[0], vals[1].as_number()?, &vals[2].as_text()),
        Func::DateBetween => date_between(&vals[0], &vals[1], &vals[2].as_text()),
        Func::FormatDate => {
            let dt = vals[0].as_datetime()?;
            let fmt = vals[1].as_text();
            let mut out = String::new();
            let has_time = matches!(vals[0], Value::DateTime(_));
            if has_time {
                write!(out, "{}", dt.format(&fmt)).ok()?;
            } else {
                write!(out, "{}", dt.date().format(&fmt)).ok()?;
            }
            Some(Value::Text(out))
        }
        Func::Contains => match &vals[0] {
            Value::List(items) => Some(Value::Bool(items.iter().any(|i| exact_eq(i, &vals[1])))),
            Value::Null => Some(Value::Bool(false)),
            other => Some(Value::Bool(other.as_text().contains(&vals[1].as_text()))),
        },
        Func::Empty => Some(Value::Bool(vals[0].is_empty())),
    }
}

fn text_arg(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::List(_) => None,
        other => Some(other.as_text()),
    }
}

fn unit_days(unit: &str) -> Option<u64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "day" | "days" => Some(1),
        "week" | "weeks" => Some(7),
        _ => None,
    }
}

fn unit_months(unit: &str) -> Option<u32> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "month" | "months" => Some(1),
        "year" | "years" => Some(12),
        _ => None,
    }
}

fn date_add(base: &Value, n: f64, unit: &str) -> Option<Value> {
    if !n.is_finite() || n.abs() > 1e9 {
        return None;
    }
    let n = n.trunc() as i64;
    let back = n < 0;
    let mag = n.unsigned_abs();
    let dt: NaiveDateTime = base.as_datetime()?;
    let moved = if let Some(per) = unit_days(unit) {
        let days = Days::new(mag.checked_mul(per)?);
        if back {
            dt.checked_sub_days(days)?
        } else {
            dt.checked_add_days(days)?
        }
    } else {
        let per = unit_months(unit)?;
        let months = Months::new(u32::try_from(mag).ok()?.checked_mul(per)?);
        if back {
            dt.checked_sub_months(months)?
        } else {
            dt.checked_add_months(months)?
        }
    };
    Some(match base {
        Value::DateTime(_) => Value::DateTime(moved),
        _ => Value::Date(moved.date()),
    })
}

fn whole_months(a: NaiveDateTime, b: NaiveDateTime) -> Option<i64> {
    use chrono::Datelike;
    let mut total =
        i64::from(a.year() - b.year()) * 12 + i64::from(a.month()) - i64::from(b.month());
    let shifted = |m: i64| -> Option<NaiveDateTime> {
        let months = Months::new(u32::try_from(m.unsigned_abs()).ok()?);
        if m >= 0 {
            b.checked_add_months(months)
        } else {
            b.checked_sub_months(months)
        }
    };
    if total > 0 && a < shifted(total)? {
        total -= 1;
    } else if total < 0 && a > shifted(total)? {
        total += 1;
    }
    Some(total)
}

fn date_between(a: &Value, b: &Value, unit: &str) -> Option<Value> {
    let (a, b) = (a.as_datetime()?, b.as_datetime()?);
    let delta: TimeDelta = a - b;
    let n = match unit.trim().to_ascii_lowercase().as_str() {
        "day" | "days" => delta.num_days(),
        "week" | "weeks" => delta.num_weeks(),
        "hour" | "hours" => delta.num_hours(),
        "minute" | "minutes" => delta.num_minutes(),
        "month" | "months" => whole_months(a, b)?,
        "year" | "years" => whole_months(a, b)? / 12,
        _ => return None,
    };
    Some(Value::Number(n as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 16)
            .unwrap()
            .and_hms_opt(9, 30, 0)
            .unwrap()
    }

    fn ctx() -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("Price".into(), Value::Number(4.5));
        m.insert("Qty".into(), Value::Number(2.0));
        m.insert("Name".into(), Value::from("Widget"));
        m.insert(
            "Due".into(),
            Value::Date(NaiveDate::from_ymd_opt(2026, 1, 31).unwrap()),
        );
        m.insert("Done".into(), Value::Bool(true));
        m.insert("Tags".into(), Value::List(vec!["a".into(), "b".into()]));
        m
    }

    fn run(src: &str) -> Value {
        Formula::parse(src).unwrap().eval(&ctx(), now())
    }

    fn num(n: f64) -> Value {
        Value::Number(n)
    }

    #[test]
    fn precedence_and_arithmetic() {
        assert_eq!(run("1 + 2 * 3"), num(7.0));
        assert_eq!(run("(1 + 2) * 3"), num(9.0));
        assert_eq!(run("10 - 4 - 3"), num(3.0));
        assert_eq!(run("-2 * -3"), num(6.0));
        assert_eq!(run("7 % 4 + 1"), num(4.0));
        assert_eq!(run("prop(\"Price\") * prop(\"Qty\")"), num(9.0));
    }

    #[test]
    fn logic_and_comparison_precedence() {
        assert_eq!(run("1 + 1 == 2 and 3 > 2"), Value::Bool(true));
        assert_eq!(run("true or false and false"), Value::Bool(true));
        assert_eq!(run("not true or true"), Value::Bool(true));
        assert_eq!(run("not (true or true)"), Value::Bool(false));
        assert_eq!(run("1 != 2"), Value::Bool(true));
        assert_eq!(run("\"a\" < \"b\""), Value::Bool(true));
        assert_eq!(run("2 <= 2 and 2 >= 2"), Value::Bool(true));
    }

    #[test]
    fn string_concat() {
        assert_eq!(run("\"x\" + 1"), Value::from("x1"));
        assert_eq!(run("prop(\"Name\") + \"!\""), Value::from("Widget!"));
        assert_eq!(run("concat(\"a\", 1, true)"), Value::from("a1true"));
        assert_eq!(run("\"q\\\"uote\""), Value::from("q\"uote"));
    }

    #[test]
    fn text_functions() {
        assert_eq!(run("length(prop(\"Name\"))"), num(6.0));
        assert_eq!(run("length(prop(\"Tags\"))"), num(2.0));
        assert_eq!(run("upper(\"ab\") + lower(\"CD\")"), Value::from("ABcd"));
        assert_eq!(run("contains(prop(\"Name\"), \"dge\")"), Value::Bool(true));
        assert_eq!(run("contains(prop(\"Tags\"), \"b\")"), Value::Bool(true));
        assert_eq!(run("empty(prop(\"Missing\"))"), Value::Bool(true));
        assert_eq!(run("empty(prop(\"Name\"))"), Value::Bool(false));
    }

    #[test]
    fn numeric_functions() {
        assert_eq!(run("round(2.5)"), num(3.0));
        assert_eq!(run("round(2.71828, 2)"), num(2.72));
        assert_eq!(run("abs(-4)"), num(4.0));
        assert_eq!(run("min(3, 1, 2)"), num(1.0));
        assert_eq!(run("max(3, 1, 2)"), num(3.0));
        assert_eq!(run("max(prop(\"Tags\"))"), Value::Null);
    }

    #[test]
    fn if_is_lazy() {
        assert_eq!(run("if(true, 1, 1 / 0)"), num(1.0));
        assert_eq!(run("if(prop(\"Done\"), \"y\", \"n\")"), Value::from("y"));
        assert_eq!(run("if(false, 1, 2)"), num(2.0));
    }

    #[test]
    fn date_functions() {
        assert_eq!(
            run("today()"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap())
        );
        assert_eq!(run("now()"), Value::DateTime(now()));
        assert_eq!(
            run("formatDate(today(), \"%d/%m/%Y\")"),
            Value::from("16/11/2026")
        );
        assert_eq!(
            run("dateAdd(prop(\"Due\"), 1, \"months\")"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 2, 28).unwrap())
        );
        assert_eq!(
            run("dateAdd(prop(\"Due\"), -2, \"weeks\")"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 1, 17).unwrap())
        );
        assert_eq!(
            run("dateBetween(today(), prop(\"Due\"), \"days\")"),
            num(289.0)
        );
        assert_eq!(
            run("dateBetween(today(), prop(\"Due\"), \"months\")"),
            num(9.0)
        );
        assert_eq!(
            run("dateBetween(prop(\"Due\"), today(), \"years\")"),
            num(0.0)
        );
        assert_eq!(
            run("dateBetween(\"2021-01-01\", \"2026-01-02\", \"years\")"),
            num(-5.0)
        );
    }

    #[test]
    fn runtime_errors_yield_null() {
        assert_eq!(run("1 / 0"), Value::Null);
        assert_eq!(run("5 % 0"), Value::Null);
        assert_eq!(run("prop(\"Missing\") * 2"), Value::Null);
        assert_eq!(run("\"a\" < 1"), Value::Null);
        assert_eq!(run("dateAdd(\"nope\", 1, \"days\")"), Value::Null);
        assert_eq!(run("dateAdd(today(), 1, \"fortnights\")"), Value::Null);
        assert_eq!(run("formatDate(today(), \"%Q\")"), Value::Null);
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(
            Formula::parse("1 +"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("(1"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("1 2"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("\"open"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("Price * 2"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("frobnicate(1)"),
            Err(FormulaError::UnknownFunction(_))
        ));
        assert!(matches!(
            Formula::parse("if(1, 2)"),
            Err(FormulaError::Arity { got: 2, .. })
        ));
        assert!(matches!(
            Formula::parse("prop(1)"),
            Err(FormulaError::Syntax { .. })
        ));
        assert!(matches!(
            Formula::parse("1 $ 2"),
            Err(FormulaError::Syntax { .. })
        ));
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        assert!(matches!(
            Formula::parse(&deep),
            Err(FormulaError::Syntax { .. })
        ));
        let negs = format!("{}1", "-".repeat(200));
        assert!(Formula::parse(&negs).is_err());
    }

    #[test]
    fn dependencies_listed_once() {
        let f = Formula::parse("prop(\"A\") + prop(\"B\") * prop(\"A\")").unwrap();
        assert_eq!(f.dependencies(), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(f.source(), "prop(\"A\") + prop(\"B\") * prop(\"A\")");
    }

    fn fdef(name: &str, expr: &str) -> PropertyDef {
        PropertyDef::new(
            name,
            PropertyType::Formula {
                expression: expr.into(),
            },
        )
    }

    #[test]
    fn topological_order() {
        let defs = vec![
            fdef("C", "prop(\"B\") + 1"),
            PropertyDef::new("Price", PropertyType::Text),
            fdef("B", "prop(\"A\") + prop(\"Price\")"),
            fdef("A", "1"),
        ];
        assert_eq!(formula_order(&defs).unwrap(), vec!["A", "B", "C"]);
    }

    #[test]
    fn cycle_detection() {
        let defs = vec![
            fdef("A", "prop(\"B\")"),
            fdef("B", "prop(\"C\")"),
            fdef("C", "prop(\"A\")"),
        ];
        let result = formula_order(&defs);
        assert!(
            matches!(&result, Err(FormulaError::Cycle(p)) if p.len() >= 3 && p.first() == p.last())
        );
        let own = vec![fdef("A", "prop(\"A\") + 1")];
        assert!(matches!(formula_order(&own), Err(FormulaError::Cycle(_))));
    }

    #[test]
    fn order_reports_parse_errors_with_property() {
        let defs = vec![fdef("Bad", "1 +")];
        assert!(matches!(
            formula_order(&defs),
            Err(FormulaError::Property { .. })
        ));
    }
}
