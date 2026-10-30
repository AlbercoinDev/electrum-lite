#![cfg(feature = "experimental-lsm")]
use anyhow::{ensure, Result};
use bitcoin::{
    absolute,
    blockdata::constants::genesis_block,
    consensus::serialize,
    hashes::{sha256, Hash},
    transaction, Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use electrum_lite::{
    archive::Archive,
    experimental_lsm::{BitcoinStore, Stage},
    index::script_hash,
    store::{Limits, Store},
    Chain,
};
use std::{fs, process::Command};

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
fn fixtures() -> Vec<Block> {
    let genesis = genesis_block(Chain::Regtest.network());
    let first = next(&genesis, vec![coinbase(1)]);
    let spend = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: first.txdata[0].compute_txid(),
                vout: 0,
            },
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
    };
    let mut second = next(&first, vec![coinbase(2), spend]);
    second.txdata[1].input[0].witness = Witness::from_slice(&[vec![1, 2, 3, 4]]);
    let reserved = [0u8; 32];
    second.txdata[0].input[0].witness = Witness::from_slice(&[reserved]);
    let commitment = Block::compute_witness_commitment(&second.witness_root().unwrap(), &reserved);
    let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    script.extend(commitment.to_byte_array());
    second.txdata[0].output.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(script),
    });
    second.header.merkle_root = second.compute_merkle_root().unwrap();
    vec![genesis, first, second]
}
fn script() -> [u8; 32] {
    sha256::Hash::hash(&[0x51]).to_byte_array()
}

#[test]
fn bitcoin_deltas_match_sqlite_and_archive_roundtrips() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let mut lsm = BitcoinStore::create(&root.join("lsm"), Chain::Regtest)?;
    let mut archive = Archive::open(&root.join("sqlite"), Chain::Regtest, true)?;
    let mut sqlite = Store::open(
        &archive.directory().join("index.sqlite"),
        Chain::Regtest,
        Limits {
            transactions: 100_000,
            entries: 100_000,
        },
    )?;
    sqlite.recover(&mut archive)?;
    let blocks = fixtures();
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
        sqlite.commit(&mut archive, height as u32, block)?;
        ensure!(
            serialize(&lsm.block(height as u32)?) == serialize(block),
            "archive bytes changed"
        );
        for transaction in &block.txdata {
            for vout in 0..transaction.output.len() {
                let outpoint = OutPoint {
                    txid: transaction.compute_txid(),
                    vout: vout as u32,
                };
                let actual = lsm.coin(outpoint)?;
                let expected = sqlite.coin(outpoint)?;
                assert_eq!(
                    actual
                        .as_ref()
                        .map(|coin| (coin.value, coin.height, coin.coinbase)),
                    expected
                        .as_ref()
                        .map(|coin| (coin.value, coin.height, coin.coinbase))
                );
            }
        }
    }
    let sql_history = sqlite
        .view(&script_hash(&ScriptBuf::from_bytes(vec![0x51])), 100)?
        .history;
    let expected: Vec<_> = sql_history
        .iter()
        .map(|entry| (entry.height, entry.position as u32, entry.txid))
        .collect();
    assert_eq!(lsm.history(script(), 100)?, expected);
    assert!(lsm.history(script(), 1).is_err());
    let before = lsm.state()?;
    let bad = next(&blocks[2], vec![coinbase(3), blocks[2].txdata[1].clone()]);
    assert!(lsm.commit(3, &bad).is_err());
    assert_eq!(lsm.state()?, before);
    drop(lsm);
    let reopened = BitcoinStore::open(&root.join("lsm"), Chain::Regtest)?;
    assert_eq!(reopened.state()?, before);
    Ok(())
}

#[test]
fn crash_child() -> Result<()> {
    let Some(path) = std::env::var_os("LITE_BITCOIN_LSM_CRASH_ROOT") else {
        return Ok(());
    };
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(std::path::Path::new(&path), Chain::Regtest)?;
    lsm.commit(0, &blocks[0])?;
    lsm.commit(1, &blocks[1])?;
    let target = std::env::var("LITE_BITCOIN_LSM_CRASH_STAGE")?;
    lsm.commit_with_hook(2, &blocks[2], |stage| {
        if matches!(
            (target.as_str(), stage),
            ("prepared", Stage::Prepared)
                | ("archive", Stage::ArchiveSynced)
                | ("commit", Stage::Committed)
        ) {
            std::process::exit(0);
        }
        Ok(())
    })?;
    anyhow::bail!("crash hook was not reached")
}

#[test]
fn recovery_at_every_durability_frontier_is_idempotent() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let blocks = fixtures();
    for stage in ["prepared", "archive", "commit"] {
        let root = temporary.path().join(stage);
        let status = Command::new(std::env::current_exe()?)
            .args(["--exact", "crash_child", "--nocapture"])
            .env("LITE_BITCOIN_LSM_CRASH_ROOT", &root)
            .env("LITE_BITCOIN_LSM_CRASH_STAGE", stage)
            .status()?;
        ensure!(status.success(), "child failed");
        let mut lsm = BitcoinStore::open(&root, Chain::Regtest)?;
        let height = if stage == "prepared" { 1 } else { 2 };
        assert_eq!(lsm.state()?.tip.as_ref().unwrap().height, height);
        assert_eq!(
            serialize(&lsm.block(height)?),
            serialize(&blocks[height as usize])
        );
        let before = lsm.state()?;
        lsm.recover()?;
        assert_eq!(lsm.state()?, before);
    }
    Ok(())
}

#[test]
fn durable_ancestor_intent_replays_and_new_branch_survives_restart() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    drop(lsm);
    // Simulate death after durable rewind intent, before DB rollback commit.
    let mut archive = Archive::open(&root, Chain::Regtest, false)?;
    archive.enable_branches();
    archive.scan(|_, _| Ok(()))?;
    archive.append(1, &blocks[1])?;
    drop(archive);
    let mut lsm = BitcoinStore::open(&root, Chain::Regtest)?;
    assert_eq!(lsm.state()?.tip.as_ref().unwrap().height, 1);
    let old_coin = OutPoint {
        txid: blocks[1].txdata[0].compute_txid(),
        vout: 0,
    };
    assert!(lsm.coin(old_coin)?.is_some());
    let replacement = next(&blocks[1], vec![coinbase(200)]);
    lsm.commit(2, &replacement)?;
    assert_eq!(lsm.history(script(), 100)?.len(), 2);
    drop(lsm);
    let mut lsm = BitcoinStore::open(&root, Chain::Regtest)?;
    assert_eq!(serialize(&lsm.block(2)?), serialize(&replacement));
    lsm.rewind(1)?;
    assert!(lsm.coin(old_coin)?.is_some());
    Ok(())
}

#[test]
fn full_generation_backup_restore_and_corruption_refusal() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root.join("source"), Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    lsm.rewind(1)?;
    let replacement = next(&blocks[1], vec![coinbase(300)]);
    lsm.commit(2, &replacement)?;
    let before = lsm.state()?;
    let backup = root.join("backup");
    lsm.backup(&backup)?;
    let mut restored = BitcoinStore::restore(&backup, &root.join("restored"), Chain::Regtest)?;
    assert_eq!(restored.state()?, before);
    assert_eq!(serialize(&restored.block(2)?), serialize(&replacement));
    restored.rewind(1)?;
    assert_eq!(restored.state()?.tip.as_ref().unwrap().height, 1);
    assert_eq!(lsm.state()?, before);
    assert!(BitcoinStore::restore(&backup, &root.join("wrong-network"), Chain::Signet).is_err());
    assert!(!root.join("wrong-network").exists());
    let segment = backup.join("regtest/blocks-00000000.elb");
    let mut data = fs::read(&segment)?;
    data[120] ^= 1;
    fs::write(segment, data)?;
    assert!(BitcoinStore::restore(&backup, &root.join("corrupt"), Chain::Regtest).is_err());
    assert!(!root.join("corrupt").exists());
    Ok(())
}

#[test]
fn torn_tail_and_missing_published_reference_fail_closed() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let blocks = fixtures();
    for mode in ["tail", "missing"] {
        let root = temporary.path().join(mode);
        let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
        lsm.commit(0, &blocks[0])?;
        lsm.commit(1, &blocks[1])?;
        drop(lsm);
        let segment = root.join("regtest/blocks-00000000.elb");
        let mut data = fs::read(&segment)?;
        if mode == "tail" {
            data.extend(b"torn");
        } else {
            data.truncate(data.len() - 1);
        }
        fs::write(&segment, &data)?;
        assert!(BitcoinStore::open(&root, Chain::Regtest).is_err());
        assert_eq!(fs::read(&segment)?, data);
    }
    Ok(())
}

#[test]
fn semantic_audit_rejects_index_only_corruption() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    lsm.commit(0, &blocks[0])?;
    lsm.commit(1, &blocks[1])?;
    drop(lsm);
    let identity = format!("lite-bitcoin-lsm-v3:regtest:{}", Chain::Regtest.genesis());
    let mut db = electrum_lite::experimental_lsm::kv::Prototype::open(
        &root.join("regtest/lsm"),
        identity.as_bytes(),
    )?;
    let mut key = vec![b'C'];
    key.extend(serialize(&OutPoint {
        txid: blocks[1].txdata[0].compute_txid(),
        vout: 0,
    }));
    db.commit(db.generation()?, &[(key, None)])?;
    drop(db);
    assert!(BitcoinStore::open(&root, Chain::Regtest).is_err());
    assert!(!fs::read_dir(&root)?.any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("lsm-audit-")));
    Ok(())
}

#[test]
fn retained_undo_is_bounded_and_deeper_rewind_never_appends() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    let mut block = genesis_block(Chain::Regtest.network());
    lsm.commit(0, &block)?;
    for height in 1..=289 {
        block = next(&block, vec![coinbase(height)]);
        lsm.commit(height, &block)?;
    }
    let path = root.join("regtest/blocks-00000000.elb");
    let before = fs::metadata(&path)?.len();
    assert!(lsm.rewind(0).is_err());
    assert_eq!(fs::metadata(&path)?.len(), before);
    lsm.rewind(1)?;
    assert_eq!(lsm.state()?.tip.as_ref().unwrap().height, 1);
    drop(lsm);
    assert_eq!(
        BitcoinStore::open(&root, Chain::Regtest)?
            .state()?
            .tip
            .as_ref()
            .unwrap()
            .height,
        1
    );
    Ok(())
}

#[test]
fn intrablock_spend_order_and_undo_restore_exact_prior_coins() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    lsm.commit(0, &blocks[0])?;
    lsm.commit(1, &blocks[1])?;
    let original = OutPoint {
        txid: blocks[1].txdata[0].compute_txid(),
        vout: 0,
    };
    let prior = lsm.coin(original)?;
    let mut payment = blocks[2].txdata[1].clone();
    payment.input[0].witness = Witness::new();
    let intermediate = OutPoint {
        txid: payment.compute_txid(),
        vout: 0,
    };
    let mut descendant = payment.clone();
    descendant.input[0].previous_output = intermediate;
    let descendant_output = OutPoint {
        txid: descendant.compute_txid(),
        vout: 0,
    };
    let invalid = next(
        &blocks[1],
        vec![coinbase(2), descendant.clone(), payment.clone()],
    );
    let segment = root.join("regtest/blocks-00000000.elb");
    let before = fs::metadata(&segment)?.len();
    assert!(lsm.commit(2, &invalid).is_err());
    assert_eq!(fs::metadata(&segment)?.len(), before);
    let ordered = next(&blocks[1], vec![coinbase(2), payment, descendant]);
    lsm.commit(2, &ordered)?;
    assert!(lsm.coin(original)?.is_none());
    assert!(lsm.coin(intermediate)?.is_none());
    assert_eq!(lsm.coin(descendant_output)?.unwrap().script, script());
    assert_eq!(lsm.history(script(), 100)?.len(), 4);
    lsm.rewind(1)?;
    assert_eq!(lsm.coin(original)?, prior);
    assert!(lsm.coin(intermediate)?.is_none());
    assert!(lsm.coin(descendant_output)?.is_none());
    assert_eq!(lsm.history(script(), 100)?.len(), 1);
    Ok(())
}

#[test]
fn lsm_snapshot_serves_complete_wallet_history_and_witness_proofs() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    let snapshot = lsm.snapshot()?;
    let genesis_script =
        sha256::Hash::hash(blocks[0].txdata[0].output[0].script_pubkey.as_bytes()).to_byte_array();
    let genesis_view = snapshot.view(genesis_script, 0, 0)?;
    assert_eq!(genesis_view.confirmed_sats, 0);
    assert!(genesis_view.history.is_empty());
    assert!(genesis_view.utxos.is_empty());
    let view = snapshot.view(script(), 100, 100)?;
    assert_eq!(view.generation, 3);
    assert_eq!(view.tip, Some((2, blocks[2].block_hash())));
    assert_eq!(view.confirmed_sats, 5_002_000_000);
    assert_eq!(view.history.len(), 3);
    assert_eq!(view.utxos.len(), 3);
    assert_eq!(
        view.utxos.iter().map(|(_, coin)| coin.value).sum::<u64>(),
        view.confirmed_sats
    );
    assert_eq!(
        snapshot.headers(0, 3)?,
        blocks
            .iter()
            .flat_map(|block| serialize(&block.header))
            .collect::<Vec<_>>()
    );
    let payment = &blocks[2].txdata[1];
    let (occurrence, bytes) = snapshot.transaction(payment.compute_txid(), Some(2), 4_000_000)?;
    assert_eq!(occurrence.position, 1);
    assert_eq!(occurrence.block_hash, blocks[2].block_hash());
    assert_eq!(bytes, serialize(payment));
    assert!(snapshot.proof(payment.compute_txid(), 2)?.verify());
    assert!(snapshot
        .proof(blocks[0].txdata[0].compute_txid(), 0)?
        .verify());
    assert!(snapshot.view(script(), 2, 100).is_err());
    assert!(snapshot.view(script(), 100, 2).is_err());
    assert!(snapshot
        .transaction(payment.compute_txid(), Some(2), bytes.len() - 1)
        .is_err());
    assert!(snapshot.headers(0, 2017).is_err());
    assert!(snapshot.headers(2, 2).is_err());
    assert!(snapshot
        .occurrences(payment.compute_txid(), None, 0)
        .is_err());
    let unknown = [255u8; 32];
    assert_eq!(snapshot.balance(unknown)?, 0);
    assert!(snapshot.list_unspent(unknown, 0)?.is_empty());
    Ok(())
}

#[test]
fn lsm_balances_headers_occurrences_and_queries_follow_durable_rewind() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    let old_txid = blocks[2].txdata[1].compute_txid();
    lsm.rewind(1)?;
    {
        let snapshot = lsm.snapshot()?;
        assert_eq!(snapshot.generation(), 4);
        assert_eq!(snapshot.balance(script())?, 5_000_000_000);
        assert_eq!(snapshot.list_unspent(script(), 100)?.len(), 1);
        assert!(snapshot.header(2).is_err());
        assert!(snapshot.transaction(old_txid, None, 4_000_000).is_err());
    }
    let replacement = next(&blocks[1], vec![coinbase(200)]);
    lsm.commit(2, &replacement)?;
    let backup = temporary.path().join("backup");
    lsm.backup(&backup)?;
    let restored =
        BitcoinStore::restore(&backup, &temporary.path().join("restored"), Chain::Regtest)?;
    let snapshot = restored.snapshot()?;
    assert_eq!(snapshot.generation(), 5);
    assert_eq!(
        snapshot.view(script(), 100, 100)?.confirmed_sats,
        10_000_000_000
    );
    assert_eq!(snapshot.header(2)?, replacement.header);
    assert!(snapshot.occurrences(old_txid, None, 16)?.is_empty());
    Ok(())
}

#[test]
fn lsm_merkle_queries_handle_odd_layers_and_reject_tampered_proofs() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    let mut left = blocks[2].txdata[1].clone();
    left.input[0].witness = Witness::new();
    left.input[0].previous_output = OutPoint {
        txid: blocks[2].txdata[1].compute_txid(),
        vout: 0,
    };
    let mut right = left.clone();
    right.input[0].previous_output.vout = 1;
    let third = next(&blocks[2], vec![coinbase(3), left, right]);
    lsm.commit(3, &third)?;
    let snapshot = lsm.snapshot()?;
    for transaction in &third.txdata {
        let mut proof = snapshot.proof(transaction.compute_txid(), 3)?;
        assert!(proof.verify());
        proof.branch[0][0] ^= 1;
        assert!(!proof.verify());
    }
    Ok(())
}

#[test]
fn lsm_query_occurrences_remain_distinct_after_repeated_txid_and_rewind() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let blocks = fixtures();
    let mut lsm = BitcoinStore::create(&root, Chain::Regtest)?;
    for (height, block) in blocks.iter().enumerate() {
        lsm.commit(height as u32, block)?;
    }
    // Synthetic occurrence test: original coin was spent at height 2, so the
    // repeated TXID does not overwrite an unspent coin. Core owns BIP34 rules.
    let repeated = next(&blocks[2], vec![blocks[1].txdata[0].clone()]);
    lsm.commit(3, &repeated)?;
    let txid = blocks[1].txdata[0].compute_txid();
    {
        let snapshot = lsm.snapshot()?;
        let found = snapshot.occurrences(txid, None, 16)?;
        assert_eq!(
            found.iter().map(|entry| entry.height).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_ne!(found[0].block_hash, found[1].block_hash);
        assert_eq!(snapshot.transaction(txid, None, 4_000_000)?.0.height, 3);
        assert_eq!(snapshot.transaction(txid, Some(1), 4_000_000)?.0.height, 1);
        assert!(snapshot.proof(txid, 1)?.verify());
        assert!(snapshot.proof(txid, 3)?.verify());
        assert!(snapshot.occurrences(txid, None, 1).is_err());
    }
    lsm.rewind(2)?;
    let snapshot = lsm.snapshot()?;
    assert_eq!(snapshot.transaction(txid, None, 4_000_000)?.0.height, 1);
    assert_eq!(snapshot.balance(script())?, 5_002_000_000);
    Ok(())
}

#[test]
fn lsm_old_experimental_format_is_rejected_without_adaptation() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("data");
    let archive = Archive::open(&root, Chain::Regtest, true)?;
    let path = archive.directory().join("lsm");
    let old_identity = format!("lite-bitcoin-lsm-v1:regtest:{}", Chain::Regtest.genesis());
    drop(electrum_lite::experimental_lsm::kv::Prototype::create(
        &path,
        old_identity.as_bytes(),
    )?);
    drop(archive);
    assert!(BitcoinStore::open(&root, Chain::Regtest).is_err());
    let untouched =
        electrum_lite::experimental_lsm::kv::Prototype::open(&path, old_identity.as_bytes())?;
    assert_eq!(untouched.generation()?, 0);
    Ok(())
}

#[test]
fn lsm_cumulative_caps_are_atomic_and_recovery_honors_configured_limits() -> Result<()> {
    use electrum_lite::experimental_lsm::{Counts, IndexLimits};
    let temporary = tempfile::tempdir()?;
    let blocks = fixtures();
    let root = temporary.path().join("limited");
    let mut store = BitcoinStore::create_with_limits(
        &root,
        Chain::Regtest,
        IndexLimits {
            transactions: 1,
            entries: 100,
        },
    )?;
    store.commit(0, &blocks[0])?;
    assert_eq!(
        store.snapshot()?.counts()?,
        Counts {
            transactions: 1,
            utxos: 0,
            history: 1
        }
    );
    let segment = root.join("regtest/blocks-00000000.elb");
    let size = fs::metadata(&segment)?.len();
    assert!(store.commit(1, &blocks[1]).is_err());
    assert_eq!(fs::metadata(segment)?.len(), size);
    drop(store);
    let root = temporary.path().join("tail");
    let mut store = BitcoinStore::create(&root, Chain::Regtest)?;
    store.commit(0, &blocks[0])?;
    assert!(store
        .commit_with_hook(1, &blocks[1], |stage| {
            if stage == Stage::ArchiveSynced {
                anyhow::bail!("injected crash");
            }
            Ok(())
        })
        .is_err());
    drop(store);
    assert!(BitcoinStore::open_with_limits(
        &root,
        Chain::Regtest,
        IndexLimits {
            transactions: 1,
            entries: 100
        }
    )
    .is_err());
    let mut store = BitcoinStore::open(&root, Chain::Regtest)?;
    assert_eq!(
        store.snapshot()?.counts()?,
        Counts {
            transactions: 2,
            utxos: 1,
            history: 2
        }
    );
    store.commit(2, &blocks[2])?;
    assert_eq!(
        store.snapshot()?.counts()?,
        Counts {
            transactions: 4,
            utxos: 3,
            history: 5
        }
    );
    store.rewind(1)?;
    assert_eq!(
        store.snapshot()?.counts()?,
        Counts {
            transactions: 2,
            utxos: 1,
            history: 2
        }
    );
    Ok(())
}
