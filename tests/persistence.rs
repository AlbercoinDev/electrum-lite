use std::{fs, process::Command};

use bitcoin::{
    absolute, blockdata::constants::genesis_block, consensus::serialize, transaction, Amount,
    Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use electrum_lite::{
    archive::Archive,
    index::{script_hash, Index},
    store::{CommitStage, Limits, Store, UNDO_WINDOW},
    Chain,
};
use tempfile::tempdir;

fn limits() -> Limits {
    Limits {
        transactions: 100_000,
        entries: 500_000,
    }
}

fn coinbase(height: u32) -> Transaction {
    Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(height.to_le_bytes().to_vec()),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(5_000_000_000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

fn next(previous: &Block, transactions: Vec<Transaction>) -> Block {
    let mut block = previous.clone();
    block.header.prev_blockhash = previous.block_hash();
    block.header.time += 1;
    block.txdata = transactions;
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block
}

fn spend(previous: OutPoint) -> Transaction {
    Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: previous,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(1_000_000),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51])
            };
            2
        ],
    }
}

#[test]
fn imports_legacy_archives_and_resumes_on_every_network() {
    for chain in [
        Chain::Mainnet,
        Chain::Signet,
        Chain::Testnet4,
        Chain::Regtest,
    ] {
        let root = tempdir().unwrap();
        let genesis = genesis_block(chain.network());
        let mut archive = Archive::open(root.path(), chain, true).unwrap();
        archive.scan(|_, _| Ok(())).unwrap();
        let location = archive.append(0, &genesis).unwrap();
        let path = archive.directory().join("index.sqlite");
        {
            let mut store = Store::open(&path, chain, limits()).unwrap();
            store.recover(&mut archive).unwrap();
            assert_eq!(store.stats().unwrap().transactions, 1);
            assert_eq!(
                store.record(0).unwrap(),
                Some((genesis.block_hash(), location))
            );
            assert_eq!(
                serialize(&archive.read(location).unwrap().1),
                serialize(&genesis)
            );
        }
        let mut store = Store::open(&path, chain, limits()).unwrap();
        store.recover(&mut archive).unwrap();
        assert_eq!(store.stats().unwrap().transactions, 1);
        assert!(Store::open(
            &path,
            if chain == Chain::Mainnet {
                Chain::Signet
            } else {
                Chain::Mainnet
            },
            limits()
        )
        .is_err());
    }
}

#[test]
fn durable_index_matches_reference_and_undo_restores_intra_block_state() {
    let root = tempdir().unwrap();
    let chain = Chain::Regtest;
    let genesis = genesis_block(chain.network());
    let first = next(&genesis, vec![coinbase(1)]);
    let payment = spend(OutPoint {
        txid: first.txdata[0].compute_txid(),
        vout: 0,
    });
    let child = spend(OutPoint {
        txid: payment.compute_txid(),
        vout: 0,
    });
    let second = next(&first, vec![coinbase(2), payment, child]);
    let hash = script_hash(&first.txdata[0].output[0].script_pubkey);
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let mut store =
        Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    let mut reference = Index::new(chain, 100);
    for (height, block) in [genesis, first.clone(), second].iter().enumerate() {
        reference.apply(height as u32, block).unwrap();
        store.commit(&mut archive, height as u32, block).unwrap();
        assert_eq!(
            serde_json::to_value(store.view(&hash, 100).unwrap()).unwrap(),
            serde_json::to_value(reference.view(&hash).unwrap()).unwrap()
        );
    }
    assert!(
        store.view(&hash, 1).is_err(),
        "large histories must never be truncated"
    );
    store.rollback_tip().unwrap();
    assert_eq!(store.stats().unwrap().tip, Some((1, first.block_hash())));
    assert_eq!(
        store.view(&hash, 100).unwrap().confirmed_sats,
        5_000_000_000
    );
    assert_eq!(store.view(&hash, 100).unwrap().history.len(), 1);
    assert_eq!(store.stats().unwrap().utxos, 1);
    store.integrity_check().unwrap();
}

#[test]
fn rejected_block_and_entry_limit_leave_archive_and_database_unchanged() {
    let root = tempdir().unwrap();
    let chain = Chain::Regtest;
    let genesis = genesis_block(chain.network());
    let first = next(&genesis, vec![coinbase(1)]);
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let mut store = Store::open(
        &archive.directory().join("index.sqlite"),
        chain,
        Limits {
            transactions: 100,
            entries: 2,
        },
    )
    .unwrap();
    store.recover(&mut archive).unwrap();
    store.commit(&mut archive, 0, &genesis).unwrap();
    let path = archive.directory().join("blocks-00000000.elb");
    let original = fs::read(&path).unwrap();
    let stats = store.stats().unwrap();
    assert!(store.commit(&mut archive, 1, &first).is_err());
    assert_eq!(store.stats().unwrap(), stats);
    assert_eq!(fs::read(&path).unwrap(), original);
    let mut bad = first.clone();
    bad.txdata.push(spend(OutPoint {
        txid: genesis.txdata[0].compute_txid(),
        vout: 0,
    }));
    bad.header.merkle_root = bad.compute_merkle_root().unwrap();
    let error = store.commit(&mut archive, 1, &bad).unwrap_err();
    assert!(error
        .to_string()
        .contains("missing or already spent prevout"));
    assert_eq!(store.stats().unwrap(), stats);
}

#[test]
fn committed_refs_reject_missing_torn_or_mixed_archive() {
    for corrupt in ["missing", "torn", "wrong-offset"] {
        let root = tempdir().unwrap();
        let chain = Chain::Regtest;
        let mut archive = Archive::open(root.path(), chain, true).unwrap();
        let path = archive.directory().join("index.sqlite");
        let mut store = Store::open(&path, chain, limits()).unwrap();
        store.recover(&mut archive).unwrap();
        store
            .commit(&mut archive, 0, &genesis_block(chain.network()))
            .unwrap();
        drop(store);
        drop(archive);
        let record = root.path().join("regtest/blocks-00000000.elb");
        match corrupt {
            "missing" => fs::rename(&record, record.with_extension("saved")).unwrap(),
            "torn" => {
                let f = fs::OpenOptions::new().write(true).open(&record).unwrap();
                f.set_len(120).unwrap();
            }
            _ => {
                let conn = rusqlite::Connection::open(&path).unwrap();
                conn.execute("UPDATE blocks SET offset=offset+1", [])
                    .unwrap();
            }
        }
        let mut archive = Archive::open(root.path(), chain, false).unwrap();
        let mut store = Store::open(&path, chain, limits()).unwrap();
        assert!(store.recover(&mut archive).is_err());
    }
}

#[test]
fn unknown_schema_fails_without_rewriting_database() {
    let root = tempdir().unwrap();
    let path = root.path().join("index.sqlite");
    drop(Store::open(&path, Chain::Mainnet, limits()).unwrap());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let bytes = fs::read(&path).unwrap();
    assert!(Store::open(&path, Chain::Mainnet, limits()).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn bounded_undo_refuses_deeper_rollback() {
    let root = tempdir().unwrap();
    let chain = Chain::Regtest;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let mut store =
        Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    let mut block = genesis_block(chain.network());
    store.commit(&mut archive, 0, &block).unwrap();
    for height in 1..=UNDO_WINDOW + 2 {
        block = next(&block, vec![coinbase(height)]);
        store.commit(&mut archive, height, &block).unwrap();
    }
    for _ in 0..UNDO_WINDOW {
        store.rollback_tip().unwrap();
    }
    assert_eq!(store.stats().unwrap().tip.unwrap().0, 2);
    assert!(store.rollback_tip().is_err());
    store.integrity_check().unwrap();
}

#[test]
fn crash_worker() {
    let Ok(root) = std::env::var("ELECTRUM_TEST_CRASH_ROOT") else {
        return;
    };
    let stage = std::env::var("ELECTRUM_TEST_CRASH_STAGE").unwrap();
    let mut archive = Archive::open(std::path::Path::new(&root), Chain::Regtest, false).unwrap();
    let mut store = Store::open(
        &archive.directory().join("index.sqlite"),
        Chain::Regtest,
        limits(),
    )
    .unwrap();
    store.recover(&mut archive).unwrap();
    let genesis = genesis_block(Chain::Regtest.network());
    let block = next(&genesis, vec![coinbase(1)]);
    store
        .commit_with_hook(&mut archive, 1, &block, |at| {
            if format!("{at:?}") == stage {
                std::process::exit(86);
            }
            Ok(())
        })
        .unwrap();
    panic!("crash stage was not reached");
}

#[test]
fn subprocess_crashes_recover_before_archive_after_archive_and_after_commit() {
    for stage in [
        CommitStage::Prepared,
        CommitStage::ArchiveSynced,
        CommitStage::ReferencesStaged,
        CommitStage::Committed,
    ] {
        let root = tempdir().unwrap();
        let chain = Chain::Regtest;
        {
            let mut archive = Archive::open(root.path(), chain, true).unwrap();
            let mut store =
                Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
            store.recover(&mut archive).unwrap();
            store
                .commit(&mut archive, 0, &genesis_block(chain.network()))
                .unwrap();
        }
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_worker", "--nocapture"])
            .env("ELECTRUM_TEST_CRASH_ROOT", root.path())
            .env("ELECTRUM_TEST_CRASH_STAGE", format!("{stage:?}"))
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(86),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let mut archive = Archive::open(root.path(), chain, false).unwrap();
        let mut store =
            Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
        assert_eq!(
            store.stats().unwrap().tip.unwrap().0,
            if stage == CommitStage::Committed {
                1
            } else {
                0
            }
        );
        store.recover(&mut archive).unwrap();
        assert_eq!(
            store.stats().unwrap().tip.unwrap().0,
            if stage == CommitStage::Prepared { 0 } else { 1 }
        );
        let before = store.stats().unwrap();
        store.recover(&mut archive).unwrap();
        assert_eq!(before, store.stats().unwrap());
        store.integrity_check().unwrap();
    }
}

#[test]
fn rebuild_creates_separate_index_and_never_overwrites_existing_output() {
    let root = tempdir().unwrap();
    let chain = Chain::Regtest;
    {
        let mut archive = Archive::open(root.path(), chain, true).unwrap();
        archive.scan(|_, _| Ok(())).unwrap();
        archive.append(0, &genesis_block(chain.network())).unwrap();
    }
    let output = root.path().join("rebuilt.sqlite");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_electrum-lite"))
            .args([
                "--network",
                "regtest",
                "--data-dir",
                root.path().to_str().unwrap(),
                "rebuild-index",
                "--output",
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    };
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let before = fs::read(&output).unwrap();
    assert!(!run().status.success());
    assert_eq!(before, fs::read(&output).unwrap());
    assert!(!root.path().join("regtest/index.sqlite").exists());
    let reserved = root.path().join("regtest/blocks-00000001.elb");
    let rejected = Command::new(env!("CARGO_BIN_EXE_electrum-lite"))
        .args([
            "--network",
            "regtest",
            "--data-dir",
            root.path().to_str().unwrap(),
            "rebuild-index",
            "--output",
            reserved.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!reserved.exists());
}

#[test]
fn archive_io_failure_rolls_back_all_prepared_sql() {
    let root = tempdir().unwrap();
    let chain = Chain::Regtest;
    let mut archive = Archive::open(root.path(), chain, true).unwrap();
    let mut store =
        Store::open(&archive.directory().join("index.sqlite"), chain, limits()).unwrap();
    store.recover(&mut archive).unwrap();
    let genesis = genesis_block(chain.network());
    store.commit(&mut archive, 0, &genesis).unwrap();
    let record = archive.directory().join("blocks-00000000.elb");
    let saved = record.with_extension("saved");
    let original = fs::read(&record).unwrap();
    let before = store.stats().unwrap();
    fs::rename(&record, &saved).unwrap();
    fs::create_dir(&record).unwrap(); // Forces append() to fail without deleting archived bytes.
    let block = next(&genesis, vec![coinbase(1)]);
    assert!(store.commit(&mut archive, 1, &block).is_err());
    assert_eq!(store.stats().unwrap(), before);
    fs::remove_dir(&record).unwrap();
    fs::rename(&saved, &record).unwrap();
    assert_eq!(fs::read(&record).unwrap(), original);
    store.recover(&mut archive).unwrap();
    store.integrity_check().unwrap();
}
