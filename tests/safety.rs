use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    process::Command,
};

use bitcoin::{
    absolute, blockdata::constants::genesis_block, consensus::serialize, hashes::Hash, transaction,
    Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use electrum_lite::{
    archive::Archive,
    index::{script_hash, Index},
    rpc::Core,
    validate_block, Chain,
};
use tempfile::tempdir;

fn next(previous: &Block, transactions: Vec<Transaction>) -> Block {
    let mut block = previous.clone();
    block.header.prev_blockhash = previous.block_hash();
    block.header.time += 1;
    block.txdata = transactions;
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block
}

fn coinbase(tag: u8) -> Transaction {
    Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(vec![1, tag]),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(5_000_000_000),
            script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
        }],
    }
}

fn spend(previous_output: OutPoint, values: &[u64]) -> Transaction {
    Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: values
            .iter()
            .map(|value| TxOut {
                value: Amount::from_sat(*value),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            })
            .collect(),
    }
}

#[test]
fn all_networks_round_trip_and_resume() {
    for chain in [
        Chain::Mainnet,
        Chain::Signet,
        Chain::Testnet4,
        Chain::Regtest,
    ] {
        let dir = tempdir().unwrap();
        let genesis = genesis_block(chain.network());
        {
            let mut archive = Archive::open(dir.path(), chain, true).unwrap();
            assert!(archive.append(0, &genesis).is_err());
            archive.scan(|_, _| Ok(())).unwrap();
            archive.append(0, &genesis).unwrap();
        }
        let mut archive = Archive::open(dir.path(), chain, false).unwrap();
        let mut index = Index::new(chain, 100);
        let mut count = 0;
        archive
            .scan(|height, block| {
                assert_eq!(serialize(block), serialize(&genesis));
                count += 1;
                index.apply(height, block)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert!(index.utxos.is_empty());
        assert!(archive.append(0, &genesis).is_err());
    }
}

#[test]
fn index_intra_block_spends_dedup_and_atomic_failure() {
    let genesis = genesis_block(Chain::Regtest.network());
    let funding = coinbase(1);
    let first = next(&genesis, vec![funding.clone()]);
    let spend1 = spend(
        OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        },
        &[2_000_000_000, 2_000_000_000],
    );
    let spend2 = spend(
        OutPoint {
            txid: spend1.compute_txid(),
            vout: 0,
        },
        &[1_000_000_000],
    );
    let second = next(&first, vec![coinbase(2), spend1.clone(), spend2]);
    let mut index = Index::new(Chain::Regtest, 100);
    index.apply(0, &genesis).unwrap();
    index.apply(1, &first).unwrap();
    index.apply(2, &second).unwrap();
    let hash = script_hash(&ScriptBuf::from_bytes(vec![0x51]));
    let view = index.view(&hash).unwrap();
    assert_eq!(view.confirmed_sats, 8_000_000_000);
    assert_eq!(view.history.len(), 4);
    assert_eq!(
        view.history
            .iter()
            .filter(|o| o.txid == spend1.compute_txid())
            .count(),
        1
    );
    let bad = next(
        &second,
        vec![
            coinbase(3),
            spend(
                OutPoint {
                    txid: bitcoin::Txid::all_zeros(),
                    vout: 1,
                },
                &[1],
            ),
        ],
    );
    let before = index.tip;
    assert!(index.apply(3, &bad).is_err());
    assert_eq!(index.tip, before);
    assert_eq!(
        index.view(&hash).unwrap().confirmed_sats,
        view.confirmed_sats
    );
}

#[test]
fn bad_merkle_witness_and_wrong_network_are_rejected() {
    let genesis = genesis_block(Chain::Regtest.network());
    let mut corrupt = genesis.clone();
    corrupt.txdata[0].output[0].value = Amount::from_sat(1);
    assert!(validate_block(&corrupt).is_err());
    let mut witness = next(&genesis, vec![coinbase(1)]);
    witness.txdata[0].input[0].witness = Witness::from_slice(&[vec![0u8; 32]]);
    assert!(validate_block(&witness).is_err());
    assert!(Index::new(Chain::Mainnet, 100).apply(0, &genesis).is_err());
    assert!(Index::new(Chain::Regtest, 0).apply(0, &genesis).is_err());
}

#[test]
fn torn_tail_corruption_and_lock_are_rejected() {
    let dir = tempdir().unwrap();
    let chain = Chain::Regtest;
    let mut archive = Archive::open(dir.path(), chain, true).unwrap();
    assert!(Archive::open(dir.path(), chain, true).is_err());
    archive.scan(|_, _| Ok(())).unwrap();
    archive.append(0, &genesis_block(chain.network())).unwrap();
    drop(archive);
    let path = dir.path().join("regtest/blocks-00000000.elb");
    let original = fs::read(&path).unwrap();
    for length in [1, 115, original.len() - 1] {
        fs::write(&path, &original[..length]).unwrap();
        let mut archive = Archive::open(dir.path(), chain, false).unwrap();
        assert!(archive.scan(|_, _| Ok(())).is_err());
        assert!(archive.append(0, &genesis_block(chain.network())).is_err());
    }
    fs::write(&path, &original).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(120)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(120)).unwrap();
    file.write_all(&[byte[0] ^ 1]).unwrap();
    let mut archive = Archive::open(dir.path(), chain, false).unwrap();
    assert!(archive.scan(|_, _| Ok(())).is_err());
}

#[test]
fn archive_network_metadata_and_size_limits_are_checked() {
    let dir = tempdir().unwrap();
    let mut archive = Archive::open(dir.path(), Chain::Mainnet, true).unwrap();
    archive.scan(|_, _| Ok(())).unwrap();
    archive
        .append(0, &genesis_block(Chain::Mainnet.network()))
        .unwrap();
    drop(archive);
    let path = dir.path().join("mainnet/blocks-00000000.elb");
    let original = fs::read(&path).unwrap();
    let mut bad = original.clone();
    bad[8] ^= 1;
    fs::write(&path, bad).unwrap();
    {
        let mut archive = Archive::open(dir.path(), Chain::Mainnet, false).unwrap();
        assert!(archive.scan(|_, _| Ok(())).is_err());
    }
    let mut bad = original;
    bad[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, bad).unwrap();
    let mut archive = Archive::open(dir.path(), Chain::Mainnet, false).unwrap();
    assert!(archive.scan(|_, _| Ok(())).is_err());
}

#[test]
fn rpc_rejects_remote_credentials_and_redirect_capable_urls() {
    let cookie = std::path::Path::new("unused");
    for url in [
        "http://example.com/",
        "http://192.168.1.5:8332/",
        "https://127.0.0.1/",
        "http://user:pass@127.0.0.1/",
        "http://127.0.0.1/?secret=x",
    ] {
        assert!(Core::new(url, cookie).is_err());
    }
    assert!(Core::new("http://127.0.0.1:8332/", cookie).is_ok());
    assert!(Core::new("http://[::1]:8332/", cookie).is_ok());
}

#[test]
fn cli_offline_inspect_history_and_transaction() {
    let dir = tempdir().unwrap();
    let genesis = genesis_block(Chain::Mainnet.network());
    {
        let mut archive = Archive::open(dir.path(), Chain::Mainnet, true).unwrap();
        archive.scan(|_, _| Ok(())).unwrap();
        archive.append(0, &genesis).unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_electrum-lite"))
            .args([
                "--network",
                "mainnet",
                "--data-dir",
                dir.path().to_str().unwrap(),
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let inspected = run(&["inspect"]);
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let hash = script_hash(&genesis.txdata[0].output[0].script_pubkey);
    let history = run(&["history", &hash]);
    assert!(history.status.success());
    let history: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history["confirmed_sats"], 0);
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
    let tx = run(&["transaction", &genesis.txdata[0].compute_txid().to_string()]);
    assert!(tx.status.success());
    let tx: serde_json::Value = serde_json::from_slice(&tx.stdout).unwrap();
    assert_eq!(tx[0]["hex"], hex::encode(serialize(&genesis.txdata[0])));
}

#[test]
fn scripthash_matches_electrum_byte_order() {
    let script = ScriptBuf::from_bytes(vec![0x51]);
    let mut expected = bitcoin::hashes::sha256::Hash::hash(script.as_bytes()).to_byte_array();
    expected.reverse();
    assert_eq!(script_hash(&script), hex::encode(expected));
}

#[test]
fn duplicate_spend_and_reorg_do_not_mutate_index() {
    let genesis = genesis_block(Chain::Regtest.network());
    let funding = coinbase(1);
    let first = next(&genesis, vec![funding.clone()]);
    let outpoint = OutPoint {
        txid: funding.compute_txid(),
        vout: 0,
    };
    let double = next(
        &first,
        vec![
            coinbase(2),
            spend(outpoint, &[100]),
            spend(outpoint, &[200]),
        ],
    );
    let mut index = Index::new(Chain::Regtest, 100);
    index.apply(0, &genesis).unwrap();
    index.apply(1, &first).unwrap();
    let before = index.tip;
    assert!(index.apply(2, &double).is_err());
    assert_eq!(index.tip, before);
    assert!(index.utxos.contains_key(&outpoint));
    let competing = next(&genesis, vec![coinbase(3)]);
    assert!(index.apply(1, &competing).is_err());
    assert_eq!(index.tip, before);
}

#[test]
fn synthetic_bip30_overwrite_preserves_history_occurrences() {
    let genesis = genesis_block(Chain::Mainnet.network());
    let funding = coinbase(1);
    let first = next(&genesis, vec![funding.clone()]);
    let mut index = Index::new(Chain::Mainnet, 100);
    index.apply(0, &genesis).unwrap();
    index.apply(1, &first).unwrap();
    // Exercise the exception branch without pretending this is a real historical block fixture.
    index.tip = Some((91841, first.block_hash()));
    let duplicate = next(&first, vec![funding]);
    index.apply(91842, &duplicate).unwrap();
    let view = index
        .view(&script_hash(&ScriptBuf::from_bytes(vec![0x51])))
        .unwrap();
    assert_eq!(view.utxos.len(), 1);
    assert_eq!(view.utxos[0].1.height, 91842);
    assert_eq!(view.history.len(), 2);
}

#[test]
fn entry_limit_rejects_before_mutating_a_block() {
    let genesis = genesis_block(Chain::Regtest.network());
    let first = next(&genesis, vec![coinbase(1)]);
    let mut index = Index::with_limits(Chain::Regtest, 100, 2);
    index.apply(0, &genesis).unwrap();
    let tip = index.tip;
    assert!(index.apply(1, &first).is_err());
    assert_eq!(index.tip, tip);
    assert!(index.utxos.is_empty());
    assert_eq!(index.transactions, 1);
}
