//! Experimental Bitcoin/LSM generation coordinator, deliberately not wired to CLI.
//! Mainnet pruning and runtime backend selection are NOT enabled by this module.
pub mod kv;
pub mod query;

use crate::{
    archive::{Archive, Location},
    validate_block, Chain,
};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::serialize,
    hashes::{sha256, Hash, HashEngine},
    Block, BlockHash, OutPoint, Txid,
};
use kv::Prototype;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

const STATE: &[u8] = b"Mstate";
const WINDOW: u32 = 288;
type Delta = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coin {
    pub value: u64,
    pub height: u32,
    pub coinbase: bool,
    // Raw SHA256 bytes, never Electrum wire-order hex inside the storage engine.
    pub script: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub height: u32,
    pub hash: BlockHash,
    pub location: Location,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub tip: Option<Record>,
    // Last processed PHYSICAL record, including repeated ancestors as rewind intent.
    pub cursor: Option<Location>,
}

#[derive(Serialize, Deserialize)]
struct Undo {
    before: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Prepared,
    ArchiveSynced,
    Committed,
}

pub struct BitcoinStore {
    archive: Archive,
    db: Prototype,
    chain: Chain,
    ready: bool,
}

fn identity(chain: Chain) -> Vec<u8> {
    format!("lite-bitcoin-lsm-v2:{}:{}", chain.name(), chain.genesis()).into_bytes()
}
fn numbered(prefix: u8, height: u32) -> Vec<u8> {
    let mut key = vec![prefix];
    key.extend(height.to_be_bytes());
    key
}
fn coin_key(outpoint: OutPoint) -> Vec<u8> {
    let mut key = vec![b'C'];
    key.extend(serialize(&outpoint));
    key
}
fn script_coin_key(script: [u8; 32], outpoint: OutPoint) -> Vec<u8> {
    let mut key = vec![b'S'];
    key.extend(script);
    key.extend(serialize(&outpoint));
    key
}
fn state(db: &Prototype) -> Result<State> {
    db.get(STATE)?
        .map(|value| serde_json::from_slice(&value).map_err(Into::into))
        .unwrap_or_else(|| Ok(State::default()))
}
fn record(db: &Prototype, height: u32) -> Result<Record> {
    Ok(serde_json::from_slice(
        &db.get(&numbered(b'B', height))?
            .context("missing block record")?,
    )?)
}
fn lookup(db: &Prototype, delta: &Delta, key: &[u8]) -> Result<Option<Vec<u8>>> {
    match delta.get(key) {
        Some(value) => Ok(value.clone()),
        None => db.get(key),
    }
}
fn coin(db: &Prototype, delta: &Delta, outpoint: OutPoint) -> Result<Option<Coin>> {
    lookup(db, delta, &coin_key(outpoint))?
        .map(|value| serde_json::from_slice(&value).map_err(Into::into))
        .transpose()
}

fn prepare(db: &Prototype, chain: Chain, height: u32, block: &Block) -> Result<Delta> {
    validate_block(block)?;
    let current = state(db)?;
    match current.tip {
        None => ensure!(
            height == 0 && block.block_hash() == chain.genesis(),
            "wrong genesis"
        ),
        Some(tip) => ensure!(
            tip.height.checked_add(1) == Some(height) && block.header.prev_blockhash == tip.hash,
            "chain discontinuity"
        ),
    }
    let mut delta = Delta::new();
    for (position, transaction) in block.txdata.iter().enumerate() {
        let txid = transaction.compute_txid();
        let mut scripts = BTreeSet::new();
        if !transaction.is_coinbase() {
            for input in &transaction.input {
                let spent =
                    coin(db, &delta, input.previous_output)?.context("missing or spent prevout")?;
                scripts.insert(spent.script);
                adjust_balance(db, &mut delta, spent.script, spent.value, false)?;
                delta.insert(coin_key(input.previous_output), None);
                delta.insert(script_coin_key(spent.script, input.previous_output), None);
            }
        }
        for (vout, output) in transaction.output.iter().enumerate() {
            let script = sha256::Hash::hash(output.script_pubkey.as_bytes()).to_byte_array();
            scripts.insert(script);
            if height == 0
                || output.script_pubkey.is_op_return()
                || output.script_pubkey.len() > 10_000
            {
                continue;
            }
            let outpoint = OutPoint {
                txid,
                vout: vout.try_into()?,
            };
            if let Some(previous) = coin(db, &delta, outpoint)? {
                ensure!(
                    chain == Chain::Mainnet
                        && matches!(height, 91842 | 91880)
                        && transaction.is_coinbase(),
                    "BIP30 duplicate outpoint"
                );
                delta.insert(script_coin_key(previous.script, outpoint), None);
                adjust_balance(db, &mut delta, previous.script, previous.value, false)?;
            }
            let created = Coin {
                value: output.value.to_sat(),
                height,
                coinbase: transaction.is_coinbase(),
                script,
            };
            let bytes = serde_json::to_vec(&created)?;
            adjust_balance(db, &mut delta, created.script, created.value, true)?;
            delta.insert(coin_key(outpoint), Some(bytes.clone()));
            delta.insert(script_coin_key(script, outpoint), Some(bytes));
        }
        // Occurrences include block hash+position, never globally unique TXIDs.
        let mut occurrence = vec![b'O'];
        occurrence.extend(txid.to_byte_array());
        occurrence.extend(block.block_hash().to_byte_array());
        occurrence.extend(u32::try_from(position)?.to_be_bytes());
        delta.insert(occurrence, Some(height.to_be_bytes().to_vec()));
        for script in scripts {
            let mut key = vec![b'H'];
            key.extend(script);
            key.extend(height.to_be_bytes());
            key.extend(u32::try_from(position)?.to_be_bytes());
            key.extend(block.block_hash().to_byte_array());
            delta.insert(key, Some(txid.to_byte_array().to_vec()));
        }
    }
    delta.insert(numbered(b'A', height), Some(serialize(&block.header)));
    Ok(delta)
}

fn adjust_balance(
    db: &Prototype,
    delta: &mut Delta,
    script: [u8; 32],
    value: u64,
    add: bool,
) -> Result<()> {
    let mut key = vec![b'V'];
    key.extend(script);
    let previous = lookup(db, delta, &key)?
        .map(|bytes| Ok::<_, anyhow::Error>(u64::from_be_bytes(bytes.as_slice().try_into()?)))
        .transpose()?
        .unwrap_or(0);
    let next = if add {
        previous
            .checked_add(value)
            .context("script balance overflow")?
    } else {
        previous
            .checked_sub(value)
            .context("script balance underflow")?
    };
    delta.insert(
        key,
        if next == 0 {
            None
        } else {
            Some(next.to_be_bytes().to_vec())
        },
    );
    Ok(())
}

fn finish_delta(
    db: &Prototype,
    mut delta: Delta,
    height: u32,
    hash: BlockHash,
    location: Location,
) -> Result<Delta> {
    let tip = Record {
        height,
        hash,
        location,
    };
    delta.insert(numbered(b'B', height), Some(serde_json::to_vec(&tip)?));
    let before = delta
        .keys()
        .map(|key| Ok((key.clone(), db.get(key)?)))
        .collect::<Result<Vec<_>>>()?;
    delta.insert(
        numbered(b'U', height),
        Some(serde_json::to_vec(&Undo { before })?),
    );
    if height >= WINDOW {
        delta.insert(numbered(b'U', height - WINDOW), None);
    }
    delta.insert(
        STATE.to_vec(),
        Some(serde_json::to_vec(&State {
            tip: Some(tip),
            cursor: Some(location),
        })?),
    );
    Ok(delta)
}

fn commit_delta(db: &mut Prototype, delta: Delta) -> Result<()> {
    db.commit(db.generation()?, &delta.into_iter().collect::<Vec<_>>())
}

fn rewind_delta(db: &Prototype, height: u32, block: &Block, cursor: Location) -> Result<Delta> {
    let current = state(db)?.tip.context("empty index")?;
    ensure!(
        height < current.height && current.height - height <= WINDOW,
        "rewind exceeds retained undo"
    );
    let target = record(db, height)?;
    ensure!(
        target.hash == block.block_hash(),
        "rewind marker is not canonical ancestor"
    );
    let mut delta = Delta::new();
    for removed in (height + 1..=current.height).rev() {
        let key = numbered(b'U', removed);
        let undo: Undo =
            serde_json::from_slice(&db.get(&key)?.context("required undo unavailable")?)?;
        for (key, previous) in undo.before {
            delta.insert(key, previous);
        }
        delta.insert(key, None);
    }
    delta.insert(
        STATE.to_vec(),
        Some(serde_json::to_vec(&State {
            tip: Some(target),
            cursor: Some(cursor),
        })?),
    );
    Ok(delta)
}

impl BitcoinStore {
    pub fn snapshot(&self) -> Result<query::Snapshot<'_>> {
        ensure!(self.ready, "recovery required");
        query::Snapshot::new(self.db.snapshot()?, &self.archive)
    }
    pub fn create(root: &Path, chain: Chain) -> Result<Self> {
        ensure!(!root.exists() && !root.is_symlink(), "new root required");
        let mut archive = Archive::open(root, chain, true)?;
        archive.enable_branches();
        archive.scan(|_, _| Ok(()))?;
        let db = Prototype::create(&archive.directory().join("lsm"), &identity(chain))?;
        Ok(Self {
            archive,
            db,
            chain,
            ready: true,
        })
    }

    pub fn open(root: &Path, chain: Chain) -> Result<Self> {
        let mut archive = Archive::open(root, chain, false)?;
        archive.enable_branches();
        let db = Prototype::open(&archive.directory().join("lsm"), &identity(chain))?;
        let mut result = Self {
            archive,
            db,
            chain,
            ready: false,
        };
        result.recover()?;
        Ok(result)
    }

    pub fn state(&self) -> Result<State> {
        Ok(self.snapshot()?.state().clone())
    }

    pub fn coin(&self, outpoint: OutPoint) -> Result<Option<Coin>> {
        self.snapshot()?.coin(outpoint)
    }

    pub fn history(&self, script: [u8; 32], limit: usize) -> Result<Vec<(u32, u32, Txid)>> {
        Ok(self
            .snapshot()?
            .history(script, limit)?
            .into_iter()
            .map(|occurrence| (occurrence.height, occurrence.position, occurrence.txid))
            .collect())
    }

    pub fn block(&self, height: u32) -> Result<Block> {
        self.snapshot()?.block(height)
    }

    /// Finish pending memtable flush and explicit compaction under writer ownership.
    pub fn settle(&mut self) -> Result<()> {
        ensure!(self.ready, "recovery required");
        self.ready = false;
        self.db.settle()?;
        self.ready = true;
        Ok(())
    }

    pub fn commit(&mut self, height: u32, block: &Block) -> Result<()> {
        self.commit_with_hook(height, block, |_| Ok(()))
    }

    pub fn commit_with_hook(
        &mut self,
        height: u32,
        block: &Block,
        mut hook: impl FnMut(Stage) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.ready, "recovery required");
        let delta = prepare(&self.db, self.chain, height, block)?;
        // Location has bounded encoding size. Preflight BEFORE appending so an
        // unsupported large block cannot poison the durable archive on purpose.
        let provisional = finish_delta(
            &self.db,
            delta.clone(),
            height,
            block.block_hash(),
            Location {
                segment: u32::MAX,
                offset: u64::MAX,
                length: u32::MAX,
            },
        )?;
        self.db.validate_changes(
            self.db.generation()?,
            &provisional.into_iter().collect::<Vec<_>>(),
        )?;
        let result = (|| {
            hook(Stage::Prepared)?;
            let location = self.archive.append(height, block)?; // validates + fsyncs bytes and directories
            hook(Stage::ArchiveSynced)?;
            let finished = finish_delta(&self.db, delta, height, block.block_hash(), location)?;
            commit_delta(&mut self.db, finished)?;
            hook(Stage::Committed)?;
            Ok(())
        })();
        if result.is_err() {
            self.ready = false;
        }
        result
    }

    pub fn rewind(&mut self, height: u32) -> Result<()> {
        ensure!(self.ready, "recovery required");
        let target = self.block(height)?;
        let provisional = rewind_delta(
            &self.db,
            height,
            &target,
            Location {
                segment: u32::MAX,
                offset: u64::MAX,
                length: u32::MAX,
            },
        )?;
        self.db.validate_changes(
            self.db.generation()?,
            &provisional.into_iter().collect::<Vec<_>>(),
        )?;
        self.ready = false;
        let location = self.archive.append(height, &target)?; // durable ancestral repeat is rewind intent
        let delta = rewind_delta(&self.db, height, &target, location)?;
        commit_delta(&mut self.db, delta)?;
        self.ready = true;
        Ok(())
    }

    pub fn recover(&mut self) -> Result<()> {
        self.ready = false;
        let saved = state(&self.db)?;
        ensure!(
            saved.tip.is_some() == saved.cursor.is_some(),
            "incomplete durable state"
        );
        if saved.cursor.is_none() {
            ensure!(
                self.db.generation()? == 0,
                "generation without durable cursor"
            );
        }
        // Verify every published reference before importing any new tail.
        self.db.visit_prefix(b"B", |_, bytes| {
            let stored: Record = serde_json::from_slice(bytes)?;
            let (height, block) = self.archive.read(stored.location)?;
            ensure!(
                height == stored.height && block.block_hash() == stored.hash,
                "missing/mixed archive reference"
            );
            Ok(())
        })?;
        let db = &mut self.db;
        let chain = self.chain;
        let mut seen = saved.cursor.is_none();
        self.archive.scan_records(|height, block, location| {
            if !seen {
                if Some(location) == saved.cursor {
                    let tip = saved.tip.as_ref().context("cursor without tip")?;
                    ensure!(
                        tip.height == height && tip.hash == block.block_hash(),
                        "cursor mismatch"
                    );
                    seen = true;
                }
                return Ok(());
            }
            let delta = if state(db)?.tip.is_some_and(|tip| height <= tip.height) {
                rewind_delta(db, height, block, location)?
            } else {
                finish_delta(
                    db,
                    prepare(db, chain, height, block)?,
                    height,
                    block.block_hash(),
                    location,
                )?
            };
            commit_delta(db, delta)
        })?;
        ensure!(seen, "durable cursor missing from archive");
        self.audit_inner()?;
        self.ready = true;
        Ok(())
    }

    /// Deliberately expensive prototype audit: independently replay every
    /// physical record into a fresh disposable LSM and compare all namespaces.
    /// This is NOT a proposed optimized startup path or a performance result.
    fn audit_inner(&mut self) -> Result<()> {
        let temporary = tempfile::Builder::new().prefix("lsm-audit-").tempdir_in(
            self.archive
                .directory()
                .parent()
                .context("missing archive parent")?,
        )?;
        let mut rebuilt =
            Prototype::create(&temporary.path().join("index"), &identity(self.chain))?;
        self.archive.scan_records(|height, block, location| {
            let delta = if state(&rebuilt)?.tip.is_some_and(|tip| height <= tip.height) {
                rewind_delta(&rebuilt, height, block, location)?
            } else {
                finish_delta(
                    &rebuilt,
                    prepare(&rebuilt, self.chain, height, block)?,
                    height,
                    block.block_hash(),
                    location,
                )?
            };
            commit_delta(&mut rebuilt, delta)
        })?;
        ensure!(
            self.db.generation()? == rebuilt.generation()?,
            "durable generation does not match physical archive log"
        );
        for prefix in [b"A", b"B", b"C", b"S", b"H", b"O", b"U", b"V", b"M"] {
            for (left, right) in [(&self.db, &rebuilt), (&rebuilt, &self.db)] {
                left.visit_prefix(prefix, |key, value| {
                    ensure!(
                        right.get(key)?.as_deref() == Some(value),
                        "semantic index mismatch"
                    );
                    Ok(())
                })?;
            }
        }
        drop(rebuilt);
        temporary.close()?;
        Ok(())
    }

    /// Full generation backup under exclusive ownership, never just DB files.
    pub fn backup(&mut self, destination: &Path) -> Result<()> {
        ensure!(self.ready, "recovery required");
        self.ready = false;
        self.audit_inner()?;
        self.ready = true;
        ensure!(
            !new_destination(destination)?.starts_with(self.archive.directory().canonicalize()?),
            "backup inside source"
        );
        fs::create_dir(destination)?;
        let archive = destination.join(self.chain.name());
        fs::create_dir(&archive)?;
        for entry in fs::read_dir(self.archive.directory())? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "elb")
            {
                ensure!(entry.file_type()?.is_file(), "archive symlink refused");
                let target = archive.join(entry.file_name());
                fs::copy(entry.path(), &target)?;
                File::open(target)?.sync_all()?;
            }
        }
        File::open(&archive)?.sync_all()?;
        self.db.backup(&destination.join("index"))?;
        let manifest = FullManifest {
            format: 1,
            network: self.chain.name().into(),
            state: self.state()?,
            files: tree_inventory(destination)?,
        };
        let path = destination.join("generation.json");
        fs::write(&path, serde_json::to_vec(&manifest)?)?;
        File::open(path)?.sync_all()?;
        File::open(destination)?.sync_all()?;
        File::open(destination.parent().context("missing backup parent")?)?.sync_all()?;
        Ok(())
    }

    pub fn restore(backup: &Path, destination: &Path, chain: Chain) -> Result<Self> {
        ensure!(
            !new_destination(destination)?.starts_with(backup.canonicalize()?),
            "restore inside backup"
        );
        let path = backup.join("generation.json");
        ensure!(
            !path.is_symlink() && fs::metadata(&path)?.len() <= 1024 * 1024,
            "invalid generation manifest"
        );
        let manifest: FullManifest = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(
            manifest.format == 1 && manifest.network == chain.name(),
            "backup identity mismatch"
        );
        let mut actual = tree_inventory(backup)?;
        actual.remove("generation.json");
        ensure!(actual == manifest.files, "full generation backup mismatch");
        fs::create_dir(destination)?;
        let archive = destination.join(chain.name());
        fs::create_dir(&archive)?;
        for entry in fs::read_dir(backup.join(chain.name()))? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file(),
                "archive must contain regular files"
            );
            let target = archive.join(entry.file_name());
            fs::copy(entry.path(), &target)?;
            File::open(target)?.sync_all()?;
        }
        drop(Prototype::restore(
            &backup.join("index"),
            &archive.join("lsm"),
            &identity(chain),
        )?);
        File::open(&archive)?.sync_all()?;
        File::open(destination)?.sync_all()?;
        File::open(destination.parent().context("missing restore parent")?)?.sync_all()?;
        let restored = Self::open(destination, chain)?;
        ensure!(
            restored.state()? == manifest.state,
            "restored generation mismatch"
        );
        Ok(restored)
    }
}

#[derive(Serialize, Deserialize)]
struct FullManifest {
    format: u32,
    network: String,
    state: State,
    files: BTreeMap<String, String>,
}

fn new_destination(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.exists() && !path.is_symlink(),
        "destination must be new"
    );
    Ok(path
        .parent()
        .context("missing parent")?
        .canonicalize()?
        .join(path.file_name().context("missing filename")?))
}

fn tree_inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, path: &Path, result: &mut BTreeMap<String, String>) -> Result<()> {
        ensure!(!path.is_symlink(), "backup symlink refused");
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(root, &entry.path(), result)?;
            } else {
                ensure!(entry.file_type()?.is_file(), "backup special file refused");
                let mut file = File::open(entry.path())?;
                let mut engine = sha256::Hash::engine();
                let mut buffer = [0; 65536];
                loop {
                    let read = file.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    engine.input(&buffer[..read]);
                }
                result.insert(
                    entry
                        .path()
                        .strip_prefix(root)?
                        .to_str()
                        .context("invalid backup filename")?
                        .to_owned(),
                    sha256::Hash::from_engine(engine).to_string(),
                );
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_bip30_overwrite_undo_keeps_old_script_coin() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut db = Prototype::create(&temporary.path().join("db"), &identity(Chain::Mainnet))?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Chain::Mainnet.network());
        let mut block = genesis.clone();
        block.header.prev_blockhash = genesis.block_hash();
        block.header.time += 1;
        let outpoint = OutPoint {
            txid: block.txdata[0].compute_txid(),
            vout: 0,
        };
        let old = Coin {
            value: 1,
            height: 91800,
            coinbase: true,
            script: [7; 32],
        };
        let location = Location {
            segment: 0,
            offset: 0,
            length: 1,
        };
        let seed = State {
            tip: Some(Record {
                height: 91841,
                hash: genesis.block_hash(),
                location,
            }),
            cursor: Some(location),
        };
        let mut balance_key = vec![b'V'];
        balance_key.extend(old.script);
        db.commit(
            0,
            &[
                (STATE.to_vec(), Some(serde_json::to_vec(&seed)?)),
                (coin_key(outpoint), Some(serde_json::to_vec(&old)?)),
                (balance_key, Some(old.value.to_be_bytes().to_vec())),
                (
                    script_coin_key(old.script, outpoint),
                    Some(serde_json::to_vec(&old)?),
                ),
            ],
        )?;
        let delta = prepare(&db, Chain::Mainnet, 91842, &block)?;
        assert_eq!(
            delta.get(&script_coin_key(old.script, outpoint)),
            Some(&None)
        );
        let finished = finish_delta(&db, delta, 91842, block.block_hash(), location)?;
        let undo: Undo = serde_json::from_slice(
            finished
                .get(&numbered(b'U', 91842))
                .unwrap()
                .as_ref()
                .unwrap(),
        )?;
        assert!(undo
            .before
            .contains(&(coin_key(outpoint), Some(serde_json::to_vec(&old)?))));
        assert!(prepare(&db, Chain::Signet, 91842, &block).is_err());
        Ok(())
    }
}
