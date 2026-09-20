//! RDB's length encoding and the string encoding built on top of it.
//!
//! Length encoding is the two-top-bits scheme documented for the format
//! (and empirically confirmed here against real dumps — the 14-bit path
//! below is exercised by a real 200-entry hash from this workspace's own
//! `redis-server`, where 200 doesn't fit the 6-bit range):
//!
//! - `00xxxxxx`             — 6-bit length, 0..=63
//! - `01xxxxxx yyyyyyyy`    — 14-bit length, big-endian across the two bytes
//! - `10000000` + 4 bytes   — 32-bit length, big-endian
//! - `10000001` + 8 bytes   — 64-bit length, big-endian (RDB v11+)
//! - `11xxxxxx`             — not a length at all: a "special" encoding,
//!   the low 6 bits select int8/int16/int32/LZF-string (see below)
use crate::cursor::Cursor;
use crate::lzf;
use anyhow::{bail, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum Length {
    Len(u64),
    /// A `11xxxxxx` special marker — string encodings interpret this
    /// (integer-as-string, or LZF-compressed); a bare length context
    /// (RESIZEDB, list/hash/set element counts, ...) never sees one and
    /// should treat it as an error.
    Special(u8),
}

pub fn read_length(c: &mut Cursor) -> Result<Length> {
    let b = c.read_u8()?;
    match b >> 6 {
        0 => Ok(Length::Len((b & 0x3f) as u64)),
        1 => {
            let b2 = c.read_u8()?;
            Ok(Length::Len((((b & 0x3f) as u64) << 8) | b2 as u64))
        }
        2 => {
            if b == 0x80 {
                let v = c.read_bytes(4)?;
                Ok(Length::Len(
                    u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as u64
                ))
            } else if b == 0x81 {
                let v = c.read_bytes(8)?;
                Ok(Length::Len(u64::from_be_bytes([
                    v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7],
                ])))
            } else {
                bail!("unsupported 0x10 length-encoding variant byte 0x{:02x}", b);
            }
        }
        3 => Ok(Length::Special(b & 0x3f)),
        _ => unreachable!("2-bit value can't exceed 3"),
    }
}

/// A plain length where a `Special` marker would be a format violation
/// (RESIZEDB's two counts, listpack/ziplist element counts read through
/// this same encoding in some contexts, etc).
pub fn read_plain_length(c: &mut Cursor) -> Result<u64> {
    match read_length(c)? {
        Length::Len(n) => Ok(n),
        Length::Special(code) => bail!(
            "expected a plain length, got special-encoding marker 0x{:02x}",
            code
        ),
    }
}

/// A decoded RDB string: length-prefixed bytes, an integer stored as a
/// string, or an LZF-compressed blob — all three collapse to owned bytes
/// once decoded, since every consumer here (key names, values headed for
/// a sub-decoder, listpack blobs) wants raw bytes either way.
pub fn read_string(c: &mut Cursor) -> Result<Vec<u8>> {
    match read_length(c)? {
        Length::Len(n) => {
            let n = usize::try_from(n).map_err(|_| anyhow::anyhow!("string length overflow"))?;
            Ok(c.read_bytes(n)?.to_vec())
        }
        Length::Special(0) => {
            let v = c.read_u8()? as i8;
            Ok(v.to_string().into_bytes())
        }
        Length::Special(1) => {
            let v = c.read_bytes(2)?;
            let v = i16::from_le_bytes([v[0], v[1]]);
            Ok(v.to_string().into_bytes())
        }
        Length::Special(2) => {
            let v = c.read_bytes(4)?;
            let v = i32::from_le_bytes([v[0], v[1], v[2], v[3]]);
            Ok(v.to_string().into_bytes())
        }
        Length::Special(3) => {
            let clen = read_plain_length(c)? as usize;
            let ulen = read_plain_length(c)? as usize;
            let compressed = c.read_bytes(clen)?;
            lzf::decompress(compressed, ulen)
        }
        Length::Special(code) => bail!("unknown special string encoding 0x{:02x}", code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_bit_length() {
        let mut c = Cursor::new(&[0x0a]);
        assert_eq!(read_length(&mut c).unwrap(), Length::Len(10));
    }

    #[test]
    fn fourteen_bit_length_matches_real_bighash_encoding() {
        // The exact bytes a real Redis wrote for a 200-field hash's field
        // count (see tests/fixtures/big_types_dump.rdb, key "bighash").
        let mut c = Cursor::new(&[0x40, 0xc8]);
        assert_eq!(read_length(&mut c).unwrap(), Length::Len(200));
    }

    #[test]
    fn thirty_two_bit_length() {
        let mut c = Cursor::new(&[0x80, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(read_length(&mut c).unwrap(), Length::Len(65536));
    }

    #[test]
    fn sixty_four_bit_length() {
        let mut c = Cursor::new(&[0x81, 0, 0, 0, 0, 0, 0, 0, 5]);
        assert_eq!(read_length(&mut c).unwrap(), Length::Len(5));
    }

    #[test]
    fn special_marker_is_not_a_length() {
        let mut c = Cursor::new(&[0xc0]);
        assert_eq!(read_length(&mut c).unwrap(), Length::Special(0));
    }

    #[test]
    fn plain_length_rejects_special_marker() {
        let mut c = Cursor::new(&[0xc0]);
        assert!(read_plain_length(&mut c).is_err());
    }

    #[test]
    fn string_int8_matches_real_counter_key() {
        // Real bytes for `SET counter 42` from tests/fixtures/valkey_dump.rdb.
        let mut c = Cursor::new(&[0xc0, 0x2a]);
        assert_eq!(read_string(&mut c).unwrap(), b"42");
    }

    #[test]
    fn string_int8_negative() {
        let mut c = Cursor::new(&[0xc0, 0xff]); // -1 as i8
        assert_eq!(read_string(&mut c).unwrap(), b"-1");
    }

    #[test]
    fn string_int16() {
        let v: i16 = -1000;
        let mut bytes = vec![0xc1];
        bytes.extend_from_slice(&v.to_le_bytes());
        let mut c = Cursor::new(&bytes);
        assert_eq!(read_string(&mut c).unwrap(), b"-1000");
    }

    #[test]
    fn string_int32() {
        let v: i32 = 70000;
        let mut bytes = vec![0xc2];
        bytes.extend_from_slice(&v.to_le_bytes());
        let mut c = Cursor::new(&bytes);
        assert_eq!(read_string(&mut c).unwrap(), b"70000");
    }

    #[test]
    fn plain_length_prefixed_string() {
        let mut bytes = vec![0x05];
        bytes.extend_from_slice(b"hello");
        let mut c = Cursor::new(&bytes);
        assert_eq!(read_string(&mut c).unwrap(), b"hello");
    }

    #[test]
    fn lzf_compressed_string_round_trips() {
        // ctrl=0 (1 literal byte 'a'), then a backref reproducing it 9
        // more times — same construction as lzf::tests::back_reference_repeats_a_run.
        let ctrl = 7u8 << 5;
        let mut bytes = vec![0xc3]; // special marker, code 3 = LZF
        bytes.push(0x05); // clen = 5 (the 5 bytes that follow)
        bytes.push(0x0a); // ulen = 10
        bytes.push(0);
        bytes.push(b'a');
        bytes.push(ctrl);
        bytes.push(0);
        bytes.push(0);
        let mut c = Cursor::new(&bytes);
        assert_eq!(read_string(&mut c).unwrap(), b"aaaaaaaaaa");
    }

    #[test]
    fn unknown_special_encoding_is_clean_error() {
        let mut c = Cursor::new(&[0xc7]); // code 7, not defined
        assert!(read_string(&mut c).is_err());
    }

    #[test]
    fn truncated_string_is_clean_error_not_panic() {
        let mut c = Cursor::new(&[0x05, b'h', b'i']); // claims 5 bytes, has 2
        assert!(read_string(&mut c).is_err());
    }
}
