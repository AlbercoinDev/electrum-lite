//! Offline regtest sync worker: authenticated Core -> new LSM -> local queries.
use anyhow::{ensure, Context, Result};
use bitcoin::consensus::serialize;
use electrum_lite::{
    experimental_lsm::{
        sync::{cycle, Options},
        BitcoinStore,
    },
    rpc::Core,
    Chain,
};
use std::{path::PathBuf, sync::atomic::AtomicBool};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let endpoint = args.next().context("RPC endpoint required")?;
    let cookie = PathBuf::from(args.next().context("cookie path required")?);
    let root = PathBuf::from(args.next().context("dedicated LSM root required")?);
    let expected: u32 = args
        .next()
        .context("expected regtest height required")?
        .parse()?;
    ensure!(expected < 500, "isolated test height exceeds scope");
    let core = Core::new(&endpoint, &cookie)?;
    let mut store = if root.exists() {
        BitcoinStore::open(&root, Chain::Regtest)?
    } else {
        BitcoinStore::create(&root, Chain::Regtest)?
    };
    let options = Options {
        max_blocks: 8,
        min_free_bytes: 64 * 1024 * 1024,
        ..Options::default()
    };
    let stop = AtomicBool::new(false);
    let mut rewind = 0;
    let mut imported = 0;
    let mut caught_up = false;
    for _ in 0..128 {
        let outcome = cycle(&mut store, &core, &options, &stop)?;
        ensure!(
            outcome.imported <= 8 && outcome.raw_bytes <= options.max_bytes,
            "cycle exceeded budget"
        );
        rewind += outcome.rewound;
        imported += outcome.imported;
        if outcome.confirmed_caught_up {
            caught_up = true;
            break;
        }
    }
    ensure!(caught_up, "LSM failed to catch observed offline Core");
    let snapshot = store.snapshot()?;
    ensure!(
        snapshot.state().tip.as_ref().context("empty index")?.height == expected,
        "unexpected indexed height"
    );
    for height in 0..=expected {
        let block = core.block(core.hash(height)?)?;
        ensure!(
            serialize(&snapshot.block(height)?) == serialize(&block),
            "LSM archived a different branch or witness"
        );
        for transaction in &block.txdata {
            let txid = transaction.compute_txid();
            ensure!(
                snapshot.transaction(txid, Some(height), 4_000_000)?.1 == serialize(transaction),
                "LSM local transaction differs"
            );
            ensure!(
                snapshot.proof(txid, height)?.verify(),
                "LSM local proof invalid"
            );
        }
    }
    println!("PASS LSM authenticated RPC cycle: height={expected}, imported={imported}, rewound={rewind}; local headers/TX/witness/proofs match Core");
    Ok(())
}
