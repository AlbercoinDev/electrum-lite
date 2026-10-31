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
/// 192-bit script hash. Collision probability for the entire chain is far
/// below any practical concern, while saving 8 bytes per history occurrence.
pub const SCRIPT_HASH_LEN: usize = 24;
pub type ScriptHash = [u8; SCRIPT_HASH_LEN];
pub const IDENTITY_PREFIX: &str = "thin-v3-txnum";
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StorageProfile {
    #[default]
    Hdd,
    Ssd,
}
/// Conservative auto-detection for the data directory's block device. If the
/// rotational flag cannot be read, HDD settings are used.
pub fn detect_storage_profile(path: &Path) -> StorageProfile {
    let mut current = path;
    let metadata = loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) => break metadata,
            Err(_) => match current.parent() {
                Some(parent) => current = parent,
                None => return StorageProfile::Hdd,
            },
        }
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = metadata.dev();
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        if let Ok(value) =
            std::fs::read_to_string(format!("/sys/dev/block/{major}:{minor}/queue/rotational"))
        {
            return match value.trim() {
                "0" => StorageProfile::Ssd,
                _ => StorageProfile::Hdd,
            };
        }
    }
    StorageProfile::Hdd
}
#[derive(Clone, Copy, Debug, Default)]
pub struct TxNumMeta {
    pub first: u32,
    pub count: u32,
}
pub const SPENT_UNDO_MARKER: u8 = 0xff;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpentOut {
    pub script: ScriptHash,
    pub value: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyCore {
    Hash,
    Full,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpentMode {
    Auto,
    Always,
    Never,
}
#[derive(Clone, Copy, Debug)]
pub struct IndexOptions {
    pub storage: StorageProfile,
    pub block_cache_mib: u64,
    pub write_buffer_mib: u64,
    pub memtables: u32,
    pub rocks_jobs: u32,
    pub rocks_subcompactions: u32,
    pub index_workers: usize,
    pub fast_sync: bool,
    pub verify_core: VerifyCore,
    pub txpos: bool,
    pub spent_mode: SpentMode,
}
impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            storage: StorageProfile::Hdd,
            block_cache_mib: 512,
            write_buffer_mib: 64,
            memtables: 3,
            rocks_jobs: 1,
            rocks_subcompactions: 1,
            index_workers: 1,
            fast_sync: false,
            verify_core: VerifyCore::Full,
            txpos: true,
            spent_mode: SpentMode::Auto,
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub last_batch_blocks: u32,
    pub last_batch_millis: u64,
    pub blocks_per_second: f64,
    pub mib_per_second: f64,
    pub bulk: bool,
    pub durable_height: Option<u32>,
}
pub struct Index {
    pub db: DB,
    pub network: Network,
    pub fetcher: crate::net::Fetcher,
    pub cache: std::path::PathBuf,
    pub public: Option<crate::public_data::Client>,
    pub storage: StorageProfile,
    tip_cache: std::sync::Mutex<Option<(u32, bitcoin::BlockHash)>>,
    txnum_index: std::sync::RwLock<Vec<TxNumMeta>>,
    options: IndexOptions,
    bulk: std::sync::atomic::AtomicBool,
    durable_tip: std::sync::Mutex<Option<u32>>,
    stats: std::sync::RwLock<IndexStats>,
}
#[derive(Default)]
struct CommitTimings {
    verify: std::time::Duration,
    prepare: std::time::Duration,
    undo: std::time::Duration,
    write: std::time::Duration,
    reused_preimages: u64,
}
struct PreparedBlock {
    height: u32,
    hash: bitcoin::BlockHash,
    delta: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    undo: Vec<u8>,
    meta: TxNumMeta,
}
pub fn script_hash(script: &[u8]) -> ScriptHash {
    let digest = sha256::Hash::hash(script).to_byte_array();
    digest[..SCRIPT_HASH_LEN]
        .try_into()
        .expect("checked hash length")
}
/// BIP30 duplicate-TXID outpoints can only predate BIP34 activation. Above
/// this height a fresh `(txid, vout)` cannot already be in the UTXO set, so we
/// avoid a database lookup for every output.
fn bip34_height(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 227_931,
        Network::Testnet => 21_111,
        _ => 1,
    }
}
pub fn key(prefix: u8, bytes: &[u8]) -> Vec<u8> {
    let mut k = vec![prefix];
    k.extend(bytes);
    k
}
pub fn height_key(prefix: u8, h: u32) -> Vec<u8> {
    key(prefix, &h.to_be_bytes())
}
fn load_txnum_index(db: &DB, tip: Option<u32>) -> Result<Vec<TxNumMeta>> {
    let Some(tip) = tip else {
        return Ok(Vec::new());
    };
    let mut result = vec![TxNumMeta::default(); tip as usize + 1];
    for row in db.iterator(IteratorMode::From(b"N", Direction::Forward)) {
        let (key, value) = row?;
        if !key.starts_with(b"N") {
            break;
        }
        ensure!(key.len() == 5 && value.len() == 8, "corrupt txnum metadata");
        let height = u32::from_be_bytes(key[1..5].try_into()?) as usize;
        ensure!(height < result.len(), "txnum height out of range");
        result[height] = TxNumMeta {
            first: u32::from_be_bytes(value[..4].try_into()?),
            count: u32::from_be_bytes(value[4..].try_into()?),
        };
    }
    for (height, meta) in result.iter().enumerate() {
        let expected = if height == 0 {
            0
        } else {
            result[height - 1].first + result[height - 1].count
        };
        ensure!(meta.first == expected, "txnum index is not contiguous");
    }
    Ok(result)
}
fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    ensure!(*offset < bytes.len(), "truncated varint");
    let first = bytes[*offset];
    *offset += 1;
    let (mut value, extra) = match first {
        0xfd => (0u64, 2),
        0xfe => (0u64, 4),
        0xff => (0u64, 8),
        n => return Ok(u64::from(n)),
    };
    ensure!(bytes.len() - *offset >= extra, "truncated varint");
    for i in 0..extra {
        value |= u64::from(bytes[*offset + i]) << (8 * i);
    }
    *offset += extra;
    Ok(value)
}
/// Parse Core's `CBlockUndo` (`/rest/spenttxouts`) for a block. Each entry is
/// the spent script and value, in input order, excluding the coinbase.
pub fn parse_spent(block: &Block, bytes: &[u8]) -> Result<Vec<Vec<SpentOut>>> {
    let mut offset = 0;
    let txs = read_varint(bytes, &mut offset)?;
    // Core returns an empty undo for the genesis block (height 0).
    if txs == 0 {
        ensure!(
            block.txdata.len() == 1 && offset == bytes.len(),
            "empty undo for non-genesis block"
        );
        return Ok(vec![Vec::new()]);
    }
    ensure!(txs as usize == block.txdata.len(), "undo tx count mismatch");
    let mut result = Vec::with_capacity(block.txdata.len());
    for tx in &block.txdata {
        let count = read_varint(bytes, &mut offset)?;
        if tx.is_coinbase() {
            ensure!(count == 0, "coinbase undo entries");
            result.push(Vec::new());
            continue;
        }
        ensure!(
            count as usize == tx.input.len(),
            "undo input count mismatch"
        );
        let mut spent = Vec::with_capacity(tx.input.len());
        for _ in 0..count {
            ensure!(bytes.len() - offset >= 8, "truncated spent value");
            let value = u64::from_le_bytes(bytes[offset..offset + 8].try_into()?);
            offset += 8;
            let script_len = read_varint(bytes, &mut offset)? as usize;
            ensure!(bytes.len() - offset >= script_len, "truncated spent script");
            spent.push(SpentOut {
                script: script_hash(&bytes[offset..offset + script_len]),
                value,
            });
            offset += script_len;
        }
        result.push(spent);
    }
    ensure!(offset == bytes.len(), "trailing spent data");
    Ok(result)
}
fn put_varint(out: &mut Vec<u8>, value: u32) {
    let mut v = u64::from(value);
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
fn get_varint(bytes: &[u8], offset: &mut usize) -> Result<u32> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        ensure!(*offset < bytes.len() && shift < 35, "corrupt txpos varint");
        let byte = bytes[*offset];
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(u32::try_from(value)?);
        }
        shift += 7;
    }
}
fn varint_len(value: u32) -> usize {
    let mut v = value;
    let mut len = 1;
    while v >= 0x80 {
        v >>= 7;
        len += 1;
    }
    len
}
fn encode_txpos(offsets: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(offsets.len() * 2 + 2);
    put_varint(&mut out, offsets.len() as u32);
    put_varint(&mut out, offsets.first().copied().unwrap_or((0, 0)).0);
    for (_, size) in offsets {
        put_varint(&mut out, *size);
    }
    out
}
fn spent_key(script: &ScriptHash, outpoint: &bitcoin::OutPoint) -> Vec<u8> {
    let mut key = key(b'S', script);
    key.extend(outpoint.txid.as_byte_array());
    key.extend(outpoint.vout.to_be_bytes());
    key
}
fn spent_marker(value: u64) -> Vec<u8> {
    let mut marker = vec![SPENT_UNDO_MARKER];
    marker.extend(value.to_le_bytes());
    marker
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
        // A checkpoint must contain everything visible through `tip`, even
        // when fast-sync has WAL disabled and rows are still in memtables.
        self.db.flush()?;
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
        let identity = format!("{IDENTITY_PREFIX}:{}", self.network);
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
        let identity = format!("{IDENTITY_PREFIX}:{}", self.network);
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
            let identity = format!("{IDENTITY_PREFIX}:{}", self.network);
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
        let other = Self::open_with_options(path, self.network, self.options)?;
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
                    let x = x?;
                    let y = y?;
                    if x != y {
                        anyhow::bail!(
                            "semantic replay differs from index at key {:02x?} (live value {} B, replay {} B)",
                            x.0.iter().take(6).collect::<Vec<_>>(),
                            x.1.len(),
                            y.1.len()
                        );
                    }
                }
                _ => anyhow::bail!("semantic replay key count mismatch"),
            }
        }
        for (k, v) in self.rows(b"U", WINDOW as usize)? {
            ensure!(
                other.db.get(&k)?.as_deref() == Some(v.as_slice()),
                "retained undo differs at height {}",
                u32::from_be_bytes(k[1..5].try_into()?)
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
        self.db.flush()?;
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
        Self::open_with_options(path, network, IndexOptions::default())
    }
    pub fn open_with_profile(
        path: &Path,
        network: Network,
        storage: StorageProfile,
    ) -> Result<Self> {
        Self::open_with_options(
            path,
            network,
            IndexOptions {
                storage,
                ..IndexOptions::default()
            },
        )
    }
    pub fn open_with_options(path: &Path, network: Network, options: IndexOptions) -> Result<Self> {
        let storage = options.storage;
        ensure!(!path.is_symlink(), "index symlink refused");
        std::fs::create_dir_all(path)?;
        let mut o = Options::default();
        o.create_if_missing(true);
        o.set_compaction_style(rocksdb::DBCompactionStyle::Level);
        o.set_write_buffer_size((options.write_buffer_mib * 1024 * 1024) as usize);
        o.set_max_write_buffer_number(options.memtables as i32);
        o.set_target_file_size_base(match storage {
            StorageProfile::Hdd => 256 * 1024 * 1024,
            StorageProfile::Ssd => 128 * 1024 * 1024,
        });
        o.set_max_open_files(128);
        o.set_max_background_jobs(options.rocks_jobs as i32);
        o.set_max_subcompactions(options.rocks_subcompactions);
        o.set_compaction_readahead_size(match storage {
            StorageProfile::Hdd => 8 * 1024 * 1024,
            StorageProfile::Ssd => 2 * 1024 * 1024,
        });
        o.set_bytes_per_sync(1024 * 1024);
        o.set_advise_random_on_open(false);
        o.set_level_compaction_dynamic_level_bytes(true);
        let mut block = rocksdb::BlockBasedOptions::default();
        block.set_block_cache(&rocksdb::Cache::new_lru_cache(
            (options.block_cache_mib * 1024 * 1024) as usize,
        ));
        block.set_bloom_filter(10.0, true);
        block.set_block_restart_interval(32);
        block.set_cache_index_and_filter_blocks(true);
        block.set_pin_l0_filter_and_index_blocks_in_cache(true);
        block.set_index_type(rocksdb::BlockBasedIndexType::TwoLevelIndexSearch);
        o.set_block_based_table_factory(&block);
        let db = DB::open(&o, path)?;
        let identity = format!("{IDENTITY_PREFIX}:{network}");
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
            public: None,
            storage,
            tip_cache: std::sync::Mutex::new(None),
            txnum_index: std::sync::RwLock::new(Vec::new()),
            options,
            bulk: std::sync::atomic::AtomicBool::new(false),
            durable_tip: std::sync::Mutex::new(None),
            stats: std::sync::RwLock::new(IndexStats::default()),
        })
        .and_then(|index| {
            let loaded = load_txnum_index(&index.db, index.tip()?)?;
            *index
                .txnum_index
                .write()
                .map_err(|_| anyhow::anyhow!("txnum index poisoned"))? = loaded;
            *index
                .durable_tip
                .lock()
                .map_err(|_| anyhow::anyhow!("durable tip poisoned"))? = index.tip()?;
            Ok(index)
        })
    }
    pub fn next_txnum(&self, h: u32) -> Result<u32> {
        let index = self
            .txnum_index
            .read()
            .map_err(|_| anyhow::anyhow!("txnum index poisoned"))?;
        if h == 0 {
            return Ok(0);
        }
        let previous = index
            .get(h as usize - 1)
            .context("txnum index missing previous height")?;
        Ok(previous.first + previous.count)
    }
    pub fn txnum_to_height(&self, txnum: u32) -> Result<(u32, u32)> {
        let index = self
            .txnum_index
            .read()
            .map_err(|_| anyhow::anyhow!("txnum index poisoned"))?;
        let height = index
            .partition_point(|meta| meta.first <= txnum)
            .checked_sub(1)
            .context("txnum outside index")?;
        let meta = index.get(height).context("txnum outside index")?;
        ensure!(
            txnum >= meta.first && txnum - meta.first < meta.count,
            "txnum outside block range"
        );
        Ok((height as u32, txnum - meta.first))
    }
    pub fn tx_position(&self, txnum: u32) -> Result<(u32, u32)> {
        let start = txnum.to_be_bytes();
        let mut iterator = self
            .db
            .iterator(IteratorMode::From(&key(b'P', &start), Direction::Forward));
        let (row_key, row_value) = iterator.next().context("txpos missing")??;
        ensure!(
            row_key.starts_with(b"P") && row_key.len() == 5,
            "corrupt txpos key"
        );
        let last = u32::from_be_bytes(row_key[1..5].try_into()?);
        let mut offset = 0;
        let count = get_varint(&row_value, &mut offset)? as usize;
        ensure!(count > 0 && last >= txnum, "corrupt txpos row");
        let first_offset = get_varint(&row_value, &mut offset)?;
        let first_txnum = last + 1 - count as u32;
        ensure!(txnum >= first_txnum, "txpos row does not contain txnum");
        let wanted = (txnum - first_txnum) as usize;
        let mut relative = 0u32;
        for index in 0..count {
            let size = get_varint(&row_value, &mut offset)?;
            if index == wanted {
                return Ok((first_offset + relative, size));
            }
            relative = relative.checked_add(size).context("txpos overflow")?;
        }
        anyhow::bail!("txpos entry missing")
    }
    fn funding_height(&self, txid: Txid) -> Result<u32> {
        let rows = self.rows(&key(b'T', &txid.as_byte_array()[..8]), 200)?;
        for (row, _) in rows.into_iter().rev() {
            ensure!(row.len() == 13, "corrupt txid entry");
            let txnum = u32::from_be_bytes(row[9..13].try_into()?);
            let (height, pos) = self.txnum_to_height(txnum)?;
            let meta = self.block_meta(height)?;
            let offset = 80 + pos as usize * 32;
            if meta.get(offset..offset + 32) == Some(txid.as_byte_array().as_slice()) {
                return Ok(height);
            }
        }
        anyhow::bail!("funding transaction not indexed")
    }
    pub fn set_bulk(&self, bulk: bool) -> Result<()> {
        if self.bulk.load(std::sync::atomic::Ordering::Relaxed) == bulk {
            return Ok(());
        }
        if bulk {
            self.db
                .set_options(&[("disable_auto_compactions", "true")])?;
            self.bulk.store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.db.flush()?;
            self.db
                .set_options(&[("disable_auto_compactions", "false")])?;
            self.bulk.store(false, std::sync::atomic::Ordering::Relaxed);
            let tip = self.tip()?;
            *self
                .durable_tip
                .lock()
                .map_err(|_| anyhow::anyhow!("durable tip poisoned"))? = tip;
        }
        if let Ok(mut stats) = self.stats.write() {
            stats.bulk = bulk;
            stats.durable_height = self.durable_tip()?;
        }
        Ok(())
    }
    pub fn durable_tip(&self) -> Result<Option<u32>> {
        if self.bulk.load(std::sync::atomic::Ordering::Relaxed) {
            Ok(*self
                .durable_tip
                .lock()
                .map_err(|_| anyhow::anyhow!("durable tip poisoned"))?)
        } else {
            self.tip()
        }
    }
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        let tip = self.tip()?;
        *self
            .durable_tip
            .lock()
            .map_err(|_| anyhow::anyhow!("durable tip poisoned"))? = tip;
        if let Ok(mut stats) = self.stats.write() {
            stats.durable_height = tip;
        }
        Ok(())
    }
    pub fn stats(&self) -> IndexStats {
        self.stats.read().map(|s| *s).unwrap_or_default()
    }
    pub fn options(&self) -> IndexOptions {
        self.options
    }
    fn record_stats(&self, blocks: u32, elapsed: std::time::Duration, mib: f64) {
        if let Ok(mut stats) = self.stats.write() {
            stats.last_batch_blocks = blocks;
            stats.last_batch_millis = elapsed.as_millis() as u64;
            stats.blocks_per_second = if elapsed.as_secs_f64() > 0.0 {
                f64::from(blocks) / elapsed.as_secs_f64()
            } else {
                0.0
            };
            stats.mib_per_second = if elapsed.as_secs_f64() > 0.0 {
                mib / elapsed.as_secs_f64()
            } else {
                0.0
            };
            stats.bulk = self.bulk.load(std::sync::atomic::Ordering::Relaxed);
            stats.durable_height = self.durable_tip().ok().flatten();
        }
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
        let mut overlay = std::collections::HashMap::new();
        self.commit_with_sync(
            h,
            block,
            true,
            true,
            None,
            &mut overlay,
            &mut CommitTimings::default(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare_block(
        &self,
        h: u32,
        block: &Block,
        verify: bool,
        spent: Option<&[Vec<SpentOut>]>,
        first_txnum: u32,
        old_tip: Option<u32>,
        overlay: &mut std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>,
        timings: &mut CommitTimings,
    ) -> Result<PreparedBlock> {
        let started = std::time::Instant::now();
        if verify {
            crate::net::verify_at(block, block.block_hash(), self.network, h)?;
        }
        timings.verify += started.elapsed();
        let started = std::time::Instant::now();
        let tx_count = block.txdata.len() as u32;
        let mut delta: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut preimages: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let remember = |preimages: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>,
                        key: &[u8],
                        value: Option<Vec<u8>>| {
            preimages.entry(key.to_vec()).or_insert(value);
        };
        remember(
            &mut preimages,
            b"tip",
            old_tip.map(|t| t.to_be_bytes().to_vec()),
        );
        let mut meta = serialize(&block.header);
        let duplicate_check = match self.network {
            Network::Bitcoin => h < bip34_height(self.network),
            Network::Testnet => h < bip34_height(self.network),
            // Regtest fixtures may deliberately re-mine identical coinbases;
            // those tests use the single-block commit path with an overlay.
            Network::Regtest => true,
            _ => false,
        };
        let mut positions = Vec::new();
        let mut offset = 80 + varint_len(tx_count) as u32;
        for (pos, tx) in block.txdata.iter().enumerate() {
            if self.options.txpos {
                let size = serialize(tx).len() as u32;
                positions.push((offset, size));
                offset = offset.checked_add(size).context("block size overflow")?;
            }
            let txid = tx.compute_txid();
            let txnum = first_txnum + pos as u32;
            meta.extend(txid.as_byte_array());
            let mut tk = key(b'T', &txid.as_byte_array()[..8]);
            tk.extend(txnum.to_be_bytes());
            delta.insert(tk, Some(Vec::new()));
            let mut scripts = std::collections::BTreeSet::new();
            let spent_tx = spent.and_then(|s| s.get(pos));
            if !tx.is_coinbase() {
                for (input_index, input) in tx.input.iter().enumerate() {
                    let ck = key(b'C', &serialize(&input.previous_output));
                    let (script, value, in_same_block) = if let Some(spent) = spent_tx {
                        let spent = spent
                            .get(input_index)
                            .context("spenttxouts missing input")?;
                        (
                            spent.script,
                            spent.value,
                            matches!(delta.get(&ck), Some(Some(_))),
                        )
                    } else {
                        match delta.get(&ck) {
                            Some(Some(v)) => {
                                ensure!(v.len() == 32, "corrupt coin");
                                (
                                    v[..SCRIPT_HASH_LEN].try_into()?,
                                    u64::from_le_bytes(v[SCRIPT_HASH_LEN..].try_into()?),
                                    true,
                                )
                            }
                            Some(None) => anyhow::bail!("prevout spent twice in block"),
                            None => match overlay.get(&ck) {
                                Some(Some(v)) => {
                                    ensure!(v.len() == 32, "corrupt coin");
                                    (
                                        v[..SCRIPT_HASH_LEN].try_into()?,
                                        u64::from_le_bytes(v[SCRIPT_HASH_LEN..].try_into()?),
                                        false,
                                    )
                                }
                                Some(None) => anyhow::bail!("prevout spent twice in batch"),
                                None => {
                                    let v = self.db.get(&ck)?.context("missing prevout")?;
                                    ensure!(v.len() == 32, "corrupt coin");
                                    remember(&mut preimages, &ck, Some(v.clone()));
                                    (
                                        v[..SCRIPT_HASH_LEN].try_into()?,
                                        u64::from_le_bytes(v[SCRIPT_HASH_LEN..].try_into()?),
                                        false,
                                    )
                                }
                            },
                        }
                    };
                    scripts.insert(script);
                    let mut coin = script.to_vec();
                    coin.extend(value.to_le_bytes());
                    let sk = spent_key(&script, &input.previous_output);
                    if in_same_block || delta.contains_key(&sk) {
                        remember(&mut preimages, &sk, None);
                    } else {
                        remember(&mut preimages, &sk, Some(spent_marker(value)));
                    }
                    if in_same_block {
                        remember(&mut preimages, &ck, None);
                    } else if !preimages.contains_key(&ck) {
                        remember(&mut preimages, &ck, Some(coin));
                    }
                    delta.insert(ck, None);
                    delta.insert(sk, None);
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let script = script_hash(output.script_pubkey.as_bytes());
                scripts.insert(script);
                if h == 0
                    || output.script_pubkey.is_op_return()
                    || output.script_pubkey.len() > 10_000
                {
                    continue;
                }
                let outpoint = bitcoin::OutPoint::new(txid, vout as u32);
                let ck = key(b'C', &serialize(&outpoint));
                let mut previous = None;
                if duplicate_check && !delta.contains_key(&ck) {
                    if let Some(old) = overlay.get(&ck).cloned().flatten() {
                        remember(&mut preimages, &ck, Some(old.clone()));
                        previous = Some(old);
                    } else {
                        let old = self.db.get(&ck)?;
                        remember(&mut preimages, &ck, old.clone());
                        previous = old;
                    }
                } else if !preimages.contains_key(&ck) {
                    remember(&mut preimages, &ck, None);
                }
                if let Some(old) = previous {
                    ensure!(old.len() == 32, "corrupt previous coin");
                    let old_script: ScriptHash = old[..SCRIPT_HASH_LEN].try_into()?;
                    let old_s = spent_key(&old_script, &outpoint);
                    let old_s_value = self.db.get(&old_s)?;
                    remember(&mut preimages, &old_s, old_s_value);
                    delta.insert(old_s, None);
                }
                let mut coin = script.to_vec();
                coin.extend(output.value.to_sat().to_le_bytes());
                delta.insert(ck, Some(coin));
                let sk = spent_key(&script, &outpoint);
                let mut s_value = h.to_be_bytes().to_vec();
                s_value.extend(output.value.to_sat().to_le_bytes());
                delta.insert(sk, Some(s_value));
            }
            if h > 0 {
                for script in scripts {
                    let mut hk = key(b'H', &script);
                    hk.extend(txnum.to_be_bytes());
                    delta.insert(hk, Some(Vec::new()));
                }
            }
        }
        delta.insert(height_key(b'B', h), Some(meta));
        let mut n = first_txnum.to_be_bytes().to_vec();
        n.extend(tx_count.to_be_bytes());
        delta.insert(height_key(b'N', h), Some(n));
        if self.options.txpos {
            for (chunk_index, chunk) in positions.chunks(64).enumerate() {
                let last_txnum = first_txnum + (chunk_index * 64 + chunk.len() - 1) as u32;
                let mut pk = vec![b'P'];
                pk.extend(last_txnum.to_be_bytes());
                delta.insert(pk, Some(encode_txpos(chunk)));
            }
        }
        delta.insert(b"tip".to_vec(), Some(h.to_be_bytes().to_vec()));
        for (k, v) in &delta {
            if k.first() == Some(&b'C') {
                overlay.insert(k.clone(), v.clone());
            }
        }
        timings.prepare += started.elapsed();
        let started = std::time::Instant::now();
        let mut undo = Vec::new();
        for k in delta.keys() {
            if self.options.fast_sync {
                continue;
            }
            undo.extend((k.len() as u32).to_le_bytes());
            undo.extend(k);
            let previous = preimages.get(k).cloned().flatten();
            if preimages.contains_key(k) {
                timings.reused_preimages += 1;
            }
            match previous {
                Some(v) => {
                    undo.extend((v.len() as u32).to_le_bytes());
                    undo.extend(v);
                }
                None => undo.extend(u32::MAX.to_le_bytes()),
            }
        }
        timings.undo += started.elapsed();
        Ok(PreparedBlock {
            height: h,
            hash: block.block_hash(),
            delta,
            undo,
            meta: TxNumMeta {
                first: first_txnum,
                count: tx_count,
            },
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn commit_with_sync(
        &self,
        h: u32,
        block: &Block,
        durable: bool,
        verify: bool,
        spent: Option<&[Vec<SpentOut>]>,
        overlay: &mut std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>,
        timings: &mut CommitTimings,
    ) -> Result<()> {
        let old_tip = self.tip()?;
        match old_tip {
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
        let first_txnum = self.next_txnum(h)?;
        let prepared = self.prepare_block(
            h,
            block,
            verify,
            spent,
            first_txnum,
            old_tip,
            overlay,
            timings,
        )?;
        self.apply_prepared(&[prepared], durable, timings)
    }
    fn apply_prepared(
        &self,
        prepared: &[PreparedBlock],
        durable: bool,
        timings: &mut CommitTimings,
    ) -> Result<()> {
        ensure!(!prepared.is_empty(), "empty prepared batch");
        let started = std::time::Instant::now();
        let mut batch = WriteBatch::default();
        for block in prepared {
            for (k, v) in &block.delta {
                match v {
                    Some(v) => batch.put(k, v),
                    None => batch.delete(k),
                }
            }
            if !self.options.fast_sync {
                batch.put(height_key(b'U', block.height), &block.undo);
                if block.height >= WINDOW {
                    batch.delete(height_key(b'U', block.height - WINDOW));
                }
            }
        }
        let mut options = write_options();
        if self.bulk.load(std::sync::atomic::Ordering::Relaxed) {
            options.disable_wal(true);
            options.set_sync(false);
        } else {
            options.set_sync(durable);
        }
        self.db.write_opt(batch, &options)?;
        {
            let mut index = self
                .txnum_index
                .write()
                .map_err(|_| anyhow::anyhow!("txnum index poisoned"))?;
            for block in prepared {
                ensure!(
                    index.len() == block.height as usize,
                    "txnum index height mismatch"
                );
                index.push(block.meta);
            }
        }
        if let Ok(mut cached) = self.tip_cache.lock() {
            if let Some(last) = prepared.last() {
                *cached = Some((last.height, last.hash));
            }
        }
        timings.write += started.elapsed();
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
                let value = &bytes[..n];
                if k.first() == Some(&b'S')
                    && k.len() == 61
                    && value.len() == 9
                    && value[0] == SPENT_UNDO_MARKER
                {
                    let txid = Txid::from_slice(&k[25..57])?;
                    let height = self.funding_height(txid)?;
                    let mut restored = height.to_be_bytes().to_vec();
                    restored.extend(&value[1..9]);
                    batch.put(k, restored);
                } else {
                    batch.put(k, value);
                }
                bytes = &bytes[n..];
            }
        }
        batch.delete(height_key(b'U', h));
        self.db.write_opt(batch, &write_options())?;
        if let Ok(mut index) = self.txnum_index.write() {
            ensure!(index.len() == h as usize + 1, "txnum index height mismatch");
            index.truncate(h as usize);
        }
        if let Ok(mut cached) = self.tip_cache.lock() {
            *cached = h
                .checked_sub(1)
                .map(|previous| {
                    self.header(previous)
                        .map(|header| (previous, header.block_hash()))
                })
                .transpose()?;
        }
        Ok(())
    }
    /// Reorg in fast-sync mode. Instead of reading per-key undo, re-read the
    /// stale block + spenttxouts, delete the rows it created and restore the
    /// UTXOs it consumed. Requires Core REST for the undo data.
    pub fn rewind_reindex(&self, core: &Core) -> Result<()> {
        let h = self.tip()?.context("empty index")?;
        let meta = *self
            .txnum_index
            .read()
            .map_err(|_| anyhow::anyhow!("txnum index poisoned"))?
            .get(h as usize)
            .context("txnum index missing height")?;
        let block = self.block_source(core, h, true)?;
        ensure!(
            block.txdata.len() as u32 == meta.count,
            "txnum count mismatch"
        );
        let rest = core.rest().context("fast-sync reorg requires Core REST")?;
        let bytes = rest.spent_txouts(block.block_hash())?;
        let spent = parse_spent(&block, &bytes)?;
        let mut batch = WriteBatch::default();
        for (pos, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            let txnum = meta.first + pos as u32;
            let mut tk = key(b'T', &txid.as_byte_array()[..8]);
            tk.extend(txnum.to_be_bytes());
            batch.delete(tk);
            let mut scripts = std::collections::BTreeSet::new();
            if !tx.is_coinbase() {
                for (input_index, input) in tx.input.iter().enumerate() {
                    let spent = spent
                        .get(pos)
                        .and_then(|s| s.get(input_index))
                        .context("spenttxouts missing reorg input")?;
                    scripts.insert(spent.script);
                    let funding = self.funding_height(input.previous_output.txid)?;
                    if funding < h {
                        let mut ck = key(b'C', &serialize(&input.previous_output));
                        let mut coin = spent.script.to_vec();
                        coin.extend(spent.value.to_le_bytes());
                        batch.put(&ck, &coin);
                        ck = spent_key(&spent.script, &input.previous_output);
                        let mut s_value = funding.to_be_bytes().to_vec();
                        s_value.extend(spent.value.to_le_bytes());
                        batch.put(ck, s_value);
                    }
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let script = script_hash(output.script_pubkey.as_bytes());
                scripts.insert(script);
                if h == 0
                    || output.script_pubkey.is_op_return()
                    || output.script_pubkey.len() > 10_000
                {
                    continue;
                }
                let outpoint = bitcoin::OutPoint::new(txid, vout as u32);
                batch.delete(key(b'C', &serialize(&outpoint)));
                batch.delete(spent_key(&script, &outpoint));
            }
            if h > 0 {
                for script in scripts {
                    let mut hk = key(b'H', &script);
                    hk.extend(txnum.to_be_bytes());
                    batch.delete(hk);
                }
            }
        }
        batch.delete(height_key(b'B', h));
        batch.delete(height_key(b'N', h));
        batch.delete(height_key(b'U', h));
        batch.put(b"tip", (h - 1).to_be_bytes());
        let p_iter = self.db.iterator(IteratorMode::From(
            &key(b'P', &meta.first.to_be_bytes()),
            Direction::Forward,
        ));
        for row in p_iter {
            let (k, _) = row?;
            if !k.starts_with(b"P") {
                break;
            }
            let last = u32::from_be_bytes(k[1..5].try_into()?);
            if last >= meta.first + meta.count {
                break;
            }
            if last >= meta.first {
                batch.delete(k);
            }
        }
        self.db.write_opt(batch, &write_options())?;
        if let Ok(mut index) = self.txnum_index.write() {
            ensure!(index.len() == h as usize + 1, "txnum index height mismatch");
            index.truncate(h as usize);
        }
        if let Ok(mut cached) = self.tip_cache.lock() {
            *cached = h
                .checked_sub(1)
                .map(|previous| self.header(previous).map(|h| (previous, h.block_hash())))
                .transpose()?;
        }
        *self
            .durable_tip
            .lock()
            .map_err(|_| anyhow::anyhow!("durable tip poisoned"))? = h.checked_sub(1);
        Ok(())
    }
    pub fn sync(&self, core: &Core) -> Result<()> {
        loop {
            let before = self.tip()?;
            self.sync_batch(core, 512)?;
            if self.tip()? == before {
                if self.options.fast_sync {
                    self.set_bulk(false)?;
                }
                return Ok(());
            }
        }
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
            let h = self.tip()?.context("empty index")?;
            if self.options.fast_sync || self.db.get(height_key(b'U', h))?.is_none() {
                self.rewind_reindex(core)?;
            } else {
                self.rewind_one()?;
            }
        }
        let core_height = info["blocks"].as_u64().context("missing height")? as u32;
        let limit = limit.min(512);
        let start = self.tip()?.map_or(0, |h| h + 1);
        let end = core_height.min(start.saturating_add(limit.saturating_sub(1)));
        if start > end {
            if self.options.fast_sync {
                self.set_bulk(false)?;
            }
            return Ok(());
        }
        if self.options.fast_sync {
            let remaining = core_height.saturating_sub(start);
            self.set_bulk(remaining > 64)?;
        }
        // Bounded two-worker fetch pipeline. Blocks may arrive out of order;
        // the single writer reorders them and remains the only DB writer.
        let mut hashes = Vec::with_capacity((end - start + 1) as usize);
        let mut next = start;
        while next <= end {
            let count = (end - next).min(15) + 1;
            hashes.extend(core.block_hashes(next, count)?);
            next += count;
        }
        ensure!(hashes.len() == (end - start + 1) as usize, "hash count");
        let workers = self.options.index_workers.max(1);
        let mut total_bytes = 0usize;
        std::thread::scope(|scope| -> Result<()> {
            let (tx, rx) = std::sync::mpsc::sync_channel::<(
                u32,
                Result<(Block, Option<Vec<Vec<SpentOut>>>)>,
                std::time::Duration,
            )>(4);
            let cursor = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut readers = Vec::new();
            for _ in 0..workers {
                let tx = tx.clone();
                let cursor = cursor.clone();
                let hashes = &hashes;
                readers.push(scope.spawn(move || {
                    loop {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        let index = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if index >= hashes.len() {
                            return;
                        }
                        let h = start + index as u32;
                        let hash = hashes[index];
                        let begun = std::time::Instant::now();
                        let result = (|| -> Result<(Block, Option<Vec<Vec<SpentOut>>>)> {
                            let block = core.block_binary(&hash.to_string()).context(
                                "Core pruned required history: stop and recover missing blocks; never skip heights",
                            )?;
                            ensure!(block.block_hash() == hash, "RPC block hash mismatch");
                            let use_spent = match self.options.spent_mode {
                                SpentMode::Always => true,
                                SpentMode::Never => false,
                                SpentMode::Auto => matches!(
                                    self.network,
                                    Network::Bitcoin | Network::Testnet
                                ),
                            };
                            let spent = if use_spent {
                                match core.rest() {
                                    Some(rest) => {
                                        let bytes = rest.spent_txouts(hash)?;
                                        Some(parse_spent(&block, &bytes)?)
                                    }
                                    None => None,
                                }
                            } else {
                                None
                            };
                            if !(self.options.verify_core == VerifyCore::Hash
                                && core.rest().is_some())
                            {
                                crate::net::verify_at(&block, hash, self.network, h)?;
                            }
                            Ok((block, spent))
                        })();
                        let failed = result.is_err();
                        if tx.send((h, result, begun.elapsed())).is_err() || failed {
                            return;
                        }
                    }
                }));
            }
            drop(tx);
            let mut fetch_time = std::time::Duration::ZERO;
            let hash_time = std::time::Duration::ZERO;
            let mut write_time = std::time::Duration::ZERO;
            let mut timings = CommitTimings::default();
            let begun = std::time::Instant::now();
            let result = (|| -> Result<()> {
                let mut buffered = BTreeMap::new();
                let mut expected = start;
                let mut pending = Vec::new();
                let mut overlay = std::collections::HashMap::new();
                let mut next_txnum = self.next_txnum(start)?;
                let mut previous_tip = self.tip()?;
                let mut previous_hash = previous_tip
                    .map(|height| self.header(height).map(|h| h.block_hash()))
                    .transpose()?;
                while expected <= end {
                    ensure!(
                        !cancel.load(std::sync::atomic::Ordering::Relaxed),
                        "synchronization interrupted"
                    );
                    let (h, block, elapsed) =
                        rx.recv().context("block reader stopped unexpectedly")?;
                    fetch_time += elapsed;
                    buffered.insert(h, block);
                    while let Some(block) = buffered.remove(&expected) {
                        let (block, spent) = block?;
                        total_bytes = total_bytes.saturating_add(block.total_size());
                        match previous_tip {
                            Some(tip) => ensure!(
                                expected == tip + 1
                                    && Some(block.header.prev_blockhash) == previous_hash,
                                "chain discontinuity"
                            ),
                            None => ensure!(
                                expected == 0
                                    && block.block_hash()
                                        == bitcoin::blockdata::constants::genesis_block(
                                            self.network
                                        )
                                        .block_hash(),
                                "wrong genesis"
                            ),
                        }
                        let prepared = self.prepare_block(
                            expected,
                            &block,
                            false,
                            spent.as_deref(),
                            next_txnum,
                            previous_tip,
                            &mut overlay,
                            &mut timings,
                        )?;
                        next_txnum = next_txnum
                            .checked_add(block.txdata.len() as u32)
                            .context("txnum overflow")?;
                        previous_tip = Some(expected);
                        previous_hash = Some(block.block_hash());
                        pending.push(prepared);
                        if pending.len() >= 32 {
                            let writing = std::time::Instant::now();
                            self.apply_prepared(&pending, expected == end, &mut timings)?;
                            write_time += writing.elapsed();
                            pending.clear();
                            overlay.clear();
                        }
                        if self.options.fast_sync
                            && (expected - start + 1) % 64 == 0
                            && expected < end
                        {
                            self.flush()?;
                        }
                        expected += 1;
                    }
                }
                if !pending.is_empty() {
                    let writing = std::time::Instant::now();
                    self.apply_prepared(&pending, true, &mut timings)?;
                    write_time += writing.elapsed();
                }
                Ok(())
            })();
            drop(rx);
            for reader in readers {
                reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("block reader panicked"))?;
            }
            result?;
            self.record_stats(end - start + 1, begun.elapsed(), 0.0);
            if start / 1000 != end / 1000 || start == 0 {
                let completed = u64::from(end) + 1;
                let total = u64::from(core_height) + 1;
                let percent = completed as f64 * 100.0 / total as f64;
                eprintln!(
                    "{} {completed}/{total} ({percent:.2}%)",
                    crate::i18n::text("indexed", "indexados")
                );
                eprintln!(
                    "  {}: {} | {}: {} ms | {}: {} ms | {}: {} ms | {}: {} ms ({})",
                    crate::i18n::text("batch blocks", "bloques del lote"),
                    end - start + 1,
                    crate::i18n::text("total", "total"),
                    begun.elapsed().as_millis(),
                    crate::i18n::text("fetch", "descarga"),
                    fetch_time.as_millis(),
                    crate::i18n::text("hashes", "hashes"),
                    hash_time.as_millis(),
                    crate::i18n::text("index", "índice"),
                    write_time.as_millis(),
                    crate::i18n::text(
                        "fetch and indexing overlap",
                        "descarga e indexado se solapan"
                    )
                );
                eprintln!(
                    "  {}: {} ms | {}: {} ms | undo: {} ms | {}: {} ms | {}: {}",
                    crate::i18n::text("verify", "verificación"),
                    timings.verify.as_millis(),
                    crate::i18n::text("prepare", "preparación"),
                    timings.prepare.as_millis(),
                    timings.undo.as_millis(),
                    crate::i18n::text("write (including sync)", "escritura (incluye sync)"),
                    timings.write.as_millis(),
                    crate::i18n::text("preimages reused", "preimágenes reutilizadas"),
                    timings.reused_preimages
                );
                let stats = self.stats();
                eprintln!(
                    "  {}: {:.1} | {}: {:.1} | {}: {:.1} MiB/s | bulk: {}",
                    crate::i18n::text("blocks/s", "bloques/s"),
                    stats.blocks_per_second,
                    crate::i18n::text("batch ms", "lote ms"),
                    stats.last_batch_millis,
                    crate::i18n::text("throughput", "rendimiento"),
                    stats.mib_per_second,
                    stats.bulk
                );
            }
            Ok(())
        })?;
        Ok(())
    }
    pub fn history(&self, script: &ScriptHash) -> Result<Value> {
        let rows = self.rows(&key(b'H', script), 100_000)?;
        let mut result = Vec::new();
        let mut previous = None;
        let mut meta = Vec::new();
        for (k, _) in rows {
            ensure!(k.len() == 29, "corrupt history key");
            let txnum = u32::from_be_bytes(k[25..29].try_into()?);
            let (h, pos) = self.txnum_to_height(txnum)?;
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
    pub fn unspent(&self, script: &ScriptHash) -> Result<Value> {
        let mut out = Vec::new();
        for (k, v) in self.rows(&key(b'S', script), 100_000)? {
            ensure!(k.len() == 61 && v.len() == 12, "corrupt unspent entry");
            let txid = Txid::from_slice(&k[25..57])?;
            let vout = u32::from_be_bytes(k[57..61].try_into()?);
            let h = u32::from_be_bytes(v[..4].try_into()?);
            out.push(json!({"tx_hash":txid.to_string(),"tx_pos":vout,"value":u64::from_le_bytes(v[4..12].try_into()?),"height":h}));
        }
        Ok(Value::Array(out))
    }
    pub fn locate(&self, txid: Txid) -> Result<(u32, u32)> {
        self.locate_at(txid, None)
    }
    pub fn locate_at(&self, txid: Txid, height: Option<u32>) -> Result<(u32, u32)> {
        let rows = self.rows(&key(b'T', &txid.as_byte_array()[..8]), 100)?;
        for (k, _) in rows.into_iter().rev() {
            ensure!(k.len() == 13, "corrupt txid entry");
            let txnum = u32::from_be_bytes(k[9..13].try_into()?);
            let (h, pos) = self.txnum_to_height(txnum)?;
            if height.is_some_and(|wanted| wanted != h) {
                continue;
            }
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
    #[allow(unused_assignments)]
    fn block_source(&self, core: &Core, h: u32, persist: bool) -> Result<Block> {
        let hash = self.header(h)?.block_hash();
        if self.cache.join(format!("{hash}.blk")).exists() {
            let block = self.fetcher.get(self.network, hash, h, &self.cache)?;
            return Ok(block);
        }
        let mut last_error = match core.block_binary(&hash.to_string()) {
            Ok(b) => {
                crate::net::verify_at(&b, hash, self.network, h)?;
                return Ok(b);
            }
            Err(error) => error,
        };
        if let Some(public) = &self.public {
            // Privacy first: only the aligned window of block hashes leaves
            // the process, never the single block that is actually needed.
            match public.block_window(self, h, hash, persist) {
                Ok(block) => {
                    crate::net::verify_at(&block, hash, self.network, h)?;
                    return Ok(block);
                }
                Err(error) => last_error = error,
            }
        }
        let p2p = if self.fetcher.peers.is_empty() {
            core.call("getpeerinfo", json!([])).ok().and_then(|peers| {
                let peers: Vec<String> = peers
                    .as_array()?
                    .iter()
                    .filter_map(|p| p["addr"].as_str().map(str::to_owned))
                    .collect();
                Some(crate::net::Fetcher {
                    peers,
                    proxy: self.fetcher.proxy.clone(),
                })
            })
        } else {
            None
        };
        let fetcher = p2p.as_ref().unwrap_or(&self.fetcher);
        match fetcher.retrieve_for_index(self.network, hash, h, &self.cache, persist) {
            Ok(block) => return Ok(block),
            Err(error) => last_error = error,
        }
        Err(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genesis_spent_undo_is_empty() {
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        assert_eq!(parse_spent(&genesis, &[0]).expect("genesis undo").len(), 1);
    }
    #[test]
    fn script_hash_is_192_bits() {
        let hash = script_hash(b"test");
        assert_eq!(hash.len(), SCRIPT_HASH_LEN);
        assert_eq!(
            hash[..],
            sha256::Hash::hash(b"test").to_byte_array()[..SCRIPT_HASH_LEN]
        );
    }
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
    fn fast_sync_reorg_reindexes_stale_block() -> Result<()> {
        use crate::core::Core;
        use bitcoin::{
            absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence,
            Transaction, TxIn, TxOut,
        };
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let root = std::env::temp_dir().join(format!(
            "el-reindex-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let options = IndexOptions {
            fast_sync: true,
            txpos: false,
            ..IndexOptions::default()
        };
        let index = Index::open_with_options(&root.join("index"), Network::Regtest, options)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        index.commit(0, &genesis)?;
        let genesis_out = OutPoint::new(genesis.txdata[0].compute_txid(), 0);
        let genesis_script = genesis.txdata[0].output[0].script_pubkey.clone();
        let new_script = ScriptBuf::from_bytes(vec![0x51]);
        let spend = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: genesis_out,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(49_0000_0000),
                script_pubkey: new_script.clone(),
            }],
        };
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 2]),
                sequence: Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_0000_0000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x52]),
            }],
        };
        let mut block = bitcoin::Block {
            header: bitcoin::block::Header {
                version: bitcoin::block::Version::TWO,
                prev_blockhash: genesis.block_hash(),
                merkle_root: bitcoin::TxMerkleNode::all_zeros(),
                time: genesis.header.time + 1,
                bits: genesis.header.bits,
                nonce: 0,
            },
            txdata: vec![coinbase, spend],
        };
        block.header.merkle_root = block.compute_merkle_root().expect("merkle");
        let spent = vec![
            Vec::new(),
            vec![SpentOut {
                script: script_hash(genesis_script.as_bytes()),
                value: 50_0000_0000,
            }],
        ];
        index.commit_with_sync(
            1,
            &block,
            true,
            true,
            Some(&spent),
            &mut std::collections::HashMap::new(),
            &mut CommitTimings::default(),
        )?;
        std::fs::create_dir_all(&index.cache)?;
        std::fs::write(
            index.cache.join(format!("{}.blk", block.block_hash())),
            serialize(&block),
        )?;
        let mut undo = Vec::new();
        undo.push(2u8);
        undo.push(0u8);
        undo.push(1u8);
        undo.extend(50_0000_0000u64.to_le_bytes());
        undo.push(genesis_script.len() as u8);
        undo.extend(genesis_script.as_bytes());
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let worker = std::thread::spawn(move || -> Result<()> {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte)?;
                    request.push(byte[0]);
                    ensure!(request.len() < 8192, "request too large");
                }
                let request = std::str::from_utf8(&request)?;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .context("request path")?;
                if path.ends_with("/rest/chaininfo.json") {
                    let body = br#"{"chain":"regtest"}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )?;
                    stream.write_all(body)?;
                } else if path.contains("/rest/spenttxouts/") {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        undo.len()
                    )?;
                    stream.write_all(&undo)?;
                } else {
                    return Err(anyhow::anyhow!("unexpected REST path {path}"));
                }
            }
            Ok(())
        });
        let cookie = root.join("cookie");
        std::fs::write(&cookie, "user:pass")?;
        let mut core = Core::new(format!("http://{address}"), cookie)?;
        core.enable_rest()?;
        index.rewind_reindex(&core)?;
        assert_eq!(index.tip()?, Some(0));
        let restored = index.unspent(&script_hash(genesis_script.as_bytes()))?;
        assert_eq!(restored.as_array().expect("array").len(), 1);
        assert!(index.db.get(key(b'C', &serialize(&genesis_out)))?.is_some());
        worker.join().expect("mock REST")?;
        drop(index);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn batched_and_single_commits_produce_identical_rows() -> Result<()> {
        use bitcoin::{
            absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence,
            Transaction, TxIn, TxOut,
        };
        let root = std::env::temp_dir().join(format!(
            "el-batch-eq-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let options = IndexOptions {
            txpos: false,
            ..IndexOptions::default()
        };
        let single = Index::open_with_options(&root.join("single"), Network::Regtest, options)?;
        let batched = Index::open_with_options(&root.join("batched"), Network::Regtest, options)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        let mut blocks = vec![genesis.clone()];
        let mut prev = genesis.clone();
        let mut spendable = bitcoin::OutPoint::null();
        for (i, script_byte) in [0x51u8, 0x53, 0x54].iter().enumerate() {
            let coinbase = Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![TxIn {
                    previous_output: OutPoint::null(),
                    script_sig: ScriptBuf::from_bytes(vec![1, 2, i as u8]),
                    sequence: Sequence::MAX,
                    witness: bitcoin::Witness::new(),
                }],
                output: vec![TxOut {
                    value: Amount::from_sat(50_0000_0000),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x52, i as u8]),
                }],
            };
            let mut txdata = vec![coinbase.clone()];
            if i == 0 {
                spendable = OutPoint::new(coinbase.compute_txid(), 0);
            } else {
                let spend = Transaction {
                    version: Version::TWO,
                    lock_time: LockTime::ZERO,
                    input: vec![TxIn {
                        previous_output: spendable,
                        script_sig: ScriptBuf::new(),
                        sequence: Sequence::MAX,
                        witness: bitcoin::Witness::new(),
                    }],
                    output: vec![TxOut {
                        value: Amount::from_sat(49_0000_0000 - i as u64),
                        script_pubkey: ScriptBuf::from_bytes(vec![*script_byte]),
                    }],
                };
                spendable = OutPoint::new(spend.compute_txid(), 0);
                txdata.push(spend);
            }
            let mut block = bitcoin::Block {
                header: bitcoin::block::Header {
                    version: bitcoin::block::Version::TWO,
                    prev_blockhash: prev.block_hash(),
                    merkle_root: bitcoin::TxMerkleNode::all_zeros(),
                    time: prev.header.time + 1,
                    bits: prev.header.bits,
                    nonce: i as u32,
                },
                txdata,
            };
            block.header.merkle_root = block.compute_merkle_root().expect("merkle");
            prev = block.clone();
            blocks.push(block);
        }
        for (h, block) in blocks.iter().enumerate() {
            single.commit(h as u32, block)?;
        }
        let mut overlay = std::collections::HashMap::new();
        let mut prepared = Vec::new();
        let mut first_txnum = 0u32;
        for (h, block) in blocks.iter().enumerate() {
            prepared.push(batched.prepare_block(
                h as u32,
                block,
                true,
                None,
                first_txnum,
                h.checked_sub(1).map(|v| v as u32),
                &mut overlay,
                &mut CommitTimings::default(),
            )?);
            first_txnum += block.txdata.len() as u32;
        }
        batched.apply_prepared(&prepared, true, &mut CommitTimings::default())?;
        let collect = |index: &Index| -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
            let mut rows = BTreeMap::new();
            for row in index.db.iterator(IteratorMode::Start) {
                let (k, v) = row?;
                rows.insert(k.to_vec(), v.to_vec());
            }
            Ok(rows)
        };
        assert_eq!(collect(&single)?, collect(&batched)?);
        drop(single);
        drop(batched);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn spent_txouts_avoid_prevout_reads_and_restore_on_rewind() -> Result<()> {
        use bitcoin::{
            absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence,
            Transaction, TxIn, TxOut,
        };
        let path = std::env::temp_dir().join(format!(
            "el-spent-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let i = Index::open(&path, Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        i.commit(0, &genesis)?;
        let genesis_out = OutPoint::new(genesis.txdata[0].compute_txid(), 0);
        let genesis_script = genesis.txdata[0].output[0].script_pubkey.clone();
        let new_script = ScriptBuf::from_bytes(vec![0x51]);
        // Simulate a pruned/absent C row: the spenttxouts path must not read it.
        i.db.delete(key(b'C', &serialize(&genesis_out)))?;
        let spend = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: genesis_out,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(49_9999_0000),
                script_pubkey: new_script.clone(),
            }],
        };
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 2]),
                sequence: Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50_0000_0000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x52]),
            }],
        };
        let mut block = bitcoin::Block {
            header: bitcoin::block::Header {
                version: bitcoin::block::Version::TWO,
                prev_blockhash: genesis.block_hash(),
                merkle_root: bitcoin::TxMerkleNode::all_zeros(),
                time: genesis.header.time + 1,
                bits: genesis.header.bits,
                nonce: 0,
            },
            txdata: vec![coinbase, spend],
        };
        block.header.merkle_root = block.compute_merkle_root().expect("merkle");
        let spent = vec![
            Vec::new(),
            vec![SpentOut {
                script: script_hash(genesis_script.as_bytes()),
                value: 50_0000_0000,
            }],
        ];
        i.commit_with_sync(
            1,
            &block,
            true,
            true,
            Some(&spent),
            &mut std::collections::HashMap::new(),
            &mut CommitTimings::default(),
        )?;
        assert_eq!(
            i.unspent(&script_hash(genesis_script.as_bytes()))?
                .as_array()
                .expect("array")
                .len(),
            0
        );
        assert_eq!(
            i.unspent(&script_hash(new_script.as_bytes()))?
                .as_array()
                .expect("array")
                .len(),
            1
        );
        assert_eq!(
            i.tx_position(1)?,
            (81, serialize(&block.txdata[0]).len() as u32)
        );
        assert_eq!(
            i.tx_position(2)?,
            (
                81 + serialize(&block.txdata[0]).len() as u32,
                serialize(&block.txdata[1]).len() as u32
            )
        );
        i.rewind_one()?;
        let restored = i.unspent(&script_hash(genesis_script.as_bytes()))?;
        assert_eq!(restored.as_array().expect("array").len(), 1);
        assert_eq!(restored[0]["height"], 0);
        assert!(i.db.get(key(b'C', &serialize(&genesis_out)))?.is_some());
        drop(i);
        std::fs::remove_dir_all(path)?;
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
