//! Listpack decoding — the compact single-blob encoding modern Redis/
//! Valkey (7.x+) use for small hashes, sorted sets, sets, and each node
//! of a quicklist. Verified against this workspace's own real dumps: a
//! 5-element list, a 3-field hash, and a 3-member sorted set all decode
//! correctly (see `tests/rdb_integration.rs`), including the specific
//! entry encoding (`10xxxxxx` 6-bit string) a real `RPUSH a b c d e`
//! produces.
//!
//! Layout: `<total-bytes:4LE><num-elements:2LE><entry>...<0xFF>`. Each
//! entry is `<encoding+data><backlen>`; `backlen` exists so the format
//! can be walked *backward* (from the tail) — since this crate only ever
//! walks forward, it's skipped by recomputing its byte width from the
//! entry length rather than decoding its value.

use crate::cursor::Cursor;
use anyhow::{bail, Result};

const LP_EOF: u8 = 0xff;

pub fn decode_all(blob: &[u8]) -> Result<Vec<Vec<u8>>> {
    if blob.len() < 7 {
        bail!("listpack blob too short ({} bytes)", blob.len());
    }
    let mut c = Cursor::new(blob);
    let total_bytes = c.read_u32_le()?;
    let _num_elements = c.read_u16_le()?; // 0xFFFF sentinel for "count unknown"; scan to EOF regardless
    if total_bytes as usize != blob.len() {
        bail!(
            "listpack header claims {} total bytes, blob is {}",
            total_bytes,
            blob.len()
        );
    }

    let mut out = Vec::new();
    loop {
        if c.peek_u8()? == LP_EOF {
            break;
        }
        let (value, entry_len) = read_entry(&mut c)?;
        out.push(value);
        let backlen_width = backlen_byte_width(entry_len);
        c.read_bytes(backlen_width)?;
    }
    Ok(out)
}

fn backlen_byte_width(entry_len: usize) -> usize {
    if entry_len <= 127 {
        1
    } else if entry_len < 16384 {
        2
    } else if entry_len < 2_097_152 {
        3
    } else if entry_len < 268_435_456 {
        4
    } else {
        5
    }
}

/// Returns the decoded value (strings verbatim, integers rendered as
/// ASCII decimal — same convention `length::read_string`'s int-as-string
/// special encoding uses, so callers treat every listpack element
/// uniformly) and the entry's encoded byte length (encoding + data,
/// excluding the trailing backlen), needed to size that backlen.
fn read_entry(c: &mut Cursor) -> Result<(Vec<u8>, usize)> {
    let start = c.position();
    let b = c.read_u8()?;

    let value = if b & 0x80 == 0 {
        // 0xxxxxxx: 7-bit unsigned int
        (b & 0x7f).to_string().into_bytes()
    } else if b & 0xc0 == 0x80 {
        // 10xxxxxx: 6-bit-length string
        let len = (b & 0x3f) as usize;
        c.read_bytes(len)?.to_vec()
    } else if b & 0xe0 == 0xc0 {
        // 110xxxxx yyyyyyyy: 13-bit signed int
        let hi = (b & 0x1f) as u16;
        let lo = c.read_u8()? as u16;
        let raw = (hi << 8) | lo;
        let value = if raw & 0x1000 != 0 {
            (raw as i32) - 0x2000
        } else {
            raw as i32
        };
        value.to_string().into_bytes()
    } else if b & 0xf0 == 0xe0 {
        // 1110xxxx yyyyyyyy: 12-bit-length string
        let hi = (b & 0x0f) as usize;
        let lo = c.read_u8()? as usize;
        let len = (hi << 8) | lo;
        c.read_bytes(len)?.to_vec()
    } else if b == 0xf0 {
        // 32-bit-length string
        let len = c.read_u32_le()? as usize;
        c.read_bytes(len)?.to_vec()
    } else if b == 0xf1 {
        let v = c.read_bytes(2)?;
        i16::from_le_bytes([v[0], v[1]]).to_string().into_bytes()
    } else if b == 0xf2 {
        let v = c.read_bytes(3)?;
        let mut raw = (v[0] as i32) | ((v[1] as i32) << 8) | ((v[2] as i32) << 16);
        if raw & 0x0080_0000 != 0 {
            raw -= 0x0100_0000;
        }
        raw.to_string().into_bytes()
    } else if b == 0xf3 {
        let v = c.read_bytes(4)?;
        i32::from_le_bytes([v[0], v[1], v[2], v[3]])
            .to_string()
            .into_bytes()
    } else if b == 0xf4 {
        let v = c.read_bytes(8)?;
        i64::from_le_bytes([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]])
            .to_string()
            .into_bytes()
    } else {
        bail!("unknown listpack entry encoding byte 0x{:02x}", b);
    };

    let entry_len = c.position() - start;
    Ok((value, entry_len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact 22-byte listpack blob a real `RPUSH mylist a b c d e`
    /// produced (extracted from `tests/fixtures/valkey_dump.rdb`), used
    /// directly rather than reconstructed by hand.
    const REAL_MYLIST_LISTPACK: [u8; 22] = [
        0x16, 0x00, 0x00, 0x00, 0x05, 0x00, 0x81, b'a', 0x02, 0x81, b'b', 0x02, 0x81, b'c', 0x02,
        0x81, b'd', 0x02, 0x81, b'e', 0x02, 0xff,
    ];

    #[test]
    fn decodes_real_5_element_list() {
        let out = decode_all(&REAL_MYLIST_LISTPACK).unwrap();
        assert_eq!(
            out,
            vec![
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"d".to_vec(),
                b"e".to_vec()
            ]
        );
    }

    #[test]
    fn seven_bit_uint_entry() {
        // header(6) + [7-bit-uint 5][backlen 1] + EOF
        let mut blob = vec![9, 0, 0, 0, 1, 0];
        blob.push(5u8); // 0x05, top bit clear -> 7-bit uint value 5
        blob.push(1); // backlen = entry_len(1)
        blob.push(LP_EOF);
        let out = decode_all(&blob).unwrap();
        assert_eq!(out, vec![b"5".to_vec()]);
    }

    #[test]
    fn thirteen_bit_negative_int() {
        // Encode -100 as a 13-bit int: raw = -100 + 0x2000 = 0x1F9C
        let raw: u16 = ((-100i32 + 0x2000) & 0x1fff) as u16;
        let b0 = 0xc0 | ((raw >> 8) as u8);
        let b1 = (raw & 0xff) as u8;
        let mut blob = vec![10, 0, 0, 0, 1, 0, b0, b1];
        blob.push(2); // backlen: entry_len = 2
        blob.push(LP_EOF);
        let out = decode_all(&blob).unwrap();
        assert_eq!(out, vec![b"-100".to_vec()]);
    }

    #[test]
    fn header_length_mismatch_is_clean_error() {
        let blob = [99u8, 0, 0, 0, 0, 0, LP_EOF];
        assert!(decode_all(&blob).is_err());
    }

    #[test]
    fn truncated_blob_is_clean_error_not_panic() {
        let blob = [7u8, 0, 0, 0, 1, 0]; // claims an element but no entry/EOF bytes follow
        assert!(decode_all(&blob).is_err());
    }

    #[test]
    fn unknown_entry_encoding_is_clean_error() {
        // 0xf5..0xfe are not defined listpack encodings.
        let blob = [8u8, 0, 0, 0, 1, 0, 0xf5, LP_EOF];
        assert!(decode_all(&blob).is_err());
    }

    #[test]
    fn backlen_width_thresholds() {
        assert_eq!(backlen_byte_width(1), 1);
        assert_eq!(backlen_byte_width(127), 1);
        assert_eq!(backlen_byte_width(128), 2);
        assert_eq!(backlen_byte_width(16383), 2);
        assert_eq!(backlen_byte_width(16384), 3);
    }
}
