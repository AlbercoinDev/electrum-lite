use bitcoin::blockdata::constants::genesis_block;
use electrum_lite::{
    archive::Archive,
    operations,
    rpc::Core,
    store::{Limits, Store},
    Chain,
};
use std::fs;
use tempfile::tempdir;

fn limits() -> Limits {
    Limits {
        transactions: 100,
        entries: 1000,
    }
}

#[test]
fn explicit_schema_migration_has_verified_backup_and_backfills_headers() {
    let root = tempdir().unwrap();
    let chain = Chain::Signet;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let path = archive.directory().join("index.sqlite");
    {
        let mut store = Store::open(&path, chain, limits()).unwrap();
        store.recover(&mut archive).unwrap();
        store
            .commit(&mut archive, 0, &genesis_block(chain.network()))
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE archive_cursor; DROP TABLE prune_audit; ALTER TABLE blocks DROP COLUMN header; PRAGMA user_version=1;").unwrap();
    }
    let old = fs::read(&path).unwrap();
    assert!(Store::open(&path, chain, limits()).is_err());
    assert_eq!(fs::read(&path).unwrap(), old);
    let backup = root.path().join("schema1-backup");
    operations::migrate(&archive, chain, &backup).unwrap();
    operations::verify_backup(&backup, chain).unwrap();
    assert_eq!(fs::read(backup.join("signet/index.sqlite")).unwrap(), old);
    let mut store = Store::open(&path, chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    assert_eq!(
        store.header(0).unwrap(),
        genesis_block(chain.network()).header
    );
    store.audit(&mut archive).unwrap();
}

#[test]
fn backup_restore_are_matching_non_destructive_and_detect_tampering() {
    let root = tempdir().unwrap();
    let chain = Chain::Testnet4;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let mut store =
        Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    store
        .commit(&mut archive, 0, &genesis_block(chain.network()))
        .unwrap();
    let fingerprint = store.fingerprint().unwrap();
    let backup = root.path().join("backup");
    operations::backup(&archive, &backup).unwrap();
    assert!(operations::backup(&archive, &backup).is_err());
    let restore = root.path().join("restored");
    operations::restore(&backup, &restore, chain).unwrap();
    assert!(operations::restore(&backup, &restore, chain).is_err());
    let mut restored_archive = Archive::open(&restore, chain, false).unwrap();
    let mut restored_store = Store::open(
        &restored_archive.directory().join("index.sqlite"),
        chain,
        limits(),
    )
    .unwrap();
    restored_store.recover(&mut restored_archive).unwrap();
    assert_eq!(restored_store.fingerprint().unwrap(), fingerprint);
    assert!(operations::verify_backup(&backup, Chain::Mainnet).is_err());
    fs::write(backup.join("testnet4/blocks-00000000.elb"), b"corruption").unwrap();
    let rejected = root.path().join("rejected");
    assert!(operations::restore(&backup, &rejected, chain).is_err());
    assert!(!rejected.exists());
    assert!(operations::backup(&archive, &archive.directory().join("nested")).is_err());
    assert!(!archive.directory().join("nested").exists());
}

#[test]
fn untrusted_manifest_cannot_traverse_paths() {
    let root = tempdir().unwrap();
    fs::create_dir(root.path().join("regtest")).unwrap();
    fs::write(
        root.path().join("BACKUP_COMPLETE.json"),
        r#"{"network":"regtest","sha256":{"../escape":"00"}}"#,
    )
    .unwrap();
    assert!(operations::verify_backup(root.path(), Chain::Regtest).is_err());
}

#[test]
fn mainnet_pruning_is_denied_before_network_or_filesystem_side_effects() {
    let root = tempdir().unwrap();
    let chain = Chain::Mainnet;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let store = Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
    let core = Core::new(
        "http://127.0.0.1:1/",
        std::path::Path::new("nonexistent-cookie"),
    )
    .unwrap();
    let backup = root.path().join("no-backup");
    let error = operations::prune(&store, &mut archive, &core, chain, &backup, 0).unwrap_err();
    assert!(error.to_string().contains("mainnet pruning"));
    assert!(!backup.exists());
}
