//! Bounded confirmed-state synchronization for the new LSM coordinator.
//! Blocking RPC/storage: call on a dedicated worker, never on a Tokio task.
use super::{BitcoinStore, WINDOW};
use crate::{rpc::Core, Chain, MAX_BLOCK_BYTES};
use anyhow::{ensure, Context, Result};
use bitcoin::{consensus::serialize, Block, BlockHash};
use serde::Serialize;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub network: Chain,
    pub height: u32,
    pub hash: BlockHash,
    pub prune_height: Option<u32>,
}

/// Small source contract also permits deterministic fork/failure tests.
pub trait Source {
    fn observe(&self, expected: Chain) -> Result<Observation>;
    fn hash(&self, height: u32) -> Result<BlockHash>;
    fn block(&self, hash: BlockHash) -> Result<Block>;
}

impl Source for Core {
    fn observe(&self, expected: Chain) -> Result<Observation> {
        let info = self.verify(expected)?;
        let height = info["blocks"]
            .as_u64()
            .context("Core height missing")?
            .try_into()?;
        let hash = info["bestblockhash"]
            .as_str()
            .context("Core tip hash missing")?
            .parse()?;
        let prune_height = if info["pruned"].as_bool() == Some(true) {
            Some(
                info["pruneheight"]
                    .as_u64()
                    .context("Core prune height missing")?
                    .try_into()?,
            )
        } else {
            None
        };
        Ok(Observation {
            network: expected,
            height,
            hash,
            prune_height,
        })
    }
    fn hash(&self, height: u32) -> Result<BlockHash> {
        Core::hash(self, height)
    }
    fn block(&self, hash: BlockHash) -> Result<Block> {
        Core::block(self, hash)
    }
}

pub struct Options {
    pub max_blocks: u32,
    pub max_bytes: usize,
    /// Checked between RPCs. An in-flight RPC still has Core's socket timeout.
    pub max_duration: Duration,
    pub min_free_bytes: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_blocks: 32,
            max_bytes: 16 * 1024 * 1024,
            max_duration: Duration::from_secs(30),
            min_free_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub observed_height: u32,
    pub observed_hash: BlockHash,
    pub indexed_tip: Option<(u32, BlockHash)>,
    pub imported: u32,
    pub rewound: u32,
    pub raw_bytes: usize,
    /// Confirmed state only: NOT permission to serve wallets or prune.
    pub confirmed_caught_up: bool,
}

fn checkpoint(started: Instant, options: &Options, stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "LSM sync stop requested");
    ensure!(
        started.elapsed() < options.max_duration,
        "LSM sync cycle time budget reached"
    );
    Ok(())
}

fn anchor(source: &impl Source, observed: Observation) -> Result<()> {
    ensure!(
        source.hash(observed.height)? == observed.hash,
        "Core branch changed during LSM synchronization; retry"
    );
    Ok(())
}

pub fn cycle(
    store: &mut BitcoinStore,
    source: &impl Source,
    options: &Options,
    stop: &AtomicBool,
) -> Result<Outcome> {
    ensure!(
        (1..=64).contains(&options.max_blocks),
        "LSM sync block budget must be 1..64"
    );
    ensure!(
        (MAX_BLOCK_BYTES..=16 * 1024 * 1024).contains(&options.max_bytes),
        "LSM sync byte budget must hold a block and be <=16 MiB"
    );
    ensure!(
        !options.max_duration.is_zero(),
        "LSM sync duration must be nonzero"
    );
    let started = Instant::now();
    checkpoint(started, options, stop)?;
    let observed = source.observe(store.chain)?;
    ensure!(observed.network == store.chain, "Core network mismatch");
    ensure!(
        source.hash(0)? == store.chain.genesis(),
        "Core genesis mismatch"
    );
    anchor(source, observed)?;
    let mut rewound = 0;
    if let Some(tip) = store.state()?.tip {
        let mut height = tip.height.min(observed.height);
        ensure!(
            tip.height - height <= WINDOW,
            "Core branch requires unavailable deep undo; recover into a new root"
        );
        loop {
            checkpoint(started, options, stop)?;
            // Any hash RPC failure propagates before mutating the index. It is
            // never interpreted as evidence that this height is on a fork.
            let remote = source.hash(height)?;
            let local = store.snapshot()?.header(height)?.block_hash();
            if remote == local {
                break;
            }
            ensure!(
                height > 0 && tip.height - height < WINDOW,
                "Core branch exceeds retained undo; recover into a new root"
            );
            height -= 1;
        }
        if height < tip.height {
            let first = height.checked_add(1).context("ancestor height overflow")?;
            ensure!(observed.prune_height.is_none_or(|pruned|first>=pruned),"required replacement blocks were already pruned; recover into a new root with trusted history");
            checkpoint(started, options, stop)?;
            anchor(source, observed)?;
            ensure!(
                fs2::available_space(store.archive.directory())? >= options.min_free_bytes,
                "LSM sync free-space threshold reached before rewind"
            );
            store.rewind(height)?;
            rewound = tip.height - height;
        }
    }
    let mut next = store
        .state()?
        .tip
        .map(|tip| tip.height.checked_add(1).context("indexed height overflow"))
        .transpose()?
        .unwrap_or(0);
    if next <= observed.height {
        ensure!(observed.prune_height.is_none_or(|height|next>=height),"required historical blocks were already pruned; provide trusted history or recover into a new root");
    }
    let mut imported = 0;
    let mut raw_bytes = 0usize;
    while next <= observed.height && imported < options.max_blocks {
        checkpoint(started, options, stop)?;
        ensure!(
            fs2::available_space(store.archive.directory())? >= options.min_free_bytes,
            "LSM sync free-space threshold reached"
        );
        let hash = source.hash(next)?;
        let block = source.block(hash)?;
        ensure!(
            block.block_hash() == hash,
            "Core returned a block with the wrong hash"
        );
        let bytes = serialize(&block).len();
        ensure!(bytes <= MAX_BLOCK_BYTES, "Core block exceeds byte limit");
        if bytes > options.max_bytes - raw_bytes {
            break;
        }
        checkpoint(started, options, stop)?;
        anchor(source, observed)?;
        ensure!(
            source.hash(next)? == hash,
            "Core active block changed before LSM commit"
        );
        anchor(source, observed)?;
        store.commit(next, &block)?;
        imported += 1;
        raw_bytes += bytes;
        next = next.checked_add(1).context("Core height overflow")?;
    }
    checkpoint(started, options, stop)?;
    let final_observation = source.observe(store.chain)?;
    ensure!(
        final_observation.network == store.chain,
        "Core network mismatch"
    );
    anchor(source, final_observation)?;
    let indexed_tip = store.state()?.tip.map(|tip| (tip.height, tip.hash));
    let confirmed_caught_up =
        indexed_tip == Some((final_observation.height, final_observation.hash));
    Ok(Outcome {
        observed_height: final_observation.height,
        observed_hash: final_observation.hash,
        indexed_tip,
        imported,
        rewound,
        raw_bytes,
        confirmed_caught_up,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{
        absolute, blockdata::constants::genesis_block, transaction, Amount, OutPoint, ScriptBuf,
        Sequence, Transaction, TxIn, TxOut, Witness,
    };

    struct Fake {
        blocks: Vec<Block>,
        fail_at: Option<u32>,
        network: Chain,
        pruned: Option<u32>,
    }
    impl Source for Fake {
        fn observe(&self, _: Chain) -> Result<Observation> {
            Ok(Observation {
                network: self.network,
                height: self.blocks.len() as u32 - 1,
                hash: self.blocks.last().unwrap().block_hash(),
                prune_height: self.pruned,
            })
        }
        fn hash(&self, height: u32) -> Result<BlockHash> {
            ensure!(Some(height) != self.fail_at, "injected RPC hash failure");
            Ok(self
                .blocks
                .get(height as usize)
                .context("missing test block")?
                .block_hash())
        }
        fn block(&self, hash: BlockHash) -> Result<Block> {
            self.blocks
                .iter()
                .find(|block| block.block_hash() == hash)
                .cloned()
                .context("missing test block")
        }
    }
    fn next(previous: &Block, tag: u32) -> Block {
        let mut block = previous.clone();
        block.header.prev_blockhash = previous.block_hash();
        block.header.time += 1;
        block.txdata = vec![Transaction {
            version: transaction::Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(tag.to_le_bytes().to_vec()),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            }],
        }];
        block.header.merkle_root = block.compute_merkle_root().unwrap();
        block
    }
    fn source() -> Fake {
        let mut blocks = vec![genesis_block(Chain::Regtest.network())];
        for tag in 1..=3 {
            blocks.push(next(blocks.last().unwrap(), tag));
        }
        Fake {
            blocks,
            fail_at: None,
            network: Chain::Regtest,
            pruned: None,
        }
    }
    fn options() -> Options {
        Options {
            max_blocks: 2,
            min_free_bytes: 0,
            ..Options::default()
        }
    }

    #[test]
    fn bounded_cycles_reorg_and_stop_preserve_generation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = BitcoinStore::create(&root.path().join("lsm"), Chain::Regtest)?;
        let mut source = source();
        let stop = AtomicBool::new(false);
        let first = cycle(&mut store, &source, &options(), &stop)?;
        assert_eq!(first.imported, 2);
        assert!(!first.confirmed_caught_up);
        assert!(cycle(&mut store, &source, &options(), &stop)?.confirmed_caught_up);
        let orphan = source.blocks[3].txdata[0].compute_txid();
        source.blocks.truncate(2);
        source.blocks.push(next(source.blocks.last().unwrap(), 200));
        source.blocks.push(next(source.blocks.last().unwrap(), 300));
        let outcome = cycle(&mut store, &source, &options(), &stop)?;
        assert_eq!(outcome.rewound, 2);
        assert!(outcome.confirmed_caught_up);
        assert!(store.snapshot()?.occurrences(orphan, None, 16)?.is_empty());
        let before = store.state()?;
        stop.store(true, Ordering::Relaxed);
        assert!(cycle(&mut store, &source, &options(), &stop).is_err());
        assert_eq!(store.state()?, before);
        Ok(())
    }

    #[test]
    fn rpc_failure_is_not_a_fork_and_wrong_identity_or_pruning_fail_closed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = BitcoinStore::create(&root.path().join("lsm"), Chain::Regtest)?;
        let mut source = source();
        let stop = AtomicBool::new(false);
        cycle(&mut store, &source, &options(), &stop)?;
        let before = store.state()?;
        let segment = root.path().join("lsm/regtest/blocks-00000000.elb");
        let size = std::fs::metadata(&segment)?.len();
        source.fail_at = Some(1);
        assert!(cycle(&mut store, &source, &options(), &stop).is_err());
        assert_eq!(store.state()?, before);
        assert_eq!(std::fs::metadata(segment)?.len(), size);
        source.fail_at = None;
        source.network = Chain::Signet;
        assert!(cycle(&mut store, &source, &options(), &stop).is_err());
        assert_eq!(store.state()?, before);
        let mut empty = BitcoinStore::create(&root.path().join("empty"), Chain::Regtest)?;
        source.network = Chain::Regtest;
        source.pruned = Some(1);
        assert!(cycle(&mut empty, &source, &options(), &stop).is_err());
        assert!(empty.state()?.tip.is_none());
        let invalid = Options {
            max_blocks: 0,
            ..options()
        };
        assert!(cycle(&mut empty, &source, &invalid, &stop).is_err());
        Ok(())
    }

    #[test]
    fn changed_anchor_and_space_budget_never_publish_unverified_blocks() -> Result<()> {
        struct Changing {
            inner: Fake,
            calls: std::cell::Cell<usize>,
        }
        impl Source for Changing {
            fn observe(&self, chain: Chain) -> Result<Observation> {
                self.inner.observe(chain)
            }
            fn hash(&self, height: u32) -> Result<BlockHash> {
                if height == 3 {
                    let calls = self.calls.get() + 1;
                    self.calls.set(calls);
                    if calls >= 3 {
                        return self.inner.hash(2);
                    }
                }
                self.inner.hash(height)
            }
            fn block(&self, hash: BlockHash) -> Result<Block> {
                self.inner.block(hash)
            }
        }
        let root = tempfile::tempdir()?;
        let mut store = BitcoinStore::create(&root.path().join("lsm"), Chain::Regtest)?;
        let source = Changing {
            inner: source(),
            calls: std::cell::Cell::new(0),
        };
        let stop = AtomicBool::new(false);
        assert!(cycle(&mut store, &source, &options(), &stop).is_err());
        assert!(store.state()?.tip.is_none());
        let space = Options {
            min_free_bytes: u64::MAX,
            ..options()
        };
        assert!(cycle(&mut store, &source.inner, &space, &stop).is_err());
        assert!(store.state()?.tip.is_none());
        Ok(())
    }

    #[test]
    fn deep_remote_rewind_requires_recovery_without_mutating_archive() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = BitcoinStore::create(&root.path().join("lsm"), Chain::Regtest)?;
        let mut block = genesis_block(Chain::Regtest.network());
        store.commit(0, &block)?;
        for tag in 1..=289 {
            block = next(&block, tag);
            store.commit(tag, &block)?;
        }
        let before = store.state()?;
        let segment = root.path().join("lsm/regtest/blocks-00000000.elb");
        let size = std::fs::metadata(&segment)?.len();
        let mut source = source();
        source.blocks.truncate(1);
        assert!(cycle(&mut store, &source, &options(), &AtomicBool::new(false)).is_err());
        assert_eq!(store.state()?, before);
        assert_eq!(std::fs::metadata(segment)?.len(), size);
        Ok(())
    }
}
