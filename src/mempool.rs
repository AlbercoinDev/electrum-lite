use crate::{
    core::Core,
    index::{key, script_hash, Index},
};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    OutPoint, Transaction, Txid,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Mempool {
    pub raw: BTreeMap<Txid, Transaction>,
    pub history: BTreeMap<[u8; 32], Vec<Value>>,
    pub delta: BTreeMap<[u8; 32], i64>,
    pub outputs: BTreeMap<OutPoint, ([u8; 32], u64, i32)>,
    pub spent: BTreeSet<OutPoint>,
}
impl Mempool {
    pub fn fee_histogram(&self) -> Result<Value> {
        let mut fees = BTreeMap::new();
        for rows in self.history.values() {
            for row in rows {
                fees.insert(
                    row["tx_hash"].as_str().context("mempool txid")?.to_owned(),
                    row["fee"].as_u64().context("mempool fee")?,
                );
            }
        }
        let mut buckets = BTreeMap::<u64, u64>::new();
        for (id, tx) in &self.raw {
            let size = tx.vsize() as u64;
            let fee = fees.get(&id.to_string()).context("missing mempool fee")?;
            *buckets.entry(fee / size.max(1)).or_default() += size;
        }
        Ok(json!(buckets
            .into_iter()
            .rev()
            .map(|(fee, size)| json!([fee, size]))
            .collect::<Vec<_>>()))
    }
    pub fn load(index: &Index, core: &Core) -> Result<Self> {
        let ids = core.call("getrawmempool", json!([]))?;
        let ids = ids.as_array().context("mempool list")?;
        ensure!(
            ids.len() <= 10_000,
            "mempool exceeds experimental 10000 transaction limit"
        );
        let mut m = Self::default();
        let mut total = 0;
        for id in ids {
            let id: Txid = id.as_str().context("txid")?.parse()?;
            let bytes = core.call("getrawtransaction", json!([id.to_string(), false]))?;
            let bytes = hex::decode(bytes.as_str().context("raw transaction")?)?;
            total += bytes.len();
            ensure!(total <= 32 * 1024 * 1024, "mempool byte limit");
            let tx: Transaction = deserialize(&bytes)?;
            ensure!(tx.compute_txid() == id, "mempool txid mismatch");
            m.raw.insert(id, tx);
        }
        let mut pending: BTreeSet<Txid> = m.raw.keys().copied().collect();
        while !pending.is_empty() {
            let before = pending.len();
            for id in pending.clone() {
                let tx = &m.raw[&id];
                if tx
                    .input
                    .iter()
                    .any(|i| pending.contains(&i.previous_output.txid))
                {
                    continue;
                }
                let height = if tx
                    .input
                    .iter()
                    .any(|i| m.raw.contains_key(&i.previous_output.txid))
                {
                    -1
                } else {
                    0
                };
                let mut scripts = BTreeSet::new();
                let mut input_value = 0u64;
                let mut output_value = 0u64;
                for input in &tx.input {
                    let op = input.previous_output;
                    ensure!(m.spent.insert(op), "mempool conflicting spends");
                    let (s, v) = if let Some((s, v, _)) = m.outputs.get(&op) {
                        (*s, *v)
                    } else {
                        let coin = index
                            .db
                            .get(key(b'C', &serialize(&op)))?
                            .context("mempool prevout unavailable")?;
                        ensure!(coin.len() == 48, "corrupt confirmed coin");
                        (
                            coin[..32].try_into()?,
                            u64::from_le_bytes(coin[32..40].try_into()?),
                        )
                    };
                    scripts.insert(s);
                    *m.delta.entry(s).or_default() -= i64::try_from(v)?;
                    input_value = input_value
                        .checked_add(v)
                        .context("input amount overflow")?;
                }
                for (n, o) in tx.output.iter().enumerate() {
                    let s = script_hash(o.script_pubkey.as_bytes());
                    let v = o.value.to_sat();
                    scripts.insert(s);
                    output_value = output_value.checked_add(v).context("output overflow")?;
                    if !o.script_pubkey.is_op_return() {
                        *m.delta.entry(s).or_default() += i64::try_from(v)?;
                        m.outputs
                            .insert(OutPoint::new(id, n as u32), (s, v, height));
                    }
                }
                let fee = input_value
                    .checked_sub(output_value)
                    .context("negative mempool fee")?;
                for s in scripts {
                    m.history
                        .entry(s)
                        .or_default()
                        .push(json!({"tx_hash":id.to_string(),"height":height,"fee":fee}));
                }
                pending.remove(&id);
            }
            ensure!(pending.len() < before, "mempool dependency cycle");
        }
        // A changing snapshot is unavailable, never silently incomplete.
        let after = core.call("getrawmempool", json!([]))?;
        let after: BTreeSet<String> = after
            .as_array()
            .context("mempool")?
            .iter()
            .map(|x| x.as_str().unwrap_or("").to_owned())
            .collect();
        let before: BTreeSet<String> = m.raw.keys().map(ToString::to_string).collect();
        ensure!(after == before, "mempool changed while reconciling; retry");
        for history in m.history.values_mut() {
            history.sort_by(|a, b| {
                b["height"]
                    .as_i64()
                    .cmp(&a["height"].as_i64())
                    .then_with(|| a["tx_hash"].as_str().cmp(&b["tx_hash"].as_str()))
            });
        }
        Ok(m)
    }
    pub fn history(&self, index: &Index, s: &[u8; 32]) -> Result<Value> {
        let mut h = index.history(s)?.as_array().context("history")?.clone();
        if let Some(rows) = self.history.get(s) {
            for row in rows {
                h.push(json!({"tx_hash":row["tx_hash"],"height":row["height"]}));
            }
        }
        ensure!(
            h.len() <= 100_000,
            "history limit exceeded; no partial response"
        );
        Ok(json!(h))
    }
    pub fn unspent(&self, index: &Index, s: &[u8; 32]) -> Result<Value> {
        let mut out = Vec::new();
        for row in index.unspent(s)?.as_array().context("unspent")? {
            let id: Txid = row["tx_hash"].as_str().context("txid")?.parse()?;
            let op = OutPoint::new(id, row["tx_pos"].as_u64().context("vout")? as u32);
            if !self.spent.contains(&op) {
                out.push(row.clone());
            }
        }
        for (op, (script, v, _)) in &self.outputs {
            if script == s && !self.spent.contains(op) {
                out.push(
                    json!({"tx_hash":op.txid.to_string(),"tx_pos":op.vout,"value":v,"height":0}),
                );
            }
        }
        ensure!(
            out.len() <= 100_000,
            "unspent limit exceeded; no partial response"
        );
        Ok(json!(out))
    }
}
