//! Unconfirmed overlay for one complete LSM confirmed generation.
//! No writes to the confirmed index/archive. Partial refreshes never serve queries.
use super::{query::Snapshot, Coin};
use crate::{rpc::Core, MAX_BLOCK_BYTES};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, Hash},
    BlockHash, OutPoint, Transaction, Txid,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq)]
pub struct Listing {
    pub instance: [u8; 32],
    pub tip: BlockHash,
    pub sequence: u64,
    pub txids: BTreeSet<Txid>,
}
pub trait Source {
    fn listing(&self, maximum: usize) -> Result<Listing>;
    fn transaction(&self, txid: Txid, maximum: usize) -> Result<Vec<u8>>;
}
impl Source for Core {
    fn listing(&self, maximum: usize) -> Result<Listing> {
        #[derive(Deserialize)]
        struct Response {
            txids: Vec<Txid>,
            mempool_sequence: u64,
        }
        ensure!(
            maximum <= 100_000,
            "mempool list budget exceeds supported scope"
        );
        let instance = self.cookie_instance_token()?;
        let tip: String = self.call("getbestblockhash", json!([]))?;
        let limit = maximum
            .checked_mul(68)
            .and_then(|bytes| bytes.checked_add(65536))
            .context("list budget overflow")?;
        let response: Response =
            self.call_with_limit("getrawmempool", json!([false, true]), limit)?;
        ensure!(
            response.txids.len() <= maximum,
            "mempool transaction budget exceeded"
        );
        let count = response.txids.len();
        let txids: BTreeSet<_> = response.txids.into_iter().collect();
        ensure!(txids.len() == count, "duplicate mempool TXID");
        ensure!(
            self.cookie_instance_token()? == instance,
            "Core cookie changed during mempool observation"
        );
        Ok(Listing {
            instance,
            tip: tip.parse()?,
            sequence: response.mempool_sequence,
            txids,
        })
    }
    fn transaction(&self, txid: Txid, maximum: usize) -> Result<Vec<u8>> {
        ensure!(
            maximum <= MAX_BLOCK_BYTES,
            "mempool raw response budget exceeds block bound"
        );
        let limit = maximum
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(4096))
            .context("raw budget overflow")?;
        let raw: String =
            self.call_with_limit("getrawtransaction", json!([txid.to_string(), false]), limit)?;
        ensure!(raw.len() <= maximum * 2, "mempool raw byte budget exceeded");
        Ok(hex::decode(raw)?)
    }
}

#[derive(Clone, Copy)]
pub struct Budget {
    pub transactions: usize,
    pub bytes: usize,
    pub entries: usize,
    pub fetch_per_pass: usize,
    pub max_duration: Duration,
    pub max_age: Duration,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            transactions: 20_000,
            bytes: 64 * 1024 * 1024,
            entries: 500_000,
            fetch_per_pass: 128,
            max_duration: Duration::from_secs(30),
            max_age: Duration::from_secs(10),
        }
    }
}
impl Budget {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            self.transactions <= 100_000
                && self.bytes > 0
                && self.bytes <= 256 * 1024 * 1024
                && self.entries <= 2_000_000,
            "mempool budget outside supported scope"
        );
        ensure!(
            (1..=512).contains(&self.fetch_per_pass)
                && !self.max_duration.is_zero()
                && !self.max_age.is_zero()
                && self.max_age <= Duration::from_secs(60),
            "invalid mempool cycle budget"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pending {
    pub txid: Txid,
    pub height: i32,
    pub fee: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Refresh {
    Warming {
        cached: usize,
        total: usize,
    },
    Ready {
        transactions: usize,
        sequence: u64,
        generation: u64,
    },
}
#[derive(Debug, Serialize)]
pub struct WalletView {
    pub generation: u64,
    pub confirmed_sats: u64,
    pub unconfirmed_sats: i64,
    pub history: Vec<super::query::Occurrence>,
    pub pending: Vec<Pending>,
    pub utxos: Vec<(OutPoint, Coin)>,
}
struct Cached {
    raw: Vec<u8>,
    tx: Transaction,
}
struct Overlay {
    generation: u64,
    tip: BlockHash,
    history: BTreeMap<[u8; 32], Vec<Pending>>,
    delta: BTreeMap<[u8; 32], i64>,
    outputs: BTreeMap<OutPoint, Coin>,
    spent: BTreeSet<OutPoint>,
    validated_at: Instant,
    max_age: Duration,
    fees: BTreeMap<Txid, u64>,
}
#[derive(Default)]
pub struct Mempool {
    listing: Option<Listing>,
    cached: BTreeMap<Txid, Cached>,
    bytes: usize,
    entries: usize,
    published: Option<Overlay>,
}

fn check(start: Instant, budget: Budget, stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "mempool refresh stop requested"
    );
    ensure!(
        start.elapsed() < budget.max_duration,
        "mempool refresh time budget reached"
    );
    Ok(())
}
fn script(bytes: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(bytes).to_byte_array()
}
fn add(delta: &mut BTreeMap<[u8; 32], i64>, script: [u8; 32], value: i64) -> Result<()> {
    let current = delta.entry(script).or_default();
    *current = current
        .checked_add(value)
        .context("mempool balance overflow")?;
    Ok(())
}

impl Mempool {
    pub fn invalidate(&mut self) {
        self.published = None;
    }
    fn clear_cache(&mut self) {
        self.cached.clear();
        self.bytes = 0;
        self.entries = 0;
        self.listing = None;
    }
    pub fn ready_for(&self, snapshot: &Snapshot<'_>) -> bool {
        self.published.as_ref().is_some_and(|overlay| {
            overlay.validated_at.elapsed() < overlay.max_age
                && overlay.generation == snapshot.generation()
                && snapshot
                    .state()
                    .tip
                    .as_ref()
                    .is_some_and(|tip| tip.hash == overlay.tip)
        })
    }
    fn overlay(&self, snapshot: &Snapshot<'_>) -> Result<&Overlay> {
        ensure!(
            self.ready_for(snapshot),
            "complete mempool snapshot for this generation required"
        );
        self.published.as_ref().context("mempool not ready")
    }
    pub fn view(
        &self,
        snapshot: &Snapshot<'_>,
        script: [u8; 32],
        limit: usize,
    ) -> Result<WalletView> {
        let (history, pending) = self.history(snapshot, script, limit)?;
        let utxos = self.list_unspent(snapshot, script, limit)?;
        let (confirmed_sats, unconfirmed_sats) = self.balance(snapshot, script)?;
        Ok(WalletView {
            generation: snapshot.generation(),
            confirmed_sats,
            unconfirmed_sats,
            history,
            pending,
            utxos,
        })
    }
    pub fn balance(&self, snapshot: &Snapshot<'_>, script: [u8; 32]) -> Result<(u64, i64)> {
        let overlay = self.overlay(snapshot)?;
        Ok((
            snapshot.balance(script)?,
            *overlay.delta.get(&script).unwrap_or(&0),
        ))
    }
    pub fn history(
        &self,
        snapshot: &Snapshot<'_>,
        script: [u8; 32],
        limit: usize,
    ) -> Result<(Vec<super::query::Occurrence>, Vec<Pending>)> {
        let overlay = self.overlay(snapshot)?;
        let history = snapshot.history(script, limit)?;
        let pending = overlay.history.get(&script);
        ensure!(
            history
                .len()
                .checked_add(pending.map_or(0, Vec::len))
                .is_some_and(|length| length <= limit),
            "combined history response limit"
        );
        let pending = pending.cloned().unwrap_or_default();
        Ok((history, pending))
    }
    pub fn list_unspent(
        &self,
        snapshot: &Snapshot<'_>,
        script: [u8; 32],
        limit: usize,
    ) -> Result<Vec<(OutPoint, Coin)>> {
        let overlay = self.overlay(snapshot)?;
        let mut utxos = snapshot.list_unspent(script, limit)?;
        utxos.retain(|(outpoint, _)| !overlay.spent.contains(outpoint));
        for (outpoint, coin) in &overlay.outputs {
            if coin.script == script {
                ensure!(utxos.len() < limit, "combined UTXO response limit");
                utxos.push((*outpoint, coin.clone()));
            }
        }
        utxos.sort_by_key(|(outpoint, coin)| {
            (
                coin.height == 0,
                coin.height,
                outpoint.txid.to_string(),
                outpoint.vout,
            )
        });
        Ok(utxos)
    }
    pub fn fee_histogram(&self, snapshot: &Snapshot<'_>) -> Result<Vec<(u64, u64)>> {
        let overlay = self.overlay(snapshot)?;
        let mut groups = BTreeMap::<u64, u64>::new();
        for (id, cached) in &self.cached {
            let size = u64::try_from(cached.tx.vsize())?;
            ensure!(size > 0, "zero transaction vsize");
            let fee = *overlay.fees.get(id).context("missing mempool fee")?;
            let value = groups.entry(fee / size).or_default();
            *value = value.checked_add(size).context("histogram size overflow")?;
        }
        let mut bins = Vec::<(u64, u64)>::new();
        for (rate, size) in groups.into_iter().rev() {
            if bins.len() < 100 {
                bins.push((rate, size));
            } else {
                let last = bins.last_mut().context("missing histogram bin")?;
                last.0 = rate;
                last.1 = last
                    .1
                    .checked_add(size)
                    .context("histogram size overflow")?;
            }
        }
        Ok(bins)
    }
    pub fn transaction(
        &self,
        snapshot: &Snapshot<'_>,
        txid: Txid,
        maximum: usize,
    ) -> Result<Vec<u8>> {
        self.overlay(snapshot)?;
        let cached = self
            .cached
            .get(&txid)
            .context("transaction not in complete mempool")?;
        ensure!(
            cached.raw.len() <= maximum && maximum <= MAX_BLOCK_BYTES,
            "mempool transaction response limit"
        );
        Ok(cached.raw.clone())
    }
    pub fn reconcile(
        &mut self,
        snapshot: &Snapshot<'_>,
        source: &impl Source,
        budget: Budget,
        stop: &AtomicBool,
    ) -> Result<Refresh> {
        self.invalidate();
        budget.validate()?;
        if self.bytes > budget.bytes
            || self.entries > budget.entries
            || self.cached.len() > budget.transactions
        {
            self.clear_cache();
        }
        let start = Instant::now();
        check(start, budget, stop)?;
        let tip = snapshot
            .state()
            .tip
            .as_ref()
            .context("confirmed index is empty")?
            .hash;
        let before = source.listing(budget.transactions)?;
        ensure!(
            before.txids.len() <= budget.transactions,
            "mempool transaction budget exceeded"
        );
        ensure!(
            before.tip == tip,
            "confirmed index is not at observed Core tip"
        );
        if self.listing.as_ref() != Some(&before) {
            self.clear_cache();
            self.listing = Some(before.clone());
        }
        let missing: Vec<_> = before
            .txids
            .iter()
            .filter(|id| !self.cached.contains_key(*id))
            .take(budget.fetch_per_pass)
            .copied()
            .collect();
        for id in missing {
            check(start, budget, stop)?;
            let maximum = budget
                .bytes
                .checked_sub(self.bytes)
                .context("mempool byte budget exceeded")?
                .min(MAX_BLOCK_BYTES);
            ensure!(maximum > 0, "mempool byte budget exceeded");
            let raw = source.transaction(id, maximum)?;
            ensure!(raw.len() <= maximum, "mempool byte budget exceeded");
            let tx: Transaction = deserialize(&raw)?;
            ensure!(
                tx.compute_txid() == id
                    && !tx.is_coinbase()
                    && !tx.input.is_empty()
                    && !tx.output.is_empty(),
                "invalid mempool transaction"
            );
            ensure!(
                serialize(&tx) == raw,
                "noncanonical mempool transaction encoding"
            );
            let entries = tx
                .input
                .len()
                .checked_add(tx.output.len())
                .context("entry overflow")?;
            let total = self
                .entries
                .checked_add(entries)
                .context("entry overflow")?;
            ensure!(
                total <= budget.entries,
                "mempool input/output budget exceeded"
            );
            self.bytes += raw.len();
            self.entries = total;
            self.cached.insert(id, Cached { raw, tx });
        }
        check(start, budget, stop)?;
        let after = source.listing(budget.transactions)?;
        if after != before {
            self.clear_cache();
            anyhow::bail!("mempool sequence, Core instance or chain changed during refresh");
        }
        if self.cached.len() != before.txids.len() {
            return Ok(Refresh::Warming {
                cached: self.cached.len(),
                total: before.txids.len(),
            });
        }
        let mut overlay = self.build(snapshot, tip, start, budget, stop)?;
        check(start, budget, stop)?;
        let validated_at = Instant::now();
        if source.listing(budget.transactions)? != before {
            self.clear_cache();
            anyhow::bail!("mempool or chain changed before publication");
        }
        check(start, budget, stop)?;
        ensure!(
            validated_at.elapsed() < budget.max_age,
            "mempool verification exceeded freshness budget"
        );
        overlay.validated_at = validated_at;
        self.published = Some(overlay);
        Ok(Refresh::Ready {
            transactions: self.cached.len(),
            sequence: before.sequence,
            generation: snapshot.generation(),
        })
    }
    fn build(
        &self,
        snapshot: &Snapshot<'_>,
        tip: BlockHash,
        start: Instant,
        budget: Budget,
        stop: &AtomicBool,
    ) -> Result<Overlay> {
        let mut degree = BTreeMap::new();
        let mut children: BTreeMap<Txid, Vec<Txid>> = BTreeMap::new();
        let mut ready = BTreeSet::new();
        for (id, cached) in &self.cached {
            let parents: BTreeSet<_> = cached
                .tx
                .input
                .iter()
                .filter_map(|input| {
                    self.cached
                        .contains_key(&input.previous_output.txid)
                        .then_some(input.previous_output.txid)
                })
                .collect();
            ensure!(!parents.contains(id), "mempool self dependency");
            if parents.is_empty() {
                ready.insert(*id);
            }
            degree.insert(*id, parents.len());
            for parent in parents {
                children.entry(parent).or_default().push(*id);
            }
        }
        let mut overlay = Overlay {
            generation: snapshot.generation(),
            tip,
            history: BTreeMap::new(),
            delta: BTreeMap::new(),
            outputs: BTreeMap::new(),
            spent: BTreeSet::new(),
            validated_at: Instant::now(),
            max_age: budget.max_age,
            fees: BTreeMap::new(),
        };
        let mut processed = 0;
        while let Some(id) = ready.pop_first() {
            check(start, budget, stop)?;
            let tx = &self.cached[&id].tx;
            let mut affected = BTreeSet::new();
            let mut input_value = 0u64;
            let mut output_value = 0u64;
            let mut parents = false;
            for input in &tx.input {
                ensure!(
                    overlay.spent.insert(input.previous_output),
                    "conflicting mempool snapshot"
                );
                let coin = if let Some(parent) = self.cached.get(&input.previous_output.txid) {
                    parents = true;
                    let output = parent
                        .tx
                        .output
                        .get(input.previous_output.vout as usize)
                        .context("missing mempool parent output")?;
                    ensure!(
                        !output.script_pubkey.is_op_return()
                            && output.script_pubkey.len() <= 10_000,
                        "unspendable mempool prevout"
                    );
                    Coin {
                        value: output.value.to_sat(),
                        height: 0,
                        coinbase: false,
                        script: script(output.script_pubkey.as_bytes()),
                    }
                } else {
                    snapshot
                        .coin(input.previous_output)?
                        .context("mempool prevout missing from confirmed generation")?
                };
                input_value = input_value
                    .checked_add(coin.value)
                    .context("input value overflow")?;
                affected.insert(coin.script);
                add(&mut overlay.delta, coin.script, -i64::try_from(coin.value)?)?;
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let hash = script(output.script_pubkey.as_bytes());
                affected.insert(hash);
                output_value = output_value
                    .checked_add(output.value.to_sat())
                    .context("output value overflow")?;
                if output.script_pubkey.is_op_return() || output.script_pubkey.len() > 10_000 {
                    continue;
                }
                add(
                    &mut overlay.delta,
                    hash,
                    i64::try_from(output.value.to_sat())?,
                )?;
                overlay.outputs.insert(
                    OutPoint {
                        txid: id,
                        vout: vout.try_into()?,
                    },
                    Coin {
                        value: output.value.to_sat(),
                        height: 0,
                        coinbase: false,
                        script: hash,
                    },
                );
            }
            let fee = input_value
                .checked_sub(output_value)
                .context("negative mempool fee")?;
            overlay.fees.insert(id, fee);
            for script in affected {
                overlay.history.entry(script).or_default().push(Pending {
                    txid: id,
                    height: if parents { -1 } else { 0 },
                    fee,
                });
            }
            processed += 1;
            for child in children.get(&id).into_iter().flatten() {
                let remaining = degree.get_mut(child).context("missing dependency count")?;
                *remaining = remaining.checked_sub(1).context("dependency underflow")?;
                if *remaining == 0 {
                    ready.insert(*child);
                }
            }
        }
        ensure!(
            processed == self.cached.len(),
            "cyclic or incomplete mempool dependencies"
        );
        overlay
            .outputs
            .retain(|outpoint, _| !overlay.spent.contains(outpoint));
        for history in overlay.history.values_mut() {
            history.sort_by_key(|entry| (-entry.height, entry.txid.to_string()));
        }
        Ok(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_published_overlay_cannot_serve_wallets() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store =
            super::super::BitcoinStore::create(&root.path().join("data"), crate::Chain::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(crate::Chain::Regtest.network());
        store.commit(0, &genesis)?;
        let snapshot = store.snapshot()?;
        let mut pool = Mempool::default();
        let overlay = pool.build(
            &snapshot,
            genesis.block_hash(),
            Instant::now(),
            Budget::default(),
            &AtomicBool::new(false),
        )?;
        pool.published = Some(overlay);
        assert!(pool.ready_for(&snapshot));
        pool.published.as_mut().unwrap().validated_at = Instant::now() - Duration::from_secs(11);
        assert!(!pool.ready_for(&snapshot));
        assert!(pool.view(&snapshot, [0; 32], 100).is_err());
        Ok(())
    }
}
