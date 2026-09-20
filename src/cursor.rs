//! A small byte-slice cursor. Every parsing function in this crate takes
//! `&mut Cursor` rather than juggling raw indices by hand — it's the same
//! reason `std::io::Cursor` exists, just specialized to `&[u8]` so callers
//! don't need a `Read` impl to get positions and bounds-checked reads.

use anyhow::{bail, Result};

pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            bail!("unexpected end of file at offset {}", self.pos);
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn peek_u8(&self) -> Result<u8> {
        if self.pos >= self.data.len() {
            bail!("unexpected end of file at offset {}", self.pos);
        }
        Ok(self.data[self.pos])
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!(
                "unexpected end of file: wanted {} bytes at offset {}, only {} remain",
                n,
                self.pos,
                self.remaining()
            );
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Little-endian u16/u32/u64 readers — the RDB format is little-endian
    /// for every fixed-width field (timestamps, ziplist/listpack headers,
    /// the trailing CRC64), unlike the big-endian length encoding.
    pub fn read_u16_le(&mut self) -> Result<u16> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn read_u32_le(&mut self) -> Result<u32> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u64_le(&mut self) -> Result<u64> {
        let b = self.read_bytes(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_i64_le(&mut self) -> Result<i64> {
        Ok(self.read_u64_le()? as i64)
    }

    pub fn read_f64_le(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64_le()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_u8_and_advances() {
        let mut c = Cursor::new(&[1, 2, 3]);
        assert_eq!(c.read_u8().unwrap(), 1);
        assert_eq!(c.read_u8().unwrap(), 2);
        assert_eq!(c.position(), 2);
    }

    #[test]
    fn read_past_end_errors_not_panics() {
        let mut c = Cursor::new(&[1]);
        c.read_u8().unwrap();
        assert!(c.read_u8().is_err());
    }

    #[test]
    fn read_bytes_bounds_checked() {
        let mut c = Cursor::new(&[1, 2, 3]);
        assert!(c.read_bytes(10).is_err());
        assert_eq!(c.position(), 0, "failed read must not consume bytes");
    }

    #[test]
    fn little_endian_fixed_width_reads() {
        let mut c = Cursor::new(&[0x01, 0x00, 0x02, 0x00, 0x00, 0x00]);
        assert_eq!(c.read_u16_le().unwrap(), 1);
        assert_eq!(c.read_u32_le().unwrap(), 2);
    }

    #[test]
    fn f64_round_trips_through_bits() {
        let bits = std::f64::consts::PI.to_bits().to_le_bytes();
        let mut c = Cursor::new(&bits);
        assert_eq!(c.read_f64_le().unwrap(), std::f64::consts::PI);
    }
}
