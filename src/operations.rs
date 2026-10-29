use crate::{archive::Archive, rpc::Core, store::Store, Chain};
use anyhow::{ensure, Context, Result};
use bitcoin::hashes::{sha256, Hash, HashEngine};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

pub fn backup(archive: &Archive, destination: &Path) -> Result<()> {
    outside(archive.directory(), destination)?;
    fs::create_dir(destination).context("backup destination must be new")?;
    let directory = destination.join(
        archive
            .directory()
            .file_name()
            .context("network directory missing")?,
    );
    fs::create_dir(&directory)?;
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(archive.directory())? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            entry.file_type()?.is_file(),
            "backup refuses symlinks/subdirectories"
        );
        if name == "archive.lock" {
            continue;
        }
        let target = directory.join(&name);
        fs::copy(entry.path(), &target)?;
        File::open(&target)?.sync_all()?;
        let mut engine = sha256::Hash::engine();
        let mut file = File::open(&target)?;
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            engine.input(&buffer[..n]);
        }
        files.insert(
            name.to_string_lossy().to_string(),
            sha256::Hash::from_engine(engine).to_string(),
        );
    }
    let mut manifest = File::create_new(destination.join("BACKUP_COMPLETE.json"))?;
    manifest.write_all(
        serde_json::to_string_pretty(
            &json!({"network":directory.file_name().unwrap().to_string_lossy(),"sha256":files}),
        )?
        .as_bytes(),
    )?;
    manifest.sync_all()?;
    File::open(&directory)?.sync_all()?;
    File::open(destination)?.sync_all()?;
    File::open(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}

pub fn migrate(archive: &Archive, chain: Chain, backup_path: &Path) -> Result<()> {
    ensure!(
        archive.directory().join("index.sqlite").is_file(),
        "schema-1 index does not exist"
    );
    backup(archive, backup_path)?;
    let mut conn = Connection::open(archive.directory().join("index.sqlite"))?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let application: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    ensure!(
        version == 1 && application == 0x454C4954,
        "only schema 1 can be migrated; backup preserved"
    );
    let (network, genesis): (String, Vec<u8>) =
        conn.query_row("SELECT network,genesis FROM identity", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    ensure!(
        network == chain.name() && genesis == chain.genesis().to_byte_array(),
        "migration network mismatch"
    );
    conn.execute_batch(
        "PRAGMA synchronous=EXTRA; PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON;",
    )?;
    let tx = conn.transaction()?;
    tx.execute_batch("ALTER TABLE blocks ADD COLUMN header BLOB CHECK(length(header)=80); CREATE TABLE archive_cursor(id INTEGER PRIMARY KEY CHECK(id=1),segment INTEGER NOT NULL,offset INTEGER NOT NULL,length INTEGER NOT NULL); INSERT INTO archive_cursor SELECT 1,segment,offset,length FROM blocks ORDER BY height DESC LIMIT 1; CREATE TABLE prune_audit(id INTEGER PRIMARY KEY,requested INTEGER NOT NULL,actual INTEGER,tip_height INTEGER NOT NULL,tip_hash BLOB NOT NULL,state TEXT NOT NULL); PRAGMA user_version=2;")?;
    tx.commit()?;
    Ok(())
}

fn outside(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let resolved = parent.canonicalize()?.join(
        destination
            .file_name()
            .context("destination needs a basename")?,
    );
    ensure!(
        !resolved.starts_with(source.canonicalize()?),
        "backup/restore destination must be outside its source"
    );
    Ok(())
}

fn checksum(path: &Path) -> Result<String> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "backup refuses non-regular files"
    );
    let mut engine = sha256::Hash::engine();
    let mut file = File::open(path)?;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        engine.input(&buffer[..n]);
    }
    Ok(sha256::Hash::from_engine(engine).to_string())
}

pub fn verify_backup(root: &Path, chain: Chain) -> Result<BTreeMap<String, String>> {
    let manifest = root.join("BACKUP_COMPLETE.json");
    ensure!(
        fs::symlink_metadata(&manifest)?.file_type().is_file(),
        "backup manifest is not a regular file"
    );
    let mut bytes = Vec::new();
    File::open(manifest)?
        .take(16_200_001)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16_200_000, "backup manifest too large");
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value["network"] == chain.name(), "backup network mismatch");
    let files: BTreeMap<String, String> = serde_json::from_value(value["sha256"].clone())?;
    ensure!(
        files.len() <= 100_000 && !files.is_empty(),
        "invalid backup file count"
    );
    let directory = root.join(chain.name());
    ensure!(
        fs::symlink_metadata(&directory)?.file_type().is_dir(),
        "backup network directory is invalid"
    );
    for (name, expected) in &files {
        ensure!(
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains(['/', '\\'])
                && name.len() <= 255,
            "invalid backup filename"
        );
        ensure!(
            checksum(&directory.join(name))? == *expected,
            "backup checksum mismatch"
        );
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        ensure!(
            files.contains_key(&entry.file_name().to_string_lossy().to_string()),
            "unmanifested file in backup"
        );
    }
    Ok(files)
}

pub fn restore(backup: &Path, destination: &Path, chain: Chain) -> Result<()> {
    let files = verify_backup(backup, chain)?;
    outside(backup, destination)?;
    fs::create_dir(destination).context("restore destination must be new")?;
    let directory = destination.join(chain.name());
    fs::create_dir(&directory)?;
    for name in files.keys() {
        let target = directory.join(name);
        fs::copy(backup.join(chain.name()).join(name), &target)?;
        File::open(target)?.sync_all()?;
    }
    fs::copy(
        backup.join("BACKUP_COMPLETE.json"),
        destination.join("BACKUP_COMPLETE.json"),
    )?;
    File::open(destination.join("BACKUP_COMPLETE.json"))?.sync_all()?;
    File::open(&directory)?.sync_all()?;
    File::open(destination)?.sync_all()?;
    File::open(
        destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    verify_backup(destination, chain)?;
    Ok(())
}

pub struct RecoveryOptions<'a> {
    pub chain: Chain,
    pub limits: crate::store::Limits,
    pub output: &'a Path,
    pub min_free: u64,
    pub historical: Option<&'a Core>,
}

/// Controlled recovery: keep the old generation untouched and build a new one.
pub fn recover_chain(
    source: &Store,
    archive: &Archive,
    core: &Core,
    options: RecoveryOptions<'_>,
    mut confirm_historical: impl FnMut(u32) -> Result<bool>,
) -> Result<Value> {
    let RecoveryOptions {
        chain,
        limits,
        output,
        min_free,
        historical,
    } = options;
    let info = core.verify(chain)?;
    let height: u32 = info["blocks"]
        .as_u64()
        .context("missing Core height")?
        .try_into()?;
    let anchor = core.hash(height)?;
    outside(archive.directory(), output)?;
    fs::create_dir(output).context("recovery output root must be new")?;
    let mut recovered_archive = Archive::open(output, chain, true)?;
    let mut recovered_store = Store::open(
        &recovered_archive.directory().join("index.sqlite"),
        chain,
        limits,
    )?;
    recovered_store.recover(&mut recovered_archive)?;
    let mut historical_confirmed = false;
    let mut historical_blocks = 0u32;
    for h in 0..=height {
        ensure!(
            fs2::available_space(output)? >= min_free,
            "insufficient free space during separate-root recovery; original data preserved"
        );
        ensure!(
            core.hash(height)? == anchor,
            "Core changed branch during recovery; original data preserved"
        );
        let hash = core.hash(h)?;
        let block = if let Some((old, location)) = source.record(h)?.filter(|(old, _)| *old == hash)
        {
            let (_, block) = archive.read(location)?;
            ensure!(block.block_hash() == old, "source archive mismatch");
            block
        } else {
            match core.block(hash) {
                Ok(block) => block,
                Err(error) => {
                    // An RPC failure alone is never evidence of pruning or permission to
                    // contact another source. Confirm the local prune floor separately.
                    let pruned_error = error
                        .downcast_ref::<crate::rpc::RpcFailure>()
                        .is_some_and(|e| e.code == -1);
                    if !pruned_error {
                        return Err(error.context("local Core block request failed"));
                    }
                    let current = core.verify(chain)?;
                    let missing = current["pruned"] == true
                        && current["pruneheight"]
                            .as_u64()
                            .is_some_and(|floor| u64::from(h) < floor);
                    if !missing {
                        return Err(error.context(
                            "local Core block unavailable but pruning was not established",
                        ));
                    }
                    let alternative = historical.context("local Core has pruned required historical blocks; configure a trusted historical Core and confirm its use")?;
                    if !historical_confirmed {
                        ensure!(
                            confirm_historical(h)?,
                            "historical source not confirmed; source generation preserved"
                        );
                        alternative
                            .verify(chain)
                            .context("historical Core identity verification failed")?;
                        historical_confirmed = true;
                    }
                    let block = alternative.block(hash).context(
                        "trusted historical Core cannot supply the required canonical block",
                    )?;
                    historical_blocks = historical_blocks
                        .checked_add(1)
                        .context("historical block count overflow")?;
                    block
                }
            }
        };
        recovered_store.commit(&mut recovered_archive, h, &block)?;
        if h > 0 && h % 1000 == 0 {
            eprintln!("separate recovery: indexed height {h}/{height}");
        }
    }
    ensure!(
        core.hash(height)? == anchor,
        "Core changed branch at recovery end; original data preserved"
    );
    recovered_store.integrity_check()?;
    Ok(
        json!({"recovered":output,"height":height,"hash":anchor,"original_unchanged":true,"historical_source_blocks":historical_blocks}),
    )
}

pub fn prune(
    store: &Store,
    archive: &mut Archive,
    core: &Core,
    chain: Chain,
    backup_path: &Path,
    min_free: u64,
) -> Result<Value> {
    ensure!(
        chain != Chain::Mainnet,
        "mainnet pruning remains gated on sustained validation; no unsafe override"
    );
    store.prune_checkpoint()?;
    ensure!(
        fs2::available_space(archive.directory())? >= min_free,
        "insufficient free space for safe prune preparation"
    );
    let info = core.verify(chain)?;
    let network: Value = core.call("getnetworkinfo", json!([]))?;
    ensure!(network["networkactive"]==false,"pruning requires an offline maintenance window: explicitly disable Core networking and stop other chain writers");
    let _: Value = core.call("syncwithvalidationinterfacequeue", json!([]))?;
    ensure!(
        info["pruned"] == true && info["automatic_pruning"] == false,
        "Core must use manual prune=1"
    );
    let (height, hash) = store.stats()?.tip.context("empty index")?;
    ensure!(
        info["blocks"]
            .as_u64()
            .is_some_and(|core_height| core_height >= u64::from(height))
            && core.hash(height)? == hash,
        "indexed prefix differs from Core active chain; pruning denied"
    );
    let eligible = height
        .checked_sub(288)
        .context("insufficient reorg margin")?;
    store.integrity_check()?;
    store.audit(archive)?;
    backup(archive, backup_path)?;
    verify_backup(backup_path, chain)?;
    let network: Value = core.call("getnetworkinfo", json!([]))?;
    ensure!(
        network["networkactive"] == false,
        "Core networking was re-enabled during prune preparation"
    );
    ensure!(core.hash(height)? == hash, "Core reorg before pruning");
    let audit_id = store.prune_start(eligible)?;
    let actual: i64 = core.call("pruneblockchain", json!([eligible]))?;
    ensure!(
        actual >= -1 && actual <= i64::from(eligible),
        "unexpected Core prune result; audit needs review"
    );
    let after = core.verify(chain)?;
    ensure!(
        core.hash(height)? == hash,
        "Core changed branch during pruning; audit needs review and further pruning is blocked"
    );
    if actual >= 1 {
        let hash = core.hash(actual.try_into()?)?;
        let error = core
            .block(hash)
            .err()
            .context("Core still serves the reported pruned block; audit needs review")?;
        ensure!(
            error
                .downcast_ref::<crate::rpc::RpcFailure>()
                .is_some_and(|e| e.code == -1),
            "could not verify actual pruned block unavailability; audit needs review"
        );
    }
    store.prune_finish(audit_id, actual)?;
    Ok(
        json!({"requested":eligible,"actual":actual,"observed_pruneheight":after["pruneheight"],"backup":backup_path,"audit_id":audit_id}),
    )
}
