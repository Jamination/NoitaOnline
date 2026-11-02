//! Minimal little-endian byte writer/reader used by the packet codec and the game protocol.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError;

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("truncated or malformed data")
    }
}

impl std::error::Error for DecodeError {}

pub type DecodeResult<T> = Result<T, DecodeError>;

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
    pub fn with_capacity(n: usize) -> Self {
        Self { buf: Vec::with_capacity(n) }
    }
    pub fn wrap(buf: Vec<u8>) -> Self {
        Self { buf }
    }
    #[inline]
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    #[inline]
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    #[inline]
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    #[inline]
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    #[inline]
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }
    #[inline]
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.u32(v.to_bits())
    }
    /// LEB128 unsigned varint.
    #[inline]
    pub fn varint(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
        self
    }
    /// Zig-zag signed varint.
    #[inline]
    pub fn svarint(&mut self, v: i64) -> &mut Self {
        self.varint(((v << 1) ^ (v >> 63)) as u64)
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    /// Length-prefixed byte string.
    pub fn blob(&mut self, b: &[u8]) -> &mut Self {
        self.varint(b.len() as u64);
        self.bytes(b)
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.blob(s.as_bytes())
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    #[inline]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    #[inline]
    pub fn take(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    #[inline]
    pub fn u8(&mut self) -> DecodeResult<u8> {
        Ok(self.take(1)?[0])
    }
    #[inline]
    pub fn u16(&mut self) -> DecodeResult<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    #[inline]
    pub fn u32(&mut self) -> DecodeResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    #[inline]
    pub fn u64(&mut self) -> DecodeResult<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    #[inline]
    pub fn i32(&mut self) -> DecodeResult<i32> {
        Ok(self.u32()? as i32)
    }
    #[inline]
    pub fn f32(&mut self) -> DecodeResult<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    pub fn varint(&mut self) -> DecodeResult<u64> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                return Err(DecodeError);
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }
    pub fn svarint(&mut self) -> DecodeResult<i64> {
        let u = self.varint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    pub fn blob(&mut self) -> DecodeResult<&'a [u8]> {
        let n = self.varint()?;
        if n > self.remaining() as u64 {
            return Err(DecodeError);
        }
        self.take(n as usize)
    }
    pub fn str(&mut self) -> DecodeResult<&'a str> {
        std::str::from_utf8(self.blob()?).map_err(|_| DecodeError)
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos..];
        self.pos = self.data.len();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_edges() {
        let vals = [0u64, 1, 127, 128, 300, 16383, 16384, u32::MAX as u64, u64::MAX];
        let mut w = Writer::new();
        for v in vals {
            w.varint(v);
        }
        let svals = [0i64, -1, 1, i64::MIN, i64::MAX, -300];
        for v in svals {
            w.svarint(v);
        }
        let buf = w.finish();
        let mut r = Reader::new(&buf);
        for v in vals {
            assert_eq!(r.varint().unwrap(), v);
        }
        for v in svals {
            assert_eq!(r.svarint().unwrap(), v);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn truncated_blob_is_error() {
        let mut w = Writer::new();
        w.varint(10).bytes(&[1, 2, 3]);
        let buf = w.finish();
        assert_eq!(Reader::new(&buf).blob(), Err(DecodeError));
    }
}
