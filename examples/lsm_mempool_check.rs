//! Controlled LSM-only mempool integration fixture. Never use an existing node.
use anyhow::{ensure, Context, Result};
use bitcoin::{
    hashes::{sha256, Hash},
    OutPoint, Txid,
};
use electrum_lite::{
    experimental_lsm::{
        mempool::{Budget, Mempool, Refresh},
        sync::{cycle, Options},
        BitcoinStore,
    },
    rpc::Core,
    Chain,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::AtomicBool};

fn sync(store: &mut BitcoinStore, core: &Core) -> Result<()> {
    for _ in 0..128 {
        if cycle(
            store,
            core,
            &Options {
                max_blocks: 8,
                min_free_bytes: 64 * 1024 * 1024,
                ..Options::default()
            },
            &AtomicBool::new(false),
        )?
        .confirmed_caught_up
        {
            return Ok(());
        }
    }
    anyhow::bail!("isolated LSM sync did not catch up")
}
fn refresh(pool: &mut Mempool, store: &BitcoinStore, core: &Core) -> Result<usize> {
    let snapshot = store.snapshot()?;
    let mut warming = 0;
    for _ in 0..32 {
        match pool.reconcile(
            &snapshot,
            core,
            Budget {
                fetch_per_pass: 1,
                ..Budget::default()
            },
            &AtomicBool::new(false),
        )? {
            Refresh::Ready { transactions, .. } => return Ok(transactions),
            Refresh::Warming { .. } => {
                warming += 1;
                ensure!(!pool.ready_for(&snapshot), "partial mempool served");
                ensure!(
                    pool.view(&snapshot, [0; 32], 100).is_err(),
                    "partial wallet view served"
                );
            }
        }
    }
    anyhow::bail!("isolated mempool failed to finish after {warming} warm-up cycles")
}
fn address(core: &Core) -> Result<(String, [u8; 32])> {
    let address: String = core.call("getnewaddress", json!(["", "bech32"]))?;
    let info: Value = core.call("getaddressinfo", json!([address]))?;
    let script = hex::decode(info["scriptPubKey"].as_str().context("script missing")?)?;
    Ok((address, sha256::Hash::hash(&script).to_byte_array()))
}
fn send(core: &Core, previous: OutPoint, destination: &str, value: f64) -> Result<(Txid, Vec<u8>)> {
    let inputs =
        json!([{"txid":previous.txid.to_string(),"vout":previous.vout,"sequence":4294967293u32}]);
    let mut outputs = serde_json::Map::new();
    outputs.insert(destination.to_owned(), json!(value));
    let raw: String = core.call("createrawtransaction", json!([inputs, outputs]))?;
    let signed: Value = core.call("signrawtransactionwithwallet", json!([raw]))?;
    ensure!(
        signed["complete"] == true,
        "test wallet did not sign transaction"
    );
    let hex = signed["hex"].as_str().context("signed bytes missing")?;
    let id: String = core.call("sendrawtransaction", json!([hex]))?;
    Ok((id.parse()?, hex::decode(hex)?))
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    ensure!(
        args.next().as_deref() == Some("--confirm-isolated-regtest"),
        "explicit isolated regtest confirmation required"
    );
    let endpoint = args.next().context("RPC endpoint")?;
    let cookie = PathBuf::from(args.next().context("cookie")?);
    let root = PathBuf::from(args.next().context("new LSM root")?);
    let core = Core::new(&endpoint, &cookie)?;
    core.verify(Chain::Regtest)?;
    let network: Value = core.call("getnetworkinfo", json!([]))?;
    ensure!(
        network["networkactive"] == false,
        "test Core must be offline"
    );
    let wallet: Value = core.call("getwalletinfo", json!([]))?;
    ensure!(
        wallet["walletname"] == "lsm-mempool-only-test",
        "test wallet marker mismatch"
    );
    let mut store = BitcoinStore::create(&root, Chain::Regtest)?;
    sync(&mut store, &core)?;
    let mut pool = Mempool::default();
    ensure!(
        refresh(&mut pool, &store, &core)? == 0,
        "expected fresh empty mempool"
    );
    let funding: Vec<Value> = core.call("listunspent", json!([101]))?;
    let coin = funding.first().context("missing matured test coinbase")?;
    let original = OutPoint {
        txid: coin["txid"].as_str().context("txid")?.parse()?,
        vout: coin["vout"].as_u64().context("vout")?.try_into()?,
    };
    let (parent_address, parent_script) = address(&core)?;
    let (child_address, _) = address(&core)?;
    let (grand_address, grand_script) = address(&core)?;
    let (parent, parent_raw) = send(&core, original, &parent_address, 49.999)?;
    let (child, _) = send(
        &core,
        OutPoint {
            txid: parent,
            vout: 0,
        },
        &child_address,
        49.998,
    )?;
    let (grandchild, grand_raw) = send(
        &core,
        OutPoint {
            txid: child,
            vout: 0,
        },
        &grand_address,
        49.997,
    )?;
    let before = store.state()?;
    ensure!(
        refresh(&mut pool, &store, &core)? == 3,
        "incomplete parent/child/grandchild snapshot"
    );
    {
        let snapshot = store.snapshot()?;
        ensure!(
            pool.transaction(&snapshot, parent, 4_000_000)? == parent_raw
                && pool.transaction(&snapshot, grandchild, 4_000_000)? == grand_raw,
            "unconfirmed witness bytes changed"
        );
        let parent_view = pool.view(&snapshot, parent_script, 100)?;
        ensure!(
            parent_view.pending.len() == 2
                && parent_view.pending[0].height == 0
                && parent_view.pending[1].height == -1
                && parent_view.utxos.is_empty(),
            "parent/child status or spent output mismatch"
        );
        let grand_view = pool.view(&snapshot, grand_script, 100)?;
        ensure!(
            grand_view.unconfirmed_sats == 4_999_700_000
                && grand_view.utxos.len() == 1
                && grand_view.pending[0].height == -1,
            "descendant balance or height mismatch"
        );
    }
    ensure!(store.state()? == before, "mempool changed confirmed state");
    println!("PASS LSM complete bounded mempool: chain 0/-1, spent outputs, fees, balances and exact witness bytes");
    let (replacement_address, replacement_script) = address(&core)?;
    let (replacement, replacement_raw) = send(&core, original, &replacement_address, 49.99)?;
    ensure!(
        refresh(&mut pool, &store, &core)? == 1,
        "RBF descendants were not removed"
    );
    {
        let snapshot = store.snapshot()?;
        ensure!(
            pool.transaction(&snapshot, parent, 4_000_000).is_err(),
            "RBF parent still exposed"
        );
        ensure!(
            pool.view(&snapshot, parent_script, 100)?.pending.is_empty(),
            "stale RBF history"
        );
        let view = pool.view(&snapshot, replacement_script, 100)?;
        ensure!(
            view.pending[0].fee == 1_000_000 && view.pending[0].height == 0,
            "replacement fee mismatch"
        );
        ensure!(
            pool.reconcile(
                &snapshot,
                &core,
                Budget {
                    bytes: 1,
                    ..Budget::default()
                },
                &AtomicBool::new(false)
            )
            .is_err()
                && !pool.ready_for(&snapshot),
            "budget failure served stale results"
        );
    }
    ensure!(
        refresh(&mut pool, &store, &core)? == 1,
        "replacement snapshot did not recover"
    );
    println!("PASS LSM RBF and budget failure invalidate stale wallet results");
    let (mining, _) = address(&core)?;
    let mined: Vec<String> = core.call("generatetoaddress", json!([1, mining]))?;
    sync(&mut store, &core)?;
    ensure!(
        !pool.ready_for(&store.snapshot()?),
        "old mempool accepted for new generation"
    );
    ensure!(
        refresh(&mut pool, &store, &core)? == 0,
        "confirmed transaction remained pending"
    );
    ensure!(
        store
            .snapshot()?
            .transaction(replacement, Some(102), 4_000_000)?
            .1
            == replacement_raw,
        "confirmed replacement witness bytes changed"
    );
    let _: Value = core.call("invalidateblock", json!([mined[0]]))?;
    sync(&mut store, &core)?;
    ensure!(
        !pool.ready_for(&store.snapshot()?),
        "reorg reused a previous generation's readiness"
    );
    ensure!(
        refresh(&mut pool, &store, &core)? == 1,
        "reorg transaction did not return to mempool"
    );
    let view = pool.view(&store.snapshot()?, replacement_script, 100)?;
    ensure!(
        view.confirmed_sats == 0 && view.unconfirmed_sats == 4_999_000_000,
        "reorg balance mismatch"
    );
    let (new_mining, _) = address(&core)?;
    let _: Value = core.call("generatetoaddress", json!([2, new_mining]))?;
    sync(&mut store, &core)?;
    ensure!(
        refresh(&mut pool, &store, &core)? == 0,
        "reconfirmed transaction remained pending"
    );
    println!("PASS LSM confirmation, real Core reorg and reconfirmation require current generation plus complete mempool");
    Ok(())
}
