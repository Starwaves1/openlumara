//! Safe evaluator for the expression language of python's `simpleeval.simple_eval` (simpleeval 1.0.x
//! defaults, which is what `pip install simpleeval` gives today).
//!
//! Supported: int (arbitrary precision), float, bool, str and None literals, the names True/False/None,
//! `+ - * / // % **`, `<< >> & | ^ ~`, `not and or`, chained comparisons (`== != < <= > >= in not in
//! is is not`), `a if c else b`, parentheses, str indexing and slicing, and the functions simpleeval
//! allows by default: rand, randint, int, float, str. As in simpleeval, `import`, lambdas,
//! comprehensions and list/dict/set/tuple literals are rejected with simpleeval's own messages.
//!
//! Not supported (reported as an error, never guessed): f-strings, bytes, complex numbers, attribute
//! access such as `"abc".upper()`, printf-style `%` formatting of strings, and results that python
//! would return as complex numbers. Unicode decimal digits are not accepted by `int()`.
//!
//! Results that python serializes as bare JSON numbers but serde_json cannot hold are sent as strings:
//! ints outside the u64/i64 range. Non-finite floats are carried as python's `Infinity` / `-Infinity` / `NaN`
//! text, which `calculator::calculate` emits unquoted.

use crate::util::py_strip;
use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use num_traits::{FromPrimitive, One, Signed, ToPrimitive, Zero};
use ol_core::pyjson::{float_repr, py_repr};
use serde_json::Value as Json;
use std::cmp::Ordering;

const MAX_STRING_LENGTH: usize = 100_000;
const MAX_POWER: i64 = 4_000_000;
const MAX_SHIFT: i64 = 10_000;
const DISALLOW_PREFIXES: &[&str] = &["_", "func_"];
const DISALLOW_METHODS: &[&str] = &["format", "format_map", "mro", "tb_frame", "gi_frame", "ag_frame", "cr_frame", "exec"];
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try",
    "while", "with", "yield",
];
const STRING_PREFIXES: &[&str] = &["r", "u", "b", "br", "rb", "f", "fr", "rf"];
const OPS3: &[&str] = &["**=", "//=", ">>=", "<<=", "..."];
const OPS2: &[&str] = &["**", "//", "<<", ">>", "<=", ">=", "==", "!=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "@=", ":=", "->"];
const OPS1: &[&str] = &["+", "-", "*", "/", "%", "&", "|", "^", "~", "<", ">", "=", ".", ",", ":", "@", "(", ")", "[", "]", "{", "}", ";"];
const AUG_OPS: &[&str] = &["+=", "-=", "*=", "/=", "//=", "%=", "**=", "<<=", ">>=", "&=", "|=", "^=", "@="];
/// the keyword name given to a `**mapping` unpacking inside a call (not an identifier, so it cannot clash)
const DOUBLE_STAR: &str = "**";

/// an exception raised while evaluating; the message is python's `str(e)`
#[derive(Debug, Clone, PartialEq)]
pub struct PyErr(pub String);

impl std::fmt::Display for PyErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Res<T> = Result<T, PyErr>;

fn fail<T>(msg: impl Into<String>) -> Res<T> {
    Err(PyErr(msg.into()))
}

fn syntax_error(msg: &str, line: usize) -> PyErr {
    PyErr(format!("{msg} (<unknown>, line {line})"))
}

/// cpython's tokenizer message for a `0x` / `0o` / `0b` literal whose digits do not parse
fn radix_literal_error(radix: u32, body: &str, line: usize) -> PyErr {
    let name = match radix {
        16 => "hexadecimal",
        8 => "octal",
        _ => "binary",
    };
    // octal and binary literals name the first decimal digit that is too large for the base
    match body.chars().find(|c| c.is_ascii_digit() && c.to_digit(radix).is_none()) {
        Some(d) if radix != 16 => syntax_error(&format!("invalid digit '{d}' in {name} literal"), line),
        _ => syntax_error(&format!("invalid {name} literal"), line),
    }
}

fn not_available(node: &str) -> String {
    format!("Sorry, {node} is not available in this evaluator")
}

/// the default simpleeval functions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Rand,
    RandInt,
    Int,
    Float,
    Str,
}

/// a python value produced by the evaluator
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Int(BigInt),
    Bool(bool),
    Float(f64),
    Str(String),
    None,
    Func(Func),
}

fn function_named(name: &str) -> Option<Func> {
    match name {
        "rand" => Some(Func::Rand),
        "randint" => Some(Func::RandInt),
        "int" => Some(Func::Int),
        "float" => Some(Func::Float),
        "str" => Some(Func::Str),
        _ => None,
    }
}

fn type_name(v: &Val) -> &'static str {
    match v {
        Val::Int(_) => "int",
        Val::Bool(_) => "bool",
        Val::Float(_) => "float",
        Val::Str(_) => "str",
        Val::None => "NoneType",
        Val::Func(Func::Int | Func::Float | Func::Str) => "type",
        Val::Func(Func::Rand) => "builtin_function_or_method",
        Val::Func(Func::RandInt) => "function",
    }
}

fn repr_str(s: &str) -> String {
    py_repr(&Json::String(s.to_string()))
}

/// python `str(float)`
fn float_str(f: f64) -> String {
    if f.is_nan() {
        "nan".into()
    } else if f.is_infinite() {
        if f > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        float_repr(f)
    }
}

/// python `str(value)` without the int digit limit (used inside error messages)
fn display(v: &Val) -> String {
    match v {
        Val::Int(i) => i.to_string(),
        Val::Bool(true) => "True".into(),
        Val::Bool(false) => "False".into(),
        Val::Float(f) => float_str(*f),
        Val::Str(s) => s.clone(),
        Val::None => "None".into(),
        Val::Func(Func::Rand) => "<built-in method random of Random object>".into(),
        Val::Func(Func::RandInt) => "<function random_int>".into(),
        Val::Func(Func::Int) => "<class 'int'>".into(),
        Val::Func(Func::Float) => "<class 'float'>".into(),
        Val::Func(Func::Str) => "<class 'str'>".into(),
    }
}

/// cpython's default `sys.get_int_max_str_digits()`
const MAX_STR_DIGITS: usize = 4300;

/// `str(int)` / `json.dumps(int)` above the digit limit (cpython's message has no digit count here)
const INT_STR_LIMIT: &str = "Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit";

/// python `str(value)` for an int: fails above the digit limit like cpython does. 2**14200 has at most
/// 4276 digits and 2**14300 has at least 4306, so only the band in between needs the exact count.
fn int_str(i: &BigInt) -> Res<String> {
    let bits = i.bits();
    if bits >= 14_300 {
        return fail(INT_STR_LIMIT);
    }
    let s = i.to_string();
    if bits > 14_200 && s.trim_start_matches('-').len() > MAX_STR_DIGITS {
        return fail(INT_STR_LIMIT);
    }
    Ok(s)
}

/// python `str(value)` where the conversion can fail (used inside error messages, where python
/// formats the operands too)
fn py_str(v: &Val) -> Res<String> {
    match v {
        Val::Int(i) => int_str(i),
        other => Ok(display(other)),
    }
}

/// python `json.dumps(value)` of the raw value a successful evaluation returns, which the
/// dispatcher serializes. Ints and floats stay JSON numbers when serde_json can hold them.
/// Ints outside the u64/i64 range and non-finite floats are carried as their python `str()` text
/// because serde_json cannot represent them (python would emit bare digits). Non-finite floats are carried
/// as the json token text (`Infinity`, `-Infinity`, `NaN`), which `calculator::calculate` emits unquoted.
/// Values python cannot serialize (functions, classes) fail with json's TypeError text.
pub fn to_json(v: &Val) -> Res<Json> {
    Ok(match v {
        Val::Int(i) => {
            let text = int_str(i)?;
            if let Ok(n) = i64::try_from(i) {
                Json::from(n)
            } else if let Ok(n) = u64::try_from(i) {
                Json::from(n)
            } else {
                Json::String(text)
            }
        }
        Val::Bool(b) => Json::Bool(*b),
        Val::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => Json::Number(n),
            None => Json::String(float_repr(*f)),
        },
        Val::Str(s) => Json::String(s.clone()),
        Val::None => Json::Null,
        Val::Func(_) => return fail(format!("Object of type {} is not JSON serializable", type_name(v))),
    })
}

fn truthy(v: &Val) -> bool {
    match v {
        Val::Int(i) => !i.is_zero(),
        Val::Bool(b) => *b,
        Val::Float(f) => *f != 0.0,
        Val::Str(s) => !s.is_empty(),
        Val::None => false,
        Val::Func(_) => true,
    }
}

/// a numeric operand: bools take part as ints, like python
enum Num {
    I(BigInt),
    F(f64),
}

fn num_of(v: &Val) -> Option<Num> {
    match v {
        Val::Int(i) => Some(Num::I(i.clone())),
        Val::Bool(b) => Some(Num::I(BigInt::from(u8::from(*b)))),
        Val::Float(f) => Some(Num::F(*f)),
        _ => None,
    }
}

fn is_int_like(v: &Val) -> bool {
    matches!(v, Val::Int(_) | Val::Bool(_))
}

/// python `int(float)` for the value of a float
fn float_to_int(f: f64) -> Res<BigInt> {
    if f.is_nan() {
        return fail("cannot convert float NaN to integer");
    }
    if f.is_infinite() {
        return fail("cannot convert float infinity to integer");
    }
    BigInt::from_f64(f.trunc()).map_or_else(|| fail("cannot convert float to integer"), Ok)
}

/// python `float(int)`, which raises for ints too large for a float
fn int_to_f64(i: &BigInt) -> Res<f64> {
    match i.to_f64() {
        Some(f) if f.is_finite() => Ok(f),
        _ => fail("int too large to convert to float"),
    }
}

/// an int argument as a machine integer (saturating, like python slice indices)
fn saturating_i64(i: &BigInt) -> i64 {
    i.to_i64().unwrap_or(if i.is_negative() { i64::MIN } else { i64::MAX })
}

fn index_arg(v: &Val) -> Res<i64> {
    match num_of(v) {
        Some(Num::I(i)) => Ok(saturating_i64(&i)),
        _ => fail(format!("'{}' object cannot be interpreted as an integer", type_name(v))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    LShift,
    RShift,
    BitOr,
    BitXor,
    BitAnd,
}

impl BinOp {
    fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::LShift => "<<",
            BinOp::RShift => ">>",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::BitAnd => "&",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UnOp {
    Neg,
    Pos,
    Invert,
    Not,
}

#[derive(Debug, Clone, Copy)]
enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    In,
    NotIn,
    Is,
    IsNot,
}

impl CmpOp {
    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::NotEq => "!=",
            CmpOp::Lt => "<",
            CmpOp::LtE => "<=",
            CmpOp::Gt => ">",
            CmpOp::GtE => ">=",
            CmpOp::In => "in",
            CmpOp::NotIn => "not in",
            CmpOp::Is => "is",
            CmpOp::IsNot => "is not",
        }
    }
}

#[derive(Debug, Clone)]
enum SubKey {
    Index(Box<Expr>),
    Slice(Option<Box<Expr>>, Option<Box<Expr>>, Option<Box<Expr>>),
}

#[derive(Debug, Clone)]
enum Expr {
    Const(Val),
    Name(String),
    /// a node that simpleeval parses but refuses to evaluate; the message is raised when reached
    Fail(String),
    /// a tuple display or bare `a, b`. simpleeval refuses it when evaluated, but its elements decide
    /// whether it is a valid assignment target
    Tuple(Vec<Expr>),
    /// `*x` in a statement's expression list. simpleeval refuses the node without evaluating its operand;
    /// as an assignment target it is valid, and only the value is evaluated
    Starred(Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Compare(Box<Expr>, Vec<(CmpOp, Expr)>),
    /// test, body, orelse
    IfExp(Box<Expr>, Box<Expr>, Box<Expr>),
    /// func, positional args, keyword args
    Call(Box<Expr>, Vec<Expr>, Vec<(String, Expr)>),
    Attr(Box<Expr>, String),
    Subscript(Box<Expr>, SubKey),
}

#[derive(Debug, Clone)]
enum Stmt {
    Expr(Expr),
    /// `target = value`: only the value is evaluated, like simpleeval
    Assign(Expr),
    /// `target op= value`: only the value is evaluated, like simpleeval
    AugAssign(Expr),
    Fail(String),
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(Val),
    Str(String),
    /// a literal this evaluator does not support; raised when the expression reaches it
    Fail(String),
    Name(String),
    Op(&'static str),
    Newline,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    tok: Tok,
    line: usize,
}

struct Lexer {
    src: Vec<char>,
    i: usize,
    line: usize,
    open: Vec<(char, usize)>,
    out: Vec<Token>,
    line_start: bool,
    /// the previous logical line ended with ':' so indented lines are a block body
    block_open: bool,
}

fn lex(text: &str) -> Res<Vec<Token>> {
    let mut lx = Lexer { src: text.chars().collect(), i: 0, line: 1, open: Vec::new(), out: Vec::new(), line_start: true, block_open: false };
    lx.run()?;
    Ok(lx.out)
}

/// cpython's tokenizer refuses characters that are not printable (`Py_UNICODE_ISPRINTABLE` is false):
/// control, format, space separator other than ' ', line and paragraph separators, private use and
/// unassigned code points. This lists the control, format, space-separator and private-use ranges
/// (plus the unassigned ones in the same blocks), which covers the characters a calculator sees.
fn is_non_printable(c: char) -> bool {
    matches!(
        c as u32,
        0x00..=0x1F
            | 0x7F..=0x9F
            | 0xA0
            | 0xAD
            | 0x600..=0x605
            | 0x61C
            | 0x6DD
            | 0x70F
            | 0x180E
            | 0x1680
            | 0x2000..=0x200F
            | 0x2028..=0x202F
            | 0x2060..=0x206F
            | 0x3000
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0xE000..=0xF8FF
            | 0xE0001
            | 0xE0020..=0xE007F
            | 0xF0000..=0x10FFFF
    )
}

fn bracket_op(c: char) -> &'static str {
    match c {
        '(' => "(",
        ')' => ")",
        '[' => "[",
        ']' => "]",
        '{' => "{",
        _ => "}",
    }
}

impl Lexer {
    fn at(&self, k: usize) -> Option<char> {
        self.src.get(self.i + k).copied()
    }

    fn push(&mut self, tok: Tok) {
        let line = self.line;
        self.out.push(Token { tok, line });
    }

    /// an indented line that does not open a block. When the line before ends in an operator, cpython's
    /// parser fails at the newline that ends it, so that is the error; otherwise it is the indent itself.
    fn unexpected_indent(&self) -> PyErr {
        if let [.., Token { tok: Tok::Op(op), .. }, Token { tok: Tok::Newline, line }] = self.out.as_slice() {
            if !matches!(*op, ")" | "]" | "}") {
                return syntax_error("invalid syntax", *line);
            }
        }
        syntax_error("unexpected indent", self.line)
    }

    fn match_op(&self) -> Option<&'static str> {
        OPS3.iter().chain(OPS2.iter()).chain(OPS1.iter()).copied().find(|op| op.chars().enumerate().all(|(k, ch)| self.at(k) == Some(ch)))
    }

    fn run(&mut self) -> Res<()> {
        while let Some(c) = self.at(0) {
            if self.line_start && self.open.is_empty() {
                self.line_start = false;
                let mut j = 0;
                while matches!(self.at(j), Some(' ' | '\t' | '\x0c')) {
                    j += 1;
                }
                let blank = matches!(self.at(j), None | Some('\n' | '\r' | '#'));
                if j == 0 && !blank {
                    self.block_open = false;
                }
                if j > 0 && !blank && !self.block_open {
                    return Err(self.unexpected_indent());
                }
                self.i += j;
                continue;
            }
            match c {
                ' ' | '\t' | '\x0c' | '\r' => self.i += 1,
                '#' => {
                    while matches!(self.at(0), Some(ch) if ch != '\n') {
                        self.i += 1;
                    }
                }
                '\n' => {
                    if self.open.is_empty() {
                        if matches!(self.out.last(), Some(Token { tok: Tok::Op(":"), .. })) {
                            self.block_open = true;
                        }
                        if !matches!(self.out.last(), None | Some(Token { tok: Tok::Newline, .. })) {
                            self.push(Tok::Newline);
                        }
                        self.line_start = true;
                    }
                    self.line += 1;
                    self.i += 1;
                }
                '\\' => match (self.at(1), self.at(2)) {
                    (Some('\n'), _) => {
                        self.i += 2;
                        self.line += 1;
                    }
                    (Some('\r'), Some('\n')) => {
                        self.i += 3;
                        self.line += 1;
                    }
                    (None, _) => return Err(syntax_error("unexpected EOF while parsing", self.line)),
                    _ => return Err(syntax_error("unexpected character after line continuation character", self.line)),
                },
                '0'..='9' => {
                    let t = self.number()?;
                    self.push(t);
                }
                '.' if matches!(self.at(1), Some('0'..='9')) => {
                    let t = self.number()?;
                    self.push(t);
                }
                '"' | '\'' => {
                    let t = self.string(String::new())?;
                    self.push(t);
                }
                _ if c.is_alphabetic() || c == '_' => {
                    let start = self.i;
                    while matches!(self.at(0), Some(ch) if ch.is_alphanumeric() || ch == '_') {
                        self.i += 1;
                    }
                    let word: String = self.src[start..self.i].iter().collect();
                    let lower = word.to_ascii_lowercase();
                    if matches!(self.at(0), Some('"' | '\'')) && STRING_PREFIXES.contains(&lower.as_str()) {
                        let t = self.string(lower)?;
                        self.push(t);
                    } else {
                        self.push(Tok::Name(word));
                    }
                }
                '(' | '[' | '{' => {
                    // cpython's tokenizer refuses the 201st open bracket (MAXLEVEL 200)
                    if self.open.len() >= 200 {
                        return Err(syntax_error("too many nested parentheses", self.line));
                    }
                    self.open.push((c, self.line));
                    self.i += 1;
                    self.push(Tok::Op(bracket_op(c)));
                }
                ')' | ']' | '}' => {
                    let want = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    match self.open.pop() {
                        None => return Err(syntax_error(&format!("unmatched '{c}'"), self.line)),
                        Some((o, ol)) if o != want => {
                            let on = if ol != self.line { format!(" on line {ol}") } else { String::new() };
                            return Err(syntax_error(&format!("closing parenthesis '{c}' does not match opening parenthesis '{o}'{on}"), self.line));
                        }
                        Some(_) => {}
                    }
                    self.i += 1;
                    self.push(Tok::Op(bracket_op(c)));
                }
                _ => {
                    if let Some(op) = self.match_op() {
                        self.i += op.chars().count();
                        self.push(Tok::Op(op));
                    } else if is_non_printable(c) {
                        return Err(syntax_error(&format!("invalid non-printable character U+{:04X}", c as u32), self.line));
                    } else if c.is_ascii() {
                        return Err(syntax_error("invalid syntax", self.line));
                    } else {
                        return Err(syntax_error(&format!("invalid character '{c}' (U+{:04X})", c as u32), self.line));
                    }
                }
            }
        }
        if let Some((open, line)) = self.open.last() {
            return Err(syntax_error(&format!("'{open}' was never closed"), *line));
        }
        if !matches!(self.out.last(), Some(Token { tok: Tok::Newline, .. })) {
            self.push(Tok::Newline);
        }
        self.push(Tok::Eof);
        Ok(())
    }

    fn eat_digits(&mut self) {
        while matches!(self.at(0), Some(ch) if ch.is_ascii_digit() || ch == '_') {
            self.i += 1;
        }
    }

    fn number(&mut self) -> Res<Tok> {
        let line = self.line;
        if self.at(0) == Some('0') {
            let radix = match self.at(1).map(|c| c.to_ascii_lowercase()) {
                Some('x') => 16,
                Some('o') => 8,
                Some('b') => 2,
                _ => 0,
            };
            if radix != 0 {
                self.i += 2;
                let start = self.i;
                while matches!(self.at(0), Some(ch) if ch.is_ascii_alphanumeric() || ch == '_') {
                    self.i += 1;
                }
                let body: String = self.src[start..self.i].iter().filter(|c| **c != '_').collect();
                return match BigInt::parse_bytes(body.as_bytes(), radix) {
                    Some(v) => Ok(Tok::Num(Val::Int(v))),
                    None => Err(radix_literal_error(radix, &body, line)),
                };
            }
        }
        let start = self.i;
        let mut is_float = false;
        self.eat_digits();
        if self.at(0) == Some('.') {
            is_float = true;
            self.i += 1;
            self.eat_digits();
        }
        if matches!(self.at(0), Some('e' | 'E')) {
            let k = if matches!(self.at(1), Some('+' | '-')) { 2 } else { 1 };
            if matches!(self.at(k), Some('0'..='9')) {
                is_float = true;
                self.i += k;
                self.eat_digits();
            } else {
                // cpython's tokenizer: an exponent marker with no exponent digits
                return Err(syntax_error("invalid decimal literal", line));
            }
        }
        // an underscore is only allowed between two digits (cpython: "1__0" and "1_" are invalid)
        let raw = &self.src[start..self.i];
        let well_placed = |k: usize| k > 0 && raw[k - 1].is_ascii_digit() && raw.get(k + 1).is_some_and(|n| n.is_ascii_digit());
        if raw.iter().enumerate().any(|(k, &c)| c == '_' && !well_placed(k)) {
            return Err(syntax_error("invalid decimal literal", line));
        }
        let text: String = raw.iter().filter(|c| **c != '_').collect();
        if matches!(self.at(0), Some('j' | 'J')) {
            self.i += 1;
            return Ok(Tok::Fail("complex numbers are not supported by this calculator".into()));
        }
        if is_float {
            let mut t = if text.starts_with('.') { format!("0{text}") } else { text };
            if t.contains(".e") || t.contains(".E") {
                t = t.replacen(".e", ".0e", 1).replacen(".E", ".0E", 1);
            }
            if t.ends_with('.') {
                t.push('0');
            }
            return t.parse::<f64>().map(|f| Tok::Num(Val::Float(f))).map_err(|_| syntax_error("invalid syntax", line));
        }
        if text.len() > 1 && text.starts_with('0') && text.chars().any(|c| c != '0') {
            return Err(syntax_error("leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers", line));
        }
        // cpython converts decimal literals under the same 4300 digit limit (and suggests hex)
        let digits = text.trim_start_matches('0').len().max(1);
        if digits > MAX_STR_DIGITS {
            return Err(syntax_error(
                &format!(
                    "Exceeds the limit (4300 digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit - Consider hexadecimal for huge integer literals to avoid decimal conversion limits."
                ),
                line,
            ));
        }
        BigInt::parse_bytes(text.as_bytes(), 10).map(|v| Tok::Num(Val::Int(v))).ok_or_else(|| syntax_error("invalid syntax", line))
    }

    fn unterminated(&self, triple: bool, line0: usize) -> PyErr {
        let kind = if triple { "unterminated triple-quoted string literal" } else { "unterminated string literal" };
        syntax_error(&format!("{kind} (detected at line {})", self.line), line0)
    }

    /// cpython's `unicode_escape` codec error for the escape that starts at `esc.0` (its backslash) in a
    /// literal whose body starts at `esc.1`. The codec positions are byte offsets into the body and end
    /// at the last character of the escape, which is the character before `self.i`.
    fn escape_error(&self, esc: (usize, usize), what: &str) -> PyErr {
        let (bs, body) = esc;
        let bytes = |from: usize, to: usize| -> usize { self.src[from..to].iter().map(|c| c.len_utf8()).sum() };
        let start = bytes(body, bs);
        let end = start + bytes(bs, self.i) - 1;
        syntax_error(&format!("(unicode error) 'unicodeescape' codec can't decode bytes in position {start}-{end}: {what}"), self.line)
    }

    fn hex_escape(&mut self, digits: usize, out: &mut String, esc: (usize, usize), truncated: &str) -> Res<()> {
        let mut v: u64 = 0;
        for _ in 0..digits {
            match self.at(0).and_then(|c| c.to_digit(16)) {
                Some(x) => {
                    v = v * 16 + u64::from(x);
                    self.i += 1;
                }
                None => return Err(self.escape_error(esc, truncated)),
            }
        }
        let ch = u32::try_from(v).ok().and_then(char::from_u32);
        out.push(ch.ok_or_else(|| self.escape_error(esc, "illegal Unicode character"))?);
        Ok(())
    }

    fn string(&mut self, prefix: String) -> Res<Tok> {
        let line0 = self.line;
        let raw = prefix.contains('r');
        let quote = self.src[self.i];
        let triple = self.at(1) == Some(quote) && self.at(2) == Some(quote);
        self.i += if triple { 3 } else { 1 };
        let body = self.i;
        let mut out = String::new();
        let mut named_escape = false;
        loop {
            let Some(c) = self.at(0) else { return Err(self.unterminated(triple, line0)) };
            if c == quote && (!triple || (self.at(1) == Some(quote) && self.at(2) == Some(quote))) {
                self.i += if triple { 3 } else { 1 };
                break;
            }
            if c == '\n' && !triple {
                return Err(self.unterminated(false, line0));
            }
            if c != '\\' {
                if c == '\n' {
                    self.line += 1;
                }
                out.push(c);
                self.i += 1;
                continue;
            }
            let Some(d) = self.at(1) else { return Err(self.unterminated(triple, line0)) };
            let esc = (self.i, body);
            self.i += 2;
            if raw {
                out.push('\\');
                out.push(d);
                if d == '\n' {
                    self.line += 1;
                }
                continue;
            }
            match d {
                '\n' => self.line += 1,
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                'a' => out.push('\u{7}'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'v' => out.push('\u{b}'),
                '0'..='7' => {
                    let mut v = d.to_digit(8).unwrap_or(0);
                    let mut n = 1;
                    while n < 3 {
                        match self.at(0).and_then(|c| c.to_digit(8)) {
                            Some(x) => {
                                v = v * 8 + x;
                                self.i += 1;
                                n += 1;
                            }
                            None => break,
                        }
                    }
                    out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                }
                'x' => self.hex_escape(2, &mut out, esc, "truncated \\xXX escape")?,
                'u' => self.hex_escape(4, &mut out, esc, "truncated \\uXXXX escape")?,
                'U' => self.hex_escape(8, &mut out, esc, "truncated \\UXXXXXXXX escape")?,
                'N' => {
                    named_escape = true;
                    if self.at(0) == Some('{') {
                        while !matches!(self.at(0), None | Some('}')) {
                            self.i += 1;
                        }
                        self.i += 1;
                    }
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        }
        if prefix.contains('b') {
            return Ok(Tok::Fail("bytes literals are not supported by this calculator".into()));
        }
        if prefix.contains('f') {
            return Ok(Tok::Fail("f-strings are not supported by this calculator".into()));
        }
        if named_escape {
            return Ok(Tok::Fail("named unicode escapes are not supported by this calculator".into()));
        }
        Ok(Tok::Str(out))
    }
}

// ---------------------------------------------------------------- parser

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// the expression as the caller gave it (simpleeval reports this text in its messages)
    source: String,
    /// nesting of unary / `not` / `**` levels being parsed
    depth: usize,
}

/// simpleeval recurses through two python frames per expression node, and cpython's default recursion
/// limit of 1000 stops it at about 495 nested nodes (measured on python 3.14). Nesting beyond this
/// fails with the same RecursionError text instead of overflowing the rust stack.
const MAX_NODE_DEPTH: usize = 495;

fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

fn is_target(e: &Expr) -> bool {
    matches!(e, Expr::Name(_) | Expr::Attr(..) | Expr::Subscript(..))
}

/// the name cpython gives an expression node in assignment errors, and whether that message ends with
/// cpython's "here. Maybe you meant '==' instead of '='?" hint
fn assign_kind(e: &Expr) -> (&'static str, bool) {
    match e {
        Expr::Const(Val::None) => ("None", false),
        Expr::Const(Val::Bool(true)) => ("True", false),
        Expr::Const(Val::Bool(false)) => ("False", false),
        Expr::Const(_) => ("literal", true),
        Expr::Call(..) => ("function call", true),
        Expr::Compare(..) => ("comparison", false),
        Expr::IfExp(..) => ("conditional expression", false),
        // cpython only adds the '==' hint to targets that parse as a bitwise_or, which excludes these
        Expr::And(_) | Expr::Or(_) | Expr::Unary(UnOp::Not, _) => ("expression", false),
        Expr::Tuple(_) => ("tuple", false),
        Expr::Starred(_) => ("starred", false),
        _ => ("expression", true),
    }
}

/// cpython's message for an assignment to something that is not a name, attribute or subscript. The
/// '==' hint is only given for the first target: cpython's hint rule does not apply after a chained '='.
fn assign_error(e: &Expr, line: usize, first_target: bool) -> PyErr {
    let (kind, hint) = assign_kind(e);
    if hint && first_target {
        syntax_error(&format!("cannot assign to {kind} here. Maybe you meant '==' instead of '='?"), line)
    } else {
        syntax_error(&format!("cannot assign to {kind}"), line)
    }
}

/// the compile error cpython gives for an assignment target, or None when the target is valid. A tuple
/// or starred target is valid when its elements are (cpython reports a bad element with the same message
/// as a bad plain target); simpleeval then evaluates only the value.
fn target_error(e: &Expr, line: usize, first_target: bool) -> Option<PyErr> {
    match e {
        Expr::Name(_) | Expr::Attr(..) | Expr::Subscript(..) => None,
        Expr::Tuple(items) => items.iter().find_map(|item| target_error(item, line, first_target)),
        Expr::Starred(inner) => target_error(inner, line, first_target),
        _ => Some(assign_error(e, line, first_target)),
    }
}

/// cpython's message for an augmented assignment to something that is not a name, attribute or subscript
fn aug_error(e: &Expr, line: usize) -> PyErr {
    let (kind, _) = assign_kind(e);
    syntax_error(&format!("'{kind}' is an illegal expression for augmented assignment"), line)
}

impl Parser {
    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_tok(&self, k: usize) -> Option<&Tok> {
        self.toks.get(self.pos + k).map(|t| &t.tok)
    }

    fn line(&self) -> usize {
        self.toks[self.pos].line
    }

    /// consumes the current token; the final Eof token is never consumed
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == op)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.tok(), Tok::Name(n) if n == kw)
    }

    fn is_name_at(&self, k: usize, kw: &str) -> bool {
        matches!(self.peek_tok(k), Some(Tok::Name(n)) if n == kw)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn invalid<T>(&self) -> Res<T> {
        let msg = if self.two_values_in_brackets() { "invalid syntax. Perhaps you forgot a comma?" } else { "invalid syntax" };
        Err(syntax_error(msg, self.line()))
    }

    /// cpython's `invalid_expression` hint: an expression directly followed by another expression,
    /// inside brackets (a name directly followed by a string is excluded, as in cpython)
    fn two_values_in_brackets(&self) -> bool {
        let Some(prev) = self.pos.checked_sub(1).map(|p| &self.toks[p].tok) else { return false };
        let is_value_name = |n: &str| !is_keyword(n) || matches!(n, "True" | "False" | "None");
        let ends_expr = match prev {
            Tok::Num(_) | Tok::Str(_) => true,
            Tok::Name(n) => is_value_name(n.as_str()),
            Tok::Op(o) => matches!(*o, ")" | "]" | "}"),
            _ => false,
        };
        let starts_expr = match self.tok() {
            Tok::Num(_) | Tok::Str(_) => true,
            Tok::Name(n) => is_value_name(n.as_str()) || n == "not",
            Tok::Op(o) => matches!(*o, "(" | "[" | "{"),
            _ => false,
        };
        if !ends_expr || !starts_expr || matches!((prev, self.tok()), (Tok::Name(_), Tok::Str(_))) {
            return false;
        }
        let mut depth = 0i64;
        for t in &self.toks[..self.pos] {
            if let Tok::Op(o) = &t.tok {
                match *o {
                    "(" | "[" | "{" => depth += 1,
                    ")" | "]" | "}" => depth -= 1,
                    _ => {}
                }
            }
        }
        depth > 0
    }

    fn expect_op(&mut self, op: &str) -> Res<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            self.invalid()
        }
    }

    /// skips the rest of a bracketed comprehension and returns the node simpleeval refuses
    fn comprehension(&mut self, node: &str, close: &str) -> Res<Expr> {
        let mut depth = 0usize;
        loop {
            match self.tok() {
                Tok::Eof | Tok::Newline => return self.invalid(),
                Tok::Op(o) if depth == 0 && *o == close => {
                    self.bump();
                    return Ok(Expr::Fail(not_available(node)));
                }
                Tok::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(*o, ")" | "]" | "}") => depth = depth.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
    }

    fn skip_to_eof(&mut self) {
        while !matches!(self.tok(), Tok::Eof) {
            self.bump();
        }
    }

    fn parse_module(&mut self) -> Res<Vec<Stmt>> {
        let mut stmts = Vec::new();
        loop {
            while matches!(self.tok(), Tok::Newline) {
                self.bump();
            }
            if matches!(self.tok(), Tok::Eof) {
                break;
            }
            stmts.push(self.statement()?);
            match self.tok() {
                Tok::Newline | Tok::Eof => {}
                Tok::Op(";") => {
                    self.bump();
                }
                _ => return self.invalid(),
            }
        }
        Ok(stmts)
    }

    fn skip_statement(&mut self) {
        while !matches!(self.tok(), Tok::Newline | Tok::Eof | Tok::Op(";")) {
            self.bump();
        }
    }

    fn statement(&mut self) -> Res<Stmt> {
        if let Tok::Name(kw) = self.tok().clone() {
            let node = match kw.as_str() {
                "import" => {
                    self.skip_statement();
                    return Ok(Stmt::Fail("Sorry, 'import' is not allowed.".into()));
                }
                "from" => Some("ImportFrom"),
                "global" => Some("Global"),
                "nonlocal" => Some("Nonlocal"),
                "del" => Some("Delete"),
                "pass" => Some("Pass"),
                "break" => Some("Break"),
                "continue" => Some("Continue"),
                "return" => Some("Return"),
                "raise" => Some("Raise"),
                "assert" => Some("Assert"),
                "elif" | "else" | "except" | "finally" => return self.invalid(),
                "if" => Some("If"),
                "while" => Some("While"),
                "for" => Some("For"),
                "try" => Some("Try"),
                "with" => Some("With"),
                "def" => Some("FunctionDef"),
                "class" => Some("ClassDef"),
                "async" => match self.peek_tok(1) {
                    Some(Tok::Name(n)) if n == "def" => Some("AsyncFunctionDef"),
                    Some(Tok::Name(n)) if n == "for" => Some("AsyncFor"),
                    Some(Tok::Name(n)) if n == "with" => Some("AsyncWith"),
                    _ => return self.invalid(),
                },
                _ => None,
            };
            if let Some(node) = node {
                // a compound statement's block is never parsed: simpleeval only refuses the node
                self.skip_to_eof();
                return Ok(Stmt::Fail(not_available(node)));
            }
        }
        let first = self.test_list()?;
        if self.is_op(":") {
            if !is_target(&first) {
                return Err(syntax_error("illegal target for annotation", self.line()));
            }
            self.skip_statement();
            return Ok(Stmt::Fail(not_available("AnnAssign")));
        }
        if self.is_op("=") {
            let mut exprs = vec![first];
            while self.eat_op("=") {
                exprs.push(self.test_list()?);
            }
            let value = exprs.pop().unwrap_or(Expr::Const(Val::None));
            if let Some(err) = exprs.iter().enumerate().find_map(|(i, t)| target_error(t, self.line(), i == 0)) {
                return Err(err);
            }
            return Ok(Stmt::Assign(value));
        }
        if matches!(self.tok(), Tok::Op(op) if AUG_OPS.contains(op)) {
            if !is_target(&first) {
                return Err(aug_error(&first, self.line()));
            }
            self.bump();
            let value = self.test_list()?;
            return Ok(Stmt::AugAssign(value));
        }
        Ok(Stmt::Expr(first))
    }

    /// `a, b` at statement level is a tuple, which simpleeval refuses to evaluate
    fn test_list(&mut self) -> Res<Expr> {
        let first = self.star_or_test()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_op(",") {
            if matches!(self.tok(), Tok::Newline | Tok::Eof | Tok::Op("=" | ";")) || matches!(self.tok(), Tok::Op(op) if AUG_OPS.contains(op)) {
                break;
            }
            items.push(self.star_or_test()?);
        }
        Ok(Expr::Tuple(items))
    }

    /// a `test`, or a starred `*expr` (its operand is a bitwise_or, as in cpython's star_expression)
    fn star_or_test(&mut self) -> Res<Expr> {
        if self.eat_op("*") {
            let operand = self.bitor()?;
            return Ok(Expr::Starred(Box::new(operand)));
        }
        self.test()
    }

    fn test(&mut self) -> Res<Expr> {
        if self.is_kw("lambda") {
            self.bump();
            while !self.is_op(":") {
                if matches!(self.tok(), Tok::Newline | Tok::Eof) {
                    return self.invalid();
                }
                self.bump();
            }
            self.bump();
            self.test()?;
            return Ok(Expr::Fail(not_available("Lambda")));
        }
        let body = self.or_test()?;
        if self.is_kw("if") {
            self.bump();
            let cond = self.or_test()?;
            if !self.is_kw("else") {
                return Err(syntax_error("expected 'else' after 'if' expression", self.line()));
            }
            self.bump();
            if matches!(self.tok(), Tok::Newline | Tok::Eof) {
                return Err(syntax_error("expected expression after 'else', but statement is given", self.line()));
            }
            let orelse = self.test()?;
            return Ok(Expr::IfExp(Box::new(cond), Box::new(body), Box::new(orelse)));
        }
        Ok(body)
    }

    fn or_test(&mut self) -> Res<Expr> {
        let mut vals = vec![self.and_test()?];
        while self.is_kw("or") {
            self.bump();
            vals.push(self.and_test()?);
        }
        if vals.len() == 1 {
            return Ok(vals.remove(0));
        }
        Ok(Expr::Or(vals))
    }

    fn and_test(&mut self) -> Res<Expr> {
        let mut vals = vec![self.not_test()?];
        while self.is_kw("and") {
            self.bump();
            vals.push(self.not_test()?);
        }
        if vals.len() == 1 {
            return Ok(vals.remove(0));
        }
        Ok(Expr::And(vals))
    }

    fn not_test(&mut self) -> Res<Expr> {
        self.enter()?;
        let r = self.not_test_inner();
        self.depth -= 1;
        r
    }

    fn not_test_inner(&mut self) -> Res<Expr> {
        if self.is_kw("not") {
            self.bump();
            let inner = self.not_test()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(inner)));
        }
        self.comparison()
    }

    /// one more level of recursive nesting (unary operators, `not`, `**`); python's evaluator gives up
    /// at about this depth with RecursionError
    fn enter(&mut self) -> Res<()> {
        if self.depth >= MAX_NODE_DEPTH {
            return fail("maximum recursion depth exceeded");
        }
        self.depth += 1;
        Ok(())
    }

    fn cmp_op(&self) -> Option<(CmpOp, usize)> {
        match self.tok() {
            Tok::Op("<") => Some((CmpOp::Lt, 1)),
            Tok::Op(">") => Some((CmpOp::Gt, 1)),
            Tok::Op("==") => Some((CmpOp::Eq, 1)),
            Tok::Op(">=") => Some((CmpOp::GtE, 1)),
            Tok::Op("<=") => Some((CmpOp::LtE, 1)),
            Tok::Op("!=") => Some((CmpOp::NotEq, 1)),
            Tok::Name(n) if n == "in" => Some((CmpOp::In, 1)),
            Tok::Name(n) if n == "not" && self.is_name_at(1, "in") => Some((CmpOp::NotIn, 2)),
            Tok::Name(n) if n == "is" && self.is_name_at(1, "not") => Some((CmpOp::IsNot, 2)),
            Tok::Name(n) if n == "is" => Some((CmpOp::Is, 1)),
            _ => None,
        }
    }

    fn comparison(&mut self) -> Res<Expr> {
        let left = self.bitor()?;
        let mut rest = Vec::new();
        while let Some((op, width)) = self.cmp_op() {
            for _ in 0..width {
                self.bump();
            }
            rest.push((op, self.bitor()?));
        }
        if rest.is_empty() {
            return Ok(left);
        }
        Ok(Expr::Compare(Box::new(left), rest))
    }

    /// one left-associative binary precedence level
    fn binary_level(&mut self, table: &[(&str, BinOp)], lower: fn(&mut Parser) -> Res<Expr>) -> Res<Expr> {
        let mut left = lower(self)?;
        loop {
            let found = match self.tok() {
                Tok::Op(o) => table.iter().find(|(s, _)| *s == *o).map(|(_, op)| *op),
                _ => None,
            };
            let Some(op) = found else { break };
            self.bump();
            let right = lower(self)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn bitor(&mut self) -> Res<Expr> {
        self.binary_level(&[("|", BinOp::BitOr)], Self::bitxor)
    }

    fn bitxor(&mut self) -> Res<Expr> {
        self.binary_level(&[("^", BinOp::BitXor)], Self::bitand)
    }

    fn bitand(&mut self) -> Res<Expr> {
        self.binary_level(&[("&", BinOp::BitAnd)], Self::shift)
    }

    fn shift(&mut self) -> Res<Expr> {
        self.binary_level(&[("<<", BinOp::LShift), (">>", BinOp::RShift)], Self::arith)
    }

    fn arith(&mut self) -> Res<Expr> {
        self.binary_level(&[("+", BinOp::Add), ("-", BinOp::Sub)], Self::term)
    }

    fn term(&mut self) -> Res<Expr> {
        let table = [("*", BinOp::Mul), ("/", BinOp::Div), ("//", BinOp::FloorDiv), ("%", BinOp::Mod)];
        let mut left = self.binary_level(&table, Self::factor)?;
        while self.eat_op("@") {
            // simpleeval has no MatMult operator: it fails when this node is reached, before its operands
            self.binary_level(&table, Self::factor)?;
            left = Expr::Fail(format!("Operator 'MatMult()' does not exist in expression '{}'", self.source));
        }
        Ok(left)
    }

    fn factor(&mut self) -> Res<Expr> {
        self.enter()?;
        let r = self.factor_inner();
        self.depth -= 1;
        r
    }

    fn factor_inner(&mut self) -> Res<Expr> {
        let op = match self.tok() {
            Tok::Op("-") => Some(UnOp::Neg),
            Tok::Op("+") => Some(UnOp::Pos),
            Tok::Op("~") => Some(UnOp::Invert),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let inner = self.factor()?;
            return Ok(Expr::Unary(op, Box::new(inner)));
        }
        self.power()
    }

    fn power(&mut self) -> Res<Expr> {
        let base = self.atom_expr()?;
        if self.eat_op("**") {
            let exp = self.factor()?;
            return Ok(Expr::Binary(BinOp::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn atom_expr(&mut self) -> Res<Expr> {
        let mut e = self.atom()?;
        loop {
            if self.eat_op("(") {
                let (args, kwargs) = self.call_args()?;
                e = Expr::Call(Box::new(e), args, kwargs);
            } else if self.eat_op("[") {
                let key = self.subscript()?;
                self.expect_op("]")?;
                e = Expr::Subscript(Box::new(e), key);
            } else if self.eat_op(".") {
                let Tok::Name(name) = self.tok().clone() else { return self.invalid() };
                if is_keyword(&name) {
                    return self.invalid();
                }
                self.bump();
                e = Expr::Attr(Box::new(e), name);
            } else {
                break;
            }
        }
        Ok(e)
    }

    /// arguments after `(`, through the closing `)`
    fn call_args(&mut self) -> Res<(Vec<Expr>, Vec<(String, Expr)>)> {
        let mut args = Vec::new();
        let mut kwargs: Vec<(String, Expr)> = Vec::new();
        // a `**` unpacking is kept among the keywords under a name no identifier can have
        let mut double_star = false;
        while !self.is_op(")") {
            if self.is_op("*") {
                if double_star {
                    return Err(syntax_error("iterable argument unpacking follows keyword argument unpacking", self.line()));
                }
                self.bump();
                self.test()?;
                args.push(Expr::Fail(not_available("Starred")));
            } else if self.is_op("**") {
                self.bump();
                let value = self.test()?;
                kwargs.push((DOUBLE_STAR.to_string(), value));
                double_star = true;
            } else if matches!(self.tok(), Tok::Name(n) if !is_keyword(n)) && matches!(self.peek_tok(1), Some(Tok::Op("="))) {
                let Tok::Name(name) = self.bump() else { return self.invalid() };
                self.bump();
                let value = self.test()?;
                kwargs.push((name, value));
            } else {
                let value = self.test()?;
                if self.is_kw("for") {
                    // f(x for x in y): a GeneratorExp argument, closed by the call's ')'
                    args.push(self.comprehension("GeneratorExp", ")")?);
                    return Ok((args, kwargs));
                }
                if double_star {
                    return Err(syntax_error("positional argument follows keyword argument unpacking", self.line()));
                }
                if !kwargs.is_empty() {
                    return Err(syntax_error("positional argument follows keyword argument", self.line()));
                }
                args.push(value);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok((args, kwargs))
    }

    fn subscript(&mut self) -> Res<SubKey> {
        let key = self.subscript_item()?;
        if !self.is_op(",") {
            return Ok(key);
        }
        // a tuple index: the container is evaluated first, then simpleeval refuses the Tuple node
        while self.eat_op(",") {
            if self.is_op("]") {
                break;
            }
            self.subscript_item()?;
        }
        Ok(SubKey::Index(Box::new(Expr::Fail(not_available("Tuple")))))
    }

    /// one item of a subscript: an index or a slice
    fn subscript_item(&mut self) -> Res<SubKey> {
        let lo = if self.is_op(":") { None } else { Some(self.test()?) };
        if !self.is_op(":") {
            return match lo {
                Some(e) => Ok(SubKey::Index(Box::new(e))),
                None => self.invalid(),
            };
        }
        self.bump();
        let slice_end = |p: &Parser| p.is_op("]") || p.is_op(",") || p.is_op(":");
        let hi = if slice_end(&*self) { None } else { Some(Box::new(self.test()?)) };
        let step = if self.eat_op(":") {
            if self.is_op("]") || self.is_op(",") {
                None
            } else {
                Some(Box::new(self.test()?))
            }
        } else {
            None
        };
        Ok(SubKey::Slice(lo.map(Box::new), hi, step))
    }

    fn atom(&mut self) -> Res<Expr> {
        match self.tok().clone() {
            Tok::Num(v) => {
                self.bump();
                Ok(Expr::Const(v))
            }
            Tok::Str(_) => {
                let mut s = String::new();
                while let Tok::Str(part) = self.tok().clone() {
                    s.push_str(&part);
                    self.bump();
                }
                Ok(Expr::Const(Val::Str(s)))
            }
            Tok::Fail(msg) => {
                self.bump();
                Ok(Expr::Fail(msg))
            }
            Tok::Name(n) => match n.as_str() {
                "True" => {
                    self.bump();
                    Ok(Expr::Const(Val::Bool(true)))
                }
                "False" => {
                    self.bump();
                    Ok(Expr::Const(Val::Bool(false)))
                }
                "None" => {
                    self.bump();
                    Ok(Expr::Const(Val::None))
                }
                // these parse as expressions that simpleeval refuses by node name
                "yield" => {
                    self.bump();
                    if self.is_kw("from") {
                        self.bump();
                        self.test()?;
                        return Ok(Expr::Fail(not_available("YieldFrom")));
                    }
                    if !matches!(self.tok(), Tok::Newline | Tok::Eof | Tok::Op(")" | "]" | "}" | "," | "=" | ";")) {
                        self.test_list()?;
                    }
                    Ok(Expr::Fail(not_available("Yield")))
                }
                "await" => {
                    self.bump();
                    self.atom_expr()?;
                    Ok(Expr::Fail(not_available("Await")))
                }
                _ if is_keyword(&n) => self.invalid(),
                _ => {
                    self.bump();
                    Ok(Expr::Name(n))
                }
            },
            Tok::Op("(") => {
                self.bump();
                if self.eat_op(")") {
                    return Ok(Expr::Tuple(Vec::new()));
                }
                let first = self.test()?;
                if self.is_kw("for") {
                    return self.comprehension("GeneratorExp", ")");
                }
                if self.eat_op(":=") {
                    // a named expression: only a name can be assigned to
                    if !matches!(first, Expr::Name(_)) {
                        return self.invalid();
                    }
                    self.test()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Fail(not_available("NamedExpr")));
                }
                if self.is_op(",") {
                    let mut items = vec![first];
                    while self.eat_op(",") {
                        if self.is_op(")") {
                            break;
                        }
                        items.push(self.test()?);
                    }
                    self.expect_op(")")?;
                    return Ok(Expr::Tuple(items));
                }
                self.expect_op(")")?;
                Ok(first)
            }
            Tok::Op("[") => {
                self.bump();
                while !self.is_op("]") {
                    // `*x` unpacks inside a list display, which simpleeval refuses as a List
                    if self.eat_op("*") {
                        self.bitor()?;
                    } else {
                        self.test()?;
                    }
                    if self.is_kw("for") {
                        return self.comprehension("ListComp", "]");
                    }
                    if !self.eat_op(",") {
                        break;
                    }
                }
                self.expect_op("]")?;
                Ok(Expr::Fail(not_available("List")))
            }
            Tok::Op("{") => {
                self.bump();
                if self.eat_op("}") {
                    return Ok(Expr::Fail(not_available("Dict")));
                }
                // the display is a Dict when its items are key: value pairs or `**` unpacks, and a Set when
                // they are values or `*` unpacks; mixing the two is a syntax error
                let mut node: Option<&'static str> = None;
                let mut first = true;
                loop {
                    let item = if self.eat_op("**") {
                        self.bitor()?;
                        "Dict"
                    } else if self.eat_op("*") {
                        self.bitor()?;
                        "Set"
                    } else {
                        self.test()?;
                        if first && self.is_kw("for") {
                            return self.comprehension("SetComp", "}");
                        }
                        if self.eat_op(":") {
                            self.test()?;
                            if first && self.is_kw("for") {
                                return self.comprehension("DictComp", "}");
                            }
                            "Dict"
                        } else {
                            "Set"
                        }
                    };
                    if node.is_some_and(|n| n != item) {
                        return self.invalid();
                    }
                    node = Some(item);
                    first = false;
                    if !self.eat_op(",") || self.is_op("}") {
                        break;
                    }
                }
                self.expect_op("}")?;
                Ok(Expr::Fail(not_available(node.unwrap_or("Set"))))
            }
            _ => self.invalid(),
        }
    }
}

fn parse(text: &str, source: &str) -> Res<Vec<Stmt>> {
    let toks = lex(text)?;
    Parser { toks, pos: 0, source: source.to_string(), depth: 0 }.parse_module()
}

// ---------------------------------------------------------------- operators

fn unary(op: UnOp, v: &Val) -> Res<Val> {
    match op {
        UnOp::Not => Ok(Val::Bool(!truthy(v))),
        UnOp::Neg => match num_of(v) {
            Some(Num::I(i)) => Ok(Val::Int(-i)),
            Some(Num::F(f)) => Ok(Val::Float(-f)),
            None => fail(format!("bad operand type for unary -: '{}'", type_name(v))),
        },
        UnOp::Pos => match num_of(v) {
            Some(Num::I(i)) => Ok(Val::Int(i)),
            Some(Num::F(f)) => Ok(Val::Float(f)),
            None => fail(format!("bad operand type for unary +: '{}'", type_name(v))),
        },
        UnOp::Invert => match num_of(v) {
            Some(Num::I(i)) => Ok(Val::Int(!i)),
            _ => fail(format!("bad operand type for unary ~: '{}'", type_name(v))),
        },
    }
}

/// `abs(v) > limit` as python evaluates it for any operand type (abs() raises for non-numbers)
fn abs_exceeds(v: &Val, int_limit: &BigInt, float_limit: f64) -> Res<bool> {
    match num_of(v) {
        Some(Num::I(i)) => Ok(i.abs() > *int_limit),
        Some(Num::F(f)) => Ok(f.abs() > float_limit),
        None => fail(format!("bad operand type for abs(): '{}'", type_name(v))),
    }
}

/// simpleeval's safe_power / safe_lshift / safe_rshift guards. They run for every operand type,
/// before the operator itself, in the same order simpleeval checks them.
fn limit_guards(op: BinOp, l: &Val, r: &Val) -> Res<()> {
    let exceeded = match op {
        // `abs(a) > MAX_POWER or abs(b) > MAX_POWER`
        BinOp::Pow => {
            let max = BigInt::from(MAX_POWER);
            abs_exceeds(l, &max, MAX_POWER as f64)? || abs_exceeds(r, &max, MAX_POWER as f64)?
        }
        // `abs(b) > MAX_SHIFT or abs(a) > MAX_SHIFT_BASE`
        BinOp::LShift | BinOp::RShift => {
            let max_shift = BigInt::from(MAX_SHIFT);
            let max_base = BigInt::from_f64(f64::MAX).unwrap_or_default();
            abs_exceeds(r, &max_shift, MAX_SHIFT as f64)? || abs_exceeds(l, &max_base, f64::MAX)?
        }
        _ => false,
    };
    if exceeded {
        return fail(format!("Sorry! I don't want to evaluate {} {} {}", py_str(l)?, op.symbol(), py_str(r)?));
    }
    Ok(())
}

fn binary(op: BinOp, l: &Val, r: &Val) -> Res<Val> {
    limit_guards(op, l, r)?;
    if let (Some(a), Some(b)) = (num_of(l), num_of(r)) {
        return numeric_binary(op, a, b, l, r);
    }
    match (op, l, r) {
        (BinOp::Add, Val::Str(a), Val::Str(b)) => {
            if a.chars().count() + b.chars().count() > MAX_STRING_LENGTH {
                return fail("Sorry, adding those two together would make something too long.");
            }
            Ok(Val::Str(format!("{a}{b}")))
        }
        (BinOp::Add, Val::Str(_), other) => fail(format!("can only concatenate str (not \"{}\") to str", type_name(other))),
        (BinOp::Mul, Val::Str(s), n) | (BinOp::Mul, n, Val::Str(s)) if is_int_like(n) => repeat_str(s, n),
        // simpleeval's safe_mult first computes `other * len(str)` for the length check
        (BinOp::Mul, Val::Str(s), other) | (BinOp::Mul, other, Val::Str(s)) => match other {
            Val::Str(_) => fail("'>' not supported between instances of 'str' and 'int'"),
            Val::Float(f) if f * s.chars().count() as f64 > MAX_STRING_LENGTH as f64 => {
                fail("Sorry, I will not evaluate something that long.")
            }
            Val::Float(_) => fail("can't multiply sequence by non-int of type 'float'"),
            _ => fail(format!("unsupported operand type(s) for *: '{}' and 'int'", type_name(other))),
        },
        (BinOp::Mod, Val::Str(_), _) => fail("string formatting with % is not supported by this calculator"),
        _ => fail(format!("unsupported operand type(s) for {}: '{}' and '{}'", op.symbol(), type_name(l), type_name(r))),
    }
}

fn repeat_str(s: &str, n: &Val) -> Res<Val> {
    let count = match num_of(n) {
        Some(Num::I(i)) => i,
        _ => BigInt::zero(),
    };
    if !count.is_positive() {
        return Ok(Val::Str(String::new()));
    }
    let len = s.chars().count();
    if BigInt::from(len) * &count > BigInt::from(MAX_STRING_LENGTH) {
        return fail("Sorry, I will not evaluate something that long.");
    }
    let times = count.to_usize().unwrap_or(0);
    Ok(Val::Str(s.repeat(times)))
}

fn numeric_binary(op: BinOp, a: Num, b: Num, l: &Val, r: &Val) -> Res<Val> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => {
            let both_bool = matches!((l, r), (Val::Bool(_), Val::Bool(_)));
            int_binary(op, x, y, both_bool)
        }
        (a, b) => {
            let x = match a {
                Num::I(i) => int_to_f64(&i)?,
                Num::F(f) => f,
            };
            let y = match b {
                Num::I(i) => int_to_f64(&i)?,
                Num::F(f) => f,
            };
            float_binary(op, x, y, l, r)
        }
    }
}

/// python `int / int` (cpython long_true_divide): the quotient correctly rounded to a double. Operands
/// that are exact doubles divide directly. Otherwise the quotient is scaled to 55+ significant bits and
/// rounded half-to-even with the remainder as the sticky bit. Results too large for a double raise.
fn int_true_div(a: &BigInt, b: &BigInt) -> Res<f64> {
    let negative = a.is_negative() != b.is_negative();
    let (x, y) = (a.magnitude(), b.magnitude());
    let magnitude = if x.is_zero() {
        0.0
    } else if x.bits() <= 53 && y.bits() <= 53 {
        x.to_f64().unwrap_or(0.0) / y.to_f64().unwrap_or(1.0)
    } else {
        let shift = 55 + y.bits() as i64 - x.bits() as i64;
        let (num, den) = if shift >= 0 {
            (x << shift as usize, y.clone())
        } else {
            (x.clone(), y << (-shift) as usize)
        };
        let (q, rem) = num.div_rem(&den);
        let drop = q.bits() as i64 - 53;
        let mant = &q >> drop as usize;
        let low = &q - (&mant << drop as usize);
        let half = BigUint::one() << (drop - 1) as usize;
        let round_up = low > half || (low == half && (!rem.is_zero() || mant.is_odd()));
        let m = mant.to_f64().unwrap_or(0.0) + if round_up { 1.0 } else { 0.0 };
        let f = ldexp(m, drop - shift);
        if !f.is_finite() {
            return fail("integer division result too large for a float");
        }
        f
    };
    Ok(if negative { -magnitude } else { magnitude })
}

/// `x * 2**e`, applied in steps so the power of two stays representable
fn ldexp(mut x: f64, mut e: i64) -> f64 {
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    x * 2f64.powi(e as i32)
}

fn int_binary(op: BinOp, x: BigInt, y: BigInt, both_bool: bool) -> Res<Val> {
    match op {
        BinOp::Add => Ok(Val::Int(x + y)),
        BinOp::Sub => Ok(Val::Int(x - y)),
        BinOp::Mul => Ok(Val::Int(x * y)),
        BinOp::Div => {
            if y.is_zero() {
                return fail("division by zero");
            }
            Ok(Val::Float(int_true_div(&x, &y)?))
        }
        BinOp::FloorDiv => {
            if y.is_zero() {
                return fail("division by zero");
            }
            Ok(Val::Int(x.div_floor(&y)))
        }
        BinOp::Mod => {
            if y.is_zero() {
                return fail("division by zero");
            }
            Ok(Val::Int(x.mod_floor(&y)))
        }
        BinOp::Pow => {
            if !y.is_negative() {
                let e = y.to_u32().unwrap_or(0);
                return Ok(Val::Int(x.pow(e)));
            }
            // int ** negative int is computed as a float, like python
            if x.is_zero() {
                return fail("zero to a negative power");
            }
            Ok(Val::Float(int_to_f64(&x)?.powf(int_to_f64(&y)?)))
        }
        BinOp::LShift | BinOp::RShift => {
            // the simpleeval limits were checked in limit_guards
            if y.is_negative() {
                return fail("negative shift count");
            }
            let n = y.to_usize().unwrap_or(0);
            Ok(Val::Int(if op == BinOp::LShift { x << n } else { x >> n }))
        }
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
            if both_bool {
                let (a, b) = (!x.is_zero(), !y.is_zero());
                return Ok(Val::Bool(match op {
                    BinOp::BitAnd => a && b,
                    BinOp::BitOr => a || b,
                    _ => a != b,
                }));
            }
            Ok(Val::Int(match op {
                BinOp::BitAnd => x & y,
                BinOp::BitOr => x | y,
                _ => x ^ y,
            }))
        }
    }
}

/// python float `//` (CPython float_divmod)
fn float_floordiv(vx: f64, wx: f64) -> f64 {
    let m = vx % wx;
    let mut div = (vx - m) / wx;
    if m != 0.0 && (wx < 0.0) != (m < 0.0) {
        div -= 1.0;
    }
    if div != 0.0 {
        let mut f = div.floor();
        if div - f > 0.5 {
            f += 1.0;
        }
        f
    } else {
        0.0_f64.copysign(vx / wx)
    }
}

/// python float `%` (CPython float_rem)
fn float_mod(vx: f64, wx: f64) -> f64 {
    let mut m = vx % wx;
    if m != 0.0 {
        if (wx < 0.0) != (m < 0.0) {
            m += wx;
        }
    } else {
        m = 0.0_f64.copysign(wx);
    }
    m
}

fn float_binary(op: BinOp, x: f64, y: f64, l: &Val, r: &Val) -> Res<Val> {
    match op {
        BinOp::Add => Ok(Val::Float(x + y)),
        BinOp::Sub => Ok(Val::Float(x - y)),
        BinOp::Mul => Ok(Val::Float(x * y)),
        BinOp::Div => {
            if y == 0.0 {
                return fail("division by zero");
            }
            Ok(Val::Float(x / y))
        }
        BinOp::FloorDiv => {
            if y == 0.0 {
                return fail("division by zero");
            }
            Ok(Val::Float(float_floordiv(x, y)))
        }
        BinOp::Mod => {
            if y == 0.0 {
                return fail("division by zero");
            }
            Ok(Val::Float(float_mod(x, y)))
        }
        BinOp::Pow => float_pow(x, y),
        _ => fail(format!("unsupported operand type(s) for {}: '{}' and '{}'", op.symbol(), type_name(l), type_name(r))),
    }
}

fn float_pow(x: f64, y: f64) -> Res<Val> {
    if x == 0.0 && y < 0.0 {
        return fail("zero to a negative power");
    }
    if x < 0.0 && y.is_finite() && y.fract() != 0.0 {
        return fail("complex results are not supported by this calculator");
    }
    let res = x.powf(y);
    if res.is_infinite() && x.is_finite() && y.is_finite() {
        return fail("(34, 'Numerical result out of range')");
    }
    Ok(Val::Float(res))
}

fn equal(l: &Val, r: &Val) -> bool {
    if let (Some(a), Some(b)) = (num_of(l), num_of(r)) {
        return cmp_num(&a, &b) == Some(Ordering::Equal);
    }
    match (l, r) {
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::None, Val::None) => true,
        (Val::Func(a), Val::Func(b)) => a == b,
        _ => false,
    }
}

fn identical(l: &Val, r: &Val) -> bool {
    match (l, r) {
        (Val::None, Val::None) => true,
        (Val::Bool(a), Val::Bool(b)) => a == b,
        // cpython caches small ints, the empty string and the one-character latin strings
        (Val::Int(a), Val::Int(b)) => a == b && (-5..=256).contains(&saturating_i64(a)),
        (Val::Str(a), Val::Str(b)) => a == b && a.chars().count() <= 1,
        (Val::Func(a), Val::Func(b)) => a == b,
        _ => false,
    }
}

/// python ordering of two numbers; None when a NaN is involved
fn cmp_num(a: &Num, b: &Num) -> Option<Ordering> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => Some(x.cmp(y)),
        (Num::F(x), Num::F(y)) => x.partial_cmp(y),
        (Num::I(i), Num::F(f)) => cmp_int_float(i, *f),
        (Num::F(f), Num::I(i)) => cmp_int_float(i, *f).map(Ordering::reverse),
    }
}

/// exact comparison of an int with a float
fn cmp_int_float(i: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    let fl = f.floor();
    let bf = BigInt::from_f64(fl)?;
    Some(match i.cmp(&bf) {
        Ordering::Equal if f == fl => Ordering::Equal,
        Ordering::Equal => Ordering::Less,
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
    })
}

fn contains(needle: &Val, hay: &Val) -> Res<bool> {
    match hay {
        Val::Str(h) => match needle {
            Val::Str(n) => Ok(h.contains(n.as_str())),
            other => fail(format!("'in <string>' requires string as left operand, not {}", type_name(other))),
        },
        other => fail(format!("argument of type '{}' is not a container or iterable", type_name(other))),
    }
}

fn compare(op: CmpOp, l: &Val, r: &Val) -> Res<bool> {
    match op {
        CmpOp::Eq => Ok(equal(l, r)),
        CmpOp::NotEq => Ok(!equal(l, r)),
        CmpOp::In => contains(l, r),
        CmpOp::NotIn => Ok(!contains(l, r)?),
        CmpOp::Is => Ok(identical(l, r)),
        CmpOp::IsNot => Ok(!identical(l, r)),
        CmpOp::Lt | CmpOp::LtE | CmpOp::Gt | CmpOp::GtE => {
            let ord = if let (Some(a), Some(b)) = (num_of(l), num_of(r)) {
                cmp_num(&a, &b)
            } else if let (Val::Str(a), Val::Str(b)) = (l, r) {
                Some(a.cmp(b))
            } else {
                return fail(format!("'{}' not supported between instances of '{}' and '{}'", op.symbol(), type_name(l), type_name(r)));
            };
            Ok(match ord {
                None => false,
                Some(o) => match op {
                    CmpOp::Lt => o == Ordering::Less,
                    CmpOp::LtE => o != Ordering::Greater,
                    CmpOp::Gt => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                },
            })
        }
    }
}

// ---------------------------------------------------------------- subscripts

fn index_value(container: &Val, key: &Val) -> Res<Val> {
    match container {
        Val::Str(s) => {
            let idx = match key {
                Val::Int(i) => i.clone(),
                Val::Bool(b) => BigInt::from(u8::from(*b)),
                other => return fail(format!("string indices must be integers, not '{}'", type_name(other))),
            };
            // cpython converts the index to a Py_ssize_t before it checks the range
            let Some(idx) = idx.to_i64() else {
                return fail("cannot fit 'int' into an index-sized integer");
            };
            let chars: Vec<char> = s.chars().collect();
            let n = i64::try_from(chars.len()).unwrap_or(i64::MAX);
            let idx = if idx < 0 { idx + n } else { idx };
            if idx < 0 || idx >= n {
                return fail("string index out of range");
            }
            Ok(Val::Str(chars[idx as usize].to_string()))
        }
        other => not_subscriptable(other),
    }
}

/// python's error for `container[key]` when the container cannot be subscripted. The type objects
/// int, float and str name themselves as types rather than as objects of type 'type'.
fn not_subscriptable<T>(container: &Val) -> Res<T> {
    match container {
        Val::Func(f @ (Func::Int | Func::Float | Func::Str)) => {
            let name = match f {
                Func::Int => "int",
                Func::Float => "float",
                _ => "str",
            };
            fail(format!("type '{name}' is not subscriptable"))
        }
        other => fail(format!("'{}' object is not subscriptable", type_name(other))),
    }
}

/// python slice.indices() for a sequence of length `n`
fn slice_indices(n: i64, start: Option<i64>, stop: Option<i64>, step: i64) -> Vec<usize> {
    let (lower, upper) = if step < 0 { (-1, n - 1) } else { (0, n) };
    let norm = |v: i64| -> i64 {
        if v < 0 {
            let v = v.saturating_add(n);
            if v < lower { lower } else { v }
        } else if v > upper {
            upper
        } else {
            v
        }
    };
    let mut i = match start {
        None => {
            if step < 0 {
                upper
            } else {
                lower
            }
        }
        Some(v) => norm(v),
    };
    let stop = match stop {
        None => {
            if step < 0 {
                lower
            } else {
                upper
            }
        }
        Some(v) => norm(v),
    };
    let mut out = Vec::new();
    while (step > 0 && i < stop) || (step < 0 && i > stop) {
        out.push(i as usize);
        i = i.saturating_add(step);
    }
    out
}

fn slice_bound(v: &Option<Val>) -> Res<Option<i64>> {
    match v {
        None | Some(Val::None) => Ok(None),
        Some(other) => match num_of(other) {
            Some(Num::I(i)) => Ok(Some(saturating_i64(&i))),
            _ => fail("slice indices must be integers or None or have an __index__ method"),
        },
    }
}

fn slice_value(container: &Val, lo: Option<Val>, hi: Option<Val>, step: Option<Val>) -> Res<Val> {
    let Val::Str(s) = container else {
        return not_subscriptable(container);
    };
    let lo = slice_bound(&lo)?;
    let hi = slice_bound(&hi)?;
    let step = slice_bound(&step)?.unwrap_or(1);
    if step == 0 {
        return fail("slice step cannot be zero");
    }
    let chars: Vec<char> = s.chars().collect();
    let picked: String = slice_indices(chars.len() as i64, lo, hi, step).into_iter().map(|i| chars[i]).collect();
    Ok(Val::Str(picked))
}

// ---------------------------------------------------------------- functions

/// the parts of a call that the python functions read: positional and keyword values
fn first_arg(args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Option<Val> {
    args.into_iter().chain(kwargs.into_iter().map(|(_, v)| v)).next()
}

fn call_function(f: Func, args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Res<Val> {
    match f {
        Func::Rand => {
            // simpleeval's `rand` is CPython's bound `random.random`, so its errors name Random.random
            if !args.is_empty() {
                return fail(format!("Random.random() takes no arguments ({} given)", args.len()));
            }
            if !kwargs.is_empty() {
                return fail("Random.random() takes no keyword arguments");
            }
            Ok(Val::Float(rand::random::<f64>()))
        }
        Func::RandInt => random_int(args, kwargs),
        Func::Int => int_call(args, kwargs),
        Func::Float => float_call(args, kwargs),
        Func::Str => str_call(args, kwargs),
    }
}

/// simpleeval's `random_int(top)`: `int(random() * top)`
fn random_int(args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Res<Val> {
    // cpython binds the keywords before it counts the positional arguments: a `top` after a positional
    // argument is a repeat, any other name is unexpected, and a repeated `top=` keeps the last value
    let mut kw_top = None;
    for (k, v) in kwargs {
        if k != "top" {
            return fail(format!("random_int() got an unexpected keyword argument '{k}'"));
        }
        if !args.is_empty() {
            return fail("random_int() got multiple values for argument 'top'");
        }
        kw_top = Some(v);
    }
    if args.len() > 1 {
        return fail(format!("random_int() takes 1 positional argument but {} were given", args.len()));
    }
    let Some(top) = args.into_iter().next().or(kw_top) else {
        return fail("random_int() missing 1 required positional argument: 'top'");
    };
    let r: f64 = rand::random();
    let product = match num_of(&top) {
        Some(Num::F(f)) => r * f,
        Some(Num::I(i)) => r * int_to_f64(&i)?,
        None => match &top {
            Val::Str(_) => return fail("can't multiply sequence by non-int of type 'float'"),
            other => return fail(format!("unsupported operand type(s) for *: 'float' and '{}'", type_name(other))),
        },
    };
    Ok(Val::Int(float_to_int(product)?))
}

fn int_call(args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Res<Val> {
    // cpython counts keyword arguments with the positional ones before it looks at their names
    let total = args.len() + kwargs.len();
    if total > 2 {
        // without keywords cpython uses its positional-only message
        if kwargs.is_empty() {
            return fail(format!("int expected at most 2 arguments, got {total}"));
        }
        return fail(format!("int() takes at most 2 arguments ({total} given)"));
    }
    // int's `x` is positional-only; `base` is the only keyword
    if let Some((k, _)) = kwargs.iter().find(|(k, _)| k != "base") {
        return fail(format!("int() got an unexpected keyword argument '{k}'"));
    }
    let mut it = args.into_iter();
    let x = it.next();
    let mut base = it.next();
    // at most two arguments in all, so a keyword can only be the base when no second positional is given
    if let Some((_, v)) = kwargs.into_iter().next() {
        base = Some(v);
    }
    let Some(x) = x else {
        if base.is_some() {
            return fail("int() missing string argument");
        }
        return Ok(Val::Int(BigInt::zero()));
    };
    if let Some(b) = base {
        let Val::Str(s) = &x else {
            return fail("int() can't convert non-string with explicit base");
        };
        let b = index_arg(&b)?;
        if b != 0 && !(2..=36).contains(&b) {
            return fail("int() base must be >= 2 and <= 36, or 0");
        }
        return parse_int_str(s, b);
    }
    match &x {
        Val::Int(i) => Ok(Val::Int(i.clone())),
        Val::Bool(b) => Ok(Val::Int(BigInt::from(u8::from(*b)))),
        Val::Float(f) => Ok(Val::Int(float_to_int(*f)?)),
        Val::Str(s) => parse_int_str(s, 10),
        other => fail(format!("int() argument must be a string, a bytes-like object or a real number, not '{}'", type_name(other))),
    }
}

/// python `int(s, base)` for base 0 (auto) or 2..=36
fn parse_int_str(s: &str, base: i64) -> Res<Val> {
    let invalid = || fail(format!("invalid literal for int() with base {base}: {}", repr_str(s)));
    let body = py_strip(s);
    let (neg, rest) = if let Some(r) = body.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = body.strip_prefix('+') {
        (false, r)
    } else {
        (false, body)
    };
    let prefix_radix: u32 = match rest.get(0..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 0,
    };
    let (radix, digits) = if prefix_radix != 0 && (base == 0 || base == i64::from(prefix_radix)) {
        (prefix_radix, &rest[2..])
    } else if base == 0 {
        if rest.len() > 1 && rest.starts_with('0') && rest.chars().any(|c| c.is_ascii_digit() && c != '0') {
            return invalid();
        }
        (10u32, rest)
    } else {
        (base as u32, rest)
    };
    let chars: Vec<char> = digits.chars().collect();
    let mut clean = String::new();
    for (k, &c) in chars.iter().enumerate() {
        if c == '_' {
            let prev_ok = k > 0 && chars[k - 1].to_digit(radix).is_some();
            let next_ok = chars.get(k + 1).is_some_and(|n| n.to_digit(radix).is_some());
            if !(prev_ok && next_ok) {
                return invalid();
            }
        } else if c.to_digit(radix).is_some() {
            clean.push(c);
        } else {
            return invalid();
        }
    }
    // cpython limits the digits of every base that is not a power of two
    if !matches!(radix, 2 | 4 | 8 | 16 | 32) && clean.len() > MAX_STR_DIGITS {
        return fail(format!(
            "Exceeds the limit (4300 digits) for integer string conversion: value has {} digits; use sys.set_int_max_str_digits() to increase the limit",
            clean.len()
        ));
    }
    let Some(v) = BigInt::parse_bytes(clean.as_bytes(), radix) else { return invalid() };
    Ok(Val::Int(if neg { -v } else { v }))
}

/// `float` with underscores removed when python allows them (only between digits)
fn parse_float_str(s: &str) -> Res<f64> {
    let body = py_strip(s);
    let chars: Vec<char> = body.chars().collect();
    let mut clean = String::new();
    let mut ok = true;
    for (k, &c) in chars.iter().enumerate() {
        if c == '_' {
            let prev_ok = k > 0 && chars[k - 1].is_ascii_digit();
            let next_ok = chars.get(k + 1).is_some_and(|n| n.is_ascii_digit());
            if !(prev_ok && next_ok) {
                ok = false;
                break;
            }
        } else {
            clean.push(c);
        }
    }
    match (ok, clean.parse::<f64>()) {
        (true, Ok(f)) => Ok(f),
        _ => fail(format!("could not convert string to float: {}", repr_str(s))),
    }
}

fn float_call(args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Res<Val> {
    if !kwargs.is_empty() {
        return fail("float() takes no keyword arguments");
    }
    let total = args.len() + kwargs.len();
    if total > 1 {
        return fail(format!("float expected at most 1 argument, got {total}"));
    }
    match first_arg(args, kwargs) {
        None => Ok(Val::Float(0.0)),
        Some(Val::Float(f)) => Ok(Val::Float(f)),
        Some(Val::Int(i)) => Ok(Val::Float(int_to_f64(&i)?)),
        Some(Val::Bool(b)) => Ok(Val::Float(if b { 1.0 } else { 0.0 })),
        Some(Val::Str(s)) => parse_float_str(&s).map(Val::Float),
        Some(other) => fail(format!("float() argument must be a string or a real number, not '{}'", type_name(&other))),
    }
}

/// python's `str(object='', encoding, errors)`: the three parameters take positional arguments in order
/// and then keywords. Without an object the result is '' (whatever the encoding and errors are).
fn str_call(args: Vec<Val>, kwargs: Vec<(String, Val)>) -> Res<Val> {
    const NAMES: [&str; 3] = ["object", "encoding", "errors"];
    let total = args.len() + kwargs.len();
    if total > NAMES.len() {
        if kwargs.is_empty() {
            return fail(format!("str expected at most 3 arguments, got {total}"));
        }
        return fail(format!("str() takes at most 3 arguments ({total} given)"));
    }
    let positional = args.len();
    let mut slots: [Option<Val>; 3] = [None, None, None];
    for (slot, v) in slots.iter_mut().zip(args) {
        *slot = Some(v);
    }
    for (k, v) in kwargs {
        let Some(i) = NAMES.iter().position(|n| *n == k) else {
            return fail(format!("str() got an unexpected keyword argument '{k}'"));
        };
        if i < positional {
            return fail(format!("argument for str() given by name ('{k}') and position ({})", i + 1));
        }
        // a repeated keyword is not an error in python: the last value wins
        slots[i] = Some(v);
    }
    let [object, encoding, errors] = slots;
    // cpython checks the type of encoding and errors before it looks at the object
    for (name, value) in [("encoding", &encoding), ("errors", &errors)] {
        if let Some(v) = value.as_ref().filter(|v| !matches!(v, Val::Str(_))) {
            let shown = if matches!(v, Val::None) { "None" } else { type_name(v) };
            return fail(format!("str() argument '{name}' must be str, not {shown}"));
        }
    }
    let Some(object) = object else {
        return Ok(Val::Str(String::new()));
    };
    if encoding.is_some() || errors.is_some() {
        return match object {
            Val::Str(_) => fail("decoding str is not supported"),
            other => fail(format!("decoding to str: need a bytes-like object, {} found", type_name(&other))),
        };
    }
    Ok(Val::Str(py_str(&object)?))
}

// ---------------------------------------------------------------- evaluator

struct Evaluator<'a> {
    /// the expression as the caller gave it (simpleeval reports this text in its messages)
    expression: &'a str,
    /// nodes currently being evaluated (see MAX_NODE_DEPTH)
    depth: std::cell::Cell<usize>,
}

impl Evaluator<'_> {
    fn exec(&self, stmt: &Stmt) -> Res<Val> {
        match stmt {
            Stmt::Expr(e) | Stmt::Assign(e) | Stmt::AugAssign(e) => self.eval(e),
            Stmt::Fail(msg) => fail(msg.clone()),
        }
    }

    fn eval(&self, e: &Expr) -> Res<Val> {
        let depth = self.depth.get() + 1;
        if depth > MAX_NODE_DEPTH {
            return fail("maximum recursion depth exceeded");
        }
        self.depth.set(depth);
        let result = self.eval_node(e);
        self.depth.set(depth - 1);
        result
    }

    fn eval_node(&self, e: &Expr) -> Res<Val> {
        match e {
            Expr::Const(v) => {
                if let Val::Str(s) = v {
                    let n = s.chars().count();
                    if n > MAX_STRING_LENGTH {
                        return fail(format!("Literal in statement is too long! ({n}, when {MAX_STRING_LENGTH} is max)"));
                    }
                }
                Ok(v.clone())
            }
            Expr::Name(n) => match function_named(n) {
                Some(f) => Ok(Val::Func(f)),
                None => fail(format!("'{n}' is not defined for expression '{}'", self.expression)),
            },
            Expr::Fail(msg) => fail(msg.clone()),
            Expr::Tuple(_) => fail(not_available("Tuple")),
            Expr::Starred(_) => fail(not_available("Starred")),
            Expr::Unary(op, x) => {
                let v = self.eval(x)?;
                unary(*op, &v)
            }
            Expr::Binary(op, a, b) => {
                let l = self.eval(a)?;
                let r = self.eval(b)?;
                binary(*op, &l, &r)
            }
            Expr::And(vals) => {
                let mut last = Val::Bool(false);
                for v in vals {
                    last = self.eval(v)?;
                    if !truthy(&last) {
                        break;
                    }
                }
                Ok(last)
            }
            Expr::Or(vals) => {
                let mut last = Val::Bool(false);
                for v in vals {
                    last = self.eval(v)?;
                    if truthy(&last) {
                        break;
                    }
                }
                Ok(last)
            }
            Expr::Compare(first, rest) => {
                let mut left = self.eval(first)?;
                let mut result = Val::Bool(true);
                for (op, c) in rest {
                    if !truthy(&result) {
                        break;
                    }
                    let right = self.eval(c)?;
                    result = Val::Bool(compare(*op, &left, &right)?);
                    left = right;
                }
                Ok(result)
            }
            Expr::IfExp(test, body, orelse) => {
                if truthy(&self.eval(test)?) {
                    self.eval(body)
                } else {
                    self.eval(orelse)
                }
            }
            Expr::Call(func, args, kwargs) => self.call(func, args, kwargs),
            Expr::Attr(base, name) => self.attribute(base, name),
            Expr::Subscript(base, key) => {
                let container = self.eval(base)?;
                self.subscript(&container, key)
            }
        }
    }

    fn call(&self, func: &Expr, args: &[Expr], kwargs: &[(String, Expr)]) -> Res<Val> {
        let callee = match func {
            Expr::Name(n) => match function_named(n) {
                Some(f) => f,
                None => return fail(format!("Function '{n}' not defined, for expression '{}'.", self.expression)),
            },
            Expr::Attr(..) => match self.eval(func)? {
                Val::Func(f) => f,
                other => return fail(format!("'{}' object is not callable", type_name(&other))),
            },
            _ => return fail("Lambda Functions not implemented"),
        };
        let mut pos = Vec::with_capacity(args.len());
        for a in args {
            pos.push(self.eval(a)?);
        }
        // simpleeval builds the keywords with dict() after the positional arguments, so every keyword
        // value (including a `**` operand) is evaluated before the call
        let mut kw = Vec::with_capacity(kwargs.len());
        let mut unpacked = false;
        for (k, v) in kwargs {
            let value = self.eval(v)?;
            if k == DOUBLE_STAR {
                unpacked = true;
            } else {
                kw.push((k.clone(), value));
            }
        }
        // the `**` mapping gets a None key, which python refuses before the callee runs
        if unpacked {
            return fail("keywords must be strings");
        }
        call_function(callee, pos, kw)
    }

    fn attribute(&self, base: &Expr, name: &str) -> Res<Val> {
        if DISALLOW_PREFIXES.iter().any(|p| name.starts_with(p)) {
            // simpleeval joins two literals here, so the text has two spaces after "__attributes"
            return fail(format!("Sorry, access to __attributes  or func_ attributes is not available. ({name})"));
        }
        if DISALLOW_METHODS.contains(&name) {
            return fail(format!("Sorry, this method is not available. ({name})"));
        }
        self.eval(base)?;
        fail(format!("Attribute '{name}' does not exist in expression '{}'", self.expression))
    }

    fn subscript(&self, container: &Val, key: &SubKey) -> Res<Val> {
        match key {
            SubKey::Index(k) => {
                let kv = self.eval(k)?;
                index_value(container, &kv)
            }
            SubKey::Slice(lo, hi, step) => {
                let lo = self.opt(lo)?;
                let hi = self.opt(hi)?;
                let step = self.opt(step)?;
                slice_value(container, lo, hi, step)
            }
        }
    }

    fn opt(&self, e: &Option<Box<Expr>>) -> Res<Option<Val>> {
        match e {
            Some(x) => Ok(Some(self.eval(x)?)),
            None => Ok(None),
        }
    }
}

/// simpleeval.simple_eval(expression): parse, then evaluate the first statement.
/// Like simpleeval, a multi-statement input is parsed in full and only its first statement runs.
pub fn simple_eval(expression: &str) -> Res<Val> {
    // simpleeval parses `expr.strip()`, python's strip (which also removes U+001C..U+001F)
    let stmts = parse(py_strip(expression), expression)?;
    let Some(first) = stmts.first() else {
        return fail("Sorry, cannot evaluate empty string");
    };
    Evaluator { expression, depth: std::cell::Cell::new(0) }.exec(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disallowed_attribute_message_matches_simpleeval_text() {
        // simpleeval 1.0.8 builds this as "...__attributes " + " or func_ attributes..." (two spaces)
        assert_eq!(
            simple_eval("1 .__class__").unwrap_err().to_string(),
            "Sorry, access to __attributes  or func_ attributes is not available. (__class__)"
        );
        assert_eq!(
            simple_eval("1 .func_globals").unwrap_err().to_string(),
            "Sorry, access to __attributes  or func_ attributes is not available. (func_globals)"
        );
    }
}
