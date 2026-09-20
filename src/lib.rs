//! `rdbscope` — parses a Redis/Valkey RDB dump file offline and reports
//! key counts, types, and encoded size per type, no live server required.
//! See the README for exactly which RDB type opcodes are supported.

pub mod crc64;
pub mod cursor;
pub mod length;
pub mod listpack;
pub mod lzf;
pub mod parser;
