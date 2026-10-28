//! Small deterministic durable KV experiment, NOT a mainnet performance benchmark.
use std::time::Instant;

use anyhow::{ensure, Result};
use redb::{Database, Durability, ReadableDatabase, ReadableTableMetadata, TableDefinition};
use rusqlite::{params, Connection};
use serde_json::json;

const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("records");
const BATCHES: u64 = 100;
const PER_BATCH: u64 = 256;

fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let payload = vec![42u8; 96];
    let sql_path = dir.path().join("sqlite.db");
    let redb_path = dir.path().join("redb.db");
    let mut sql = Connection::open(&sql_path)?;
    sql.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA cache_size=-8192; CREATE TABLE records(k INTEGER PRIMARY KEY, v BLOB NOT NULL);")?;
    let started = Instant::now();
    for batch in 0..BATCHES {
        let tx = sql.transaction()?;
        for n in 0..PER_BATCH {
            tx.execute(
                "INSERT INTO records VALUES(?1,?2)",
                params![(batch * PER_BATCH + n) as i64, payload],
            )?;
        }
        if batch > 0 {
            for n in 0..64 {
                tx.execute(
                    "DELETE FROM records WHERE k=?1",
                    [((batch - 1) * PER_BATCH + n) as i64],
                )?;
            }
        }
        tx.commit()?;
    }
    let sql_write = started.elapsed().as_millis();
    let started = Instant::now();
    for n in 0..1000u64 {
        let key = (n * 997) % (BATCHES * PER_BATCH);
        let _: Option<Vec<u8>> = sql
            .query_row("SELECT v FROM records WHERE k=?1", [key as i64], |r| {
                r.get(0)
            })
            .optional()?;
    }
    let sql_read = started.elapsed().as_micros();
    let sql_count: i64 = sql.query_row("SELECT count(*) FROM records", [], |r| r.get(0))?;
    drop(sql);

    let db = Database::builder()
        .set_cache_size(8 * 1024 * 1024)
        .create(&redb_path)?;
    let started = Instant::now();
    for batch in 0..BATCHES {
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate)?;
        {
            let mut table = tx.open_table(TABLE)?;
            for n in 0..PER_BATCH {
                table.insert(batch * PER_BATCH + n, payload.as_slice())?;
            }
            if batch > 0 {
                for n in 0..64 {
                    table.remove((batch - 1) * PER_BATCH + n)?;
                }
            }
        }
        tx.commit()?;
    }
    let redb_write = started.elapsed().as_millis();
    let started = Instant::now();
    for n in 0..1000u64 {
        let tx = db.begin_read()?;
        let table = tx.open_table(TABLE)?;
        let _ = table.get((n * 997) % (BATCHES * PER_BATCH))?;
    }
    let redb_read = started.elapsed().as_micros();
    let count = db.begin_read()?.open_table(TABLE)?.len()?;
    ensure!(count == sql_count as u64, "backend row counts disagree");
    drop(db);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "scope":"synthetic KV; one run, warm reads, no crash or RSS comparison",
            "sqlite_version":rusqlite::version(),
            "batches":BATCHES,"puts_per_batch":PER_BATCH,"deletes_per_later_batch":64,"rows":count,
            "sqlite":{"write_ms":sql_write,"read_1000_us":sql_read,"bytes":std::fs::metadata(sql_path)?.len(),"durability":"DELETE + EXTRA"},
            "redb":{"write_ms":redb_write,"read_1000_us":redb_read,"bytes":std::fs::metadata(redb_path)?.len(),"durability":"Immediate"}
        }))?
    );
    Ok(())
}

use rusqlite::OptionalExtension;
