//! Byte-level encoding primitives: fixed little-endian integers, LEB128 varints, zigzag,
//! length-prefixed bytes/strings and postcard payloads.
//!
//! Every [`Reader`] read is bounds-checked against the remaining input *and* a caller-given
//! maximum before anything is allocated, so hostile packets can neither panic the decoder nor
//! make it allocate more than the packet size.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Decoding failure. Decoders never panic on arbitrary input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input ended early.
    #[error("unexpected end of input")]
    Eof,
    /// A varint is longer than its type allows.
    #[error("varint overflows its type")]
    VarintOverflow,
    /// A length prefix exceeds its limit.
    #[error("length {len} exceeds limit {max}")]
    TooLong {
        /// Declared length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// A string is not UTF-8.
    #[error("invalid utf-8")]
    InvalidUtf8,
    /// An enum discriminant / kind / channel id is unknown.
    #[error("unknown {what} {value}")]
    Unknown {
        /// What was being decoded.
        what: &'static str,
        /// The offending value.
        value: u64,
    },
    /// Structurally invalid data.
    #[error("invalid: {0}")]
    Invalid(&'static str),
    /// An entity reference points at a `NetId` the client does not know yet; the value is
    /// parked in pending refs and retried (see `mapping`).
    #[error("unresolved entity reference")]
    Unresolved,
    /// A postcard payload failed to decode.
    #[error("postcard: {0}")]
    Postcard(String),
}

/// Encoding failure (only postcard payloads can fail).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("encode error: {0}")]
pub struct EncodeError(pub String);

/// Growable output buffer.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty writer with preallocated capacity.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    /// Reuse an existing allocation (cleared).
    pub fn from_vec(mut buf: Vec<u8>) -> Self {
        buf.clear();
        Self { buf }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Drop the contents, keep the allocation.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Truncate to `len` bytes (roll back a partially written record).
    pub fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// The written bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consume into the byte vector.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// One byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Little-endian `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Little-endian `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Little-endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Little-endian IEEE `f32`.
    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Unsigned LEB128.
    pub fn varu64(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    /// Unsigned LEB128 (≤ 5 bytes).
    pub fn varu32(&mut self, v: u32) {
        self.varu64(u64::from(v));
    }

    /// Unsigned LEB128 (≤ 3 bytes).
    pub fn varu16(&mut self, v: u16) {
        self.varu64(u64::from(v));
    }

    /// Zigzag + LEB128.
    pub fn vari64(&mut self, v: i64) {
        self.varu64(((v << 1) ^ (v >> 63)) as u64);
    }

    /// Zigzag + LEB128 (≤ 5 bytes).
    pub fn vari32(&mut self, v: i32) {
        self.varu32(((v << 1) ^ (v >> 31)) as u32);
    }

    /// Raw bytes, no length prefix.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// `varu32` length prefix + bytes.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.varu32(bytes.len() as u32);
        self.raw(bytes);
    }

    /// `varu32` length prefix + UTF-8.
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Length-prefixed postcard encoding of `value`.
    pub fn postcard<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), EncodeError> {
        let bytes = postcard::to_stdvec(value).map_err(|e| EncodeError(e.to_string()))?;
        self.bytes(&bytes);
        Ok(())
    }
}

/// Bounds-checked cursor over a received packet.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Read from `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not consumed yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether everything was consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Current offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Next `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(DecodeError::Eof);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.raw(N)?);
        Ok(a)
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.raw(1)?[0])
    }

    /// Little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        self.array().map(u16::from_le_bytes)
    }

    /// Little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_le_bytes)
    }

    /// Little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_le_bytes)
    }

    /// Little-endian IEEE `f32`.
    pub fn f32(&mut self) -> Result<f32, DecodeError> {
        self.array().map(f32::from_le_bytes)
    }

    /// Unsigned LEB128. Rejects encodings longer than 10 bytes and non-canonical (over-long)
    /// encodings such as `0x80 0x00`, so every value has exactly one wire form.
    pub fn varu64(&mut self) -> Result<u64, DecodeError> {
        let mut out = 0u64;
        for i in 0..10 {
            let b = self.u8()?;
            let chunk = u64::from(b & 0x7f);
            if i == 9 && chunk > 1 {
                return Err(DecodeError::VarintOverflow);
            }
            out |= chunk << (7 * i);
            if b & 0x80 == 0 {
                if b == 0 && i > 0 {
                    return Err(DecodeError::VarintOverflow);
                }
                return Ok(out);
            }
        }
        Err(DecodeError::VarintOverflow)
    }

    /// Unsigned LEB128 that must fit in `u16`.
    pub fn varu16(&mut self) -> Result<u16, DecodeError> {
        u16::try_from(self.varu64()?).map_err(|_| DecodeError::VarintOverflow)
    }

    /// Unsigned LEB128 that must fit in `u32`.
    pub fn varu32(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.varu64()?).map_err(|_| DecodeError::VarintOverflow)
    }

    /// Zigzag + LEB128.
    pub fn vari64(&mut self) -> Result<i64, DecodeError> {
        let v = self.varu64()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// Zigzag + LEB128 that must fit in `i32`.
    pub fn vari32(&mut self) -> Result<i32, DecodeError> {
        let v = self.varu32()?;
        Ok(((v >> 1) as i32) ^ -((v & 1) as i32))
    }

    /// `varu32` length prefix (≤ `max`) + bytes, borrowed from the input.
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], DecodeError> {
        let len = self.varu32()? as usize;
        if len > max {
            return Err(DecodeError::TooLong { len, max });
        }
        self.raw(len)
    }

    /// `varu32` length prefix (≤ `max` bytes) + UTF-8.
    pub fn str(&mut self, max: usize) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes(max)?).map_err(|_| DecodeError::InvalidUtf8)
    }

    /// Length-prefixed (≤ `max`) postcard value.
    pub fn postcard<T: DeserializeOwned>(&mut self, max: usize) -> Result<T, DecodeError> {
        let bytes = self.bytes(max)?;
        postcard_exact(bytes)
    }
}

/// Decode a postcard value that must use **all** of `bytes` (trailing garbage is rejected).
pub fn postcard_exact<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    match postcard::take_from_bytes::<T>(bytes) {
        Ok((v, [])) => Ok(v),
        Ok(_) => Err(DecodeError::Invalid("trailing bytes")),
        Err(e) => Err(DecodeError::Postcard(e.to_string())),
    }
}

/// Postcard-encode a value (plain data types never fail; a failure is reported as [`EncodeError`]).
pub fn to_postcard<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, EncodeError> {
    postcard::to_stdvec(value).map_err(|e| EncodeError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip() {
        let mut w = Writer::new();
        for v in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            w.varu64(v);
        }
        for v in [0i32, -1, 1, i32::MIN, i32::MAX] {
            w.vari32(v);
        }
        let mut r = Reader::new(w.as_slice());
        for v in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(r.varu64().unwrap(), v);
        }
        for v in [0i32, -1, 1, i32::MIN, i32::MAX] {
            assert_eq!(r.vari32().unwrap(), v);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn hostile_input_is_rejected() {
        assert_eq!(Reader::new(&[0xff; 11]).varu64(), Err(DecodeError::VarintOverflow));
        assert_eq!(Reader::new(&[0x80]).varu64(), Err(DecodeError::Eof));
        assert_eq!(Reader::new(&[0x80, 0x00]).varu64(), Err(DecodeError::VarintOverflow));
        assert_eq!(Reader::new(&[0xff, 0xff, 0x03]).varu16(), Ok(u16::MAX));
        assert_eq!(
            Reader::new(&[0xff, 0xff, 0x04]).varu16(),
            Err(DecodeError::VarintOverflow)
        );
        let mut w = Writer::new();
        w.varu32(1_000_000);
        assert_eq!(
            Reader::new(w.as_slice()).bytes(1024),
            Err(DecodeError::TooLong {
                len: 1_000_000,
                max: 1024
            })
        );
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_varu64_roundtrip(v in any::<u64>()) {
            let mut w = Writer::new();
            w.varu64(v);
            prop_assert!(w.len() <= 10);
            let mut r = Reader::new(w.as_slice());
            prop_assert_eq!(r.varu64().unwrap(), v);
            prop_assert!(r.is_empty());
        }

        #[test]
        fn prop_zigzag_roundtrip(a in any::<i32>(), b in any::<i64>()) {
            let mut w = Writer::new();
            w.vari32(a);
            w.vari64(b);
            let mut r = Reader::new(w.as_slice());
            prop_assert_eq!(r.vari32().unwrap(), a);
            prop_assert_eq!(r.vari64().unwrap(), b);
        }

        #[test]
        fn prop_small_magnitudes_are_short(a in -63i32..=63) {
            let mut w = Writer::new();
            w.vari32(a);
            prop_assert_eq!(w.len(), 1);
        }

        #[test]
        fn prop_reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let mut r = Reader::new(&bytes);
            let _ = r.varu64();
            let _ = r.vari32();
            let _ = r.bytes(16);
            let _ = r.str(16);
            let _ = r.postcard::<(u32, String)>(32);
            let _ = r.u64();
            let _ = postcard_exact::<Vec<u16>>(&bytes);
        }
    }
}
