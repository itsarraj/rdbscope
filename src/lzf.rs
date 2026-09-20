//! LZF decompression — the compression `rdbcompression yes` (the default)
//! uses for individual string values, including listpack/ziplist blobs
//! embedded inside quicklist nodes. This is a straight port of the
//! `lzf_d.c` decode loop (the format has no separate spec document; the
//! reference decoder *is* the spec), with every array access bounds
//! checked so a truncated or corrupted compressed blob is a clean error
//! instead of a panic or an out-of-bounds read.

use anyhow::{bail, Result};

pub fn decompress(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected_len);
    let mut ip = 0usize;

    while ip < input.len() {
        let ctrl = input[ip] as usize;
        ip += 1;

        if ctrl < 32 {
            // Literal run: ctrl+1 raw bytes follow.
            let len = ctrl + 1;
            let end = ip
                .checked_add(len)
                .filter(|&e| e <= input.len())
                .ok_or_else(|| anyhow::anyhow!("LZF literal run overruns input"))?;
            out.extend_from_slice(&input[ip..end]);
            ip = end;
        } else {
            // Back reference: top 3 bits (after the initial 2-bit marker)
            // give a length, low 5 bits are the high bits of a 13-bit
            // back-offset; a length nibble of 7 means "read one more
            // length byte" (this is how LZF encodes runs longer than 8).
            let mut len = ctrl >> 5;
            if len == 7 {
                if ip >= input.len() {
                    bail!("LZF back-reference length byte truncated");
                }
                len += input[ip] as usize;
                ip += 1;
            }
            if ip >= input.len() {
                bail!("LZF back-reference offset byte truncated");
            }
            let ref_low = input[ip] as usize;
            ip += 1;
            let offset = ((ctrl & 0x1f) << 8) | ref_low;
            let offset = offset + 1;

            if offset > out.len() {
                bail!(
                    "LZF back-reference offset {} exceeds decoded output so far ({} bytes)",
                    offset,
                    out.len()
                );
            }

            // length+2 bytes copied from `offset` bytes back, one at a
            // time — deliberately not `extend_from_slice` from a computed
            // range, since overlapping copies (offset < length) are legal
            // and exactly how LZF encodes runs of a repeated byte.
            let copy_len = len + 2;
            let src_start = out.len() - offset;
            for src in src_start..src_start + copy_len {
                let b = out[src];
                out.push(b);
            }
        }
    }

    if out.len() != expected_len {
        bail!(
            "LZF decompressed to {} bytes, expected {}",
            out.len(),
            expected_len
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_literal_run() {
        // ctrl=4 means "5 literal bytes follow" (ctrl+1).
        let input = [4u8, b'h', b'e', b'l', b'l', b'o'];
        let out = decompress(&input, 5).unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn back_reference_repeats_a_run() {
        // Encodes "aaaaaaaaaa" (10 'a's): one literal 'a', then a back
        // reference of length 9 at offset 1 (ctrl>>5==7 path, since 9-2=7
        // needs the extra length byte: len=7, +1 extra-length byte = 8,
        // +2 = 10 total copied... constructed by hand and cross-checked
        // against a real LZF blob from a live Redis dump in
        // rdb_integration tests, not just this synthetic case).
        // literal 'a' (ctrl=0, 1 byte)
        // backref: ctrl = (7<<5) | (offset_high & 0x1f); len=7 => extra length byte
        // want total copy_len = len_total + 2 = 9  => len_total = 7, so ctrl len nibble=7, extra byte = 0
        // offset = 1 -> offset-1 = 0 -> low byte = 0, high bits in ctrl low5 = 0
        let ctrl = 7u8 << 5; // 0b11100000
        let input = [0u8, b'a', ctrl, 0u8, 0u8];
        let out = decompress(&input, 10).unwrap();
        assert_eq!(out, b"aaaaaaaaaa");
    }

    #[test]
    fn truncated_backref_offset_is_clean_error() {
        // A backref ctrl (>=32: length nibble 1, the minimum a real
        // backref can encode — 0-31 is definitionally the literal range)
        // with no offset byte following it at all.
        let ctrl = 1u8 << 5;
        let input = [ctrl];
        assert!(decompress(&input, 3).is_err());
    }

    #[test]
    fn offset_beyond_output_is_clean_error_not_panic() {
        // A backref (length nibble 1, offset-high bits 0) with its offset
        // byte present but pointing before any output exists yet — there's
        // nothing to copy from.
        let ctrl = 1u8 << 5;
        let input = [ctrl, 0u8];
        assert!(decompress(&input, 3).is_err());
    }

    #[test]
    fn length_mismatch_is_reported() {
        let input = [1u8, b'h', b'i'];
        assert!(decompress(&input, 99).is_err());
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let out = decompress(&[], 0).unwrap();
        assert!(out.is_empty());
    }
}
