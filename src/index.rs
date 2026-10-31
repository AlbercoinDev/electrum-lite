use crate::core::Core;
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, Hash},
    Block, Network, Transaction, Txid,
};
use rocksdb::{Direction, IteratorMode, Options, WriteBatch, WriteOptions, DB};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

const WINDOW: u32 = 288;
/// Core returned a valid chain hash different from the audited checkpoint.
/// RPC failures and malformed responses are deliberately not classified here.
#[derive(Debug)]
pub struct AuditTipChanged;
impl std::fmt::Display for AuditTipChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Core chain changed during semantic audit")
    }
}
impl std::error::Error for AuditTipChanged {}

fn audit_expect_hash(actual: Value, expected: bitcoin::BlockHash) -> Result<()> {
    let actual: bitcoin::BlockHash = actual
        .as_str()
        .context("invalid Core audit hash")?
        .parse()
        .context("invalid Core audit hash")?;
    if actual != expected {
        return Err(AuditTipChanged.into());
    }
    Ok(())
}
pub struct Index {
    pub db: DB,
    pub network: Network,
    pub fetcher: crate::net::Fetcher,
    pub cache: std::path::PathBuf,
}
pub fn script_hash(script: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(script).to_byte_array()
}
pub fn key(prefix: u8, bytes: &[u8]) -> Vec<u8> {
    let mut k = vec![prefix];
    k.extend(bytes);
    k
}
pub fn height_key(prefix: u8, h: u32) -> Vec<u8> {
    key(prefix, &h.to_be_bytes())
}
fn write_options() -> WriteOptions {
    let mut w = WriteOptions::default();
    w.set_sync(true);
    w
}
impl Index {
    /// Validate prepared state while holding the only live index lock. This is
    /// not prune authorization: the supervisor must own a successful audit
    /// process and the caller must recheck Core and persist pruning intent.
    pub fn verify_prepared_audit(
        &self,
        snapshot: &Path,
        audit: &Path,
    ) -> Result<(u32, bitcoin::BlockHash)> {
        crate::retention::verify_audit_snapshot(audit, snapshot, self.network)?;
        self.verify_snapshot_matches(snapshot)
    }

    /// Create an immutable, NEW checkpoint while the sole index writer is held.
    /// Before pruning it must be audited and compared against the live index.
    pub fn create_snapshot(&self, path: &Path) -> Result<(u32, bitcoin::BlockHash)> {
        ensure!(
            std::fs::symlink_metadata(path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "snapshot destination must be NEW"
        );
        let height = self.tip()?.context("empty index")?;
        let hash = self.header(height)?.block_hash();
        rocksdb::checkpoint::Checkpoint::new(&self.db)?.create_checkpoint(path)?;
        std::fs::File::open(path)?.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?
            .sync_all()?;
        }
        Ok((height, hash))
    }

    /// Compare an independently audited checkpoint with the currently locked
    /// index before a prepared prune. This is not itself prune authorization:
    /// the caller must also verify Core's tip, network and mempool immediately
    /// before invoking the pruning RPC, and preserve durable prune intent.
    pub fn verify_snapshot_matches(&self, path: &Path) -> Result<(u32, bitcoin::BlockHash)> {
        ensure!(
            std::fs::symlink_metadata(path)?.file_type().is_dir(),
            "snapshot must be a real directory"
        );
        let snapshot = DB::open_for_read_only(&Options::default(), path, false)?;
        let identity = format!("thin-v1:{}", self.network);
        ensure!(
            snapshot.get(b"identity")?.as_deref() == Some(identity.as_bytes()),
            "snapshot network identity mismatch"
        );
        let height = self.tip()?.context("empty live index")?;
        ensure!(
            snapshot.get(b"tip")?.as_deref() == Some(height.to_be_bytes().as_slice()),
            "snapshot tip differs from live index"
        );
        let mut live = self.db.iterator(IteratorMode::Start);
        let mut frozen = snapshot.iterator(IteratorMode::Start);
        loop {
            match (live.next(), frozen.next()) {
                (None, None) => break,
                (Some(a), Some(b)) => ensure!(a? == b?, "snapshot state differs from live index"),
                _ => anyhow::bail!("snapshot and live index key count differ"),
            }
        }
        Ok((height, self.header(height)?.block_hash()))
    }

    /// Create a NEW checkpoint from an independently verified audit prefix.
    ///
    /// This only checks the local network and ancestor header. The caller must
    /// first verify the audit/backup pair and its durable completion record,
    /// then compare the ancestor and final tip with Core before using this as
    /// part of an incremental audit. Existing generations are never modified.
    pub fn checkpoint_audit_prefix(&self, base: &Path, destination: &Path) -> Result<u32> {
        ensure!(
            std::fs::symlink_metadata(destination)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "audit checkpoint destination must be NEW"
        );
        ensure!(
            std::fs::symlink_metadata(base)?.file_type().is_dir(),
            "audit base must be a real directory"
        );
        let source = DB::open_for_read_only(&Options::default(), base, false)?;
        let identity = format!("thin-v1:{}", self.network);
        ensure!(
            source.get(b"identity")?.as_deref() == Some(identity.as_bytes()),
            "audit base network identity mismatch"
        );
        let value = source.get(b"tip")?.context("audit base tip missing")?;
        let height = u32::from_be_bytes(
            value
                .as_slice()
                .try_into()
                .context("invalid audit base tip")?,
        );
        ensure!(
            self.tip()?.is_some_and(|tip| height <= tip),
            "audit base is ahead of current index"
        );
        let meta = source
            .get(height_key(b'B', height))?
            .context("audit base header missing")?;
        ensure!(meta.len() >= 80, "invalid audit base header");
        let header: bitcoin::block::Header = deserialize(&meta[..80])?;
        ensure!(
            header.block_hash() == self.header(height)?.block_hash(),
            "audit base is not an ancestor of current index"
        );
        // A RocksDB checkpoint created from a read-only DB can omit WAL-backed
        // state. Copy a stable closed generation byte-for-byte instead, then
        // reopen the independent copy and compare every key (including undo).
        std::fs::create_dir(destination)?;
        let names = |dir: &Path| -> Result<Vec<std::ffi::OsString>> {
            let mut result = Vec::new();
            for item in std::fs::read_dir(dir)? {
                result.push(item?.file_name());
                ensure!(result.len() <= 100_000, "audit base file count limit");
            }
            result.sort();
            Ok(result)
        };
        let entries = names(base)?;
        ensure!(!entries.is_empty(), "empty audit base directory");
        for name in &entries {
            let src = base.join(name);
            let expected = crate::core_blockfile::file_fingerprint(&src)?;
            let mut input = std::fs::File::open(&src)?;
            ensure!(
                crate::core_blockfile::fingerprint(&input.metadata()?)? == expected,
                "audit base file changed before copying"
            );
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination.join(name))?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            ensure!(
                crate::core_blockfile::fingerprint(&input.metadata()?)? == expected
                    && crate::core_blockfile::file_fingerprint(&src)? == expected,
                "audit base file changed during copying"
            );
        }
        ensure!(
            names(base)? == entries,
            "audit base directory changed during copying"
        );
        std::fs::File::open(destination)?.sync_all()?;
        if let Some(parent) = destination.parent() {
            std::fs::File::open(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?
            .sync_all()?;
        }
        let copy = DB::open_for_read_only(&Options::default(), destination, false)?;
        let mut original_rows = source.iterator(IteratorMode::Start);
        let mut copied_rows = copy.iterator(IteratorMode::Start);
        loop {
            match (original_rows.next(), copied_rows.next()) {
                (None, None) => break,
                (Some(a), Some(b)) => ensure!(a? == b?, "audit prefix copy differs from source"),
                _ => anyhow::bail!("audit prefix copy key count differs"),
            }
        }
        Ok(height)
    }

    /// Full semantic replay into a NEW generation. Source and audit copy are preserved.
    pub fn audit(&self, core: &Core, path: &Path) -> Result<()> {
        self.audit_with_cancel(core, path, &std::sync::atomic::AtomicBool::new(false))
    }
    pub fn audit_with_cancel(
        &self,
        core: &Core,
        path: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        self.audit_with_verified_base(core, path, cancel, None)
    }

    /// Audit from a base selected by `retention::verified_audit_base`, or replay
    /// fully if no base exists or its branch no longer matches. RPC failures and
    /// inconsistent current Core/index state are errors, never fork evidence.
    pub fn audit_with_verified_base(
        &self,
        core: &Core,
        path: &Path,
        cancel: &std::sync::atomic::AtomicBool,
        base: Option<&Path>,
    ) -> Result<()> {
        ensure!(!path.exists(), "audit destination must be NEW");
        let tip = self.tip()?.context("empty index")?;
        let mut incremental = false;
        let mut first = Some(0u32);
        if let Some(base) = base {
            ensure!(
                std::fs::symlink_metadata(base)?.file_type().is_dir(),
                "audit base must be a real directory"
            );
            let source = DB::open_for_read_only(&Options::default(), base, false)?;
            let identity = format!("thin-v1:{}", self.network);
            ensure!(
                source.get(b"identity")?.as_deref() == Some(identity.as_bytes()),
                "audit base network identity mismatch"
            );
            let value = source.get(b"tip")?.context("audit base tip missing")?;
            let height = u32::from_be_bytes(
                value
                    .as_slice()
                    .try_into()
                    .context("invalid audit base tip")?,
            );
            if height <= tip {
                let meta = source
                    .get(height_key(b'B', height))?
                    .context("audit base header missing")?;
                ensure!(meta.len() >= 80, "invalid audit base header");
                let base_header: bitcoin::block::Header = deserialize(&meta[..80])?;
                let index_hash = self.header(height)?.block_hash();
                if base_header.block_hash() == index_hash {
                    audit_expect_hash(core.call("getblockhash", json!([height]))?, index_hash)?;
                    drop(source);
                    let copied_height = self.checkpoint_audit_prefix(base, path)?;
                    ensure!(copied_height == height, "audit base changed during copying");
                    incremental = true;
                    first = if height < tip { Some(height + 1) } else { None };
                }
            }
        }
        let other = Self::open(path, self.network)?;
        for h in first.into_iter().flat_map(|h| h..=tip) {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "semantic audit interrupted; partial audit generation preserved"
            );
            let hash = self.header(h)?.block_hash();
            audit_expect_hash(core.call("getblockhash", json!([h]))?, hash)?;
            let block = self.block_source(core, h, false)?;
            other.commit(h, &block)?;
        }
        audit_expect_hash(
            core.call("getbestblockhash", json!([]))?,
            self.header(tip)?.block_hash(),
        )?;
        // A rewind can leave fewer than 288 retained undos; this is not a
        // confirmed-state difference. Compare every retained undo separately.
        let mut a = self
            .db
            .iterator(IteratorMode::Start)
            .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
        let mut b = other
            .db
            .iterator(IteratorMode::Start)
            .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
        loop {
            match (a.next(), b.next()) {
                (None, None) => break,
                (Some(x), Some(y)) => {
                    ensure!(x? == y?, "semantic replay differs from index");
                }
                _ => anyhow::bail!("semantic replay key count mismatch"),
            }
        }
        for (k, v) in self.rows(b"U", WINDOW as usize)? {
            ensure!(
                other.db.get(k)?.as_deref() == Some(v.as_slice()),
                "retained undo differs from semantic replay"
            );
        }
        let message = if incremental {
            crate::i18n::text(
                "Incremental semantic replay verified at height",
                "Reproducción semántica incremental verificada en la altura",
            )
        } else {
            crate::i18n::text(
                "Semantic replay verified at height",
                "Reproducción semántica verificada en la altura",
            )
        };
        println!("{message} {tip}: {}", path.display());
        Ok(())
    }
    pub fn backup(&self, path: &Path) -> Result<()> {
        ensure!(!path.exists(), "backup destination must be NEW");
        rocksdb::checkpoint::Checkpoint::new(&self.db)?.create_checkpoint(path)?;
        let check = DB::open_for_read_only(&Options::default(), path, false)?;
        ensure!(
            check.get(b"identity")? == self.db.get(b"identity")?
                && check.get(b"tip")? == self.db.get(b"tip")?,
            "backup verification failed"
        );
        for item in check.iterator(IteratorMode::Start) {
            let (k, v) = item?;
            ensure!(
                self.db.get(&k)?.as_deref() == Some(v.as_ref()),
                "backup content mismatch"
            );
        }
        std::fs::File::open(path)?.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?
            .sync_all()?;
        }
        println!(
            "{}: {} ({})",
            crate::i18n::text("Verified checkpoint", "Copia de seguridad verificada"),
            path.display(),
            crate::i18n::text(
                "historical cache is replaceable and not included",
                "la caché histórica es reemplazable y no está incluida"
            )
        );
        Ok(())
    }
    pub fn open(path: &Path, network: Network) -> Result<Self> {
        ensure!(!path.is_symlink(), "index symlink refused");
        std::fs::create_dir_all(path)?;
        let mut o = Options::default();
        o.create_if_missing(true);
        o.set_write_buffer_size(32 * 1024 * 1024);
        let db = DB::open(&o, path)?;
        let identity = format!("thin-v1:{network}");
        if let Some(v) = db.get(b"identity")? {
            ensure!(v == identity.as_bytes(), "wrong network or database format");
        } else {
            ensure!(
                db.iterator(IteratorMode::Start).next().is_none(),
                "unrecognized nonempty database"
            );
            db.put_opt(b"identity", identity, &write_options())?;
        }
        Ok(Self {
            db,
            network,
            fetcher: crate::net::Fetcher {
                peers: Vec::new(),
                proxy: None,
            },
            cache: path.parent().context("index parent")?.join("cache"),
        })
    }
    pub fn tip(&self) -> Result<Option<u32>> {
        self.db
            .get(b"tip")?
            .map(|b| Ok(u32::from_be_bytes(b.as_slice().try_into()?)))
            .transpose()
    }
    pub fn rows(&self, prefix: &[u8], max: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for row in self
            .db
            .iterator(IteratorMode::From(prefix, Direction::Forward))
        {
            let (k, v) = row?;
            if !k.starts_with(prefix) {
                break;
            }
            ensure!(out.len() < max, "query limit exceeded; no partial response");
            out.push((k.to_vec(), v.to_vec()));
        }
        Ok(out)
    }
    pub fn block_meta(&self, h: u32) -> Result<Vec<u8>> {
        let meta = self
            .db
            .get(height_key(b'B', h))?
            .context("height not indexed")?;
        ensure!(
            meta.len() >= 112 && (meta.len() - 80) % 32 == 0,
            "corrupt block metadata"
        );
        Ok(meta)
    }
    pub fn header(&self, h: u32) -> Result<bitcoin::block::Header> {
        Ok(deserialize(&self.block_meta(h)?[..80])?)
    }
    pub fn commit(&self, h: u32, block: &Block) -> Result<()> {
        self.commit_with_sync(h, block, true)
    }
    fn commit_with_sync(&self, h: u32, block: &Block, durable: bool) -> Result<()> {
        crate::net::verify_at(block, block.block_hash(), self.network, h)?;
        match self.tip()? {
            Some(t) => ensure!(
                h == t + 1 && block.header.prev_blockhash == self.header(t)?.block_hash(),
                "chain discontinuity"
            ),
            None => ensure!(
                h == 0
                    && block.block_hash()
                        == bitcoin::blockdata::constants::genesis_block(self.network).block_hash(),
                "wrong genesis"
            ),
        }
        let mut delta: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut meta = serialize(&block.header);
        for (pos, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            meta.extend(txid.as_byte_array());
            let mut occurrence = h.to_be_bytes().to_vec();
            occurrence.extend((pos as u32).to_be_bytes());
            let mut tk = key(b'T', &txid.as_byte_array()[..8]);
            tk.extend(&occurrence);
            delta.insert(tk, Some(Vec::new()));
            let mut scripts = std::collections::BTreeSet::new();
            if !tx.is_coinbase() {
                for input in &tx.input {
                    let outpoint = serialize(&input.previous_output);
                    let ck = key(b'C', &outpoint);
                    let coin = match delta.get(&ck) {
                        Some(v) => v.clone(),
                        None => self.db.get(&ck)?,
                    }
                    .context("missing prevout")?;
                    ensure!(coin.len() == 48, "corrupt coin");
                    let script: [u8; 32] = coin[..32].try_into()?;
                    scripts.insert(script);
                    let mut sk = key(b'S', &script);
                    ensure!(coin.len() == 48, "corrupt coin");
                    sk.extend(&coin[40..48]);
                    sk.extend(input.previous_output.vout.to_be_bytes());
                    delta.insert(ck, None);
                    delta.insert(sk, None);
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let script = script_hash(output.script_pubkey.as_bytes());
                scripts.insert(script);
                // History includes unspendable outputs; only the UTXO set excludes them.
                if h == 0
                    || output.script_pubkey.is_op_return()
                    || output.script_pubkey.len() > 10_000
                {
                    continue;
                }
                let op = bitcoin::OutPoint::new(txid, vout as u32);
                let bytes = serialize(&op);
                let ck = key(b'C', &bytes);
                let previous = match delta.get(&ck) {
                    Some(v) => v.clone(),
                    None => self.db.get(&ck)?,
                };
                if let Some(old) = previous {
                    ensure!(old.len() == 48, "corrupt previous coin");
                    let mut old_script = key(b'S', &old[..32]);
                    old_script.extend(&old[40..48]);
                    old_script.extend((vout as u32).to_be_bytes());
                    delta.insert(old_script, None);
                }
                let mut coin = script.to_vec();
                coin.extend(output.value.to_sat().to_le_bytes());
                coin.extend(h.to_be_bytes());
                coin.extend((pos as u32).to_be_bytes());
                delta.insert(ck, Some(coin.clone()));
                let mut sk = key(b'S', &script);
                sk.extend(&occurrence);
                sk.extend((vout as u32).to_be_bytes());
                delta.insert(sk, Some(coin[32..40].to_vec()));
            }
            if h > 0 {
                for script in scripts {
                    let mut hk = key(b'H', &script);
                    hk.extend(&occurrence);
                    delta.insert(hk, Some(Vec::new()));
                }
            }
        }
        delta.insert(height_key(b'B', h), Some(meta));
        delta.insert(b"tip".to_vec(), Some(h.to_be_bytes().to_vec()));
        let mut undo = Vec::new();
        for k in delta.keys() {
            undo.extend((k.len() as u32).to_le_bytes());
            undo.extend(k);
            match self.db.get(k)? {
                Some(v) => {
                    undo.extend((v.len() as u32).to_le_bytes());
                    undo.extend(v);
                }
                None => undo.extend(u32::MAX.to_le_bytes()),
            }
        }
        let mut batch = WriteBatch::default();
        for (k, v) in delta {
            match v {
                Some(v) => batch.put(k, v),
                None => batch.delete(k),
            }
        }
        batch.put(height_key(b'U', h), undo);
        if h >= WINDOW {
            batch.delete(height_key(b'U', h - WINDOW));
        }
        let mut options = write_options();
        options.set_sync(durable);
        self.db.write_opt(batch, &options)?;
        Ok(())
    }
    pub fn rewind_one(&self) -> Result<()> {
        let h = self.tip()?.context("empty index")?;
        let undo = self
            .db
            .get(height_key(b'U', h))?
            .context("undo unavailable: restore backup or rebuild into NEW root")?;
        let mut bytes = undo.as_slice();
        let mut batch = WriteBatch::default();
        fn number(b: &mut &[u8]) -> Result<u32> {
            ensure!(b.len() >= 4, "corrupt undo");
            let n = u32::from_le_bytes(b[..4].try_into()?);
            *b = &b[4..];
            Ok(n)
        }
        while !bytes.is_empty() {
            let n = number(&mut bytes)? as usize;
            ensure!(bytes.len() >= n, "corrupt undo key");
            let k = bytes[..n].to_vec();
            bytes = &bytes[n..];
            let n = number(&mut bytes)?;
            if n == u32::MAX {
                batch.delete(k);
            } else {
                let n = n as usize;
                ensure!(bytes.len() >= n, "corrupt undo value");
                batch.put(k, &bytes[..n]);
                bytes = &bytes[n..];
            }
        }
        batch.delete(height_key(b'U', h));
        self.db.write_opt(batch, &write_options())?;
        Ok(())
    }
    pub fn sync(&self, core: &Core) -> Result<()> {
        self.sync_batch(core, u32::MAX)
    }
    pub fn sync_batch(&self, core: &Core, limit: u32) -> Result<()> {
        self.sync_batch_with_cancel(core, limit, &std::sync::atomic::AtomicBool::new(false))
    }
    pub fn sync_batch_with_cancel(
        &self,
        core: &Core,
        limit: u32,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        let info = core.call("getblockchaininfo", json!([]))?;
        let expected = match self.network {
            Network::Bitcoin => "main",
            Network::Testnet4 => "testnet4",
            Network::Signet => "signet",
            Network::Regtest => "regtest",
            _ => "unsupported",
        };
        ensure!(info["chain"] == expected, "Core network mismatch");
        // Detect deep reorg before changing anything.
        let mut ancestor = self.tip()?;
        let mut depth = 0;
        while let Some(h) = ancestor {
            if h as u64 <= info["blocks"].as_u64().context("missing Core height")?
                && core.call("getblockhash", json!([h]))?
                    == self.header(h)?.block_hash().to_string()
            {
                break;
            }
            depth += 1;
            ensure!(depth <= WINDOW, "reorg exceeds undo window");
            ancestor = h.checked_sub(1);
        }
        while self.tip()? != ancestor {
            self.rewind_one()?;
        }
        let core_height = info["blocks"].as_u64().context("missing height")? as u32;
        let start = self.tip()?.map_or(0, |h| h + 1);
        let end = core_height.min(start.saturating_add(limit.saturating_sub(1)));
        if start > end {
            return Ok(());
        }
        // A rendezvous channel allows exactly one prepared block to overlap
        // the writer. There is no unbounded queue and no second DB writer.
        // Core::call bounds each RPC response to 32 MiB. The producer exits
        // when the receiver is dropped on any writer error or cancellation.
        std::thread::scope(|scope| -> Result<()> {
            let (tx, rx) = std::sync::mpsc::sync_channel(0);
            let producer = scope.spawn(move || {
                let mut hashes = std::collections::VecDeque::new();
                for h in start..=end {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let begun = std::time::Instant::now();
                    let mut hash_time = std::time::Duration::ZERO;
                    let result = (|| -> Result<Block> {
                        if hashes.is_empty() {
                            hashes.extend(core.block_hashes(h, (end - h).min(15) + 1)?);
                            hash_time = begun.elapsed();
                        }
                        let hash = hashes.pop_front().context("missing prefetched hash")?;
                        let block = core.block(&hash.to_string()).context(
                            "Core pruned required history: stop and recover missing blocks; never skip heights",
                        )?;
                        ensure!(block.block_hash() == hash, "RPC block hash mismatch");
                        Ok(block)
                    })();
                    let failed = result.is_err();
                    if tx.send((h, result, begun.elapsed(), hash_time)).is_err() || failed {
                        break;
                    }
                }
            });
            let mut fetch_time = std::time::Duration::ZERO;
            let mut hash_time = std::time::Duration::ZERO;
            let mut write_time = std::time::Duration::ZERO;
            let begun = std::time::Instant::now();
            let result = (|| -> Result<()> {
                for expected in start..=end {
                    ensure!(
                        !cancel.load(std::sync::atomic::Ordering::Relaxed),
                        "synchronization interrupted"
                    );
                    let (h, block, elapsed, hash_elapsed) =
                        rx.recv().context("block reader stopped unexpectedly")?;
                    ensure!(h == expected, "block reader returned unexpected height");
                    let block = block?;
                    fetch_time += elapsed;
                    hash_time += hash_elapsed;
                    let writing = std::time::Instant::now();
                    self.commit_with_sync(h, &block, h == end || (h - start + 1) % 64 == 0)?;
                    write_time += writing.elapsed();
                }
                Ok(())
            })();
            drop(rx);
            producer
                .join()
                .map_err(|_| anyhow::anyhow!("block reader panicked"))?;
            result?;
            if start / 1000 != end / 1000 || start == 0 {
                let completed = u64::from(end) + 1;
                let total = u64::from(core_height) + 1;
                let percent = completed as f64 * 100.0 / total as f64;
                eprintln!("{} {completed}/{total} ({percent:.2}%)", crate::i18n::text("indexed", "indexados"));
                eprintln!(
                    "  {}: {} | {}: {} ms | {}: {} ms | {}: {} ms | {}: {} ms ({})",
                    crate::i18n::text("batch blocks", "bloques del lote"), end - start + 1,
                    crate::i18n::text("total", "total"), begun.elapsed().as_millis(),
                    crate::i18n::text("fetch", "descarga"), fetch_time.as_millis(),
                    crate::i18n::text("hashes", "hashes"), hash_time.as_millis(),
                    crate::i18n::text("index", "índice"), write_time.as_millis(),
                    crate::i18n::text("fetch and indexing overlap", "descarga e indexado se solapan")
                );
            }
            Ok(())
        })?;
        Ok(())
    }
    pub fn history(&self, script: &[u8; 32]) -> Result<Value> {
        let rows = self.rows(&key(b'H', script), 100_000)?;
        let mut result = Vec::new();
        let mut previous = None;
        let mut meta = Vec::new();
        for (k, _) in rows {
            ensure!(k.len() == 41, "corrupt history key");
            let h = u32::from_be_bytes(k[33..37].try_into()?);
            let pos = u32::from_be_bytes(k[37..41].try_into()?);
            if previous != Some(h) {
                meta = self.block_meta(h)?;
                previous = Some(h);
            }
            let offset = 80 + pos as usize * 32;
            let txid = Txid::from_slice(
                meta.get(offset..offset + 32)
                    .context("corrupt history position")?,
            )?;
            result.push(json!({"height":h,"tx_hash":txid.to_string()}));
        }
        Ok(json!(result))
    }
    pub fn unspent(&self, script: &[u8; 32]) -> Result<Value> {
        let mut out = Vec::new();
        let mut previous = None;
        let mut meta = Vec::new();
        for (k, v) in self.rows(&key(b'S', script), 100_000)? {
            ensure!(k.len() == 45 && v.len() == 8, "corrupt unspent entry");
            let h = u32::from_be_bytes(k[33..37].try_into()?);
            let pos = u32::from_be_bytes(k[37..41].try_into()?);
            let vout = u32::from_be_bytes(k[41..45].try_into()?);
            if previous != Some(h) {
                meta = self.block_meta(h)?;
                previous = Some(h);
            }
            let offset = 80 + pos as usize * 32;
            let txid = Txid::from_slice(
                meta.get(offset..offset + 32)
                    .context("corrupt unspent position")?,
            )?;
            out.push(json!({"tx_hash":txid.to_string(),"tx_pos":vout,"value":u64::from_le_bytes(v[..8].try_into()?),"height":h}));
        }
        Ok(Value::Array(out))
    }
    pub fn locate(&self, txid: Txid) -> Result<(u32, u32)> {
        self.locate_at(txid, None)
    }
    pub fn locate_at(&self, txid: Txid, height: Option<u32>) -> Result<(u32, u32)> {
        let rows = self.rows(&key(b'T', &txid.as_byte_array()[..8]), 100)?;
        for (k, _) in rows.into_iter().rev() {
            ensure!(k.len() == 17, "corrupt txid entry");
            let h = u32::from_be_bytes(k[9..13].try_into()?);
            if height.is_some_and(|wanted| wanted != h) {
                continue;
            }
            let pos = u32::from_be_bytes(k[13..17].try_into()?);
            let meta = self.block_meta(h)?;
            let offset = 80 + pos as usize * 32;
            if meta.get(offset..offset + 32) == Some(txid.as_byte_array().as_slice()) {
                return Ok((h, pos));
            }
        }
        anyhow::bail!("transaction not indexed")
    }
    pub fn transaction(&self, core: &Core, txid: Txid) -> Result<Transaction> {
        let (h, _) = self.locate(txid)?;
        self.block_at(core, h)?
            .txdata
            .into_iter()
            .find(|t| t.compute_txid() == txid)
            .context("transaction absent from block")
    }
    fn block_at(&self, core: &Core, h: u32) -> Result<Block> {
        self.block_source(core, h, true)
    }
    fn block_source(&self, core: &Core, h: u32, persist: bool) -> Result<Block> {
        let hash = self.header(h)?.block_hash();
        if self.cache.join(format!("{hash}.blk")).exists() {
            let block = self.fetcher.get(self.network, hash, h, &self.cache)?;
            return Ok(block);
        }
        let block = match core.block(&hash.to_string()) {
            Ok(b) => {
                crate::net::verify_at(&b, hash, self.network, h)?;
                b
            }
            Err(_) => {
                if self.fetcher.peers.is_empty() {
                    let peers = core.call("getpeerinfo", json!([]))?;
                    let peers = peers
                        .as_array()
                        .context("peer list")?
                        .iter()
                        .filter_map(|p| p["addr"].as_str().map(str::to_owned))
                        .collect();
                    crate::net::Fetcher {
                        peers,
                        proxy: self.fetcher.proxy.clone(),
                    }
                    .retrieve_for_index(
                        self.network,
                        hash,
                        h,
                        &self.cache,
                        persist,
                    )?
                } else {
                    self.fetcher
                        .retrieve_for_index(self.network, hash, h, &self.cache, persist)?
                }
            }
        };
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audit_distinguishes_chain_change_from_invalid_reply() {
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest).block_hash();
        let other = bitcoin::blockdata::constants::genesis_block(Network::Bitcoin).block_hash();
        assert!(audit_expect_hash(json!(genesis.to_string()), genesis).is_ok());
        assert!(audit_expect_hash(json!(other.to_string()), genesis)
            .unwrap_err()
            .downcast_ref::<AuditTipChanged>()
            .is_some());
        assert!(audit_expect_hash(json!("invalid"), genesis)
            .unwrap_err()
            .downcast_ref::<AuditTipChanged>()
            .is_none());
        assert!(audit_expect_hash(Value::Null, genesis)
            .unwrap_err()
            .downcast_ref::<AuditTipChanged>()
            .is_none());
    }
    #[test]
    fn prepared_snapshot_rejects_tip_or_state_changes() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-snapshot-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let live = Index::open(&root.join("live"), Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        live.commit(0, &genesis)?;
        let snapshot = root.join("snapshot");
        live.backup(&snapshot)?;
        assert_eq!(
            live.verify_snapshot_matches(&snapshot)?,
            (0, genesis.block_hash())
        );
        // A change to an arbitrary row at the same tip must also be detected.
        live.db.put(b"unexpected", b"changed")?;
        assert!(live.verify_snapshot_matches(&snapshot).is_err());
        live.db.delete(b"unexpected")?;
        assert!(live.verify_snapshot_matches(&snapshot).is_ok());
        let mut next = genesis.clone();
        next.header.prev_blockhash = genesis.block_hash();
        next.header.time += 1;
        live.commit(1, &next)?;
        assert!(live.verify_snapshot_matches(&snapshot).is_err());
        assert!(snapshot.exists(), "rejected snapshots must be preserved");
        drop(live);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn prepared_audit_requires_three_matching_independent_states() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-prepared-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let live = Index::open(&root.join("live"), Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        live.commit(0, &genesis)?;
        let snapshot = root.join("snapshot");
        let audit = root.join("audit");
        let backup = root.join("backup");
        live.create_snapshot(&snapshot)?;
        live.create_snapshot(&audit)?;
        {
            let frozen = Index::open(&snapshot, Network::Regtest)?;
            frozen.backup(&backup)?;
        }
        assert_eq!(
            live.verify_snapshot_matches(&backup)?,
            (0, genesis.block_hash())
        );
        {
            let changed = Index::open(&backup, Network::Regtest)?;
            changed.db.put(height_key(b'U', 0), b"altered undo")?;
        }
        assert!(
            live.verify_snapshot_matches(&backup).is_err(),
            "same-tip backup with different undo must be rejected"
        );
        assert_eq!(
            live.verify_prepared_audit(&snapshot, &audit)?,
            (0, genesis.block_hash())
        );
        assert!(live.verify_prepared_audit(&snapshot, &snapshot).is_err());
        live.db.put(b"unexpected", b"changed")?;
        assert!(live.verify_prepared_audit(&snapshot, &audit).is_err());
        live.db.delete(b"unexpected")?;
        {
            let changed = Index::open(&audit, Network::Regtest)?;
            changed.db.put(b"unexpected", b"changed")?;
        }
        assert!(live.verify_prepared_audit(&snapshot, &audit).is_err());
        assert!(
            snapshot.exists() && audit.exists(),
            "rejected data must be preserved"
        );
        drop(live);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn incremental_audit_replays_suffix_and_handles_reorg_and_rpc_failure() -> Result<()> {
        use std::io::{Read, Write};
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        let root = std::env::temp_dir().join(format!(
            "el-incremental-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let current = Index::open(&root.join("current"), Network::Regtest)?;
        let base = Index::open(&root.join("base"), Network::Regtest)?;
        let fork = Index::open(&root.join("fork"), Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        let mut one = genesis.clone();
        one.header.prev_blockhash = genesis.block_hash();
        one.header.time += 1;
        let mut two = one.clone();
        two.header.prev_blockhash = one.block_hash();
        two.header.time += 1;
        let mut alternative = one.clone();
        alternative.header.time += 10;
        for index in [&current, &base, &fork] {
            index.commit(0, &genesis)?;
        }
        current.commit(1, &one)?;
        current.commit(2, &two)?;
        base.commit(1, &one)?;
        fork.commit(1, &alternative)?;
        drop(base);
        drop(fork);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop_server = Arc::new(AtomicBool::new(false));
        let fail_rpc = Arc::new(AtomicBool::new(false));
        let blocks_read = Arc::new(AtomicUsize::new(0));
        let stop_worker = stop_server.clone();
        let fail_worker = fail_rpc.clone();
        let reads_worker = blocks_read.clone();
        let blocks = [genesis, one, two];
        let worker = std::thread::spawn(move || -> Result<()> {
            while !stop_worker.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte)?;
                    headers.push(byte[0]);
                    ensure!(headers.len() < 16384, "test RPC headers too large");
                }
                let headers = std::str::from_utf8(&headers)?;
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                    .context("test RPC content length")?
                    .parse()?;
                let mut body = vec![0; length];
                stream.read_exact(&mut body)?;
                let request: Value = serde_json::from_slice(&body)?;
                let method = request["method"].as_str().context("test RPC method")?;
                let mut error = Value::Null;
                let result = match method {
                    "getblockhash" if fail_worker.load(Ordering::Relaxed) => {
                        error = json!({"code":-28});
                        Value::Null
                    }
                    "getblockhash" => json!(blocks
                        [request["params"][0].as_u64().context("height")? as usize]
                        .block_hash()
                        .to_string()),
                    "getbestblockhash" => {
                        json!(blocks.last().context("blocks")?.block_hash().to_string())
                    }
                    "getblock" => {
                        reads_worker.fetch_add(1, Ordering::Relaxed);
                        let hash = request["params"][0].as_str().context("hash")?;
                        let block = blocks
                            .iter()
                            .find(|b| b.block_hash().to_string() == hash)
                            .context("missing mock block")?;
                        json!(hex::encode(serialize(block)))
                    }
                    _ => anyhow::bail!("unexpected mock RPC"),
                };
                let response =
                    json!({"id":request["id"],"result":result,"error":error}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )?;
            }
            Ok(())
        });
        let cookie = root.join("cookie");
        std::fs::write(&cookie, "user:password")?;
        let core = Core::new(format!("http://{address}"), cookie)?;
        let cancel = AtomicBool::new(false);
        current.audit_with_verified_base(
            &core,
            &root.join("incremental"),
            &cancel,
            Some(&root.join("base")),
        )?;
        assert_eq!(
            blocks_read.swap(0, Ordering::Relaxed),
            1,
            "only the new block should be fetched"
        );
        current.audit_with_verified_base(
            &core,
            &root.join("reorg-full"),
            &cancel,
            Some(&root.join("fork")),
        )?;
        assert_eq!(
            blocks_read.swap(0, Ordering::Relaxed),
            3,
            "fork must replay from genesis"
        );
        fail_rpc.store(true, Ordering::Relaxed);
        assert!(current
            .audit_with_verified_base(
                &core,
                &root.join("rpc-failed"),
                &cancel,
                Some(&root.join("base"))
            )
            .is_err());
        assert!(!root.join("rpc-failed").exists());
        fail_rpc.store(false, Ordering::Relaxed);
        {
            let db = DB::open(&Options::default(), root.join("base"))?;
            db.put(b"unexpected", b"corruption")?;
        }
        assert!(current
            .audit_with_verified_base(
                &core,
                &root.join("corrupt"),
                &cancel,
                Some(&root.join("base"))
            )
            .is_err());
        assert!(root.join("corrupt").exists(), "partial audit is preserved");
        stop_server.store(true, Ordering::Relaxed);
        worker.join().expect("mock RPC worker")?;
        drop(current);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn checkpoint_audit_prefix_preserves_source_and_rejects_forks() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-audit-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let base_path = root.join("base");
        let current_path = root.join("current");
        let fork_path = root.join("fork");
        let copy_path = root.join("copy");
        let current = Index::open(&current_path, Network::Regtest)?;
        let base = Index::open(&base_path, Network::Regtest)?;
        let fork = Index::open(&fork_path, Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        let mut first = genesis.clone();
        first.header.prev_blockhash = genesis.block_hash();
        first.header.time += 1;
        let mut second = first.clone();
        second.header.prev_blockhash = first.block_hash();
        second.header.time += 1;
        let mut alternative = first.clone();
        alternative.header.time += 10;
        for index in [&base, &current, &fork] {
            index.commit(0, &genesis)?;
        }
        base.commit(1, &first)?;
        current.commit(1, &first)?;
        current.commit(2, &second)?;
        fork.commit(1, &alternative)?;
        drop(base);
        drop(fork);
        assert!(current
            .checkpoint_audit_prefix(&fork_path, &root.join("bad-copy"))
            .is_err());
        assert!(!root.join("bad-copy").exists());
        assert_eq!(current.checkpoint_audit_prefix(&base_path, &copy_path)?, 1);
        assert!(current
            .checkpoint_audit_prefix(&base_path, &copy_path)
            .is_err());
        let copied = Index::open(&copy_path, Network::Regtest)?;
        assert_eq!(copied.tip()?, Some(1));
        copied.commit(2, &second)?;
        drop(copied);
        assert_eq!(Index::open(&base_path, Network::Regtest)?.tip()?, Some(1));
        drop(current);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn genesis_and_rewind() -> Result<()> {
        let path = std::env::temp_dir().join(format!("el-thin-test-{}", std::process::id()));
        let i = Index::open(&path, Network::Regtest)?;
        let b = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        i.commit(0, &b)?;
        assert_eq!(i.tip()?, Some(0));
        assert_eq!(i.header(0)?, b.header);
        assert!(i.commit(2, &b).is_err());
        i.rewind_one()?;
        assert_eq!(i.tip()?, None);
        i.commit(0, &b)?;
        let mut b1 = b.clone();
        b1.header.prev_blockhash = b.block_hash();
        b1.header.time += 1;
        i.commit(1, &b1)?;
        let mut b2 = b1.clone();
        b2.header.prev_blockhash = b1.block_hash();
        b2.header.time += 1;
        i.commit(2, &b2)?;
        let script = script_hash(b.txdata[0].output[0].script_pubkey.as_bytes());
        assert_eq!(i.history(&script)?.as_array().expect("array").len(), 2);
        let coins = i.unspent(&script)?;
        assert_eq!(coins.as_array().expect("array").len(), 1);
        assert_eq!(coins[0]["height"], 2);
        assert_eq!(i.locate(b.txdata[0].compute_txid())?, (2, 0));
        i.rewind_one()?;
        assert_eq!(i.unspent(&script)?[0]["height"], 1);
        drop(i);
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
