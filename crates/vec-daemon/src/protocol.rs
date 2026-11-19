//! Wire format of the embedding daemon, protocol version 2.
//!
//! All integers are little-endian `u32`. Every reply has the same shape:
//!
//! ```text
//! reply   = status:u32  len:u32  payload[len]
//!           status 0 = success, 1 = error (payload: UTF-8 message)
//! ```
//!
//! A connection starts with a handshake, then carries any number of requests:
//!
//! ```text
//! client  hello   = "VECD"  version:u32 (= 2)
//! daemon  reply   payload = JSON ModelIdentity {"name", "dim", "sha256"}
//!
//! client  request = count:u32  count × (len:u32  text[len], UTF-8)
//! daemon  reply   payload = count × dim f32 (LE), one unit vector per text
//! ```
//!
//! A request is at most `[daemon] max_request_bytes` long, headers
//! included, and carries at most `[daemon] max_texts_per_request` texts. The
//! client closes the connection when it is done. A daemon whose connections
//! are all in use answers the hello with an error reply ("busy ...") and
//! closes the connection.
//!
//! Version 1 (a bare `len text` request) is not supported. A version 1
//! daemon answers the hello with an error reply ("request too large"), so a
//! new client reports a protocol mismatch instead of hanging.

use std::io::{self, Read};

/// First four bytes of the hello.
pub const MAGIC: [u8; 4] = *b"VECD";
/// Protocol version spoken by this crate.
pub const VERSION: u32 = 2;
/// Reply status: success.
pub const STATUS_OK: u32 = 0;
/// Reply status: error, the payload is a UTF-8 message.
pub const STATUS_ERROR: u32 = 1;

/// Bytes of the hello: magic and version.
pub(crate) const HELLO_LEN: usize = 8;
/// Bytes of a length or count field.
pub(crate) const FIELD_LEN: usize = 4;

/// The hello a client sends.
#[must_use]
pub fn hello() -> [u8; HELLO_LEN] {
    let mut h = [0u8; HELLO_LEN];
    h[..4].copy_from_slice(&MAGIC);
    h[4..].copy_from_slice(&VERSION.to_le_bytes());
    h
}

/// Encoded size of a request for `texts`.
#[must_use]
pub fn request_len(texts: &[&str]) -> usize {
    texts.iter().fold(FIELD_LEN, |n, t| n + FIELD_LEN + t.len())
}

/// Encode a request (one buffer, one write).
#[must_use]
pub fn encode_request(texts: &[&str]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(request_len(texts));
    push_len(&mut buf, texts.len());
    for t in texts {
        push_len(&mut buf, t.len());
        buf.extend_from_slice(t.as_bytes());
    }
    buf
}

/// Encode a reply (one buffer, one write).
#[must_use]
pub fn encode_reply(status: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 * FIELD_LEN + payload.len());
    buf.extend_from_slice(&status.to_le_bytes());
    push_len(&mut buf, payload.len());
    buf.extend_from_slice(payload);
    buf
}

/// Encode a success reply carrying `vectors` as consecutive little-endian
/// f32 (one buffer, one write).
#[must_use]
pub fn encode_vectors_reply(vectors: &[Vec<f32>]) -> Vec<u8> {
    let payload_len = vectors.iter().map(Vec::len).sum::<usize>() * size_of::<f32>();
    let mut buf = Vec::with_capacity(2 * FIELD_LEN + payload_len);
    buf.extend_from_slice(&STATUS_OK.to_le_bytes());
    push_len(&mut buf, payload_len);
    for x in vectors.iter().flatten() {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

/// Decode a vectors payload of `dim`-float vectors.
#[must_use]
pub fn decode_vectors(payload: &[u8], dim: usize) -> Vec<Vec<f32>> {
    let (floats, _) = payload.as_chunks::<4>();
    floats
        .chunks(dim.max(1))
        .map(|v| v.iter().map(|b| f32::from_le_bytes(*b)).collect())
        .collect()
}

/// Read one little-endian `u32`.
pub fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; FIELD_LEN];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Read one `u32`, or `None` on a clean end of stream before its first byte.
pub fn read_u32_or_eof(r: &mut impl Read) -> io::Result<Option<u32>> {
    let mut b = [0u8; FIELD_LEN];
    let mut got = 0;
    while got < b.len() {
        match r.read(&mut b[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(u32::from_le_bytes(b)))
}

/// Append a length field. Lengths are bounded by `max_request_bytes` (a
/// `usize` from the config) before they get here; saturate rather than wrap.
fn push_len(buf: &mut Vec<u8>, len: usize) {
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    buf.extend_from_slice(&len.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_encoding_matches_its_length() {
        let texts = ["a", "", "héllo"];
        let buf = encode_request(&texts);
        assert_eq!(buf.len(), request_len(&texts));
        let mut r = buf.as_slice();
        assert_eq!(read_u32(&mut r).unwrap(), 3);
        assert_eq!(read_u32(&mut r).unwrap(), 1);
    }

    #[test]
    fn eof_before_a_field_is_clean_but_inside_is_an_error() {
        assert_eq!(read_u32_or_eof(&mut [].as_slice()).unwrap(), None);
        let err = read_u32_or_eof(&mut [1u8, 2].as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn vectors_round_trip() {
        let v = vec![vec![1.0, -2.5], vec![0.0, 3.25]];
        let reply = encode_vectors_reply(&v);
        let mut r = reply.as_slice();
        assert_eq!(read_u32(&mut r).unwrap(), STATUS_OK);
        assert_eq!(read_u32(&mut r).unwrap(), 16);
        assert_eq!(decode_vectors(r, 2), v);
    }

    #[test]
    fn hello_starts_with_magic_and_version() {
        let h = hello();
        assert_eq!(&h[..4], b"VECD");
        assert_eq!(u32::from_le_bytes(h[4..].try_into().unwrap()), VERSION);
    }
}
