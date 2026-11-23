// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A JSON parser and serializer (RFC 8259).
//!
//! Objects keep the order of their members. Integers that fit in an `i64`
//! are kept exact; every other number is an `f64`.

use std::fmt;

/// Nesting deeper than this is rejected, so hostile input cannot overflow
/// the stack.
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Byte offset in the input at which the problem was found.
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for Error {}

/// Parses one JSON document. Anything but whitespace after it is an error.
pub fn parse(input: &str) -> Result<Value, Error> {
    let mut parser = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("unexpected data after the document"));
    }
    Ok(value)
}

/// Parses a JSON document from bytes, which must be UTF-8.
pub fn parse_bytes(input: &[u8]) -> Result<Value, Error> {
    let text = std::str::from_utf8(input).map_err(|e| Error {
        offset: e.valid_up_to(),
        message: "input is not valid UTF-8",
    })?;
    parse(text)
}

impl Value {
    /// Looks up a member of an object. Returns `None` for other kinds of
    /// value and for missing members.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|n| u64::try_from(n).ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(members) => Some(members),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Builds an object from key and value pairs.
    pub fn object<K: Into<String>>(members: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Object(members.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Builds an array of strings.
    pub fn strings<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> Value {
        Value::Array(
            items
                .into_iter()
                .map(|s| Value::String(s.as_ref().to_string()))
                .collect(),
        )
    }

    /// Serializes on one line with no extra whitespace.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// Serializes with two-space indentation, for files people read.
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, level: usize) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(n) => out.push_str(&n.to_string()),
            Value::Float(n) => write_float(out, *n),
            Value::String(s) => write_string(out, s),
            Value::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent, level + 1);
                    item.write(out, indent, level + 1);
                }
                newline(out, indent, level);
                out.push(']');
            }
            Value::Object(members) => {
                if members.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, indent, level + 1);
                    write_string(out, key);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    value.write(out, indent, level + 1);
                }
                newline(out, indent, level);
                out.push('}');
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(s)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Int(n)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(width) = indent {
        out.push('\n');
        for _ in 0..width * level {
            out.push(' ');
        }
    }
}

fn write_float(out: &mut String, n: f64) {
    if !n.is_finite() {
        // JSON has no way to write these.
        out.push_str("null");
        return;
    }
    let text = format!("{n:?}");
    out.push_str(&text);
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> Error {
        Error {
            offset: self.pos,
            message,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, literal: &[u8], value: Value) -> Result<Value, Error> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.expect_literal(b"null", Value::Null),
            Some(b't') => self.expect_literal(b"true", Value::Bool(true)),
            Some(b'f') => self.expect_literal(b"false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Error> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Error> {
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.error("expected a digit")),
        }
        let mut integer = true;
        if self.peek() == Some(b'.') {
            integer = false;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit after '.'"));
            }
            self.digits();
        }
        if let Some(b'e' | b'E') = self.peek() {
            integer = false;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit in the exponent"));
            }
            self.digits();
        }
        // The slice holds only ASCII digits, signs, '.' and 'e'.
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
        if integer {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Value::Int(n));
            }
        }
        match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Value::Float(n)),
            _ => Err(Error {
                offset: start,
                message: "number out of range",
            }),
        }
    }

    fn digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Result<String, Error> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let run_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The input came in as a `&str`, and the run ends only at an
            // ASCII byte, so it is valid UTF-8.
            out.push_str(
                std::str::from_utf8(&self.bytes[run_start..self.pos])
                    .map_err(|_| self.error("invalid UTF-8 in string"))?,
            );
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error("control character in string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Error> {
        let Some(b) = self.peek() else {
            return Err(self.error("unterminated string"));
        };
        self.pos += 1;
        match b {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let code = if (0xD800..0xDC00).contains(&first) {
                    // A high surrogate must be followed by a low one.
                    if self.bytes[self.pos..].starts_with(b"\\u") {
                        self.pos += 2;
                        let second = self.hex4()?;
                        if !(0xDC00..0xE000).contains(&second) {
                            return Err(self.error("invalid surrogate pair"));
                        }
                        0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                    } else {
                        return Err(self.error("unpaired surrogate"));
                    }
                } else if (0xDC00..0xE000).contains(&first) {
                    return Err(self.error("unpaired surrogate"));
                } else {
                    first
                };
                match char::from_u32(code) {
                    Some(c) => out.push(c),
                    None => return Err(self.error("invalid unicode escape")),
                }
            }
            _ => {
                self.pos -= 1;
                return Err(self.error("invalid escape"));
            }
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let mut code = 0u32;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(b @ b'0'..=b'9') => b - b'0',
                Some(b @ b'a'..=b'f') => b - b'a' + 10,
                Some(b @ b'A'..=b'F') => b - b'A' + 10,
                _ => return Err(self.error("expected four hex digits")),
            };
            code = code * 16 + u32::from(digit);
            self.pos += 1;
        }
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse("null"), Ok(Value::Null));
        assert_eq!(parse(" true "), Ok(Value::Bool(true)));
        assert_eq!(parse("false"), Ok(Value::Bool(false)));
        assert_eq!(parse("0"), Ok(Value::Int(0)));
        assert_eq!(parse("-12"), Ok(Value::Int(-12)));
        assert_eq!(parse("1.5"), Ok(Value::Float(1.5)));
        assert_eq!(parse("1e3"), Ok(Value::Float(1000.0)));
        assert_eq!(parse("-0.25E-1"), Ok(Value::Float(-0.025)));
        assert_eq!(parse("\"hi\""), Ok(Value::String("hi".into())));
    }

    #[test]
    fn integer_limits() {
        assert_eq!(parse("9223372036854775807"), Ok(Value::Int(i64::MAX)));
        assert_eq!(parse("-9223372036854775808"), Ok(Value::Int(i64::MIN)));
        // One past the limit falls back to a float.
        assert_eq!(
            parse("9223372036854775808"),
            Ok(Value::Float(9223372036854775808.0))
        );
        assert!(parse("1e999").is_err());
    }

    #[test]
    fn rejects_bad_numbers() {
        for bad in ["01", "-", "1.", ".5", "1e", "1e+", "+1", "0x10", "--1"] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn string_escapes() {
        assert_eq!(
            parse(r#""a\"b\\c\/d\b\f\n\r\t""#),
            Ok(Value::String("a\"b\\c/d\u{8}\u{c}\n\r\t".into()))
        );
        assert_eq!(parse(r#""\u00e9""#), Ok(Value::String("é".into())));
        assert_eq!(parse(r#""\ud83d\ude00""#), Ok(Value::String("😀".into())));
        assert_eq!(parse("\"é😀\""), Ok(Value::String("é😀".into())));
    }

    #[test]
    fn rejects_bad_strings() {
        for bad in [
            "\"abc",
            "\"a\\qb\"",
            "\"\\u12\"",
            "\"\\ud83d\"",
            "\"\\ude00\"",
            "\"\\ud83d\\u0041\"",
            "\"a\nb\"",
            "\"a\tb\"",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn containers() {
        let v = parse(r#" { "a" : [1, 2, {"b": null}], "c": {}, "d": [] } "#).unwrap();
        assert_eq!(v.get("a").unwrap().as_array().unwrap().len(), 3);
        assert_eq!(v.get("a").unwrap().as_array().unwrap()[1].as_i64(), Some(2));
        assert!(v.get("a").unwrap().as_array().unwrap()[2]
            .get("b")
            .unwrap()
            .is_null());
        assert_eq!(v.get("c"), Some(&Value::Object(vec![])));
        assert_eq!(v.get("d"), Some(&Value::Array(vec![])));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn object_order_is_kept() {
        let v = parse(r#"{"z":1,"a":2,"m":3}"#).unwrap();
        let keys: Vec<&str> = v
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["z", "a", "m"]);
    }

    #[test]
    fn rejects_bad_structure() {
        for bad in [
            "",
            "   ",
            "[1,]",
            "[1 2]",
            "{\"a\":1,}",
            "{a:1}",
            "{\"a\" 1}",
            "[",
            "{",
            "nul",
            "true false",
            "[1]]",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn depth_limit() {
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert_eq!(parse(&deep).unwrap_err().message, "nested too deeply");
        // Far past the limit must fail cleanly, not overflow the stack.
        assert!(parse(&"[".repeat(1_000_000)).is_err());
    }

    #[test]
    fn error_offset() {
        assert_eq!(parse("[1, x]").unwrap_err().offset, 4);
    }

    #[test]
    fn rejects_invalid_utf8() {
        assert!(parse_bytes(b"\"\xff\"").is_err());
        assert_eq!(parse_bytes(b"[1]"), Ok(Value::Array(vec![Value::Int(1)])));
    }

    #[test]
    fn serialize_compact() {
        let v = Value::object([
            ("name", Value::from("a \"quoted\" \\ name\n")),
            ("n", Value::Int(-3)),
            ("f", Value::Float(1.0)),
            ("ok", Value::Bool(true)),
            ("none", Value::Null),
            ("list", Value::strings(["x", "y"])),
            ("ctl", Value::from("\u{1}")),
        ]);
        assert_eq!(
            v.to_json(),
            r#"{"name":"a \"quoted\" \\ name\n","n":-3,"f":1.0,"ok":true,"none":null,"list":["x","y"],"ctl":"\u0001"}"#
        );
    }

    #[test]
    fn serialize_pretty() {
        let v = Value::object([
            ("a", Value::Array(vec![Value::Int(1), Value::Int(2)])),
            ("b", Value::Object(vec![])),
        ]);
        assert_eq!(
            v.to_json_pretty(),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {}\n}\n"
        );
    }

    #[test]
    fn round_trip() {
        let text = r#"{"a":[1,2.5,"x\ny",{"b":[true,false,null]}],"é":"😀","big":1e300}"#;
        let v = parse(text).unwrap();
        assert_eq!(parse(&v.to_json()).unwrap(), v);
        assert_eq!(parse(&v.to_json_pretty()).unwrap(), v);
    }

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(Value::Float(f64::NAN).to_json(), "null");
        assert_eq!(Value::Float(f64::INFINITY).to_json(), "null");
    }
}
