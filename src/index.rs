use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{bail, ensure, Result};
use bitcoin::{
    hashes::{sha256, Hash},
    Block, BlockHash, OutPoint, Script, Txid,
};
use serde::Serialize;

use crate::{validate_block, Chain};

#[derive(Clone, Debug, Serialize)]
pub struct Coin {
    pub value: u64,
    pub height: u32,
    pub coinbase: bool,
    pub scripthash: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Occurrence {
    pub txid: Txid,
    pub height: u32,
    pub position: usize,
}

#[derive(Serialize)]
pub struct ScriptView {
    pub confirmed_sats: u64,
    pub history: Vec<Occurrence>,
    pub utxos: Vec<(OutPoint, Coin)>,
}

/// Electrum wire-order scripthash. Never use display-order reversal for TXIDs.
pub fn script_hash(script: &Script) -> String {
    let mut bytes = sha256::Hash::hash(script.as_bytes()).to_byte_array();
    bytes.reverse();
    hex::encode(bytes)
}

pub struct Index {
    pub chain: Chain,
    pub tip: Option<(u32, BlockHash)>,
    pub transactions: usize,
    pub utxos: HashMap<OutPoint, Coin>,
    history: HashMap<String, Vec<Occurrence>>,
    limit: usize,
    entry_limit: usize,
    history_entries: usize,
}

impl Index {
    pub fn new(chain: Chain, limit: usize) -> Self {
        Self::with_limits(chain, limit, 500_000)
    }

    pub fn with_limits(chain: Chain, limit: usize, entry_limit: usize) -> Self {
        Self {
            chain,
            tip: None,
            transactions: 0,
            utxos: HashMap::new(),
            history: HashMap::new(),
            limit,
            entry_limit,
            history_entries: 0,
        }
    }

    pub fn apply(&mut self, height: u32, block: &Block) -> Result<()> {
        validate_block(block)?;
        match self.tip {
            None => ensure!(
                height == 0 && block.block_hash() == self.chain.genesis(),
                "index must start at the selected network genesis"
            ),
            Some((h, hash)) => ensure!(
                height == h + 1 && block.header.prev_blockhash == hash,
                "chain discontinuity or reorg; beta cannot roll back"
            ),
        }
        ensure!(block.txdata.len() <= self.limit.saturating_sub(self.transactions), "in-memory transaction limit reached; increase --max-index-transactions only if sufficient RAM is available");

        // Stage changes so missing prevouts cannot leave a partially applied block.
        let mut changes: HashMap<OutPoint, Option<Coin>> = HashMap::new();
        let mut events = Vec::new();
        for (position, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            let mut scripts = HashSet::new();
            if !tx.is_coinbase() {
                for input in &tx.input {
                    let coin = match changes.get(&input.previous_output) {
                        Some(coin) => coin.clone(),
                        None => self.utxos.get(&input.previous_output).cloned(),
                    };
                    let Some(coin) = coin else {
                        bail!("missing or already spent prevout at height {height}")
                    };
                    scripts.insert(coin.scripthash);
                    changes.insert(input.previous_output, None);
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let hash = script_hash(&output.script_pubkey);
                scripts.insert(hash.clone());
                // Genesis coinbase is not part of Bitcoin's spendable UTXO set.
                if height == 0
                    || output.script_pubkey.is_op_return()
                    || output.script_pubkey.len() > 10_000
                {
                    continue;
                }
                let outpoint = OutPoint {
                    txid,
                    vout: vout.try_into()?,
                };
                let exists = match changes.get(&outpoint) {
                    Some(coin) => coin.is_some(),
                    None => self.utxos.contains_key(&outpoint),
                };
                // Bitcoin's two historical BIP30 overwrite exceptions.
                let bip30 = self.chain == Chain::Mainnet
                    && matches!(height, 91842 | 91880)
                    && tx.is_coinbase();
                ensure!(
                    !exists || bip30,
                    "duplicate unspent outpoint outside historical BIP30 exception"
                );
                changes.insert(
                    outpoint,
                    Some(Coin {
                        value: output.value.to_sat(),
                        height,
                        coinbase: tx.is_coinbase(),
                        scripthash: hash,
                    }),
                );
            }
            for hash in scripts {
                events.push((
                    hash,
                    Occurrence {
                        txid,
                        height,
                        position,
                    },
                ));
            }
        }
        let mut next_utxos = self.utxos.len();
        for (outpoint, coin) in &changes {
            match (self.utxos.contains_key(outpoint), coin.is_some()) {
                (true, false) => next_utxos -= 1,
                (false, true) => next_utxos += 1,
                _ => {}
            }
        }
        let next_history = self
            .history_entries
            .checked_add(events.len())
            .ok_or_else(|| anyhow::anyhow!("history size overflow"))?;
        ensure!(next_utxos.checked_add(next_history).is_some_and(|entries| entries <= self.entry_limit), "in-memory entry limit reached; increase --max-index-entries only if sufficient RAM is available");
        for (outpoint, coin) in changes {
            if let Some(coin) = coin {
                self.utxos.insert(outpoint, coin);
            } else {
                self.utxos.remove(&outpoint);
            }
        }
        for (hash, occurrence) in events {
            self.history.entry(hash).or_default().push(occurrence);
        }
        self.transactions += block.txdata.len();
        self.history_entries = next_history;
        self.tip = Some((height, block.block_hash()));
        Ok(())
    }

    pub fn view(&self, hash: &str) -> Result<ScriptView> {
        ensure!(
            hex::decode(hash)?.len() == 32,
            "scripthash must contain 32 bytes"
        );
        let hash = hash.to_ascii_lowercase();
        let sorted: BTreeMap<_, _> = self
            .utxos
            .iter()
            .filter(|(_, coin)| coin.scripthash == hash)
            .map(|(o, c)| (*o, c.clone()))
            .collect();
        let confirmed_sats = sorted.values().try_fold(0u64, |sum, coin| {
            sum.checked_add(coin.value)
                .ok_or_else(|| anyhow::anyhow!("balance overflow"))
        })?;
        Ok(ScriptView {
            confirmed_sats,
            history: self.history.get(&hash).cloned().unwrap_or_default(),
            utxos: sorted.into_iter().collect(),
        })
    }
}
