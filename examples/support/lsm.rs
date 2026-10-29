//! Throwaway LSM backend contract experiment. Not connected to Store or pruning.
//! No archive cursor or Bitcoin semantics are claimed by this KV-only layer.
use anyhow::{ensure, Context, Result};
use bitcoin::hashes::{sha256, Hash, HashEngine};
use rocksdb::{
    checkpoint::Checkpoint, BlockBasedOptions, Cache, Options, WriteBatch, WriteOptions, DB,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Read,
    path::Path,
};

const IDENTITY: &[u8] = b"\0lite-lsm-prototype-v1";
const GENERATION: &[u8] = b"\0generation";
pub type Changes = [(Vec<u8>, Option<Vec<u8>>)];

pub struct Prototype {
    db: DB,
    failed: bool,
}

fn options(create: bool) -> Options {
    let mut options = Options::default();
    options.create_if_missing(create);
    options.set_error_if_exists(create);
    options.set_paranoid_checks(true);
    options.set_use_fsync(true);
    options.set_max_open_files(128);
    options.set_write_buffer_size(4 * 1024 * 1024);
    options.set_max_write_buffer_number(2);
    options.set_max_background_jobs(2);
    // Automatic compaction stays enabled; never hide deferred IBD work.
    let mut table = BlockBasedOptions::default();
    table.set_block_cache(&Cache::new_lru_cache(8 * 1024 * 1024));
    options.set_block_based_table_factory(&table);
    options
}

fn durable() -> WriteOptions {
    let mut write = WriteOptions::default();
    write.disable_wal(false);
    write.set_sync(true);
    write
}

impl Prototype {
    pub fn create(path: &Path, identity: &[u8]) -> Result<Self> {
        ensure!(
            !path.exists() && !path.is_symlink(),
            "destination must be new"
        );
        ensure!(!identity.is_empty(), "empty identity");
        let db = DB::open(&options(true), path)?;
        let mut batch = WriteBatch::default();
        batch.put(IDENTITY, identity);
        batch.put(GENERATION, 0u64.to_be_bytes());
        db.write_opt(batch, &durable())?;
        File::open(path)?.sync_all()?;
        File::open(path.parent().context("missing parent")?)?.sync_all()?;
        Ok(Self { db, failed: false })
    }

    pub fn open(path: &Path, identity: &[u8]) -> Result<Self> {
        ensure!(path.is_dir() && !path.is_symlink(), "missing database");
        let db = DB::open(&options(false), path)?;
        ensure!(
            db.get(IDENTITY)?.as_deref() == Some(identity),
            "identity mismatch"
        );
        let result = Self { db, failed: false };
        result.generation()?;
        Ok(result)
    }

    pub fn generation(&self) -> Result<u64> {
        ensure!(!self.failed, "reopen required after write failure");
        let bytes = self.db.get(GENERATION)?.context("missing generation")?;
        Ok(u64::from_be_bytes(bytes.as_slice().try_into()?))
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        ensure!(!self.failed, "reopen required after write failure");
        ensure!(!key.is_empty() && key[0] != 0, "reserved key");
        Ok(self.db.get(key)?)
    }

    /// A single ordered owner must resolve Bitcoin state before passing deltas.
    /// All mutations and generation publish in one WAL-enabled sync WriteBatch.
    pub fn commit(&mut self, expected: u64, changes: &Changes) -> Result<()> {
        ensure!(self.generation()? == expected, "stale generation");
        ensure!(
            !changes.is_empty() && changes.len() <= 65_536,
            "invalid delta count"
        );
        let next = expected.checked_add(1).context("generation overflow")?;
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let mut batch = WriteBatch::default();
        for (key, value) in changes {
            ensure!(
                !key.is_empty() && key[0] != 0 && key.len() <= 128,
                "invalid key"
            );
            ensure!(seen.insert(key), "duplicate mutation");
            bytes = bytes
                .checked_add(key.len() + value.as_ref().map_or(0, Vec::len))
                .context("delta overflow")?;
            ensure!(bytes <= 16 * 1024 * 1024, "delta byte limit");
            if let Some(value) = value {
                batch.put(key, value);
            } else {
                batch.delete(key);
            }
        }
        batch.put(GENERATION, next.to_be_bytes());
        if let Err(error) = self.db.write_opt(batch, &durable()) {
            self.failed = true;
            return Err(error.into());
        }
        Ok(())
    }

    pub fn settle(&self) -> Result<()> {
        ensure!(!self.failed, "reopen required after write failure");
        self.db.flush()?;
        self.db.compact_range(None::<&[u8]>, None::<&[u8]>);
        Ok(())
    }

    /// Checkpoint API rather than copying arbitrary files of an active DB.
    /// Exclusive mutable borrow prevents commits during checkpoint+manifest.
    pub fn backup(&mut self, destination: &Path) -> Result<()> {
        ensure!(!self.failed, "reopen required after write failure");
        ensure!(
            !new_path(destination)?.starts_with(self.db.path().canonicalize()?),
            "backup must be outside live database"
        );
        fs::create_dir(destination).context("backup destination must be new")?;
        let checkpoint = destination.join("db");
        Checkpoint::new(&self.db)?.create_checkpoint(&checkpoint)?;
        let files = inventory(&checkpoint, true)?;
        let manifest = Manifest {
            format: 1,
            generation: self.generation()?,
            files,
        };
        let path = destination.join("manifest.json");
        fs::write(&path, serde_json::to_vec(&manifest)?)?;
        File::open(path)?.sync_all()?;
        File::open(destination)?.sync_all()?;
        File::open(destination.parent().context("missing backup parent")?)?.sync_all()?;
        Ok(())
    }

    pub fn restore(backup: &Path, destination: &Path, identity: &[u8]) -> Result<Self> {
        ensure!(
            !new_path(destination)?.starts_with(backup.canonicalize()?),
            "restore must be outside backup"
        );
        let path = backup.join("manifest.json");
        ensure!(
            !path.is_symlink() && fs::metadata(&path)?.len() <= 1024 * 1024,
            "invalid manifest"
        );
        let manifest: Manifest = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(manifest.format == 1, "unknown backup format");
        let source = backup.join("db");
        ensure!(
            inventory(&source, false)? == manifest.files,
            "backup mismatch"
        );
        fs::create_dir(destination).context("restore destination must be new")?;
        for name in manifest.files.keys() {
            let target = destination.join(name);
            fs::copy(source.join(name), &target)?;
            File::open(target)?.sync_all()?;
        }
        File::open(destination)?.sync_all()?;
        File::open(destination.parent().context("missing restore parent")?)?.sync_all()?;
        ensure!(
            inventory(destination, false)? == manifest.files,
            "restored bytes mismatch"
        );
        let restored = Self::open(destination, identity)?;
        ensure!(
            restored.generation()? == manifest.generation,
            "generation mismatch"
        );
        Ok(restored)
    }
}

fn new_path(path: &Path) -> Result<std::path::PathBuf> {
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

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    generation: u64,
    files: BTreeMap<String, String>,
}

fn inventory(directory: &Path, sync: bool) -> Result<BTreeMap<String, String>> {
    ensure!(
        !directory.is_symlink() && directory.is_dir(),
        "invalid checkpoint directory"
    );
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file(),
            "checkpoint links/subdirectories refused"
        );
        let mut file = File::open(entry.path())?;
        let mut engine = sha256::Hash::engine();
        let mut buffer = [0u8; 65536];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            engine.input(&buffer[..length]);
        }
        if sync {
            file.sync_all()?;
        }
        result.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 filename"))?,
            sha256::Hash::from_engine(engine).to_string(),
        );
    }
    if sync {
        File::open(directory)?.sync_all()?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_child() -> Result<()> {
        if let Some(path) = std::env::var_os("LITE_LSM_CRASH_CHILD") {
            let mut db = Prototype::create(Path::new(&path), b"crash-test")?;
            db.commit(0, &[(b"coin".to_vec(), Some(b"durable".to_vec()))])?;
            // Skip DB destruction and memtable flush, like process termination
            // after acknowledgement. Does NOT simulate a physical power loss.
            std::process::exit(0);
        }
        Ok(())
    }

    #[test]
    fn acknowledged_wal_survives_process_exit_without_destructors() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "lsm::tests::crash_child", "--nocapture"])
            .env("LITE_LSM_CRASH_CHILD", &path)
            .status()?;
        ensure!(status.success(), "child failed");
        let db = Prototype::open(&path, b"crash-test")?;
        assert_eq!(db.generation()?, 1);
        assert_eq!(db.get(b"coin")?, Some(b"durable".to_vec()));
        Ok(())
    }

    #[test]
    fn snapshot_is_consistent_across_generation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Prototype::create(&root.path().join("db"), b"snapshot-test")?;
        db.commit(0, &[(b"coin".to_vec(), Some(b"old".to_vec()))])?;
        let snapshot = db.db.snapshot();
        let mut batch = WriteBatch::default();
        batch.put(b"coin", b"new");
        batch.put(GENERATION, 2u64.to_be_bytes());
        // Direct access here only to test the generic library snapshot. Normal
        // writers must exclusively use Prototype::commit.
        db.db.write_opt(batch, &durable())?;
        assert_eq!(snapshot.get(b"coin")?, Some(b"old".to_vec()));
        assert_eq!(snapshot.get(GENERATION)?, Some(1u64.to_be_bytes().to_vec()));
        assert_eq!(db.generation()?, 2);
        Ok(())
    }

    #[test]
    fn reopen_backup_restore_and_refusals() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let mut db = Prototype::create(&path, b"signet-prototype")?;
        db.commit(0, &[(b"coin".to_vec(), Some(b"witness bytes".to_vec()))])?;
        assert!(db.commit(0, &[(b"other".to_vec(), None)]).is_err());
        assert!(db.commit(1, &[(GENERATION.to_vec(), None)]).is_err());
        assert!(db
            .commit(1, &[(b"coin".to_vec(), None), (b"coin".to_vec(), None)])
            .is_err());
        drop(db);
        assert!(Prototype::open(&path, b"wrong-network").is_err());
        let mut db = Prototype::open(&path, b"signet-prototype")?;
        assert_eq!(db.generation()?, 1);
        assert_eq!(db.get(b"coin")?, Some(b"witness bytes".to_vec()));
        let backup = root.path().join("backup");
        db.backup(&backup)?;
        db.commit(1, &[(b"coin".to_vec(), None)])?;
        let restored_path = root.path().join("restore");
        let restored = Prototype::restore(&backup, &restored_path, b"signet-prototype")?;
        assert_eq!(restored.generation()?, 1);
        assert_eq!(restored.get(b"coin")?, Some(b"witness bytes".to_vec()));
        assert!(db.backup(&backup).is_err());
        assert!(Prototype::restore(&backup, &restored_path, b"signet-prototype").is_err());
        fs::write(backup.join("db/injected"), b"tamper")?;
        assert!(
            Prototype::restore(&backup, &root.path().join("bad"), b"signet-prototype").is_err()
        );
        assert!(!root.path().join("bad").exists());
        Ok(())
    }
}
