//! Hand-rolled little-endian binary codec for the wire protocol.
//!
//! Everything that crosses a socket goes through here. Rolling our own instead
//! of pulling in a serialization crate buys two things that matter for a UDP
//! game: every field's byte cost is visible at the call site (so staying under
//! the MTU is a decision, not a surprise), and the format never shifts because
//! a dependency bumped a major version.
//!
//! Decoding is total: a `Reader` fed hostile or truncated bytes returns `Err`,
//! never panics. Packets arrive from the network, so that is a hard requirement
//! rather than a nicety.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Ran off the end of the buffer.
    Eof,
    /// An enum discriminant that does not correspond to any variant.
    BadTag(&'static str, u8),
    /// A string field that was not valid UTF-8.
    BadUtf8,
    /// A length prefix that exceeds the sanity cap for its field.
    TooLong,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Eof => write!(f, "unexpected end of packet"),
            DecodeError::BadTag(ty, v) => write!(f, "invalid {ty} discriminant {v}"),
            DecodeError::BadUtf8 => write!(f, "string field was not valid utf-8"),
            DecodeError::TooLong => write!(f, "length prefix exceeded sanity limit"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T> = std::result::Result<T, DecodeError>;

/// Anything that can be written to the wire.
pub trait Encode {
    fn encode(&self, w: &mut Writer);

    fn to_vec(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode(&mut w);
        w.into_inner()
    }
}

/// Anything that can be read back off the wire.
pub trait Decode: Sized {
    fn decode(r: &mut Reader<'_>) -> Result<Self>;

    fn from_slice(bytes: &[u8]) -> Result<Self> {
        Reader::new(bytes).read()
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap) }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// A signed value quantized to 16 bits at `scale` steps per unit.
    ///
    /// Used for velocities, which are signed and need far less range than an
    /// `f32` but enough precision that replayed prediction stays in step.
    pub fn signed_fixed16(&mut self, v: f32, scale: f32) -> &mut Self {
        self.i16((v * scale).clamp(i16::MIN as f32, i16::MAX as f32) as i16)
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Length-prefixed UTF-8, truncated to 255 bytes on a char boundary.
    pub fn string(&mut self, s: &str) -> &mut Self {
        let mut len = s.len().min(255);
        while len > 0 && !s.is_char_boundary(len) {
            len -= 1;
        }
        self.u8(len as u8);
        self.raw(&s.as_bytes()[..len])
    }

    /// An angle in radians, quantized to 1/65536 of a turn (~0.005 degrees).
    ///
    /// Halves the cost of every heading and turret bearing in a snapshot, and
    /// the error is far below what a player can perceive.
    pub fn angle(&mut self, radians: f32) -> &mut Self {
        let norm = radians.rem_euclid(crate::math::TAU) / crate::math::TAU;
        self.u16((norm * 65536.0) as u16)
    }

    /// A value in `0..=max` quantized to 16 bits. Used for health-style fields
    /// whose absolute magnitude the client can bound in advance.
    pub fn unorm16(&mut self, v: f32, max: f32) -> &mut Self {
        let t = (v / max).clamp(0.0, 1.0);
        self.u16((t * 65535.0).round() as u16)
    }

    /// A value in `0..=1` quantized to a single byte, for progress bars.
    pub fn unorm8(&mut self, v: f32) -> &mut Self {
        self.u8((v.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    pub fn vec2(&mut self, v: crate::math::Vec2) -> &mut Self {
        self.f32(v.x).f32(v.y)
    }

    pub fn write<T: Encode>(&mut self, v: &T) -> &mut Self {
        v.encode(self);
        self
    }

    /// Writes a `u8`-counted list. Callers are responsible for keeping lists
    /// short enough to fit the MTU; see `protocol::MAX_PAYLOAD`.
    pub fn list<T, F>(&mut self, items: &[T], mut f: F) -> &mut Self
    where
        F: FnMut(&mut Writer, &T),
    {
        let n = items.len().min(255);
        self.u8(n as u8);
        for item in &items[..n] {
            f(self, item);
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError::Eof);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn signed_fixed16(&mut self, scale: f32) -> Result<f32> {
        Ok(self.i16()? as f32 / scale)
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub fn string(&mut self) -> Result<String> {
        let len = self.u8()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| DecodeError::BadUtf8)
    }

    pub fn angle(&mut self) -> Result<f32> {
        let raw = self.u16()? as f32 / 65536.0;
        Ok(crate::math::wrap_angle(raw * crate::math::TAU))
    }

    pub fn unorm16(&mut self, max: f32) -> Result<f32> {
        Ok(self.u16()? as f32 / 65535.0 * max)
    }

    pub fn unorm8(&mut self) -> Result<f32> {
        Ok(self.u8()? as f32 / 255.0)
    }

    pub fn vec2(&mut self) -> Result<crate::math::Vec2> {
        Ok(crate::math::Vec2::new(self.f32()?, self.f32()?))
    }

    pub fn read<T: Decode>(&mut self) -> Result<T> {
        T::decode(self)
    }

    /// Reads a `u8`-counted list written by [`Writer::list`].
    pub fn list<T, F>(&mut self, mut f: F) -> Result<Vec<T>>
    where
        F: FnMut(&mut Reader<'a>) -> Result<T>,
    {
        let n = self.u8()? as usize;
        let mut out = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            out.push(f(self)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Vec2, wrap_angle};

    #[test]
    fn scalars_round_trip() {
        let mut w = Writer::new();
        w.u8(7).u16(0xBEEF).u32(0xDEAD_BEEF).u64(u64::MAX).f32(-1.5).bool(true).i8(-3);
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.f32().unwrap(), -1.5);
        assert!(r.bool().unwrap());
        assert_eq!(r.i8().unwrap(), -3);
        assert!(r.is_empty());
    }

    #[test]
    fn truncated_input_errors_instead_of_panicking() {
        let mut w = Writer::new();
        w.u32(1);
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes[..2]);
        assert_eq!(r.u32(), Err(DecodeError::Eof));
    }

    #[test]
    fn angle_quantization_stays_under_a_hundredth_of_a_degree() {
        for step in 0..720 {
            let a = wrap_angle(step as f32 * 0.5_f32.to_radians());
            let mut w = Writer::new();
            w.angle(a);
            let got = Reader::new(w.as_slice()).angle().unwrap();
            let err = crate::math::angle_delta(a, got).abs().to_degrees();
            assert!(err < 0.01, "angle {a} round-tripped to {got} (err {err} deg)");
        }
    }

    #[test]
    fn strings_truncate_on_char_boundaries() {
        let long = "\u{1F600}".repeat(100); // 4 bytes each, so 400 > 255
        let mut w = Writer::new();
        w.string(&long);
        let got = Reader::new(w.as_slice()).string().unwrap();
        assert!(got.len() <= 255);
        assert!(long.starts_with(&got));
    }

    #[test]
    fn vec2_and_lists_round_trip() {
        let pts = vec![Vec2::new(1.0, 2.0), Vec2::new(-3.5, 4.25)];
        let mut w = Writer::new();
        w.list(&pts, |w, p| {
            w.vec2(*p);
        });
        let got = Reader::new(w.as_slice()).list(|r| r.vec2()).unwrap();
        assert_eq!(got, pts);
    }
}
