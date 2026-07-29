//! BSER (binary serialization) encoder/decoder matching Facebook Watchman's
//! wire format, including both the classic PDU ("v1", magic `\x00\x01`) and
//! the "v2" PDU used by the real `watchman` binary and the `watchman_client`
//! / `serde_bser` Rust crates (magic `\x00\x02` followed by a 4-byte
//! capability bitfield). buck2 talks to the daemon socket exclusively in v2,
//! so real-world compatibility requires both. See docs/watchman-cpp.md.

use crate::value::Value;
use std::io::{self, Read, Write};

pub const MAGIC_V1: [u8; 2] = [0x00, 0x01];
pub const MAGIC_V2: [u8; 2] = [0x00, 0x02];

const TYPE_ARRAY: u8 = 0x00;
const TYPE_OBJECT: u8 = 0x01;
const TYPE_STRING: u8 = 0x02; // "bytestring" in v2 terminology
const TYPE_INT8: u8 = 0x03;
const TYPE_INT16: u8 = 0x04;
const TYPE_INT32: u8 = 0x05;
const TYPE_INT64: u8 = 0x06;
const TYPE_DOUBLE: u8 = 0x07;
const TYPE_TRUE: u8 = 0x08;
const TYPE_FALSE: u8 = 0x09;
const TYPE_NULL: u8 = 0x0a;
const TYPE_UTF8STRING: u8 = 0x0d;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduVersion {
    V1,
    V2,
}

fn encode_int(out: &mut Vec<u8>, v: i64) {
    if let Ok(v) = i8::try_from(v) {
        out.push(TYPE_INT8);
        out.push(v as u8);
    } else if let Ok(v) = i16::try_from(v) {
        out.push(TYPE_INT16);
        out.extend_from_slice(&v.to_le_bytes());
    } else if let Ok(v) = i32::try_from(v) {
        out.push(TYPE_INT32);
        out.extend_from_slice(&v.to_le_bytes());
    } else {
        out.push(TYPE_INT64);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn encode_string(out: &mut Vec<u8>, s: &str, version: PduVersion) {
    out.push(if version == PduVersion::V2 {
        TYPE_UTF8STRING
    } else {
        TYPE_STRING
    });
    encode_int(out, s.len() as i64);
    out.extend_from_slice(s.as_bytes());
}

fn encode_value(out: &mut Vec<u8>, v: &Value, version: PduVersion) {
    match v {
        Value::Null => out.push(TYPE_NULL),
        Value::Bool(true) => out.push(TYPE_TRUE),
        Value::Bool(false) => out.push(TYPE_FALSE),
        Value::Int(i) => encode_int(out, *i),
        Value::Double(d) => {
            out.push(TYPE_DOUBLE);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Str(s) => encode_string(out, s, version),
        Value::Array(items) => {
            out.push(TYPE_ARRAY);
            encode_int(out, items.len() as i64);
            for item in items {
                encode_value(out, item, version);
            }
        }
        Value::Object(pairs) => {
            out.push(TYPE_OBJECT);
            encode_int(out, pairs.len() as i64);
            for (k, val) in pairs {
                encode_string(out, k, version);
                encode_value(out, val, version);
            }
        }
    }
}

/// Encode a full framed BSER PDU (magic + [v2 capabilities] + length + body).
pub fn encode(v: &Value, version: PduVersion) -> Vec<u8> {
    let mut body = Vec::new();
    encode_value(&mut body, v, version);

    let mut out = Vec::with_capacity(body.len() + 16);
    match version {
        PduVersion::V1 => out.extend_from_slice(&MAGIC_V1),
        PduVersion::V2 => {
            out.extend_from_slice(&MAGIC_V2);
            out.extend_from_slice(&[0u8; 4]); // capabilities bitfield, none set
        }
    }
    encode_int(&mut out, body.len() as i64);
    out.extend_from_slice(&body);
    out
}

pub fn write_pdu<W: Write>(w: &mut W, v: &Value, version: PduVersion) -> io::Result<()> {
    w.write_all(&encode(v, version))?;
    w.flush()
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bser: truncated",
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_int(&mut self) -> io::Result<i64> {
        let tag = self.u8()?;
        Ok(match tag {
            TYPE_INT8 => self.take(1)?[0] as i8 as i64,
            TYPE_INT16 => i16::from_le_bytes(self.take(2)?.try_into().unwrap()) as i64,
            TYPE_INT32 => i32::from_le_bytes(self.take(4)?.try_into().unwrap()) as i64,
            TYPE_INT64 => i64::from_le_bytes(self.take(8)?.try_into().unwrap()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bser: expected int",
                ));
            }
        })
    }

    fn read_string_body(&mut self) -> io::Result<String> {
        let len = self.read_int()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    fn read_value(&mut self) -> io::Result<Value> {
        let tag = self.u8()?;
        Ok(match tag {
            TYPE_NULL => Value::Null,
            TYPE_TRUE => Value::Bool(true),
            TYPE_FALSE => Value::Bool(false),
            TYPE_INT8 => Value::Int(self.take(1)?[0] as i8 as i64),
            TYPE_INT16 => Value::Int(i16::from_le_bytes(self.take(2)?.try_into().unwrap()) as i64),
            TYPE_INT32 => Value::Int(i32::from_le_bytes(self.take(4)?.try_into().unwrap()) as i64),
            TYPE_INT64 => Value::Int(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            TYPE_DOUBLE => Value::Double(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            TYPE_STRING | TYPE_UTF8STRING => Value::Str(self.read_string_body()?),
            TYPE_ARRAY => {
                let len = self.read_int()? as usize;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(self.read_value()?);
                }
                Value::Array(items)
            }
            TYPE_OBJECT => {
                let len = self.read_int()? as usize;
                let mut pairs = Vec::with_capacity(len);
                for _ in 0..len {
                    // Keys are always encoded as string values (either tag).
                    let key_tag = self.u8()?;
                    if key_tag != TYPE_STRING && key_tag != TYPE_UTF8STRING {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "bser: expected string key",
                        ));
                    }
                    let key = self.read_string_body()?;
                    let val = self.read_value()?;
                    pairs.push((key, val));
                }
                Value::Object(pairs)
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bser: unsupported type tag {other:#x}"),
                ));
            }
        })
    }
}

/// Decode a single BSER-encoded value from a body buffer (no magic/length/
/// capabilities prefix).
pub fn decode(body: &[u8]) -> io::Result<Value> {
    let mut r = Reader { buf: body, pos: 0 };
    r.read_value()
}

fn read_length<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    Ok(match tag[0] {
        TYPE_INT8 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            b[0] as i8 as i64
        }
        TYPE_INT16 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            i16::from_le_bytes(b) as i64
        }
        TYPE_INT32 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            i32::from_le_bytes(b) as i64
        }
        TYPE_INT64 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            i64::from_le_bytes(b)
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bser: bad length tag {other:#x}"),
            ));
        }
    })
}

/// Read one full PDU (magic + [v2 capabilities] + length + body) from a
/// blocking reader, auto-detecting v1 vs v2 framing from the second magic
/// byte.
pub fn read_pdu<R: Read>(r: &mut R) -> io::Result<(Value, PduVersion)> {
    let mut magic = [0u8; 2];
    r.read_exact(&mut magic)?;
    let version = if magic == MAGIC_V1 {
        PduVersion::V1
    } else if magic == MAGIC_V2 {
        PduVersion::V2
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bser: bad magic",
        ));
    };
    if version == PduVersion::V2 {
        let mut caps = [0u8; 4];
        r.read_exact(&mut caps)?;
    }
    let len = read_length(r)?;
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let value = decode(&body)?;
    Ok((value, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v1() {
        let v = Value::obj()
            .set("version", "1.0")
            .set("ok", true)
            .set("count", 42i64)
            .set(
                "items",
                vec![Value::Int(1), Value::Str("x".into()), Value::Null],
            )
            .build();
        let encoded = encode(&v, PduVersion::V1);
        let mut cursor = io::Cursor::new(encoded);
        let (decoded, version) = read_pdu(&mut cursor).unwrap();
        assert_eq!(v, decoded);
        assert_eq!(version, PduVersion::V1);
    }

    #[test]
    fn roundtrip_v2() {
        let v = Value::obj()
            .set("sockname", "/tmp/x.sock")
            .set("version", "2024")
            .build();
        let encoded = encode(&v, PduVersion::V2);
        assert_eq!(&encoded[0..2], &MAGIC_V2);
        let mut cursor = io::Cursor::new(encoded);
        let (decoded, version) = read_pdu(&mut cursor).unwrap();
        assert_eq!(v, decoded);
        assert_eq!(version, PduVersion::V2);
    }
}
