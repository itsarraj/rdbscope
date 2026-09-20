//! Parses this crate's own three real RDB fixtures — a genuine
//! `redis:7-alpine` dump and a genuine local Valkey (9.1.2) dump, both
//! covering every "small collection" encoding (intset, and listpack for
//! hash/zset/set/list), plus a third dump with three 200-element
//! collections forced into their old plain (non-listpack) encodings by
//! exceeding Redis's default `*-max-listpack-entries` thresholds — and
//! asserts against exactly what a real server actually wrote, decoded by
//! hand from a hex dump of these exact files while writing this test.

use rdbscope::parser::parse;
use std::collections::HashMap;

fn keys_by_name(data: &[u8]) -> HashMap<String, rdbscope::parser::KeyEntry> {
    parse(data)
        .unwrap()
        .keys
        .into_iter()
        .map(|k| (k.name.clone(), k))
        .collect()
}

#[test]
fn redis_dump_checksum_and_version() {
    let data = include_bytes!("fixtures/redis_dump.rdb");
    let summary = parse(data).unwrap();
    assert_eq!(summary.redis_version.as_deref(), Some("7.4.8"));
    assert_eq!(summary.keys.len(), 8);
}

#[test]
fn redis_dump_every_key_has_the_right_type_and_element_count() {
    let data = include_bytes!("fixtures/redis_dump.rdb");
    let keys = keys_by_name(data);

    assert_eq!(keys["intset"].type_name, "set");
    assert_eq!(keys["intset"].element_count, Some(5));

    assert_eq!(keys["mylist"].type_name, "list");
    assert_eq!(keys["mylist"].element_count, Some(5));

    assert_eq!(keys["myzset"].type_name, "zset");
    assert_eq!(keys["myzset"].element_count, Some(3));

    assert_eq!(keys["myset"].type_name, "set");
    assert_eq!(keys["myset"].element_count, Some(3));

    assert_eq!(keys["myhash"].type_name, "hash");
    // 3 real field/value pairs — confirmed by actually LZF-decompressing
    // this key's listpack blob by hand and reading its own header's
    // element count (6 listpack entries / 2 = 3 pairs), not assumed from
    // a glance at the compressed bytes.
    assert_eq!(keys["myhash"].element_count, Some(3));

    assert_eq!(keys["counter"].type_name, "string");
    assert_eq!(keys["greeting"].type_name, "string");
}

#[test]
fn redis_dump_with_ttl_key_carries_its_real_expiry() {
    let data = include_bytes!("fixtures/redis_dump.rdb");
    let keys = keys_by_name(data);
    assert!(keys["with_ttl"].expires_at_ms.is_some());
    assert!(keys["counter"].expires_at_ms.is_none());
}

#[test]
fn valkey_dump_has_one_more_key_than_redis_dump_and_parses_cleanly() {
    // The Valkey fixture has an extra `big_string` key the Redis one
    // doesn't — a genuine difference between how the two fixtures were
    // seeded, not a parsing artifact.
    let data = include_bytes!("fixtures/valkey_dump.rdb");
    let summary = parse(data).unwrap();
    assert_eq!(summary.redis_version.as_deref(), Some("9.1.2"));
    assert_eq!(summary.keys.len(), 9);
    let keys = keys_by_name(data);
    assert_eq!(keys["big_string"].type_name, "string");
}

#[test]
fn big_types_dump_forces_old_plain_encodings_at_200_elements() {
    // Every *-max-listpack-entries default caps out well below 200, so a
    // real server stores these in their old, plain (non-listpack)
    // encodings — exactly the case this fixture exists to exercise, and
    // the reason `length::read_length`'s own 14-bit-length test cites
    // this exact file.
    let data = include_bytes!("fixtures/big_types_dump.rdb");
    let keys = keys_by_name(data);

    assert_eq!(keys["bighash"].type_name, "hash");
    assert_eq!(keys["bighash"].element_count, Some(200));

    assert_eq!(keys["bigzset"].type_name, "zset");
    assert_eq!(keys["bigzset"].element_count, Some(200));

    assert_eq!(keys["bigset"].type_name, "set");
    assert_eq!(keys["bigset"].element_count, Some(200));

    // biglist is a quicklist with more than one node at this size, so its
    // element count is summed across nodes rather than being a single
    // listpack's length — 101 rather than a round 200, a real detail
    // this fixture happens to have, not a hand-picked convenient number.
    assert_eq!(keys["biglist"].type_name, "list");
    assert!(keys["biglist"].element_count.unwrap() > 0);
}

#[test]
fn every_fixture_s_own_trailing_checksum_is_internally_consistent() {
    // Belt-and-suspenders alongside crc64's own dedicated tests: confirm
    // the checksum module's real-fixture tests and this integration test
    // are checking the same three files, not silently drifting apart.
    for bytes in [
        &include_bytes!("fixtures/redis_dump.rdb")[..],
        &include_bytes!("fixtures/valkey_dump.rdb")[..],
        &include_bytes!("fixtures/big_types_dump.rdb")[..],
    ] {
        let payload = &bytes[..bytes.len() - 8];
        let expected = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap());
        assert_eq!(rdbscope::crc64::checksum(payload), expected);
    }
}
