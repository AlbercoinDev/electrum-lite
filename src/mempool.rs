use crate::{
    index::{script_hash, Coin},
    rpc::Core,
    store::Store,
    MAX_BLOCK_BYTES,
};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    OutPoint, Transaction, Txid,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Serialize)]
pub struct Pending {
    pub tx_hash: Txid,
    pub height: i32,
    pub fee: u64,
}

#[derive(Default)]
pub struct Mempool {
    pub raw: HashMap<Txid, Transaction>,
    pub history: HashMap<String, Vec<Pending>>,
    pub delta: HashMap<String, i64>,
    pub outputs: HashMap<OutPoint, Coin>,
    pub spent: HashSet<OutPoint>,
    fees: HashMap<Txid, u64>,
}

#[derive(Clone, Copy, Serialize)]
pub struct Budget {
    pub transactions: usize,
    pub bytes: usize,
    pub entries: usize,
}

impl Mempool {
    pub fn reconcile(
        &mut self,
        core: &Core,
        store: &Store,
        budget: Budget,
        stop: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let tip = store.stats()?.tip.context("empty confirmed index")?.1;
        let max_transactions = budget.transactions;
        let max_bytes = budget.bytes;
        let response_limit = max_transactions
            .saturating_mul(68)
            .saturating_add(65536)
            .min(64 * 1024 * 1024);
        let ids: Vec<Txid> =
            core.call_with_limit("getrawmempool", json!([false]), response_limit)?;
        ensure!(
            ids.len() <= max_transactions,
            "mempool transaction budget exceeded; refusing incomplete history"
        );
        let set: HashSet<_> = ids.iter().copied().collect();
        self.raw.retain(|id, _| set.contains(id));
        let mut bytes: usize = self.raw.values().map(|tx| serialize(tx).len()).sum();
        let mut fetched = 0;
        for id in &ids {
            ensure!(
                !stop.is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Relaxed)),
                "shutdown requested"
            );
            if self.raw.contains_key(id) {
                continue;
            }
            ensure!(fetched < 500, "mempool snapshot warming; retry next cycle");
            let hex: String = core.call("getrawtransaction", json!([id.to_string(), false]))?;
            ensure!(
                hex.len() <= MAX_BLOCK_BYTES * 2,
                "mempool transaction too large"
            );
            let raw = hex::decode(hex)?;
            bytes = bytes
                .checked_add(raw.len())
                .context("mempool bytes overflow")?;
            ensure!(
                bytes <= max_bytes,
                "mempool byte budget exceeded; refusing incomplete history"
            );
            let transaction: Transaction = deserialize(&raw)?;
            ensure!(transaction.compute_txid() == *id, "mempool TXID mismatch");
            self.raw.insert(*id, transaction);
            fetched += 1;
        }
        ensure!(bytes <= max_bytes, "mempool byte budget exceeded");
        let entries: usize = self
            .raw
            .values()
            .map(|tx| tx.input.len() + tx.output.len())
            .sum();
        ensure!(
            entries <= budget.entries,
            "mempool input/output entry budget exceeded; refusing incomplete history"
        );
        let mut history: HashMap<String, Vec<Pending>> = HashMap::new();
        let mut delta = HashMap::new();
        let mut outputs = HashMap::new();
        let mut spent = HashSet::new();
        let mut fees = HashMap::new();
        for (id, tx) in &self.raw {
            let mut affected = HashSet::new();
            let mut input_value = 0u64;
            let mut output_value = 0u64;
            let mut parents = false;
            for input in &tx.input {
                ensure!(
                    spent.insert(input.previous_output),
                    "conflicting mempool snapshot"
                );
                let coin = if let Some(parent) = self.raw.get(&input.previous_output.txid) {
                    parents = true;
                    let output = parent
                        .output
                        .get(input.previous_output.vout as usize)
                        .context("invalid mempool prevout")?;
                    Coin {
                        value: output.value.to_sat(),
                        height: 0,
                        coinbase: false,
                        scripthash: script_hash(&output.script_pubkey),
                    }
                } else {
                    store
                        .coin(input.previous_output)?
                        .context("mempool/confirmed snapshot changed")?
                };
                input_value = input_value
                    .checked_add(coin.value)
                    .context("mempool value overflow")?;
                affected.insert(coin.scripthash.clone());
                add(&mut delta, &coin.scripthash, -i64::try_from(coin.value)?)?;
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let hash = script_hash(&output.script_pubkey);
                affected.insert(hash.clone());
                output_value = output_value
                    .checked_add(output.value.to_sat())
                    .context("mempool value overflow")?;
                if output.script_pubkey.is_op_return() || output.script_pubkey.len() > 10_000 {
                    continue;
                }
                let value = output.value.to_sat();
                add(&mut delta, &hash, i64::try_from(value)?)?;
                outputs.insert(
                    OutPoint {
                        txid: *id,
                        vout: vout.try_into()?,
                    },
                    Coin {
                        value,
                        height: 0,
                        coinbase: false,
                        scripthash: hash,
                    },
                );
            }
            let fee = input_value
                .checked_sub(output_value)
                .context("negative mempool fee")?;
            fees.insert(*id, fee);
            for hash in affected {
                history.entry(hash).or_default().push(Pending {
                    tx_hash: *id,
                    height: if parents { -1 } else { 0 },
                    fee,
                });
            }
        }
        let after: Vec<Txid> =
            core.call_with_limit("getrawmempool", json!([false]), response_limit)?;
        let active: String = core.call("getbestblockhash", json!([]))?;
        ensure!(
            active == tip.to_string() && after.into_iter().collect::<HashSet<_>>() == set,
            "mempool/chain changed during reconciliation"
        );
        for list in history.values_mut() {
            list.sort_by_key(|p| (-p.height, p.tx_hash.to_string()));
        }
        self.history = history;
        self.delta = delta;
        self.outputs = outputs;
        self.spent = spent;
        self.fees = fees;
        Ok(())
    }

    pub fn fee_histogram(&self) -> Value {
        let mut bins: BTreeMap<u64, u64> = BTreeMap::new();
        for (id, tx) in &self.raw {
            let fee = self.fees.get(id).copied().unwrap_or(0);
            let size = tx.vsize() as u64;
            let rate = fee / size.max(1);
            let bin = if rate == 0 {
                0
            } else {
                1u64 << (63 - rate.leading_zeros())
            };
            *bins.entry(bin).or_default() += size;
        }
        json!(bins
            .into_iter()
            .rev()
            .map(|(fee, size)| json!([fee, size]))
            .collect::<Vec<_>>())
    }
}

fn add(map: &mut HashMap<String, i64>, hash: &str, value: i64) -> Result<()> {
    let entry = map.entry(hash.to_owned()).or_default();
    *entry = entry
        .checked_add(value)
        .context("pending balance overflow")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fee_histogram_is_bounded_and_accounts_for_all_vbytes() {
        let mut pool = Mempool::default();
        let base = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest).txdata
            [0]
        .clone();
        let mut expected = 0;
        for i in 0..100 {
            let mut tx = base.clone();
            tx.lock_time = bitcoin::absolute::LockTime::from_consensus(i);
            let id = tx.compute_txid();
            expected += tx.vsize() as u64;
            pool.fees.insert(id, u64::from(i) * 1000);
            pool.raw.insert(id, tx);
        }
        let histogram = pool.fee_histogram();
        let bins = histogram.as_array().unwrap();
        assert!(bins.len() <= 64);
        assert_eq!(
            bins.iter().map(|b| b[1].as_u64().unwrap()).sum::<u64>(),
            expected
        );
    }
}
