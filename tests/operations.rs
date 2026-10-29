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
fn unfinished_prune_blocks_scheduling_across_restart() {
    let root = tempdir().unwrap();
    let chain = Chain::Signet;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let path = archive.directory().join("index.sqlite");
    let mut store = Store::open(&path, chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    store
        .commit(&mut archive, 0, &genesis_block(chain.network()))
        .unwrap();
    assert_eq!(store.prune_checkpoint().unwrap(), 0);
    store.prune_start(0).unwrap();
    drop(store);
    let store = Store::open(&path, chain, limits()).unwrap();
    assert!(store.prune_checkpoint().is_err());
    assert!(store.prune_start(0).is_err());
    let core = Core::new("http://127.0.0.1:1/", &root.path().join("absent-cookie")).unwrap();
    let backup = root.path().join("must-not-create");
    let error = operations::prune(&store, &mut archive, &core, chain, &backup, 0).unwrap_err();
    assert!(error.to_string().contains("previous prune request"));
    assert!(!backup.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn automatic_mainnet_pruning_is_denied_without_creating_roots() {
    let root = tempdir().unwrap();
    let data = root.path().join("data");
    let core = root.path().join("core");
    let backups = root.path().join("backups");
    fs::create_dir(&backups).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_electrum-lite"))
        .args(["--network", "mainnet", "--data-dir"])
        .arg(&data)
        .args([
            "managed-serve",
            "--bitcoind",
            "/does-not-exist",
            "--core-dir",
        ])
        .arg(&core)
        .args([
            "--rpc-port",
            "18499",
            "--confirm-dedicated",
            "--auto-prune",
            "--prune-backup-root",
        ])
        .arg(&backups)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("limited to test networks"));
    assert!(!data.exists());
    assert!(!core.exists());
    assert_eq!(fs::read_dir(backups).unwrap().count(), 0);
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

#[test]
fn historical_core_is_contacted_only_after_pruned_block_and_confirmation() {
    use bitcoin::consensus::serialize;
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    let root = tempdir().unwrap();
    let cookie = root.path().join("cookie");
    fs::write(&cookie, "user:secret").unwrap();
    let genesis = genesis_block(Chain::Regtest.network());
    let mut block = genesis.clone();
    block.header.prev_blockhash = genesis.block_hash();
    block.header.time += 1;
    let blocks = vec![genesis.clone(), block.clone()];
    let stop = Arc::new(AtomicBool::new(false));
    let alt_count = Arc::new(AtomicUsize::new(0));
    let local_error = Arc::new(AtomicI64::new(-1));
    let local_pruned = Arc::new(AtomicBool::new(true));
    let mut handles = Vec::new();
    let mut urls = Vec::new();
    for alternative in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        urls.push(format!("http://{}/", listener.local_addr().unwrap()));
        let stop = stop.clone();
        let alt_count = alt_count.clone();
        let local_error = local_error.clone();
        let local_pruned = local_pruned.clone();
        let blocks = blocks.clone();
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() { Ok((stream,_)) => stream, Err(e) if e.kind()==std::io::ErrorKind::WouldBlock => {std::thread::sleep(Duration::from_millis(5)); continue}, Err(e)=>panic!("{e}") };
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut bytes=Vec::new(); let mut buf=[0u8;4096];
                let (body_start, length) = loop {
                    let n=stream.read(&mut buf).unwrap(); assert!(n>0); bytes.extend_from_slice(&buf[..n]);
                    if let Some(end)=bytes.windows(4).position(|v|v==b"\r\n\r\n") {
                        let head=String::from_utf8_lossy(&bytes[..end]);
                        let length:usize=head.lines().find_map(|l|l.to_ascii_lowercase().strip_prefix("content-length:").map(|s|s.trim().parse().unwrap())).unwrap();
                        if bytes.len()>=end+4+length {break (end+4,length)}
                    }
                };
                if alternative {alt_count.fetch_add(1,Ordering::Relaxed);}
                let request:Value=serde_json::from_slice(&bytes[body_start..body_start+length]).unwrap();
                let method=request["method"].as_str().unwrap();
                let result=match method {
                    "getblockchaininfo"=>json!({"chain":"regtest","blocks":1,"pruned":!alternative && local_pruned.load(Ordering::Relaxed),"pruneheight":2}),
                    "getblockhash"=>json!(blocks[request["params"][0].as_u64().unwrap() as usize].block_hash()),
                    "getblock"=>{
                        let hash=request["params"][0].as_str().unwrap();
                        if !alternative && local_pruned.load(Ordering::Relaxed) && hash==blocks[1].block_hash().to_string() {Value::Null}
                        else {json!(hex::encode(serialize(blocks.iter().find(|b|b.block_hash().to_string()==hash).unwrap())))}
                    },
                    _=>panic!("unexpected RPC {method}"),
                };
                let body=if result.is_null(){json!({"id":"electrum-lite","result":null,"error":{"code":local_error.load(Ordering::Relaxed),"message":"pruned"}})}else{json!({"id":"electrum-lite","result":result,"error":null})}.to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        }));
    }
    let local = Core::new(&urls[0], &cookie).unwrap();
    let historical = Core::new(&urls[1], &cookie).unwrap();
    let mut archive = Archive::open(root.path(), Chain::Regtest, true).unwrap();
    let mut store = Store::open(
        &archive.directory().join("index.sqlite"),
        Chain::Regtest,
        limits(),
    )
    .unwrap();
    store.recover(&mut archive).unwrap();
    local_pruned.store(false, Ordering::Relaxed);
    let output = root.path().join("local-only");
    let options = operations::RecoveryOptions {
        chain: Chain::Regtest,
        limits: limits(),
        output: &output,
        min_free: 0,
        historical: Some(&historical),
    };
    let result = operations::recover_chain(&store, &archive, &local, options, |_| {
        panic!("source not needed")
    })
    .unwrap();
    assert_eq!(result["historical_source_blocks"], 0);
    assert_eq!(alt_count.load(Ordering::Relaxed), 0);
    local_pruned.store(true, Ordering::Relaxed);
    local_error.store(-28, Ordering::Relaxed);
    let output = root.path().join("rpc-failure");
    let options = operations::RecoveryOptions {
        chain: Chain::Regtest,
        limits: limits(),
        output: &output,
        min_free: 0,
        historical: Some(&historical),
    };
    let err = operations::recover_chain(&store, &archive, &local, options, |_| {
        panic!("RPC failure must not request confirmation")
    })
    .unwrap_err();
    assert!(err
        .downcast_ref::<electrum_lite::rpc::RpcFailure>()
        .is_some_and(|e| e.code == -28));
    assert_eq!(alt_count.load(Ordering::Relaxed), 0);
    local_error.store(-1, Ordering::Relaxed);
    let output = root.path().join("denied");
    let options = operations::RecoveryOptions {
        chain: Chain::Regtest,
        limits: limits(),
        output: &output,
        min_free: 0,
        historical: Some(&historical),
    };
    let err = operations::recover_chain(&store, &archive, &local, options, |height| {
        assert_eq!(height, 1);
        Ok(false)
    })
    .unwrap_err();
    assert!(err.to_string().contains("not confirmed"));
    assert_eq!(alt_count.load(Ordering::Relaxed), 0);
    assert_eq!(store.stats().unwrap().tip, None);
    let output = root.path().join("accepted");
    let options = operations::RecoveryOptions {
        chain: Chain::Regtest,
        limits: limits(),
        output: &output,
        min_free: 0,
        historical: Some(&historical),
    };
    let result = operations::recover_chain(&store, &archive, &local, options, |height| {
        assert_eq!(height, 1);
        Ok(true)
    })
    .unwrap();
    assert_eq!(result["historical_source_blocks"], 1);
    assert!(alt_count.load(Ordering::Relaxed) > 0);
    let mut recovered = Archive::open(&output, Chain::Regtest, false).unwrap();
    let mut index = Store::open(
        &recovered.directory().join("index.sqlite"),
        Chain::Regtest,
        limits(),
    )
    .unwrap();
    index.recover(&mut recovered).unwrap();
    assert_eq!(
        serialize(
            &recovered
                .read(index.record(1).unwrap().unwrap().1)
                .unwrap()
                .1
        ),
        serialize(&block)
    );
    index.audit(&mut recovered).unwrap();
    assert_eq!(store.stats().unwrap().tip, None);
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        handle.join().unwrap();
    }
}
