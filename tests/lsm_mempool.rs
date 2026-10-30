#![cfg(feature = "experimental-lsm")]
use anyhow::{Context, Result};
use bitcoin::{
    absolute,
    blockdata::constants::genesis_block,
    consensus::serialize,
    hashes::{sha256, Hash},
    transaction, Amount, Block, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness,
};
use electrum_lite::{
    experimental_lsm::{
        mempool::{Budget, Listing, Mempool, Refresh, Source},
        BitcoinStore,
    },
    Chain,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

struct Fake {
    list: Listing,
    raw: BTreeMap<Txid, Vec<u8>>,
    calls: Cell<usize>,
    change_after: Option<usize>,
    fail: bool,
}
impl Source for Fake {
    fn listing(&self, _: usize) -> Result<Listing> {
        anyhow::ensure!(!self.fail, "injected source failure");
        let count = self.calls.get() + 1;
        self.calls.set(count);
        let mut list = self.list.clone();
        if self.change_after.is_some_and(|after| count >= after) {
            list.sequence += 1;
        }
        Ok(list)
    }
    fn transaction(&self, txid: Txid, _: usize) -> Result<Vec<u8>> {
        self.raw.get(&txid).cloned().context("test TX missing")
    }
}
fn hash(script: u8) -> [u8; 32] {
    sha256::Hash::hash(&[script]).to_byte_array()
}
fn payment(previous: OutPoint, value: u64, script: u8) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: previous,
            script_sig: ScriptBuf::new(),
            sequence: Sequence(0xffff_fffd),
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value),
            script_pubkey: ScriptBuf::from_bytes(vec![script]),
        }],
    }
}
fn next(previous: &Block, txs: Vec<Transaction>) -> Block {
    let mut block = previous.clone();
    block.header.prev_blockhash = previous.block_hash();
    block.header.time += 1;
    block.txdata = txs;
    block.header.merkle_root = block.compute_merkle_root().unwrap();
    block
}
fn fixture() -> Result<(tempfile::TempDir, BitcoinStore, OutPoint)> {
    let root = tempfile::tempdir()?;
    let mut store = BitcoinStore::create(&root.path().join("data"), Chain::Regtest)?;
    let genesis = genesis_block(Chain::Regtest.network());
    store.commit(0, &genesis)?;
    let mut coinbase = payment(OutPoint::null(), 5_000_000_000, 0x51);
    coinbase.input[0].script_sig = ScriptBuf::from_bytes(vec![1, 1]);
    let block = next(&genesis, vec![coinbase]);
    let outpoint = OutPoint {
        txid: block.txdata[0].compute_txid(),
        vout: 0,
    };
    store.commit(1, &block)?;
    Ok((root, store, outpoint))
}
fn source(store: &BitcoinStore, txs: &[Transaction]) -> Result<Fake> {
    let raw: BTreeMap<_, _> = txs
        .iter()
        .map(|tx| (tx.compute_txid(), serialize(tx)))
        .collect();
    Ok(Fake {
        list: Listing {
            instance: [1; 32],
            tip: store.state()?.tip.unwrap().hash,
            sequence: 10,
            txids: raw.keys().copied().collect(),
        },
        raw,
        calls: Cell::new(0),
        change_after: None,
        fail: false,
    })
}
fn budget() -> Budget {
    Budget {
        fetch_per_pass: 1,
        ..Budget::default()
    }
}

#[test]
fn warmup_chains_rbf_and_generation_readiness_never_mutate_confirmed_state() -> Result<()> {
    let (_root, mut store, original) = fixture()?;
    let parent = payment(original, 4_999_900_000, 0x52);
    let child = payment(
        OutPoint {
            txid: parent.compute_txid(),
            vout: 0,
        },
        4_999_800_000,
        0x53,
    );
    let grandchild = payment(
        OutPoint {
            txid: child.compute_txid(),
            vout: 0,
        },
        4_999_700_000,
        0x54,
    );
    let mut source = source(&store, &[parent.clone(), child.clone(), grandchild.clone()])?;
    let mut pool = Mempool::default();
    let stop = AtomicBool::new(false);
    let before = store.state()?;
    {
        let snapshot = store.snapshot()?;
        assert!(matches!(
            pool.reconcile(&snapshot, &source, budget(), &stop)?,
            Refresh::Warming {
                cached: 1,
                total: 3
            }
        ));
        assert!(!pool.ready_for(&snapshot));
        assert!(pool.view(&snapshot, hash(0x51), 100).is_err());
        assert!(matches!(
            pool.reconcile(&snapshot, &source, budget(), &stop)?,
            Refresh::Warming {
                cached: 2,
                total: 3
            }
        ));
        assert!(matches!(
            pool.reconcile(&snapshot, &source, budget(), &stop)?,
            Refresh::Ready {
                transactions: 3,
                ..
            }
        ));
        let root = pool.view(&snapshot, hash(0x51), 100)?;
        assert_eq!(root.confirmed_sats, 5_000_000_000);
        assert_eq!(root.unconfirmed_sats, -5_000_000_000);
        assert!(root.utxos.is_empty());
        let intermediate = pool.view(&snapshot, hash(0x52), 100)?;
        assert_eq!(intermediate.unconfirmed_sats, 0);
        assert!(intermediate.utxos.is_empty());
        assert_eq!(
            intermediate
                .pending
                .iter()
                .map(|entry| entry.height)
                .collect::<Vec<_>>(),
            vec![0, -1]
        );
        let final_ = pool.view(&snapshot, hash(0x54), 100)?;
        assert_eq!(final_.unconfirmed_sats, 4_999_700_000);
        assert_eq!(final_.utxos.len(), 1);
        assert_eq!(final_.utxos[0].1.height, 0);
        assert_eq!(final_.pending[0].height, -1);
        assert_eq!(final_.pending[0].fee, 100_000);
        assert_eq!(
            pool.transaction(&snapshot, parent.compute_txid(), 4_000_000)?,
            serialize(&parent)
        );
        assert!(pool.view(&snapshot, hash(0x52), 1).is_err());
        let replacement = payment(original, 4_999_000_000, 0x55);
        source.raw = BTreeMap::from([(replacement.compute_txid(), serialize(&replacement))]);
        source.list.txids = source.raw.keys().copied().collect();
        source.list.sequence += 1;
        assert!(matches!(
            pool.reconcile(&snapshot, &source, budget(), &stop)?,
            Refresh::Ready {
                transactions: 1,
                ..
            }
        ));
        assert!(pool
            .transaction(&snapshot, parent.compute_txid(), 4_000_000)
            .is_err());
        assert!(pool.view(&snapshot, hash(0x52), 100)?.pending.is_empty());
        assert_eq!(
            pool.view(&snapshot, hash(0x55), 100)?.pending[0].fee,
            1_000_000
        );
    }
    assert_eq!(store.state()?, before);
    store.rewind(0)?;
    let snapshot = store.snapshot()?;
    assert!(!pool.ready_for(&snapshot));
    assert!(pool.view(&snapshot, hash(0x55), 100).is_err());
    assert!(pool.reconcile(&snapshot, &source, budget(), &stop).is_err());
    source.list.tip = snapshot.state().tip.as_ref().unwrap().hash;
    assert!(pool.reconcile(&snapshot, &source, budget(), &stop).is_err());
    Ok(())
}

#[test]
fn sequence_instance_errors_stop_and_budgets_invalidate_published_results() -> Result<()> {
    let (_root, store, original) = fixture()?;
    let parent = payment(original, 4_999_900_000, 0x52);
    let mut source = source(&store, std::slice::from_ref(&parent))?;
    let snapshot = store.snapshot()?;
    let stop = AtomicBool::new(false);
    let mut pool = Mempool::default();
    pool.reconcile(&snapshot, &source, budget(), &stop)?;
    assert!(pool.ready_for(&snapshot));
    source.change_after = Some(source.calls.get() + 2);
    assert!(pool.reconcile(&snapshot, &source, budget(), &stop).is_err());
    assert!(!pool.ready_for(&snapshot));
    source.change_after = None;
    source.list.sequence += 1;
    pool.reconcile(&snapshot, &source, budget(), &stop)?;
    let mut modified = parent.clone();
    modified.input[0].witness = Witness::from_slice(&[vec![1, 2, 3]]);
    assert_eq!(modified.compute_txid(), parent.compute_txid());
    source
        .raw
        .insert(modified.compute_txid(), serialize(&modified));
    source.list.instance = [2; 32];
    pool.reconcile(&snapshot, &source, budget(), &stop)?;
    assert_eq!(
        pool.transaction(&snapshot, parent.compute_txid(), 4_000_000)?,
        serialize(&modified)
    );
    stop.store(true, Ordering::Relaxed);
    assert!(pool.reconcile(&snapshot, &source, budget(), &stop).is_err());
    assert!(!pool.ready_for(&snapshot));
    stop.store(false, Ordering::Relaxed);
    for tiny in [
        Budget {
            transactions: 0,
            ..budget()
        },
        Budget {
            bytes: 1,
            ..budget()
        },
        Budget {
            entries: 0,
            ..budget()
        },
    ] {
        assert!(pool.reconcile(&snapshot, &source, tiny, &stop).is_err());
        assert!(!pool.ready_for(&snapshot));
    }
    source.fail = true;
    assert!(pool.reconcile(&snapshot, &source, budget(), &stop).is_err());
    assert!(!pool.ready_for(&snapshot));
    Ok(())
}

#[test]
fn conflicts_missing_prevouts_and_negative_fees_fail_closed() -> Result<()> {
    let (_root, store, original) = fixture()?;
    let snapshot = store.snapshot()?;
    let stop = AtomicBool::new(false);
    let parent = payment(original, 4_999_900_000, 0x52);
    let conflict = payment(original, 4_999_800_000, 0x53);
    let missing = payment(
        OutPoint {
            txid: parent.compute_txid(),
            vout: 100,
        },
        1,
        0x52,
    );
    let overspend = payment(original, 5_000_000_001, 0x52);
    let all = Budget {
        fetch_per_pass: 32,
        ..budget()
    };
    for transactions in [
        vec![parent.clone(), conflict],
        vec![missing],
        vec![overspend],
    ] {
        let source = source(&store, &transactions)?;
        let mut pool = Mempool::default();
        assert!(pool.reconcile(&snapshot, &source, all, &stop).is_err());
        assert!(!pool.ready_for(&snapshot));
    }
    let source = source(&store, &[])?;
    let mut pool = Mempool::default();
    assert!(matches!(
        pool.reconcile(&snapshot, &source, all, &stop)?,
        Refresh::Ready {
            transactions: 0,
            ..
        }
    ));
    assert_eq!(
        pool.view(&snapshot, hash(0x51), 100)?.confirmed_sats,
        5_000_000_000
    );
    assert_eq!(pool.view(&snapshot, hash(0x51), 100)?.unconfirmed_sats, 0);
    Ok(())
}
