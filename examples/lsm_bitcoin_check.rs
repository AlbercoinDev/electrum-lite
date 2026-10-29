//! Check fresh, Core-generated offline regtest blocks against both backends.
//! No performance claim: semantic startup audits deliberately replay the archive.
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, Hash},
    Block, OutPoint,
};
use electrum_lite::{
    archive::Archive,
    experimental_lsm::BitcoinStore,
    index::script_hash,
    store::{Limits, Store},
    Chain,
};
use std::{collections::BTreeSet, fs, path::PathBuf};

fn main() -> Result<()> {
    let input = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("raw block directory required")?,
    );
    let temporary = tempfile::Builder::new()
        .prefix("lsm-bitcoin-check-")
        .tempdir_in(input.parent().context("missing parent")?)?;
    let mut lsm = BitcoinStore::create(&temporary.path().join("lsm"), Chain::Regtest)?;
    let mut archive = Archive::open(&temporary.path().join("sqlite"), Chain::Regtest, true)?;
    let mut sqlite = Store::open(
        &archive.directory().join("index.sqlite"),
        Chain::Regtest,
        Limits {
            transactions: 100_000,
            entries: 1_000_000,
        },
    )?;
    sqlite.recover(&mut archive)?;
    let mut outpoints = Vec::new();
    let mut scripts = BTreeSet::new();
    let mut witnessed_spend = false;
    let mut count = 0u32;
    loop {
        let path = input.join(format!("{count:08}.raw"));
        if !path.exists() {
            break;
        }
        let bytes = fs::read(path)?;
        let block: Block = deserialize(&bytes)?;
        lsm.commit(count, &block)?;
        sqlite.commit(&mut archive, count, &block)?;
        ensure!(
            serialize(&lsm.block(count)?) == bytes,
            "witness archive bytes differ"
        );
        for transaction in &block.txdata {
            witnessed_spend |= !transaction.is_coinbase()
                && transaction
                    .input
                    .iter()
                    .any(|input| !input.witness.is_empty());
            for (vout, output) in transaction.output.iter().enumerate() {
                outpoints.push(OutPoint {
                    txid: transaction.compute_txid(),
                    vout: vout.try_into()?,
                });
                scripts.insert((
                    sha256::Hash::hash(output.script_pubkey.as_bytes()).to_byte_array(),
                    script_hash(&output.script_pubkey),
                ));
            }
        }
        for outpoint in &outpoints {
            let actual = lsm.coin(*outpoint)?;
            let expected = sqlite.coin(*outpoint)?;
            ensure!(
                actual
                    .as_ref()
                    .map(|coin| (coin.value, coin.height, coin.coinbase))
                    == expected
                        .as_ref()
                        .map(|coin| (coin.value, coin.height, coin.coinbase)),
                "UTXO mismatch"
            );
        }
        count += 1;
        ensure!(count <= 1000, "fixture exceeds bounded scope");
    }
    ensure!(
        count >= 103 && witnessed_spend,
        "expected mature coinbase and signed witness spend"
    );
    for (raw, wire) in scripts {
        let expected: Vec<_> = sqlite
            .view(&wire, 10000)?
            .history
            .into_iter()
            .filter(|entry| entry.height != 0)
            .map(|entry| (entry.height, entry.position as u32, entry.txid))
            .collect();
        ensure!(lsm.history(raw, 10000)? == expected, "history mismatch");
    }
    let backup = temporary.path().join("backup");
    lsm.backup(&backup)?;
    let restored =
        BitcoinStore::restore(&backup, &temporary.path().join("restored"), Chain::Regtest)?;
    ensure!(restored.state()? == lsm.state()?, "restored state differs");
    for height in 0..count {
        ensure!(
            serialize(&restored.block(height)?) == serialize(&lsm.block(height)?),
            "restored archive differs"
        );
    }
    drop(restored);
    drop(lsm);
    let reopened = BitcoinStore::open(&temporary.path().join("lsm"), Chain::Regtest)?;
    ensure!(
        reopened.state()?.tip.context("missing tip")?.height == count - 1,
        "reopen tip differs"
    );
    println!("PASS {count} Core-generated blocks: SQLite/LSM UTXO+history, byte-exact witness archive, semantic replay, full backup+restore");
    drop(reopened);
    drop(sqlite);
    drop(archive);
    temporary.close()?;
    println!("PASS experimental indexes, archives, audit scratch and backups removed");
    Ok(())
}
