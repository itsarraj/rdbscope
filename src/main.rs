use std::fs;
use std::path::PathBuf;

use clap::Parser;
use rdbscope::crc64;
use rdbscope::parser;

#[derive(Parser)]
#[command(
    name = "rdbscope",
    about = "Parses a Redis/Valkey RDB dump file offline and reports key counts, types, and encoded size per type"
)]
struct Cli {
    /// RDB dump file to inspect (e.g. dump.rdb).
    rdb_file: PathBuf,
    /// How many of the largest individual keys (by encoded size) to list.
    #[arg(short = 'n', long, default_value_t = 10)]
    top: usize,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let data = fs::read(&cli.rdb_file)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", cli.rdb_file.display()))?;

    if data.len() >= 8 {
        let payload = &data[..data.len() - 8];
        let trailer: [u8; 8] = data[data.len() - 8..].try_into().unwrap();
        let expected = u64::from_le_bytes(trailer);
        let actual = crc64::checksum(payload);
        if expected == 0 {
            println!("checksum: not present in this file (rdbchecksum disabled)\n");
        } else if actual == expected {
            println!("checksum: OK (0x{actual:016x})\n");
        } else {
            println!("checksum: MISMATCH — file may be truncated or corrupted (expected 0x{expected:016x}, computed 0x{actual:016x})\n");
        }
    }

    let summary = parser::parse(&data)?;
    if let Some(version) = &summary.redis_version {
        println!("server version: {version}");
    }
    println!("{} key(s) total\n", summary.keys.len());

    println!("by type:");
    let counts = summary.count_by_type();
    let sizes = summary.encoded_bytes_by_type();
    for (type_name, count) in &counts {
        let size = sizes.get(type_name).copied().unwrap_or(0);
        println!("  {type_name:<8} {count:>6} key(s)   {size:>10} bytes encoded");
    }

    let mut by_size: Vec<_> = summary.keys.iter().collect();
    by_size.sort_by_key(|k| std::cmp::Reverse(k.encoded_size));
    println!(
        "\ntop {} largest key(s) by encoded size:",
        cli.top.min(by_size.len())
    );
    for key in by_size.iter().take(cli.top) {
        let elements = key
            .element_count
            .map(|n| format!(", {n} element(s)"))
            .unwrap_or_default();
        let expiry = key
            .expires_at_ms
            .map(|ms| format!(", expires at ms={ms}"))
            .unwrap_or_default();
        println!(
            "  {:<20} {:<8} {:>8} bytes{}{}",
            key.name, key.type_name, key.encoded_size, elements, expiry
        );
    }

    Ok(())
}
