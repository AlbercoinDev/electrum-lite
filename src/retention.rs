//! Conservative cleanup of completed, supervisor-owned maintenance generations.
use anyhow::{ensure, Context, Result};
use rocksdb::{IteratorMode, Options, DB};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn is_absent(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(false),
    }
}

fn generation_name(name: &str, prefix: &str) -> Option<String> {
    let rest = name.strip_prefix(prefix)?;
    let (height, stamp) = rest.split_once('-')?;
    if height.is_empty()
        || stamp.is_empty()
        || !height.bytes().all(|x| x.is_ascii_digit())
        || !stamp.bytes().all(|x| x.is_ascii_digit())
    {
        return None;
    }
    height.parse::<u32>().ok()?;
    stamp.parse::<u128>().ok()?;
    Some(rest.to_owned())
}

fn record(path: &Path) -> Result<BTreeMap<String, String>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file() && metadata.len() <= 16384,
        "invalid prune record"
    );
    let mut fields = BTreeMap::new();
    for line in fs::read_to_string(path)?.lines() {
        let (name, value) = line.split_once('=').context("invalid prune record field")?;
        ensure!(
            !name.is_empty() && fields.insert(name.to_owned(), value.to_owned()).is_none(),
            "duplicate prune record field"
        );
    }
    ensure!(fields.len() == 5, "unexpected prune record fields");
    fields
        .get("requested_height")
        .context("missing requested height")?
        .parse::<u32>()?;
    fields
        .get("actual_pruneheight")
        .context("missing actual height")?
        .parse::<u32>()?;
    ensure!(
        matches!(
            fields.get("missing_block_verified").map(String::as_str),
            Some("true" | "false")
        ),
        "invalid verification flag"
    );
    Ok(fields)
}

fn owned_pair(fields: &BTreeMap<String, String>, maintenance: &Path) -> Result<(PathBuf, PathBuf)> {
    let audit = PathBuf::from(fields.get("audit").context("missing audit path")?);
    let backup = PathBuf::from(fields.get("backup").context("missing backup path")?);
    let audit_name = audit
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid audit name")?;
    let backup_name = backup
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid backup name")?;
    ensure!(
        generation_name(audit_name, "audit-")
            .is_some_and(|s| generation_name(backup_name, "backup-") == Some(s)),
        "unrecognized maintenance pair"
    );
    let dir = fs::canonicalize(maintenance)?;
    for path in [&audit, &backup] {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.file_type().is_dir(),
            "maintenance generation must be a real directory"
        );
        ensure!(
            fs::canonicalize(path)?.parent() == Some(dir.as_path()),
            "maintenance path is outside supervisor directory"
        );
    }
    Ok((audit, backup))
}

fn known_file(name: &str) -> bool {
    if matches!(name, "CURRENT" | "IDENTITY" | "LOCK" | "LOG") {
        return true;
    }
    if let Some(s) = name.strip_prefix("LOG.old.") {
        return !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    }
    for prefix in ["MANIFEST-", "OPTIONS-"] {
        if let Some(s) = name.strip_prefix(prefix) {
            return !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        }
    }
    name.rsplit_once('.').is_some_and(|(number, extension)| {
        !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
            && matches!(extension, "sst" | "log" | "ldb")
    })
}

fn recognized_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "generation is not a real directory"
    );
    let mut count = 0usize;
    for item in fs::read_dir(path)? {
        let item = item?;
        count += 1;
        ensure!(count <= 100_000, "too many generation files");
        ensure!(
            item.file_type()?.is_file() && item.file_name().to_str().is_some_and(known_file),
            "unrecognized generation content; preserving it"
        );
    }
    ensure!(count > 0, "empty generation");
    Ok(())
}

fn verify_current(audit: &Path, backup: &Path, expected_identity: &[u8]) -> Result<()> {
    let audit = DB::open_for_read_only(&Options::default(), audit, false)?;
    let backup = DB::open_for_read_only(&Options::default(), backup, false)?;
    ensure!(
        audit.get(b"identity")?.as_deref() == Some(expected_identity)
            && backup.get(b"identity")?.as_deref() == Some(expected_identity),
        "retention network identity mismatch"
    );
    let tip = backup.get(b"tip")?.context("backup tip missing")?;
    ensure!(
        audit.get(b"tip")?.as_deref() == Some(tip.as_slice()),
        "retention generation tips differ"
    );
    let mut a = audit
        .iterator(IteratorMode::Start)
        .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
    let mut b = backup
        .iterator(IteratorMode::Start)
        .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (Some(x), Some(y)) => ensure!(x? == y?, "retention audit and backup differ"),
            _ => anyhow::bail!("retention audit and backup counts differ"),
        }
    }
    for item in backup.iterator(IteratorMode::From(b"U", rocksdb::Direction::Forward)) {
        let (key, value) = item?;
        if !key.starts_with(b"U") {
            break;
        }
        ensure!(
            audit.get(&key)?.as_deref() == Some(value.as_ref()),
            "retention undo differs"
        );
    }
    Ok(())
}

/// Select the highest independently verified supervisor audit with a complete
/// durable prune record. A bad or missing generation is never treated as proof.
/// The caller must still check its ancestry against both the index and Core.
pub fn verified_audit_base(
    root: &Path,
    network: bitcoin::Network,
    max_height: u32,
) -> Result<Option<PathBuf>> {
    ensure!(
        is_absent(&root.join("prune-pending"))?,
        "pending prune prevents audit base selection"
    );
    let maintenance = root.join("supervisor");
    if is_absent(&maintenance)? {
        return Ok(None);
    }
    ensure!(
        fs::symlink_metadata(&maintenance)?.file_type().is_dir(),
        "invalid supervisor directory"
    );
    let mut candidates = Vec::new();
    for item in fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(height) = name
            .strip_prefix("prune-complete-")
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if height > max_height {
            continue;
        }
        let Ok(fields) = record(&item.path()) else {
            continue;
        };
        if fields
            .get("requested_height")
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(|n| n.checked_add(288))
            != Some(height)
        {
            continue;
        }
        let Ok((audit, backup)) = owned_pair(&fields, &maintenance) else {
            continue;
        };
        candidates.push((height, audit, backup));
        ensure!(
            candidates.len() <= 100_000,
            "too many audit base candidates"
        );
    }
    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (height, audit, backup) in candidates {
        if verify_current(&audit, &backup, format!("thin-v1:{network}").as_bytes()).is_err() {
            continue;
        }
        let db = DB::open_for_read_only(&Options::default(), &audit, false)?;
        let Some(tip) = db.get(b"tip")? else { continue };
        if tip.as_slice() != height.to_be_bytes() {
            continue;
        }
        return Ok(Some(audit));
    }
    Ok(None)
}

/// Retain the newly completed generation. Ambiguous, incomplete or unknown
/// generations are intentionally left alone. The caller owns the supervisor.
pub fn clean(
    root: &Path,
    maintenance: &Path,
    current_audit: &Path,
    current_backup: &Path,
    network: bitcoin::Network,
) -> Result<usize> {
    ensure!(
        is_absent(&root.join("prune-pending"))?,
        "pending prune prevents retention"
    );
    ensure!(
        fs::symlink_metadata(maintenance)?.file_type().is_dir(),
        "invalid supervisor directory"
    );
    let current_audit = fs::canonicalize(current_audit)?;
    let current_backup = fs::canonicalize(current_backup)?;
    let mut current_proven = false;
    let mut previous = Vec::new();
    for item in fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(height) = name.strip_prefix("prune-complete-") else {
            continue;
        };
        let Ok(height) = height.parse::<u32>() else {
            continue;
        };
        let Ok(fields) = record(&item.path()) else {
            continue;
        };
        if fields
            .get("requested_height")
            .and_then(|s| s.parse::<u32>().ok())
            .and_then(|n| n.checked_add(288))
            != Some(height)
        {
            continue;
        }
        let Ok((audit, backup)) = owned_pair(&fields, maintenance) else {
            continue;
        };
        let a = fs::canonicalize(&audit)?;
        let b = fs::canonicalize(&backup)?;
        if a == current_audit && b == current_backup {
            current_proven = true;
        } else if a != current_audit && b != current_backup {
            previous.push((audit, backup));
        }
    }
    ensure!(
        current_proven,
        "current maintenance lacks a completed ownership record"
    );
    verify_current(
        &current_audit,
        &current_backup,
        format!("thin-v1:{network}").as_bytes(),
    )?;
    let mut removed = 0;
    for (audit, backup) in previous {
        // Unknown contents, symlinks and incomplete generations are never
        // removed, even if their names resemble ours.
        if recognized_directory(&audit).is_err() || recognized_directory(&backup).is_err() {
            continue;
        }
        ensure!(
            is_absent(&root.join("prune-pending"))?,
            "pending prune prevents retention"
        );
        fs::remove_dir_all(&audit)?;
        fs::remove_dir_all(&backup)?;
        fs::File::open(maintenance)?.sync_all()?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn db(path: &Path, value: &[u8]) -> Result<()> {
        let mut options = Options::default();
        options.create_if_missing(true);
        let db = DB::open(&options, path)?;
        db.put(b"identity", b"thin-v1:regtest")?;
        db.put(b"tip", 1000u32.to_be_bytes())?;
        db.put(b"data", value)?;
        drop(db);
        Ok(())
    }

    fn completed(
        root: &Path,
        maintenance: &Path,
        height: u32,
        stamp: u32,
        value: &[u8],
    ) -> Result<(PathBuf, PathBuf)> {
        let a = maintenance.join(format!("audit-{height}-{stamp}"));
        let b = maintenance.join(format!("backup-{height}-{stamp}"));
        db(&a, value)?;
        db(&b, value)?;
        fs::write(root.join(format!("prune-complete-{height}")), format!(
            "requested_height={}\nbackup={}\naudit={}\nactual_pruneheight=0\nmissing_block_verified=false\n",
            height - 288, b.display(), a.display()
        ))?;
        Ok((a, b))
    }

    #[test]
    fn only_completed_known_old_pairs_are_removed() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-retention-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let maintenance = root.join("supervisor");
        fs::create_dir_all(&maintenance)?;
        let (old_a, old_b) = completed(&root, &maintenance, 1000, 1, b"a")?;
        let (foreign_a, foreign_b) = completed(&root, &maintenance, 1001, 2, b"b")?;
        fs::write(foreign_a.join("user-notes.txt"), b"keep")?;
        let (current_a, current_b) = completed(&root, &maintenance, 1002, 3, b"c")?;
        let orphan = maintenance.join("audit-1003-4");
        db(&orphan, b"orphan")?;
        #[cfg(unix)]
        let (outside, linked, linked_backup) = {
            use std::os::unix::fs::symlink;
            let outside = root.join("user-database");
            db(&outside, b"outside")?;
            let linked = maintenance.join("audit-1004-5");
            let linked_backup = maintenance.join("backup-1004-5");
            db(&linked_backup, b"outside")?;
            symlink(&outside, &linked)?;
            fs::write(root.join("prune-complete-1004"), format!(
                "requested_height=716\nbackup={}\naudit={}\nactual_pruneheight=0\nmissing_block_verified=false\n",
                linked_backup.display(), linked.display()
            ))?;
            (outside, linked, linked_backup)
        };
        let pending = root.join("prune-pending");
        fs::write(&pending, b"unfinished")?;
        assert!(clean(
            &root,
            &maintenance,
            &current_a,
            &current_b,
            bitcoin::Network::Regtest
        )
        .is_err());
        assert!(old_a.exists() && old_b.exists());
        fs::remove_file(&pending)?;
        assert_eq!(
            clean(
                &root,
                &maintenance,
                &current_a,
                &current_b,
                bitcoin::Network::Regtest
            )?,
            1
        );
        assert!(!old_a.exists() && !old_b.exists());
        assert!(foreign_a.join("user-notes.txt").exists() && foreign_b.exists() && orphan.exists());
        assert!(current_a.exists() && current_b.exists());
        assert!(root.join("prune-complete-1000").exists());
        #[cfg(unix)]
        assert!(outside.exists() && linked.is_symlink() && linked_backup.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn mismatched_latest_pair_preserves_all_previous_data() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-retention-mismatch-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let maintenance = root.join("supervisor");
        fs::create_dir_all(&maintenance)?;
        let (old_a, old_b) = completed(&root, &maintenance, 1000, 1, b"a")?;
        let (a, b) = completed(&root, &maintenance, 1001, 2, b"b")?;
        {
            let db = DB::open(&Options::default(), &b)?;
            db.put(b"data", b"changed")?;
        }
        assert!(clean(&root, &maintenance, &a, &b, bitcoin::Network::Regtest).is_err());
        assert!(old_a.exists() && old_b.exists() && a.exists() && b.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn audit_base_requires_completed_matching_pair_and_tip() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-retention-base-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let maintenance = root.join("supervisor");
        fs::create_dir_all(&maintenance)?;
        let (good, backup) = completed(&root, &maintenance, 1000, 1, b"ok")?;
        // This marker claims a different height than the database tip.
        completed(&root, &maintenance, 1001, 2, b"newer")?;
        assert_eq!(
            verified_audit_base(&root, bitcoin::Network::Regtest, 1001)?,
            Some(good.clone())
        );
        assert_eq!(
            verified_audit_base(&root, bitcoin::Network::Regtest, 999)?,
            None
        );
        {
            let database = DB::open(&Options::default(), &backup)?;
            database.put(b"data", b"corrupt")?;
        }
        assert_eq!(
            verified_audit_base(&root, bitcoin::Network::Regtest, 1001)?,
            None
        );
        fs::write(root.join("prune-pending"), b"incomplete")?;
        assert!(verified_audit_base(&root, bitcoin::Network::Regtest, 1001).is_err());
        assert!(good.exists() && backup.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
