//! Minimal JSON encoder/decoder for the text-protocol fallback (one JSON value
//! per line). We deliberately avoid pulling in serde_json to keep the
//! dependency tree tiny.

use crate::value::Value;

pub fn encode(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Double(d) => out.push_str(&d.to_string()),
        Value::Str(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, val);
            }
            out.push('}');
        }
    }
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

pub fn decode(input: &str) -> Result<Value, String> {
    let mut p = Parser {
        chars: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value()?;
    Ok(v)
}

struct Parser<'a> {
    chars: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(
            self.peek(),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        ) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("json: expected '{}' at {}", b as char, self.pos))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't') => {
                self.expect_lit("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect_lit("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.expect_lit("null")?;
                Ok(Value::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            other => Err(format!("json: unexpected byte {other:?} at {}", self.pos)),
        }
    }

    fn expect_lit(&mut self, lit: &str) -> Result<(), String> {
        let end = self.pos + lit.len();
        if end <= self.chars.len() && &self.chars[self.pos..end] == lit.as_bytes() {
            self.pos = end;
            Ok(())
        } else {
            Err(format!("json: expected '{lit}' at {}", self.pos))
        }
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                is_float = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.chars[start..self.pos]).unwrap();
        if is_float {
            s.parse::<f64>()
                .map(Value::Double)
                .map_err(|e| e.to_string())
        } else {
            s.parse::<i64>().map(Value::Int).map_err(|e| e.to_string())
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err("json: unterminated string".into()),
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => {
                            s.push('"');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            s.push('\\');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            s.push('/');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            s.push('\n');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            s.push('\t');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            s.push('\r');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let hex = std::str::from_utf8(&self.chars[self.pos..self.pos + 4])
                                .map_err(|e| e.to_string())?;
                            let cp = u32::from_str_radix(hex, 16).map_err(|e| e.to_string())?;
                            if let Some(c) = char::from_u32(cp) {
                                s.push(c);
                            }
                            self.pos += 4;
                        }
                        other => return Err(format!("json: bad escape {other:?}")),
                    }
                }
                Some(_) => {
                    // Copy one utf8 char worth of bytes.
                    let rest =
                        std::str::from_utf8(&self.chars[self.pos..]).map_err(|e| e.to_string())?;
                    let c = rest.chars().next().unwrap();
                    s.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
        Ok(s)
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                other => return Err(format!("json: expected ',' or ']' got {other:?}")),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.parse_value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                other => return Err(format!("json: expected ',' or '}}' got {other:?}")),
            }
        }
        Ok(Value::Object(pairs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let v = Value::obj()
            .set("a", 1i64)
            .set("b", "hi")
            .set("c", vec![Value::Bool(true), Value::Null])
            .build();
        let s = encode(&v);
        let back = decode(&s).unwrap();
        assert_eq!(v, back);
    }
}
