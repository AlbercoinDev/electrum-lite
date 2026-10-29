//! Durable SQLite index. Hold the Archive's exclusive lock for this store's lifetime.
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::File,
    path::Path,
};

use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::Hash,
    Block, BlockHash, OutPoint, Txid,
};
use rusqlite::{
    params, params_from_iter, types::Value, Connection, OpenFlags, OptionalExtension, Transaction,
};
use serde::Serialize;

use crate::{
    archive::{Archive, Location},
    index::{Coin, Occurrence, ScriptView},
    validate_block_txids, Chain,
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
        // 64 MiB page cache: random B-tree inserts over billions of rows otherwise
        // evict every page. It is a cap, not a preallocation, and stays documented.
        // 32 KiB pages reduced random TXID insertion costs in the local bench;
        // the pragma only takes effect on a brand-new database, so
        // existing generations keep their page size (no silent rewrite).
        conn.execute_batch("PRAGMA page_size=32768; PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA foreign_keys=ON; PRAGMA cache_size=-65536; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; PRAGMA trusted_schema=OFF;")?;
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
                let mut sql = BatchSql::new(&tx);
                let mut running = Running::load(&tx)?;
                for (height, block) in blocks {
                    prepare_block(
                        &tx,
                        self.chain,
                        self.limits,
                        &mut running,
                        *height,
                        block,
                        &mut sql,
                    )?;
                    let location = archive.append_staged(*height, block)?;
                    first.get_or_insert(location.segment);
                    publish(&tx, *height, block, location)?;
                }
                running.save(&tx)?;
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

const INSERT_CHUNK: usize = 100;

/// Counters kept in memory for a whole batch: one `state` read and one `state`
/// write instead of two queries per block.
struct Running {
    tip: Option<(u32, BlockHash)>,
    transactions: i64,
    utxos: i64,
    history: i64,
}

impl Running {
    fn load(conn: &Connection) -> Result<Self> {
        let s = stats(conn)?;
        Ok(Self {
            tip: s.tip,
            transactions: s.transactions,
            utxos: s.utxos,
            history: s.history,
        })
    }

    fn save(&self, tx: &Transaction<'_>) -> Result<()> {
        tx.execute(
            "UPDATE state SET transactions=?1,utxos=?2,history=?3 WHERE id=1",
            params![self.transactions, self.utxos, self.history],
        )?;
        Ok(())
    }
}

struct UtxoRow {
    key: [u8; 36],
    script: [u8; 32],
    value: i64,
    height: u32,
    coinbase: bool,
}

fn placeholders(rows: usize, columns: usize) -> String {
    let mut sql = String::with_capacity(rows * (columns * 2 + 3));
    for row in 0..rows {
        if row > 0 {
            sql.push(',');
        }
        sql.push('(');
        for column in 0..columns {
            if column > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');
    }
    sql
}

fn outpoint_key(txid: Txid, vout: u32) -> [u8; 36] {
    let mut key = [0u8; 36];
    key[..32].copy_from_slice(txid.as_byte_array());
    key[32..].copy_from_slice(&vout.to_le_bytes());
    key
}

fn prepare(
    tx: &Transaction<'_>,
    chain: Chain,
    limits: Limits,
    height: u32,
    block: &Block,
) -> Result<()> {
    let mut sql = BatchSql::new(tx);
    let mut running = Running::load(tx)?;
    prepare_block(tx, chain, limits, &mut running, height, block, &mut sql)?;
    running.save(tx)
}

fn prepare_block(
    tx: &Transaction<'_>,
    chain: Chain,
    limits: Limits,
    running: &mut Running,
    height: u32,
    block: &Block,
    sql: &mut BatchSql<'_, '_>,
) -> Result<()> {
    let txids = validate_block_txids(block)?;
    match running.tip {
        None => ensure!(
            height == 0 && block.block_hash() == chain.genesis(),
            "persistent index must start at selected genesis"
        ),
        Some((h, hash)) => ensure!(
            h.checked_add(1) == Some(height) && block.header.prev_blockhash == hash,
            "persistent index discontinuity or reorg; automatic rollback is not enabled"
        ),
    }
    running.transactions = running
        .transactions
        .checked_add(block.txdata.len().try_into()?)
        .context("transaction count overflow")?;
    ensure!(
        running.transactions <= i64::try_from(limits.transactions)?,
        "persistent transaction limit reached; adjust --max-index-transactions"
    );
    // Stage the parent before its children, avoiding deferred-FK child scans.
    // These placeholder references are never committed: publish replaces them
    // after archive append, before the durable SQL commit.
    tx.execute(
        "INSERT INTO blocks VALUES(?1,?2,0,0,1,?3)",
        params![
            height,
            block.block_hash().as_byte_array().as_slice(),
            serialize(&block.header)
        ],
    )?;
    tx.execute("INSERT INTO undo_blocks VALUES(?1)", [height])?;
    // BIP30 exceptions are the only place where an unspent outpoint may be
    // overwritten; every other conflict is a hard integrity error.
    let bip30 = chain == Chain::Mainnet && matches!(height, 91842 | 91880);
    let mut utxo_rows: Vec<UtxoRow> = Vec::new();
    let mut history_rows: Vec<([u8; 32], u32, i64)> = Vec::new();
    let mut occurrence_rows: Vec<(u32, i64, Vec<u8>)> = Vec::new();
    let mut undo_rows: Vec<([u8; 36], Prevout)> = Vec::new();
    let mut missing_rows: Vec<[u8; 36]> = Vec::new();
    for (position, transaction) in block.txdata.iter().enumerate() {
        let position = i64::try_from(position)?;
        let txid = txids[usize::try_from(position)?];
        occurrence_rows.push((height, position, txid.as_byte_array().to_vec()));
        let mut scripts: HashSet<[u8; 32]> = HashSet::new();
        if !transaction.is_coinbase() {
            let keys: Vec<[u8; 36]> = transaction
                .input
                .iter()
                .map(|input| outpoint_key(input.previous_output.txid, input.previous_output.vout))
                .collect();
            let prevouts = lookup_prevouts(tx, sql, &keys)?;
            for key in &keys {
                let prevout = prevouts
                    .get(key)
                    .context("missing or already spent prevout")?;
                scripts.insert(prevout.script);
                undo_rows.push((
                    *key,
                    Prevout {
                        script: prevout.script,
                        value: prevout.value,
                        height: prevout.height,
                        coinbase: prevout.coinbase,
                    },
                ));
            }
            // Undo intent must be durable before the prevouts disappear.
            insert_undo_existing(tx, sql, height, &undo_rows)?;
            undo_rows.clear();
            let removed = delete_utxos(tx, sql, &keys)?;
            let unique = keys.iter().copied().collect::<BTreeSet<[u8; 36]>>().len();
            ensure!(removed == unique, "missing or already spent prevout");
            running.utxos -= i64::try_from(removed)?;
        }
        let coinbase = transaction.is_coinbase();
        for (vout, output) in transaction.output.iter().enumerate() {
            let script = bitcoin::hashes::sha256::Hash::hash(output.script_pubkey.as_bytes())
                .to_byte_array();
            scripts.insert(script);
            if height == 0
                || output.script_pubkey.is_op_return()
                || output.script_pubkey.len() > 10_000
            {
                continue;
            }
            let key = outpoint_key(txid, vout.try_into()?);
            missing_rows.push(key);
            utxo_rows.push(UtxoRow {
                key,
                script,
                value: i64::try_from(output.value.to_sat())?,
                height,
                coinbase,
            });
        }
        // The two BIP30 blocks re-create unspent coinbase outpoints; capture
        // their old value before `ON CONFLICT` overwrites it. Everywhere else a
        // marker is enough: the outpoint cannot exist before this block.
        let overwrite = bip30 && coinbase;
        if overwrite {
            insert_undo_from_utxos(tx, sql, height, &missing_rows)?;
        }
        insert_undo_missing(tx, sql, height, &missing_rows)?;
        missing_rows.clear();
        for script in scripts {
            history_rows.push((script, height, position));
        }
        // Outputs of this transaction must be visible before the next one can
        // spend them, so UTXOs are flushed per transaction (still multi-row).
        let inserted = insert_utxos(tx, sql, &utxo_rows, overwrite)?;
        utxo_rows.clear();
        if !overwrite {
            running.utxos += i64::try_from(inserted)?;
        }
    }
    if bip30 {
        running.utxos = tx.query_row("SELECT count(*) FROM utxos", [], |r| r.get(0))?;
    }
    insert_occurrences(tx, sql, &occurrence_rows)?;
    running.history += i64::try_from(insert_history(tx, sql, &history_rows)?)?;
    running.tip = Some((height, block.block_hash()));
    ensure!(
        running
            .utxos
            .checked_add(running.history)
            .is_some_and(|n| usize::try_from(n).is_ok_and(|n| n <= limits.entries)),
        "persistent entry limit reached; adjust --max-index-entries"
    );
    // Retain a bounded window of reversible blocks; never delete immutable archived bytes.
    if height >= UNDO_WINDOW {
        tx.execute(
            "DELETE FROM undo_blocks WHERE height<=?1",
            [height - UNDO_WINDOW],
        )?;
    }
    Ok(())
}

/// Lazily prepared multi-row statements, one per row count. They are created
/// once per durable batch so row batching never pays a prepare per block.
struct BatchSql<'a, 'conn> {
    tx: &'a Transaction<'conn>,
    lookup: Vec<Option<rusqlite::Statement<'a>>>,
    delete: Vec<Option<rusqlite::Statement<'a>>>,
    utxos: Vec<Option<rusqlite::Statement<'a>>>,
    utxos_bip30: Vec<Option<rusqlite::Statement<'a>>>,
    undo_existing: Vec<Option<rusqlite::Statement<'a>>>,
    undo_missing: Vec<Option<rusqlite::Statement<'a>>>,
    undo_select: Vec<Option<rusqlite::Statement<'a>>>,
    occurrences: Vec<Option<rusqlite::Statement<'a>>>,
    history: Vec<Option<rusqlite::Statement<'a>>>,
}

impl<'a, 'conn> BatchSql<'a, 'conn> {
    fn new(tx: &'a Transaction<'conn>) -> Self {
        let empty = || {
            std::iter::repeat_with(|| None)
                .take(INSERT_CHUNK + 1)
                .collect()
        };
        Self {
            tx,
            lookup: empty(),
            delete: empty(),
            utxos: empty(),
            utxos_bip30: empty(),
            undo_existing: empty(),
            undo_missing: empty(),
            undo_select: empty(),
            occurrences: empty(),
            history: empty(),
        }
    }

    fn lookup(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.lookup, rows, || {
            format!(
                "SELECT outpoint,script,value,height,coinbase FROM utxos WHERE outpoint IN ({})",
                vec!["?"; rows].join(",")
            )
        })
    }

    fn delete(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.delete, rows, || {
            format!(
                "DELETE FROM utxos WHERE outpoint IN ({})",
                vec!["?"; rows].join(",")
            )
        })
    }

    fn utxos(&mut self, rows: usize, bip30: bool) -> Result<&mut rusqlite::Statement<'a>> {
        if bip30 {
            prepared(self.tx, &mut self.utxos_bip30, rows, || {
                format!(
                    "INSERT INTO utxos VALUES{} ON CONFLICT(outpoint) DO UPDATE SET script=excluded.script,value=excluded.value,height=excluded.height,coinbase=excluded.coinbase",
                    placeholders(rows, 5)
                )
            })
        } else {
            prepared(self.tx, &mut self.utxos, rows, || {
                format!(
                    "INSERT OR IGNORE INTO utxos VALUES{}",
                    placeholders(rows, 5)
                )
            })
        }
    }

    fn undo_existing(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.undo_existing, rows, || {
            format!("INSERT OR IGNORE INTO undo VALUES{}", placeholders(rows, 6))
        })
    }

    fn undo_missing(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.undo_missing, rows, || {
            format!(
                "INSERT OR IGNORE INTO undo(height,outpoint) VALUES{}",
                placeholders(rows, 2)
            )
        })
    }

    fn undo_select(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.undo_select, rows, || {
            format!(
                "INSERT OR IGNORE INTO undo SELECT ?1,outpoint,script,value,height,coinbase FROM utxos WHERE outpoint IN ({})",
                vec!["?"; rows].join(",")
            )
        })
    }

    fn occurrences(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.occurrences, rows, || {
            format!("INSERT INTO occurrences VALUES{}", placeholders(rows, 3))
        })
    }

    fn history(&mut self, rows: usize) -> Result<&mut rusqlite::Statement<'a>> {
        prepared(self.tx, &mut self.history, rows, || {
            format!("INSERT INTO history VALUES{}", placeholders(rows, 3))
        })
    }
}

fn prepared<'a, 'b, 'conn>(
    tx: &'a Transaction<'conn>,
    slots: &'b mut [Option<rusqlite::Statement<'a>>],
    rows: usize,
    build: impl FnOnce() -> String,
) -> Result<&'b mut rusqlite::Statement<'a>> {
    if slots[rows].is_none() {
        slots[rows] = Some(tx.prepare(&build())?);
    }
    Ok(slots[rows].as_mut().expect("statement just prepared"))
}

struct Prevout {
    script: [u8; 32],
    value: i64,
    height: i64,
    coinbase: bool,
}

fn lookup_prevouts(
    _tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    keys: &[[u8; 36]],
) -> Result<HashMap<[u8; 36], Prevout>> {
    let unique: Vec<[u8; 36]> = keys
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut found = HashMap::with_capacity(unique.len());
    for chunk in unique.chunks(INSERT_CHUNK) {
        let values = chunk
            .iter()
            .map(|key| Value::Blob(key.to_vec()))
            .collect::<Vec<_>>();
        let mut rows = sql.lookup(chunk.len())?.query(params_from_iter(values))?;
        while let Some(row) = rows.next()? {
            let key: Vec<u8> = row.get(0)?;
            let script: Vec<u8> = row.get(1)?;
            let key: [u8; 36] = key
                .as_slice()
                .try_into()
                .context("invalid stored outpoint")?;
            let script: [u8; 32] = script
                .as_slice()
                .try_into()
                .context("invalid stored script hash")?;
            found.insert(
                key,
                Prevout {
                    script,
                    value: row.get(2)?,
                    height: row.get(3)?,
                    coinbase: row.get(4)?,
                },
            );
        }
    }
    Ok(found)
}

/// Record the pre-block state of every outpoint a transaction touches. Inputs
/// are known from the prevout lookup; outputs only need a "did not exist"
/// marker, except during the two historical BIP30 overwrites.
fn insert_undo_existing(
    _tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    height: u32,
    rows: &[([u8; 36], Prevout)],
) -> Result<()> {
    let height = i64::from(height);
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() * 6);
        for (key, prevout) in chunk {
            values.push(Value::Integer(height));
            values.push(Value::Blob(key.to_vec()));
            values.push(Value::Blob(prevout.script.to_vec()));
            values.push(Value::Integer(prevout.value));
            values.push(Value::Integer(prevout.height));
            values.push(Value::Integer(i64::from(prevout.coinbase)));
        }
        sql.undo_existing(chunk.len())?
            .execute(params_from_iter(values))?;
    }
    Ok(())
}

fn insert_undo_missing(
    _tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    height: u32,
    keys: &[[u8; 36]],
) -> Result<()> {
    let height = i64::from(height);
    for chunk in keys.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() * 2);
        for key in chunk {
            values.push(Value::Integer(height));
            values.push(Value::Blob(key.to_vec()));
        }
        sql.undo_missing(chunk.len())?
            .execute(params_from_iter(values))?;
    }
    Ok(())
}

fn insert_undo_from_utxos(
    _tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    height: u32,
    keys: &[[u8; 36]],
) -> Result<()> {
    for chunk in keys.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() + 1);
        values.push(Value::Integer(i64::from(height)));
        for key in chunk {
            values.push(Value::Blob(key.to_vec()));
        }
        sql.undo_select(chunk.len())?
            .execute(params_from_iter(values))?;
    }
    Ok(())
}

fn delete_utxos(
    tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    keys: &[[u8; 36]],
) -> Result<usize> {
    let unique: Vec<[u8; 36]> = keys
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut removed = 0;
    for chunk in unique.chunks(INSERT_CHUNK) {
        let values = chunk
            .iter()
            .map(|key| Value::Blob(key.to_vec()))
            .collect::<Vec<_>>();
        sql.delete(chunk.len())?.execute(params_from_iter(values))?;
        removed += usize::try_from(tx.changes())?;
    }
    Ok(removed)
}

fn insert_utxos(
    tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    rows: &[UtxoRow],
    bip30: bool,
) -> Result<usize> {
    let mut inserted = 0;
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() * 5);
        for row in chunk {
            values.push(Value::Blob(row.key.to_vec()));
            values.push(Value::Blob(row.script.to_vec()));
            values.push(Value::Integer(row.value));
            values.push(Value::Integer(i64::from(row.height)));
            values.push(Value::Integer(i64::from(row.coinbase)));
        }
        sql.utxos(chunk.len(), bip30)?
            .execute(params_from_iter(values))?;
        let changed = usize::try_from(tx.changes())?;
        if !bip30 {
            ensure!(
                changed == chunk.len(),
                "duplicate unspent outpoint outside BIP30 exception"
            );
            inserted += changed;
        }
    }
    Ok(inserted)
}

fn insert_occurrences(
    tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    rows: &[(u32, i64, Vec<u8>)],
) -> Result<()> {
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() * 3);
        for (height, position, txid) in chunk {
            values.push(Value::Integer(i64::from(*height)));
            values.push(Value::Integer(*position));
            values.push(Value::Blob(txid.clone()));
        }
        sql.occurrences(chunk.len())?
            .execute(params_from_iter(values))?;
        ensure!(
            usize::try_from(tx.changes())? == chunk.len(),
            "occurrence insert mismatch"
        );
    }
    Ok(())
}

fn insert_history(
    tx: &Transaction<'_>,
    sql: &mut BatchSql<'_, '_>,
    rows: &[([u8; 32], u32, i64)],
) -> Result<usize> {
    let mut inserted = 0;
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut values = Vec::with_capacity(chunk.len() * 3);
        for (script, height, position) in chunk {
            values.push(Value::Blob(script.to_vec()));
            values.push(Value::Integer(i64::from(*height)));
            values.push(Value::Integer(*position));
        }
        sql.history(chunk.len())?
            .execute(params_from_iter(values))?;
        let changed = usize::try_from(tx.changes())?;
        ensure!(changed == chunk.len(), "history insert mismatch");
        inserted += changed;
    }
    Ok(inserted)
}

fn publish(tx: &Transaction<'_>, height: u32, block: &Block, location: Location) -> Result<()> {
    let changed = tx.execute(
        "UPDATE blocks SET hash=?2,segment=?3,offset=?4,length=?5,header=?6 WHERE height=?1",
        params![
            height,
            block.block_hash().as_byte_array().as_slice(),
            location.segment,
            i64::try_from(location.offset)?,
            location.length,
            serialize(&block.header)
        ],
    )?;
    ensure!(changed == 1, "missing staged block reference");
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
