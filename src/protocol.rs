//! PDU framing: read either a BSER frame (v1 magic `\x00\x01` or v2 magic
//! `\x00\x02`) or a line of JSON, matching watchman's "sniff the first
//! bytes" behavior on its socket. Responses are always written back using
//! the same framing/version the request arrived in, exactly as the real
//! watchman server does.

use crate::bser::{self, PduVersion};
use crate::json;
use crate::value::Value;
use std::io::{self, BufRead, Read, Write};

#[derive(Clone, Copy)]
pub enum Framing {
    Bser(PduVersion),
    Json,
}

/// Read one request PDU, auto-detecting BSER (v1/v2) vs JSON-line framing.
pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Option<(Value, Framing)>> {
    let mut first = [0u8; 1];
    if r.read(&mut first)? == 0 {
        return Ok(None);
    }
    if first[0] == bser::MAGIC_V1[0] {
        let mut second = [0u8; 1];
        r.read_exact(&mut second)?;
        let magic = [first[0], second[0]];
        if magic != bser::MAGIC_V1 && magic != bser::MAGIC_V2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol: bad bser magic",
            ));
        }
        // Reuse bser::read_pdu logic on the remaining stream by chaining the
        // already-consumed magic bytes back in.
        let mut chained = io::Cursor::new(magic).chain(r);
        let (v, version) = bser::read_pdu(&mut chained)?;
        Ok(Some((v, Framing::Bser(version))))
    } else {
        let mut line = Vec::new();
        line.push(first[0]);
        r.read_until(b'\n', &mut line)?;
        let s = String::from_utf8_lossy(&line);
        let v =
            json::decode(s.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some((v, Framing::Json)))
    }
}

pub fn write_response<W: Write>(w: &mut W, v: &Value, framing: &Framing) -> io::Result<()> {
    match framing {
        Framing::Bser(version) => bser::write_pdu(w, v, *version),
        Framing::Json => {
            let s = json::encode(v);
            w.write_all(s.as_bytes())?;
            w.write_all(b"\n")?;
            w.flush()
        }
    }
}
