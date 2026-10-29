//! Isolated durable KV experiment, NOT a Bitcoin index or performance claim.
use anyhow::{ensure, Result};
use bitcoin::hashes::{sha256, Hash};
use electrum_lite::experimental_lsm::kv::Prototype;
use rusqlite::{params, Connection};
use std::time::Instant;

fn key(id: u64) -> Vec<u8> {
    let mut key = vec![b'd'];
    key.extend_from_slice(sha256::Hash::hash(&id.to_be_bytes()).as_byte_array());
    key
}

fn main() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("lite-lsm-")
        .tempdir_in(".")?;
    let mut sql = Connection::open(root.path().join("sqlite"))?;
    sql.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; CREATE TABLE kv(k BLOB PRIMARY KEY,v BLOB NOT NULL) WITHOUT ROWID; CREATE TABLE state(g INTEGER NOT NULL); INSERT INTO state VALUES(0);")?;
    let mut rocks = Prototype::create(&root.path().join("rocks"), b"synthetic-kv-v1")?;
    let mut sql_elapsed = std::time::Duration::ZERO;
    let mut rocks_elapsed = std::time::Duration::ZERO;
    for generation in 0..100u64 {
        let changes: Vec<_> = (0..512u64)
            .map(|n| {
                let id = generation * 512 + n;
                (key(id), Some(vec![(id % 251) as u8; 256]))
            })
            .collect();
        let start = Instant::now();
        let tx = sql.transaction()?;
        {
            let mut put = tx.prepare("INSERT INTO kv VALUES(?1,?2)")?;
            for (key, value) in &changes {
                put.execute(params![key, value])?;
            }
        }
        tx.execute("UPDATE state SET g=?1", [(generation + 1) as i64])?;
        tx.commit()?;
        sql_elapsed += start.elapsed();
        let start = Instant::now();
        rocks.commit(generation, &changes)?;
        rocks_elapsed += start.elapsed();
    }
    let start = Instant::now();
    rocks.settle()?;
    let settle = start.elapsed();
    let backup = root.path().join("backup");
    rocks.backup(&backup)?;
    let restored_path = root.path().join("restored");
    let restored = Prototype::restore(&backup, &restored_path, b"synthetic-kv-v1")?;
    ensure!(restored.generation()? == 100, "restore generation mismatch");
    for id in 0..51_200u64 {
        let expected: Vec<u8> =
            sql.query_row("SELECT v FROM kv WHERE k=?1", [key(id)], |row| row.get(0))?;
        ensure!(
            restored.get(&key(id))? == Some(expected),
            "backend mismatch"
        );
    }
    println!("synthetic only: SQLite DELETE/EXTRA {} ms; RocksDB WAL/sync {} ms; additional flush+compaction {} ms; 51200 restored rows verified", sql_elapsed.as_millis(), rocks_elapsed.as_millis(), settle.as_millis());
    drop(restored);
    drop(rocks);
    drop(sql);
    root.close()?;
    println!("PASS temporary databases and backups removed");
    Ok(())
}
