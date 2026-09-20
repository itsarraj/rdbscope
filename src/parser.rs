//! Walks a full RDB file's opcode stream: the 9-byte magic/version header,
//! then a sequence of `AUX`/`SELECTDB`/`RESIZEDB`/`EXPIRETIME(_MS)`
//! opcodes and key-value pairs, until `EOF` + the trailing CRC64.
//!
//! Type coverage is deliberately scoped to what this tool's own real
//! `tests/fixtures/*.rdb` files (genuine dumps from a real `redis-server`
//! and a real Valkey instance) actually contain, decoded by hand against
//! a hex dump of those exact files rather than guessed from the spec
//! alone — see the README for the full list and what's out of scope.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};

use crate::cursor::Cursor;
use crate::length::{read_plain_length, read_string};
use crate::listpack;

const OP_SLOT_INFO: u8 = 0xf5;
const OP_IDLE: u8 = 0xf8;
const OP_FREQ: u8 = 0xf9;
const OP_AUX: u8 = 0xfa;
const OP_RESIZEDB: u8 = 0xfb;
const OP_EXPIRETIME_MS: u8 = 0xfc;
const OP_EXPIRETIME: u8 = 0xfd;
const OP_SELECTDB: u8 = 0xfe;
const OP_EOF: u8 = 0xff;

pub const TYPE_STRING: u8 = 0;
pub const TYPE_SET: u8 = 2;
pub const TYPE_HASH: u8 = 4;
pub const TYPE_ZSET_2: u8 = 5;
pub const TYPE_SET_INTSET: u8 = 11;
pub const TYPE_HASH_LISTPACK: u8 = 16;
pub const TYPE_ZSET_LISTPACK: u8 = 17;
pub const TYPE_LIST_QUICKLIST_2: u8 = 18;
pub const TYPE_SET_LISTPACK: u8 = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct KeyEntry {
    pub name: String,
    pub db: u64,
    pub type_name: &'static str,
    /// Bytes the value occupied in the RDB's own encoding — a real,
    /// honestly-labeled figure, not an estimate of Redis's actual
    /// in-memory object overhead (which is a materially different, much
    /// larger number for small collections; see README).
    pub encoded_size: usize,
    pub element_count: Option<u64>,
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub redis_version: Option<String>,
    pub keys: Vec<KeyEntry>,
}

impl Summary {
    pub fn count_by_type(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for key in &self.keys {
            *counts.entry(key.type_name).or_insert(0) += 1;
        }
        counts
    }

    pub fn encoded_bytes_by_type(&self) -> BTreeMap<&'static str, usize> {
        let mut sizes = BTreeMap::new();
        for key in &self.keys {
            *sizes.entry(key.type_name).or_insert(0) += key.encoded_size;
        }
        sizes
    }
}

fn type_name(type_byte: u8) -> Result<&'static str> {
    Ok(match type_byte {
        TYPE_STRING => "string",
        TYPE_SET | TYPE_SET_INTSET | TYPE_SET_LISTPACK => "set",
        TYPE_HASH | TYPE_HASH_LISTPACK => "hash",
        TYPE_ZSET_2 | TYPE_ZSET_LISTPACK => "zset",
        TYPE_LIST_QUICKLIST_2 => "list",
        other => bail!(
            "unsupported RDB value type 0x{other:02x} — see README for the type opcodes this build understands"
        ),
    })
}

/// Reads one key's value for the given type byte, returning its element
/// count where that concept applies (field/member/entry count — `None`
/// for a plain string) and how many bytes of `data` the value consumed.
fn read_value(c: &mut Cursor, type_byte: u8) -> Result<Option<u64>> {
    match type_byte {
        TYPE_STRING => {
            read_string(c).context("reading string value")?;
            Ok(None)
        }
        TYPE_HASH => {
            let count = read_plain_length(c).context("reading hash field count")?;
            for _ in 0..count {
                read_string(c).context("reading hash field")?;
                read_string(c).context("reading hash value")?;
            }
            Ok(Some(count))
        }
        TYPE_SET => {
            let count = read_plain_length(c).context("reading set member count")?;
            for _ in 0..count {
                read_string(c).context("reading set member")?;
            }
            Ok(Some(count))
        }
        TYPE_ZSET_2 => {
            let count = read_plain_length(c).context("reading zset member count")?;
            for _ in 0..count {
                read_string(c).context("reading zset member")?;
                c.read_f64_le().context("reading zset score")?;
            }
            Ok(Some(count))
        }
        TYPE_SET_INTSET => {
            let blob = read_string(c).context("reading intset blob")?;
            if blob.len() < 8 {
                bail!("intset blob too short ({} bytes)", blob.len());
            }
            let length = u32::from_le_bytes([blob[4], blob[5], blob[6], blob[7]]);
            Ok(Some(length as u64))
        }
        TYPE_HASH_LISTPACK => {
            let blob = read_string(c).context("reading hash listpack blob")?;
            let elements = listpack::decode_all(&blob).context("decoding hash listpack")?;
            Ok(Some((elements.len() / 2) as u64))
        }
        TYPE_ZSET_LISTPACK => {
            let blob = read_string(c).context("reading zset listpack blob")?;
            let elements = listpack::decode_all(&blob).context("decoding zset listpack")?;
            Ok(Some((elements.len() / 2) as u64))
        }
        TYPE_SET_LISTPACK => {
            let blob = read_string(c).context("reading set listpack blob")?;
            let elements = listpack::decode_all(&blob).context("decoding set listpack")?;
            Ok(Some(elements.len() as u64))
        }
        TYPE_LIST_QUICKLIST_2 => {
            let node_count = read_plain_length(c).context("reading quicklist node count")?;
            let mut total = 0u64;
            for _ in 0..node_count {
                let container =
                    read_plain_length(c).context("reading quicklist node container type")?;
                let blob = read_string(c).context("reading quicklist node blob")?;
                if container == 2 {
                    // PACKED: the blob is itself a listpack of one or more elements.
                    let elements =
                        listpack::decode_all(&blob).context("decoding quicklist listpack node")?;
                    total += elements.len() as u64;
                } else {
                    // PLAIN: the blob is a single element's raw bytes.
                    total += 1;
                }
            }
            Ok(Some(total))
        }
        other => bail!("unsupported RDB value type 0x{other:02x}"),
    }
}

pub fn parse(data: &[u8]) -> Result<Summary> {
    if data.len() < 9 {
        bail!("file too short to be an RDB dump ({} bytes)", data.len());
    }
    let magic = &data[0..9];
    if !magic.starts_with(b"REDIS") && !magic.starts_with(b"VALKEY") {
        bail!(
            "not an RDB file: header {:?} doesn't start with REDIS/VALKEY magic",
            String::from_utf8_lossy(magic)
        );
    }

    let mut c = Cursor::new(&data[9..]);
    let mut summary = Summary::default();
    let mut current_db = 0u64;
    let mut pending_expiry: Option<u64> = None;

    loop {
        let opcode = c.read_u8().context("reading next opcode/type byte")?;
        match opcode {
            OP_EOF => break,
            OP_SELECTDB => {
                current_db = read_plain_length(&mut c).context("reading SELECTDB db number")?;
            }
            OP_RESIZEDB => {
                read_plain_length(&mut c).context("reading RESIZEDB hash size hint")?;
                read_plain_length(&mut c).context("reading RESIZEDB expires size hint")?;
            }
            OP_AUX => {
                let key = read_string(&mut c).context("reading AUX key")?;
                let value = read_string(&mut c).context("reading AUX value")?;
                if key == b"redis-ver" || key == b"valkey-ver" {
                    summary.redis_version = Some(String::from_utf8_lossy(&value).into_owned());
                }
            }
            OP_EXPIRETIME_MS => {
                pending_expiry = Some(c.read_u64_le().context("reading EXPIRETIME_MS")?);
            }
            OP_EXPIRETIME => {
                let secs = c.read_u32_le().context("reading EXPIRETIME")?;
                pending_expiry = Some(secs as u64 * 1000);
            }
            OP_IDLE => {
                read_plain_length(&mut c).context("reading IDLE opcode value")?;
            }
            OP_FREQ => {
                c.read_u8().context("reading FREQ opcode value")?;
            }
            OP_SLOT_INFO => {
                bail!("cluster SLOT_INFO opcode (0xf5) isn't supported — see README");
            }
            type_byte => {
                let start = c.position();
                let key_bytes = read_string(&mut c).context("reading key name")?;
                let name = String::from_utf8_lossy(&key_bytes).into_owned();
                let name_tn = type_name(type_byte)?;
                let element_count = read_value(&mut c, type_byte)
                    .with_context(|| format!("reading value for key {name:?}"))?;
                let encoded_size = c.position() - start;
                summary.keys.push(KeyEntry {
                    name,
                    db: current_db,
                    type_name: name_tn,
                    encoded_size,
                    element_count,
                    expires_at_ms: pending_expiry.take(),
                });
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_file_too_short_to_have_a_header() {
        assert!(parse(b"short").is_err());
    }

    #[test]
    fn rejects_a_file_with_the_wrong_magic() {
        assert!(parse(b"NOTREDIS0").is_err());
    }

    /// A minimal but complete synthetic RDB: header, AUX redis-ver, a
    /// SELECTDB/RESIZEDB pair, one STRING key, EOF + 8 zero checksum
    /// bytes — hand-built to test the opcode-walking skeleton in
    /// isolation from the real fixture files (those are covered by
    /// `tests/rdb_integration.rs`).
    fn minimal_rdb() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(0xfa); // AUX
        v.push(9);
        v.extend_from_slice(b"redis-ver");
        v.push(5);
        v.extend_from_slice(b"7.4.0");
        v.push(0xfe); // SELECTDB
        v.push(0x00);
        v.push(0xfb); // RESIZEDB
        v.push(0x01);
        v.push(0x00);
        v.push(TYPE_STRING); // key-value pair
        v.push(3);
        v.extend_from_slice(b"foo");
        v.push(3);
        v.extend_from_slice(b"bar");
        v.push(0xff); // EOF
        v.extend_from_slice(&[0u8; 8]);
        v
    }

    #[test]
    fn parses_aux_redis_version() {
        let summary = parse(&minimal_rdb()).unwrap();
        assert_eq!(summary.redis_version.as_deref(), Some("7.4.0"));
    }

    #[test]
    fn parses_one_string_key() {
        let summary = parse(&minimal_rdb()).unwrap();
        assert_eq!(summary.keys.len(), 1);
        assert_eq!(summary.keys[0].name, "foo");
        assert_eq!(summary.keys[0].type_name, "string");
        assert_eq!(summary.keys[0].element_count, None);
    }

    #[test]
    fn expiretime_ms_attaches_to_the_following_key() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(0xfe);
        v.push(0x00);
        v.push(0xfc); // EXPIRETIME_MS
        v.extend_from_slice(&1_700_000_000_000u64.to_le_bytes());
        v.push(TYPE_STRING);
        v.push(1);
        v.extend_from_slice(b"k");
        v.push(1);
        v.extend_from_slice(b"v");
        v.push(0xff);
        v.extend_from_slice(&[0u8; 8]);

        let summary = parse(&v).unwrap();
        assert_eq!(summary.keys[0].expires_at_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn a_key_with_no_expiry_opcode_has_none() {
        let summary = parse(&minimal_rdb()).unwrap();
        assert_eq!(summary.keys[0].expires_at_ms, None);
    }

    #[test]
    fn expiry_does_not_leak_onto_a_later_unrelated_key() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(0xfe);
        v.push(0x00);
        v.push(0xfc);
        v.extend_from_slice(&1_700_000_000_000u64.to_le_bytes());
        v.push(TYPE_STRING);
        v.push(1);
        v.extend_from_slice(b"a");
        v.push(1);
        v.extend_from_slice(b"1");
        v.push(TYPE_STRING); // no expiry opcode before this one
        v.push(1);
        v.extend_from_slice(b"b");
        v.push(1);
        v.extend_from_slice(b"2");
        v.push(0xff);
        v.extend_from_slice(&[0u8; 8]);

        let summary = parse(&v).unwrap();
        assert_eq!(summary.keys[0].expires_at_ms, Some(1_700_000_000_000));
        assert_eq!(summary.keys[1].expires_at_ms, None);
    }

    #[test]
    fn selectdb_is_recorded_against_each_key() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(0xfe);
        v.push(0x05); // db 5
        v.push(TYPE_STRING);
        v.push(1);
        v.extend_from_slice(b"a");
        v.push(1);
        v.extend_from_slice(b"1");
        v.push(0xff);
        v.extend_from_slice(&[0u8; 8]);

        let summary = parse(&v).unwrap();
        assert_eq!(summary.keys[0].db, 5);
    }

    #[test]
    fn unsupported_type_byte_is_a_clean_error_not_a_panic() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(200); // not a type this build understands
        v.push(1);
        v.extend_from_slice(b"a");
        let err = parse(&v).unwrap_err();
        assert!(err.to_string().contains("0xc8") || format!("{err:#}").contains("0xc8"));
    }

    #[test]
    fn parses_old_style_plain_set_type() {
        let mut v = Vec::new();
        v.extend_from_slice(b"REDIS0011");
        v.push(0xfe);
        v.push(0x00);
        v.push(TYPE_SET);
        v.push(1);
        v.extend_from_slice(b"s");
        v.push(2); // 2 members
        v.push(1);
        v.extend_from_slice(b"a");
        v.push(1);
        v.extend_from_slice(b"b");
        v.push(0xff);
        v.extend_from_slice(&[0u8; 8]);

        let summary = parse(&v).unwrap();
        assert_eq!(summary.keys[0].type_name, "set");
        assert_eq!(summary.keys[0].element_count, Some(2));
    }

    #[test]
    fn count_by_type_and_encoded_bytes_by_type_aggregate_correctly() {
        let summary = parse(&minimal_rdb()).unwrap();
        assert_eq!(summary.count_by_type().get("string"), Some(&1));
        assert!(summary.encoded_bytes_by_type().get("string").unwrap() > &0);
    }
}
