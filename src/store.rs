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
const SCHEMA: i64 = 2;
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
                "unsupported index schema; schema 1 requires migrate-index with a backup; otherwise rebuild-index separately"
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
        archive.enable_branches();
        let cursor = self.cursor()?;
        let tip = self.stats()?.tip;
        ensure!(
            tip.is_some() == cursor.is_some(),
            "index tip/cursor mismatch"
        );
        let mut matched = 0u64;
        let mut cursor_seen = cursor.is_none();
        let mut pending = false;
        archive.scan_records(|height, block, location| {
            if cursor.is_none_or(|c| location > c) {
                pending = true;
            }
            if cursor == Some(location) {
                cursor_seen = true;
            }
            if let Some((hash, reference)) = self.record(height)? {
                if reference == location {
                    ensure!(hash == block.block_hash(), "database/archive hash mismatch");
                    let header: Option<Vec<u8>> = self.conn.query_row(
                        "SELECT header FROM blocks WHERE height=?1",
                        [height],
                        |r| r.get(0),
                    )?;
                    let bytes = serialize(&block.header);
                    if let Some(header) = header {
                        ensure!(header == bytes, "stored header mismatch");
                    } else {
                        self.conn.execute(
                            "UPDATE blocks SET header=?1 WHERE height=?2",
                            params![bytes, height],
                        )?;
                    }
                    matched += 1;
                }
            }
            Ok(())
        })?;
        ensure!(
            cursor_seen && matched == tip.map_or(0, |(h, _)| u64::from(h) + 1),
            "database/archive generation mismatch or missing references"
        );
        if pending {
            archive.scan_records(|height, block, location| {
                if cursor.is_none_or(|c| location > c) {
                    self.apply_record(height, block, location)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn cursor(&self) -> Result<Option<Location>> {
        let row: Option<(u32, i64, u32)> = self
            .conn
            .query_row(
                "SELECT segment,offset,length FROM archive_cursor WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(segment, offset, length)| {
            Ok(Location {
                segment,
                offset: offset.try_into()?,
                length,
            })
        })
        .transpose()
    }

    pub fn rewind(&mut self, archive: &mut Archive, height: u32) -> Result<()> {
        self.check_undo(height)?;
        let (_, location) = self.record(height)?.context("ancestor absent")?;
        let (_, block) = archive.read(location)?;
        let marker = archive.append(height, &block)?; // Durable intent; no original bytes deleted.
        self.apply_record(height, &block, marker)
    }

    fn check_undo(&self, height: u32) -> Result<()> {
        let tip = self.stats()?.tip.context("empty index")?.0;
        ensure!(height <= tip, "ancestor after tip");
        let count: u32 = self.conn.query_row(
            "SELECT count(*) FROM undo_blocks WHERE height>?1 AND height<=?2",
            params![height, tip],
            |r| r.get(0),
        )?;
        ensure!(
            count == tip - height,
            "reorg exceeds retained undo; restore/rebuild safely"
        );
        Ok(())
    }

    /// Normal coordinator: validate/stage SQL, fsync archive, then publish all SQL atomically.
    pub fn commit(&mut self, archive: &mut Archive, height: u32, block: &Block) -> Result<()> {
        self.commit_with_hook(archive, height, block, |_| Ok(()))
    }

    /// One durable generation for contiguous blocks; incomplete archive tails fail closed.
    pub fn commit_batch(&mut self, archive: &mut Archive, blocks: &[(u32, Block)]) -> Result<()> {
        self.commit_batch_with_hook(archive, blocks, |_| Ok(()))
    }

    #[doc(hidden)]
    pub fn commit_batch_with_hook(
        &mut self,
        archive: &mut Archive,
        blocks: &[(u32, Block)],
        mut hook: impl FnMut(CommitStage) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !blocks.is_empty() && blocks.len() <= 64,
            "invalid commit batch size"
        );
        let result = (|| -> Result<()> {
            let tx = self.conn.transaction()?;
            let mut first = None;
            {
                let mut statements = BlockStatements::new(&tx)?;
                for (height, block) in blocks {
                    prepare_using(
                        &tx,
                        self.chain,
                        self.limits,
                        *height,
                        block,
                        &mut statements,
                    )?;
                    let location = archive.append_staged(*height, block)?;
                    first.get_or_insert(location.segment);
                    publish(&tx, *height, block, location)?;
                }
            }
            hook(CommitStage::Prepared)?;
            archive.sync_staged(first.context("empty batch")?)?;
            hook(CommitStage::ArchiveSynced)?;
            hook(CommitStage::ReferencesStaged)?;
            tx.commit()?;
            hook(CommitStage::Committed)?;
            Ok(())
        })();
        if result.is_err() {
            archive.require_scan();
        }
        result
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
        if self
            .record(height)?
            .is_some_and(|(hash, _)| hash == block.block_hash())
        {
            self.check_undo(height)?;
            while self.stats()?.tip.is_some_and(|(h, _)| h > height) {
                self.rollback_tip()?;
            }
            let tx = self.conn.transaction()?;
            tx.execute(
                "UPDATE blocks SET segment=?1,offset=?2,length=?3 WHERE height=?4",
                params![
                    location.segment,
                    i64::try_from(location.offset)?,
                    location.length,
                    height
                ],
            )?;
            set_cursor(&tx, location)?;
            tx.commit()?;
            return Ok(());
        }
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

    pub fn header(&self, height: u32) -> Result<bitcoin::block::Header> {
        let bytes: Vec<u8> =
            self.conn
                .query_row("SELECT header FROM blocks WHERE height=?1", [height], |r| {
                    r.get(0)
                })?;
        Ok(deserialize(&bytes)?)
    }

    pub fn headers(&self, start: u32, count: u32) -> Result<Vec<u8>> {
        ensure!(count <= 2016, "header chunk exceeds limit");
        let end = start.checked_add(count).context("header range overflow")?;
        let mut stmt = self
            .conn
            .prepare("SELECT header FROM blocks WHERE height>=?1 AND height<?2 ORDER BY height")?;
        let rows = stmt.query_map(params![start, end], |r| r.get::<_, Vec<u8>>(0))?;
        let mut bytes = Vec::with_capacity(count as usize * 80);
        for row in rows {
            let header = row?;
            ensure!(header.len() == 80, "invalid stored header size");
            bytes.extend(header);
        }
        ensure!(
            bytes.len() == count as usize * 80,
            "missing indexed headers"
        );
        Ok(bytes)
    }

    pub fn header_hashes(&self, height: u32) -> Result<Vec<[u8; 32]>> {
        ensure!(
            height <= 2_000_000,
            "header checkpoint exceeds resource limit"
        );
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM blocks WHERE height<=?1 ORDER BY height")?;
        let rows = stmt.query_map([height], |r| r.get::<_, Vec<u8>>(0))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(
                row?.try_into()
                    .map_err(|_| anyhow::anyhow!("invalid header hash"))?,
            );
        }
        ensure!(
            result.len() == height as usize + 1,
            "checkpoint beyond indexed chain"
        );
        Ok(result)
    }

    pub fn coin(&self, outpoint: OutPoint) -> Result<Option<Coin>> {
        let row: Option<(Vec<u8>, i64, u32, bool)> = self
            .conn
            .query_row(
                "SELECT script,value,height,coinbase FROM utxos WHERE outpoint=?1",
                [serialize(&outpoint)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(mut script, value, height, coinbase)| {
            script.reverse();
            Ok(Coin {
                value: value.try_into()?,
                height,
                coinbase,
                scripthash: hex::encode(script),
            })
        })
        .transpose()
    }

    pub fn fingerprint(&self) -> Result<String> {
        use bitcoin::hashes::{sha256, HashEngine};
        let mut engine = sha256::Hash::engine();
        for table in [
            "identity",
            "state",
            "blocks",
            "occurrences",
            "history",
            "utxos",
            "undo_blocks",
            "undo",
            "archive_cursor",
        ] {
            engine.input(&[255]);
            engine.input(&(table.len() as u64).to_le_bytes());
            engine.input(table.as_bytes());
            let columns: i64 = self.conn.query_row(
                &format!("SELECT count(*) FROM pragma_table_info('{table}')"),
                [],
                |r| r.get(0),
            )?;
            let order = (1..=columns)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                engine.input(&[254]);
                for i in 0..columns as usize {
                    use rusqlite::types::ValueRef;
                    match row.get_ref(i)? {
                        ValueRef::Null => engine.input(&[0]),
                        ValueRef::Integer(n) => {
                            engine.input(&[1]);
                            engine.input(&n.to_le_bytes());
                        }
                        ValueRef::Real(n) => {
                            engine.input(&[2]);
                            engine.input(&n.to_le_bytes());
                        }
                        ValueRef::Text(b) => {
                            engine.input(&[3]);
                            engine.input(&(b.len() as u64).to_le_bytes());
                            engine.input(b);
                        }
                        ValueRef::Blob(b) => {
                            engine.input(&[4]);
                            engine.input(&(b.len() as u64).to_le_bytes());
                            engine.input(b);
                        }
                    }
                }
            }
        }
        Ok(sha256::Hash::from_engine(engine).to_string())
    }

    pub fn audit(&self, archive: &mut Archive) -> Result<()> {
        let temporary = tempfile::NamedTempFile::new_in(archive.directory())?;
        let mut rebuilt = Store::open(
            temporary.path(),
            self.chain,
            Limits {
                transactions: i64::MAX as usize,
                entries: i64::MAX as usize,
            },
        )?;
        rebuilt.recover(archive)?;
        ensure!(
            self.fingerprint()? == rebuilt.fingerprint()?,
            "semantic audit mismatch; pruning denied"
        );
        Ok(())
    }

    pub fn prune_start(&self, requested: u32) -> Result<i64> {
        let pending: i64 = self.conn.query_row(
            "SELECT count(*) FROM prune_audit WHERE state='requested'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            pending == 0,
            "previous prune request needs operator review before another prune"
        );
        let (height, hash) = self.stats()?.tip.context("empty index")?;
        self.conn.execute("INSERT INTO prune_audit(requested,tip_height,tip_hash,state) VALUES(?1,?2,?3,'requested')",params![requested,height,hash.as_byte_array().as_slice()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn prune_finish(&self, id: i64, actual: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE prune_audit SET actual=?1,state='completed' WHERE id=?2",
            params![actual, id],
        )?;
        Ok(())
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

fn remember(
    existing: &mut rusqlite::Statement<'_>,
    missing: &mut rusqlite::Statement<'_>,
    height: u32,
    key: &[u8],
) -> Result<()> {
    existing.execute(params![height, key])?;
    missing.execute(params![height, key])?;
    Ok(())
}

struct BlockStatements<'a> {
    occurrence: rusqlite::Statement<'a>,
    lookup: rusqlite::Statement<'a>,
    remove: rusqlite::Statement<'a>,
    exists_query: rusqlite::Statement<'a>,
    insert_coin: rusqlite::Statement<'a>,
    insert_history: rusqlite::Statement<'a>,
    undo_existing: rusqlite::Statement<'a>,
    undo_missing: rusqlite::Statement<'a>,
}

impl<'a> BlockStatements<'a> {
    fn new(tx: &'a Transaction<'_>) -> Result<Self> {
        Ok(Self {
            occurrence: tx.prepare("INSERT INTO occurrences VALUES(?1,?2,?3)")?,
            lookup: tx.prepare("SELECT script FROM utxos WHERE outpoint=?1")?,
            remove: tx.prepare("DELETE FROM utxos WHERE outpoint=?1")?,
            exists_query: tx.prepare("SELECT EXISTS(SELECT 1 FROM utxos WHERE outpoint=?1)")?,
            insert_coin: tx.prepare("INSERT INTO utxos VALUES(?1,?2,?3,?4,?5) ON CONFLICT(outpoint) DO UPDATE SET script=excluded.script,value=excluded.value,height=excluded.height,coinbase=excluded.coinbase")?,
            insert_history: tx.prepare("INSERT INTO history VALUES(?1,?2,?3)")?,
            undo_existing: tx.prepare("INSERT OR IGNORE INTO undo SELECT ?1,outpoint,script,value,height,coinbase FROM utxos WHERE outpoint=?2")?,
            undo_missing: tx.prepare("INSERT OR IGNORE INTO undo(height,outpoint) VALUES(?1,?2)")?,
        })
    }
}

fn prepare(
    tx: &Transaction<'_>,
    chain: Chain,
    limits: Limits,
    height: u32,
    block: &Block,
) -> Result<()> {
    let mut statements = BlockStatements::new(tx)?;
    prepare_using(tx, chain, limits, height, block, &mut statements)
}

fn prepare_using(
    tx: &Transaction<'_>,
    chain: Chain,
    limits: Limits,
    height: u32,
    block: &Block,
    statements: &mut BlockStatements<'_>,
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
    let BlockStatements {
        occurrence,
        lookup,
        remove,
        exists_query,
        insert_coin,
        insert_history,
        undo_existing,
        undo_missing,
    } = statements;
    for (position, transaction) in block.txdata.iter().enumerate() {
        let txid = transaction.compute_txid();
        occurrence.execute(params![
            height,
            i64::try_from(position)?,
            txid.as_byte_array().as_slice()
        ])?;
        let mut scripts = HashSet::new();
        if !transaction.is_coinbase() {
            for input in &transaction.input {
                let key = serialize(&input.previous_output);
                let script: Vec<u8> = lookup
                    .query_row([&key], |r| r.get(0))
                    .optional()?
                    .context("missing or already spent prevout")?;
                scripts.insert(script);
                remember(undo_existing, undo_missing, height, &key)?;
                utxos -= remove.execute([&key])? as i64;
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
            let exists: bool = exists_query.query_row([&key], |r| r.get(0))?;
            let bip30 = chain == Chain::Mainnet
                && matches!(height, 91842 | 91880)
                && transaction.is_coinbase();
            ensure!(
                !exists || bip30,
                "duplicate unspent outpoint outside BIP30 exception"
            );
            remember(undo_existing, undo_missing, height, &key)?;
            insert_coin.execute(params![
                key,
                script,
                i64::try_from(output.value.to_sat())?,
                height,
                transaction.is_coinbase()
            ])?;
            if !exists {
                utxos += 1;
            }
        }
        for script in scripts {
            history +=
                insert_history.execute(params![script, height, i64::try_from(position)?])? as i64;
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
        "INSERT INTO blocks VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            height,
            block.block_hash().as_byte_array().as_slice(),
            location.segment,
            i64::try_from(location.offset)?,
            location.length,
            serialize(&block.header)
        ],
    )?;
    set_cursor(tx, location)
}

fn set_cursor(tx: &Transaction<'_>, location: Location) -> Result<()> {
    tx.execute("INSERT INTO archive_cursor VALUES(1,?1,?2,?3) ON CONFLICT(id) DO UPDATE SET segment=excluded.segment,offset=excluded.offset,length=excluded.length", params![location.segment,i64::try_from(location.offset)?,location.length])?;
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
