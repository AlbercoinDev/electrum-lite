//! Durable SQLite index. Hold the Archive's exclusive lock for this store's lifetime.
use std::{collections::HashSet, fs::File, path::Path};

use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::Hash,
    Block, BlockHash, OutPoint, Txid,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde::Serialize;

use crate::{
    archive::{Archive, Location},
    index::{Coin, Occurrence, ScriptView},
    validate_block, Chain,
};

pub const UNDO_WINDOW: u32 = 288;
const SCHEMA: i64 = 1;
const APPLICATION: i64 = 0x454C4954;

#[derive(Clone, Copy)]
pub struct Limits {
    pub transactions: usize,
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub tip: Option<(u32, BlockHash)>,
    pub transactions: i64,
    pub utxos: i64,
    pub history: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitStage {
    Prepared,
    ArchiveSynced,
    ReferencesStaged,
    Committed,
}

pub struct Store {
    conn: Connection,
    chain: Chain,
    limits: Limits,
}

impl Store {
    pub fn open(path: &Path, chain: Chain, limits: Limits) -> Result<Self> {
        ensure!(
            limits.transactions > 0 && limits.entries > 0,
            "index limits must be positive"
        );
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(1))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let app: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if version == 0 && app == 0 {
            let tables: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                tables == 0,
                "unrecognized database; preserve it and rebuild separately"
            );
        } else {
            ensure!(
                version == SCHEMA && app == APPLICATION,
                "unsupported index schema; preserve database and use rebuild-index into a new file"
            );
        }
        // EXTRA also syncs the directory after rollback-journal removal on Unix.
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA foreign_keys=ON; PRAGMA cache_size=-8192; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; PRAGMA trusted_schema=OFF;")?;
        let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        ensure!(
            journal == "delete" && synchronous == 3 && foreign_keys,
            "SQLite durability/foreign-key configuration was not applied"
        );
        if version == 0 {
            let tx = conn.transaction()?;
            tx.execute_batch(include_str!("schema.sql"))?;
            tx.execute(
                "INSERT INTO identity VALUES(?1,?2)",
                params![chain.name(), chain.genesis().as_byte_array().as_slice()],
            )?;
            tx.commit()?;
            File::open(path)?.sync_all()?;
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?
            .sync_all()?;
        }
        let (name, genesis): (String, Vec<u8>) =
            conn.query_row("SELECT network,genesis FROM identity", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        ensure!(
            name == chain.name() && genesis == chain.genesis().to_byte_array(),
            "index network mismatch"
        );
        let store = Self {
            conn,
            chain,
            limits,
        };
        store.integrity_check()?;
        Ok(store)
    }

    pub fn stats(&self) -> Result<Stats> {
        stats(&self.conn)
    }

    pub fn record(&self, height: u32) -> Result<Option<(BlockHash, Location)>> {
        let row: Option<(Vec<u8>, u32, i64, u32)> = self
            .conn
            .query_row(
                "SELECT hash,segment,offset,length FROM blocks WHERE height=?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(hash, segment, offset, length)| {
            Ok((
                BlockHash::from_slice(&hash)?,
                Location {
                    segment,
                    offset: offset.try_into()?,
                    length,
                },
            ))
        })
        .transpose()
    }

    /// Verify every archived record; index only complete records beyond the committed prefix.
    /// A corrupt tail is never discarded, and database-ahead/mixed generations fail closed.
    pub fn recover(&mut self, archive: &mut Archive) -> Result<()> {
        let committed = self.stats()?.tip;
        let mut last = None;
        archive.scan_records(|height, block, location| {
            if committed.is_some_and(|(tip, _)| height <= tip) {
                ensure!(self.record(height)? == Some((block.block_hash(), location)), "database/archive generation mismatch; restore a matching backup or rebuild separately");
            } else {
                self.apply_record(height, block, location)?;
            }
            last = Some((height, block.block_hash()));
            Ok(())
        })?;
        ensure!(
            self.stats()?.tip == last,
            "database references missing archive blocks; restore matching files"
        );
        Ok(())
    }

    /// Normal coordinator: validate/stage SQL, fsync archive, then publish all SQL atomically.
    pub fn commit(&mut self, archive: &mut Archive, height: u32, block: &Block) -> Result<()> {
        self.commit_with_hook(archive, height, block, |_| Ok(()))
    }

    /// Fault-injection hook for tests. There is deliberately no unsafe production CLI flag.
    #[doc(hidden)]
    pub fn commit_with_hook(
        &mut self,
        archive: &mut Archive,
        height: u32,
        block: &Block,
        mut hook: impl FnMut(CommitStage) -> Result<()>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        prepare(&tx, self.chain, self.limits, height, block)?;
        hook(CommitStage::Prepared)?;
        let location = archive.append(height, block)?;
        hook(CommitStage::ArchiveSynced)?;
        publish(&tx, height, block, location)?;
        hook(CommitStage::ReferencesStaged)?;
        tx.commit()?;
        hook(CommitStage::Committed)?;
        Ok(())
    }

    fn apply_record(&mut self, height: u32, block: &Block, location: Location) -> Result<()> {
        let tx = self.conn.transaction()?;
        prepare(&tx, self.chain, self.limits, height, block)?;
        publish(&tx, height, block, location)?;
        tx.commit()?;
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))?;
        ensure!(
            result == "ok",
            "SQLite integrity check failed; preserve database"
        );
        let violations: i64 =
            self.conn
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        ensure!(violations == 0, "index foreign-key violation");
        let actual: (i64,i64,i64) = self.conn.query_row("SELECT (SELECT count(*) FROM occurrences),(SELECT count(*) FROM utxos),(SELECT count(*) FROM history)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let s = self.stats()?;
        ensure!(
            actual == (s.transactions, s.utxos, s.history),
            "index counters inconsistent"
        );
        Ok(())
    }

    /// Bounded single-script response: error rather than silently truncating a large history.
    pub fn view(&self, hash: &str, limit: usize) -> Result<ScriptView> {
        let mut script = hex::decode(hash)?;
        ensure!(
            script.len() == 32 && limit > 0,
            "invalid scripthash or query limit"
        );
        script.reverse(); // Electrum display order -> canonical SHA256 bytes in storage.
        let sql_limit = i64::try_from(limit)?
            .checked_add(1)
            .context("query limit overflow")?;
        let mut stmt = self.conn.prepare("SELECT o.txid,h.height,h.position FROM history h JOIN occurrences o USING(height,position) WHERE h.script=?1 ORDER BY h.height,h.position LIMIT ?2")?;
        let mut history = Vec::new();
        let mut rows = stmt.query(params![script, sql_limit])?;
        while let Some(row) = rows.next()? {
            ensure!(
                history.len() < limit,
                "history exceeds --max-query-results; no partial result returned"
            );
            let bytes: Vec<u8> = row.get(0)?;
            history.push(Occurrence {
                txid: Txid::from_slice(&bytes)?,
                height: row.get(1)?,
                position: row.get::<_, u32>(2)?.try_into()?,
            });
        }
        let mut stmt = self.conn.prepare("SELECT outpoint,value,height,coinbase FROM utxos WHERE script=?1 ORDER BY outpoint LIMIT ?2")?;
        let mut rows = stmt.query(params![script, sql_limit])?;
        let mut utxos = Vec::new();
        let mut confirmed_sats = 0u64;
        while let Some(row) = rows.next()? {
            ensure!(
                utxos.len() < limit,
                "UTXO list exceeds --max-query-results; no partial result returned"
            );
            let key: Vec<u8> = row.get(0)?;
            let value: i64 = row.get(1)?;
            let value: u64 = value.try_into()?;
            confirmed_sats = confirmed_sats
                .checked_add(value)
                .context("balance overflow")?;
            utxos.push((
                deserialize(&key)?,
                Coin {
                    value,
                    height: row.get(2)?,
                    coinbase: row.get(3)?,
                    scripthash: hash.to_ascii_lowercase(),
                },
            ));
        }
        // Match the reference in-memory CLI's deterministic OutPoint ordering.
        utxos.sort_by_key(|(point, _): &(OutPoint, Coin)| *point);
        Ok(ScriptView {
            confirmed_sats,
            history,
            utxos,
        })
    }

    pub fn occurrences(&self, txid: Txid, height: Option<u32>) -> Result<Vec<Occurrence>> {
        let mut stmt = self.conn.prepare("SELECT height,position FROM occurrences WHERE txid=?1 AND (?2 IS NULL OR height=?2) ORDER BY height,position LIMIT 17")?;
        let rows = stmt.query_map(params![txid.as_byte_array().as_slice(), height], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
        })?;
        let mut found = Vec::new();
        for row in rows {
            ensure!(found.len() < 16, "too many occurrences; specify --height");
            let (height, position) = row?;
            found.push(Occurrence {
                txid,
                height,
                position: position.try_into()?,
            });
        }
        Ok(found)
    }

    /// Internal reversible-index primitive only. Does not rewind the immutable archive or Core.
    /// Not exposed as a CLI reorg/recovery command until branch-aware archives are implemented.
    pub fn rollback_tip(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (height, _) = stats(&tx)?.tip.context("empty index")?;
        ensure!(height > 0, "cannot roll back genesis");
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM undo_blocks WHERE height=?1)",
            [height],
            |r| r.get(0),
        )?;
        ensure!(retained, "undo unavailable; safe reconstruction required");
        {
            let mut stmt = tx.prepare("SELECT outpoint,old_script,old_value,old_height,old_coinbase FROM undo WHERE height=?1")?;
            let mut rows = stmt.query([height])?;
            while let Some(row) = rows.next()? {
                let key: Vec<u8> = row.get(0)?;
                tx.execute("DELETE FROM utxos WHERE outpoint=?1", [&key])?;
                if let Some(value) = row.get::<_, Option<i64>>(2)? {
                    tx.execute(
                        "INSERT INTO utxos VALUES(?1,?2,?3,?4,?5)",
                        params![
                            key,
                            row.get::<_, Vec<u8>>(1)?,
                            value,
                            row.get::<_, u32>(3)?,
                            row.get::<_, bool>(4)?
                        ],
                    )?;
                }
            }
        }
        tx.execute("DELETE FROM blocks WHERE height=?1", [height])?;
        tx.execute("UPDATE state SET transactions=(SELECT count(*) FROM occurrences),utxos=(SELECT count(*) FROM utxos),history=(SELECT count(*) FROM history) WHERE id=1", [])?;
        tx.commit()?;
        Ok(())
    }
}

fn stats(conn: &Connection) -> Result<Stats> {
    let (transactions, utxos, history) = conn.query_row(
        "SELECT transactions,utxos,history FROM state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let row: Option<(u32, Vec<u8>)> = conn
        .query_row(
            "SELECT height,hash FROM blocks ORDER BY height DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let tip = row
        .map(|(h, b)| Ok::<_, anyhow::Error>((h, BlockHash::from_slice(&b)?)))
        .transpose()?;
    Ok(Stats {
        tip,
        transactions,
        utxos,
        history,
    })
}

fn remember(tx: &Transaction<'_>, height: u32, key: &[u8]) -> Result<()> {
    tx.execute("INSERT OR IGNORE INTO undo SELECT ?1,outpoint,script,value,height,coinbase FROM utxos WHERE outpoint=?2", params![height,key])?;
    tx.execute(
        "INSERT OR IGNORE INTO undo(height,outpoint) VALUES(?1,?2)",
        params![height, key],
    )?;
    Ok(())
}

fn prepare(
    tx: &Transaction<'_>,
    chain: Chain,
    limits: Limits,
    height: u32,
    block: &Block,
) -> Result<()> {
    validate_block(block)?;
    let old = stats(tx)?;
    match old.tip {
        None => ensure!(
            height == 0 && block.block_hash() == chain.genesis(),
            "persistent index must start at selected genesis"
        ),
        Some((h, hash)) => ensure!(
            h.checked_add(1) == Some(height) && block.header.prev_blockhash == hash,
            "persistent index discontinuity or reorg; automatic rollback is not enabled"
        ),
    }
    let transactions = old
        .transactions
        .checked_add(block.txdata.len().try_into()?)
        .context("transaction count overflow")?;
    ensure!(
        transactions <= i64::try_from(limits.transactions)?,
        "persistent transaction limit reached; adjust --max-index-transactions"
    );
    let mut utxos = old.utxos;
    let mut history = old.history;
    tx.execute("INSERT INTO undo_blocks VALUES(?1)", [height])?;
    for (position, transaction) in block.txdata.iter().enumerate() {
        let txid = transaction.compute_txid();
        tx.execute(
            "INSERT INTO occurrences VALUES(?1,?2,?3)",
            params![
                height,
                i64::try_from(position)?,
                txid.as_byte_array().as_slice()
            ],
        )?;
        let mut scripts = HashSet::new();
        if !transaction.is_coinbase() {
            for input in &transaction.input {
                let key = serialize(&input.previous_output);
                let script: Vec<u8> = tx
                    .query_row("SELECT script FROM utxos WHERE outpoint=?1", [&key], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .context("missing or already spent prevout")?;
                scripts.insert(script);
                remember(tx, height, &key)?;
                utxos -= tx.execute("DELETE FROM utxos WHERE outpoint=?1", [&key])? as i64;
            }
        }
        for (vout, output) in transaction.output.iter().enumerate() {
            let script = bitcoin::hashes::sha256::Hash::hash(output.script_pubkey.as_bytes())
                .to_byte_array()
                .to_vec();
            scripts.insert(script.clone());
            if height == 0
                || output.script_pubkey.is_op_return()
                || output.script_pubkey.len() > 10_000
            {
                continue;
            }
            let key = serialize(&OutPoint {
                txid,
                vout: vout.try_into()?,
            });
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM utxos WHERE outpoint=?1)",
                [&key],
                |r| r.get(0),
            )?;
            let bip30 = chain == Chain::Mainnet
                && matches!(height, 91842 | 91880)
                && transaction.is_coinbase();
            ensure!(
                !exists || bip30,
                "duplicate unspent outpoint outside BIP30 exception"
            );
            remember(tx, height, &key)?;
            tx.execute("INSERT INTO utxos VALUES(?1,?2,?3,?4,?5) ON CONFLICT(outpoint) DO UPDATE SET script=excluded.script,value=excluded.value,height=excluded.height,coinbase=excluded.coinbase", params![key,script,i64::try_from(output.value.to_sat())?,height,transaction.is_coinbase()])?;
            if !exists {
                utxos += 1;
            }
        }
        for script in scripts {
            history += tx.execute(
                "INSERT INTO history VALUES(?1,?2,?3)",
                params![script, height, i64::try_from(position)?],
            )? as i64;
        }
    }
    ensure!(
        utxos
            .checked_add(history)
            .is_some_and(|n| usize::try_from(n).is_ok_and(|n| n <= limits.entries)),
        "persistent entry limit reached; adjust --max-index-entries"
    );
    tx.execute(
        "UPDATE state SET transactions=?1,utxos=?2,history=?3 WHERE id=1",
        params![transactions, utxos, history],
    )?;
    // Retain a bounded window of reversible blocks; never delete immutable archived bytes.
    if height >= UNDO_WINDOW {
        tx.execute(
            "DELETE FROM undo_blocks WHERE height<=?1",
            [height - UNDO_WINDOW],
        )?;
    }
    Ok(())
}

fn publish(tx: &Transaction<'_>, height: u32, block: &Block, location: Location) -> Result<()> {
    tx.execute(
        "INSERT INTO blocks VALUES(?1,?2,?3,?4,?5)",
        params![
            height,
            block.block_hash().as_byte_array().as_slice(),
            location.segment,
            i64::try_from(location.offset)?,
            location.length
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_full_aborts_before_appending_archive() {
        let root = tempfile::tempdir().unwrap();
        let chain = Chain::Regtest;
        let mut archive = Archive::open(root.path(), chain, true).unwrap();
        let mut store = Store::open(
            &archive.directory().join("index.sqlite"),
            chain,
            Limits {
                transactions: 100_000,
                entries: 500_000,
            },
        )
        .unwrap();
        store.recover(&mut archive).unwrap();
        let genesis = bitcoin::blockdata::constants::genesis_block(chain.network());
        store.commit(&mut archive, 0, &genesis).unwrap();
        let before = store.stats().unwrap();
        let path = archive.directory().join("blocks-00000000.elb");
        let bytes = std::fs::read(&path).unwrap();
        let pages: i64 = store
            .conn
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        store
            .conn
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let mut block = genesis.clone();
        block.header.prev_blockhash = genesis.block_hash();
        block.txdata[0].output = vec![genesis.txdata[0].output[0].clone(); 2048];
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        let error = store.commit(&mut archive, 1, &block).unwrap_err();
        let sqlite_error = error.downcast_ref::<rusqlite::Error>().unwrap();
        assert!(
            matches!(sqlite_error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DiskFull)
        );
        assert_eq!(store.stats().unwrap(), before);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        store.integrity_check().unwrap();
    }
}
