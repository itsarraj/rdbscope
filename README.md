# rdbscope

Parses a Redis/Valkey RDB dump file offline — key counts, types, and
encoded size per type — with no live server required. Redis itself ships
nothing like this; the closest things (`rdb` by sripathikrishnan, a
Python tool) are third-party and unmaintained. This is a from-scratch
Rust implementation of enough of the real RDB binary format to answer
"what's actually in this dump file" without loading it into a server.

## Usage

```bash
rdbscope dump.rdb
rdbscope dump.rdb -n 20     # show top 20 largest keys instead of the default 10
```

## What it reports

Server version (from the `redis-ver`/`valkey-ver` AUX field), a checksum
verdict (the file's own trailing CRC64, recomputed and compared — catches
a truncated or corrupted dump before you trust anything else in the
report), a count and total **encoded** size per type, and the largest
individual keys by encoded size. "Encoded size" is exactly that — how
many bytes the value occupies in the RDB's own on-disk encoding, not an
estimate of Redis's actual in-memory object overhead (which is a
materially larger, different number for small collections, since Redis's
in-memory hashtable/skiplist/quicklist structures carry real per-entry
pointer overhead that the RDB's compact encoding doesn't).

## Status: built and verified against three real dump files, not synthetic fixtures

Every fixture this crate's tests run against is a **genuine dump from a
real server** — `docker run redis:7-alpine` for one, this host's real
installed Valkey (9.1.2) for the other two — not hand-constructed bytes
pretending to be RDB format. `tests/fixtures/*.rdb` are committed as
binary test data.

- **48 unit tests** (`cargo test --lib`) across six modules:
  - `cursor` (5): bounds-checked reads, a failed read not partially
    consuming bytes, little-endian fixed-width fields, `f64` round-trip
    through its bit representation.
  - `length` (14): every one of RDB's length-encoding branches (6-bit,
    14-bit — **the exact real bytes** `0x40 0xc8` a real 200-field hash's
    field count produced, not a synthetic 200 — 32-bit, 64-bit), every
    "special" string encoding (int8/16/32, LZF-compressed) including a
    real `SET counter 42` int8-encoded value pulled straight from a real
    dump, and clean errors for truncated/malformed input rather than
    panics.
  - `lzf` (7): a pure literal run, a real back-reference run
    reconstructed against the actual algorithm real Redis's own `lzf_d.c`
    implements (cross-checked against a real LZF blob from a live dump —
    see `length`'s own LZF test), and — a genuine bug this pass caught by
    actually running these tests for the first time (the crate had never
    successfully compiled before, since `lib.rs`/`main.rs` didn't exist
    yet) — a clippy-flagged `(7u8 << 5) | 0` no-op cleaned up, and two
    test fixtures whose `ctrl` byte accidentally fell in LZF's *literal*
    range (0–31) instead of the *back-reference* range (32–255) they were
    meant to exercise, so they were silently testing the wrong code path
    while still (coincidentally) passing. Fixed by using a `ctrl` byte
    that's actually ≥32.
  - `listpack` (9): a real 5-element list's exact listpack bytes
    (extracted from `valkey_dump.rdb`) decoded correctly, every integer
    entry-encoding width (7-bit, 13-bit, 16/24/32/64-bit), and clean
    errors for a header/blob length mismatch or truncated data. **A
    second real pre-existing bug caught the same way**: the 13-bit signed
    integer test's own listpack header declared `9` total bytes while the
    hand-built blob was actually `10` bytes long — again, never actually
    run until this pass, since the crate didn't compile. Fixed to `10`.
  - `crc64` (5): matches Redis's real Jones-polynomial variant (reflected,
    zero-seeded) against the **real trailing checksum** of both
    `redis_dump.rdb` and `valkey_dump.rdb` — not a textbook CRC64 test
    vector, the actual bytes a real server wrote.
  - `parser` (10, new this pass): the opcode-walking skeleton — `AUX`
    version extraction, `SELECTDB` tracked per key, `EXPIRETIME_MS`
    correctly attaching to the *next* key and not leaking onto the one
    after, and a clean (not panicking) error for a type byte this build
    doesn't understand.
- **6 integration tests** (`tests/rdb_integration.rs`) against all three
  real fixture files together: every key in `redis_dump.rdb` and
  `valkey_dump.rdb` (a plain string, an int-encoded string, a `with_ttl`
  key's real expiry timestamp present while an untouched key's is
  correctly absent, an intset, a listpack-encoded list/hash/set/zset)
  matched against values decoded **by hand from a hex dump of the actual
  files** while writing these tests — including catching my own wrong
  assumption that `myhash` had 1 field/value pair from a too-quick glance
  at its (still-LZF-compressed) bytes, corrected to the real 3 pairs
  after actually decompressing it; and `big_types_dump.rdb`'s three
  200-element collections, each large enough to exceed Redis's default
  `*-max-listpack-entries` thresholds and force the old, plain
  (non-listpack) encodings — the reason this fixture exists at all, and
  the exact real data `length::read_length`'s own 14-bit-length test
  already cited before this integration test existed.
- **Live-verified against the actual compiled binary and all three real
  files**: ran `rdbscope` against each and got a correct checksum
  verdict, correct server version, correct per-type counts, and a
  correctly-ordered top-N list for every one — including catching a
  **third real gap** this way: `big_types_dump.rdb` initially errored
  with "unsupported RDB value type 0x02" on its first real run, because
  the old plain `SET` encoding (type 2 — a `bigset` key with 200 members,
  forced out of listpack encoding the same way `bighash`/`bigzset` are)
  hadn't been wired into the type dispatch at all. Added support and
  reran to a clean report.

**Not done / deliberately deferred**: modules (`RDB_TYPE_MODULE_2`) and
Redis Functions (`RDB_OPCODE_FUNCTION2`) — proprietary/complex payloads
with no simple generic size accounting; cluster-mode `SLOT_INFO`
(returns a clean, explicit error rather than silently mis-parsing);
`RDB_TYPE_STREAM_LISTPACKS*` (Redis Streams) — a materially more complex
nested encoding than any type here, a natural v2; and expired-key
filtering — every key is reported regardless of whether its `expires_at`
has already passed, since this tool reports what's *in the file*, not
what a server would still consider live right now.
