//! Confirmed-only LSM queries from one published generation.
//! Script hashes are raw internally; reversal belongs to the protocol boundary.
use super::{coin_key, kv::ReadSnapshot, numbered, Coin, Counts, Record, State, COUNTS, STATE};
use crate::{archive::Archive, MAX_BLOCK_BYTES};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    block::Header,
    consensus::{deserialize, serialize},
    hashes::{sha256d, Hash},
    Block, BlockHash, OutPoint, Txid,
};
use serde::Serialize;

pub struct Snapshot<'a> {
    read: ReadSnapshot<'a>,
    archive: &'a Archive,
    state: State,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    pub txid: Txid,
    pub block_hash: BlockHash,
    pub height: u32,
    pub position: u32,
}

#[derive(Debug, Serialize)]
pub struct ScriptView {
    pub generation: u64,
    pub tip: Option<(u32, BlockHash)>,
    pub confirmed_sats: u64,
    pub history: Vec<Occurrence>,
    pub utxos: Vec<(OutPoint, Coin)>,
}

#[derive(Debug, Serialize)]
pub struct Proof {
    pub occurrence: Occurrence,
    pub header: Header,
    pub branch: Vec<[u8; 32]>,
}

fn pair(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(&left);
    bytes[32..].copy_from_slice(&right);
    sha256d::Hash::hash(&bytes).to_byte_array()
}

pub fn merkle(mut hashes: Vec<[u8; 32]>, mut position: usize) -> Result<(Vec<[u8; 32]>, [u8; 32])> {
    ensure!(position < hashes.len(), "Merkle position out of range");
    let mut branch = Vec::new();
    while hashes.len() > 1 {
        if !hashes.len().is_multiple_of(2) {
            hashes.push(*hashes.last().context("empty Merkle layer")?);
        }
        branch.push(hashes[position ^ 1]);
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|layer| pair(layer[0], layer[1]))
            .collect();
        position /= 2;
    }
    Ok((branch, hashes[0]))
}

impl Proof {
    pub fn verify(&self) -> bool {
        if self.branch.len() > 32 || self.header.block_hash() != self.occurrence.block_hash {
            return false;
        }
        let mut hash = self.occurrence.txid.to_byte_array();
        let mut position = self.occurrence.position;
        for sibling in &self.branch {
            hash = if position & 1 == 0 {
                pair(hash, *sibling)
            } else {
                pair(*sibling, hash)
            };
            position >>= 1;
        }
        position == 0 && hash == self.header.merkle_root.to_byte_array()
    }
}

impl<'a> Snapshot<'a> {
    pub(super) fn new(read: ReadSnapshot<'a>, archive: &'a Archive) -> Result<Self> {
        let state = read
            .get(STATE)?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            read,
            archive,
            state,
        })
    }
    pub fn generation(&self) -> u64 {
        self.read.generation()
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn counts(&self) -> Result<Counts> {
        match self.read.get(COUNTS)? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => {
                ensure!(self.generation() == 0, "missing durable counts");
                Ok(Counts::default())
            }
        }
    }
    pub fn header_hashes(&self, height: u32) -> Result<Vec<[u8; 32]>> {
        ensure!(height <= 2_000_000, "checkpoint exceeds limit");
        self.record(height)?;
        (0..=height)
            .map(|height| Ok(self.record(height)?.hash.to_byte_array()))
            .collect()
    }
    fn record(&self, height: u32) -> Result<Record> {
        ensure!(
            self.state
                .tip
                .as_ref()
                .is_some_and(|tip| height <= tip.height),
            "height beyond snapshot"
        );
        let record: Record = serde_json::from_slice(
            &self
                .read
                .get(&numbered(b'B', height))?
                .context("missing indexed block")?,
        )?;
        ensure!(record.height == height, "block height mismatch");
        Ok(record)
    }
    pub fn coin(&self, outpoint: OutPoint) -> Result<Option<Coin>> {
        self.read
            .get(&coin_key(outpoint))?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }
    pub fn balance(&self, script: [u8; 32]) -> Result<u64> {
        let mut key = vec![b'V'];
        key.extend(script);
        match self.read.get(&key)? {
            Some(bytes) => Ok(u64::from_be_bytes(bytes.as_slice().try_into()?)),
            None => Ok(0),
        }
    }
    pub fn history(&self, script: [u8; 32], limit: usize) -> Result<Vec<Occurrence>> {
        ensure!(limit <= 1_000_000, "history budget outside supported range");
        let mut prefix = vec![b'H'];
        prefix.extend(script);
        let mut result = Vec::new();
        self.read.visit_prefix(&prefix, |key, value| {
            ensure!(
                key.len() == 73 && value.len() == 32,
                "invalid history entry"
            );
            let height = u32::from_be_bytes(key[33..37].try_into()?);
            if height == 0 {
                return Ok(());
            }
            ensure!(result.len() < limit, "history response limit");
            let hash = BlockHash::from_slice(&key[41..73])?;
            ensure!(
                self.record(height)?.hash == hash,
                "noncanonical script history"
            );
            result.push(Occurrence {
                txid: Txid::from_slice(value)?,
                block_hash: hash,
                height,
                position: u32::from_be_bytes(key[37..41].try_into()?),
            });
            Ok(())
        })?;
        Ok(result)
    }
    pub fn list_unspent(&self, script: [u8; 32], limit: usize) -> Result<Vec<(OutPoint, Coin)>> {
        ensure!(limit <= 1_000_000, "UTXO budget outside supported range");
        let mut prefix = vec![b'S'];
        prefix.extend(script);
        let mut result = Vec::new();
        let mut total = 0u64;
        self.read.visit_prefix(&prefix, |key, value| {
            ensure!(key.len() == 69, "invalid script UTXO key");
            ensure!(result.len() < limit, "UTXO response limit");
            let outpoint: OutPoint = deserialize(&key[33..])?;
            let coin: Coin = serde_json::from_slice(value)?;
            ensure!(
                coin.script == script && self.coin(outpoint)?.as_ref() == Some(&coin),
                "script UTXO mismatch"
            );
            ensure!(
                self.state
                    .tip
                    .as_ref()
                    .is_some_and(|tip| coin.height <= tip.height),
                "UTXO beyond snapshot"
            );
            total = total.checked_add(coin.value).context("balance overflow")?;
            result.push((outpoint, coin));
            Ok(())
        })?;
        ensure!(total == self.balance(script)?, "balance aggregate mismatch");
        Ok(result)
    }
    pub fn view(
        &self,
        script: [u8; 32],
        history_limit: usize,
        utxo_limit: usize,
    ) -> Result<ScriptView> {
        let history = self.history(script, history_limit)?;
        let utxos = self.list_unspent(script, utxo_limit)?;
        Ok(ScriptView {
            generation: self.generation(),
            tip: self.state.tip.as_ref().map(|tip| (tip.height, tip.hash)),
            confirmed_sats: self.balance(script)?,
            history,
            utxos,
        })
    }
    pub fn header(&self, height: u32) -> Result<Header> {
        let record = self.record(height)?;
        let header: Header = deserialize(
            &self
                .read
                .get(&numbered(b'A', height))?
                .context("missing indexed header")?,
        )?;
        ensure!(header.block_hash() == record.hash, "header hash mismatch");
        Ok(header)
    }
    pub fn headers(&self, start: u32, count: u32) -> Result<Vec<u8>> {
        ensure!(count <= 2016, "header chunk exceeds limit");
        let end = start.checked_add(count).context("header range overflow")?;
        let mut result = Vec::with_capacity(count as usize * 80);
        for height in start..end {
            result.extend(serialize(&self.header(height)?));
        }
        Ok(result)
    }
    pub fn block(&self, height: u32) -> Result<Block> {
        let record = self.record(height)?;
        let (actual, block) = self.archive.read(record.location)?;
        ensure!(
            actual == height && block.block_hash() == record.hash,
            "archive reference mismatch"
        );
        Ok(block)
    }
    pub fn occurrences(
        &self,
        txid: Txid,
        height: Option<u32>,
        limit: usize,
    ) -> Result<Vec<Occurrence>> {
        ensure!(limit <= 1024, "occurrence budget outside supported range");
        let mut prefix = vec![b'O'];
        prefix.extend(txid.to_byte_array());
        let mut result = Vec::new();
        self.read.visit_prefix(&prefix, |key, value| {
            ensure!(
                key.len() == 69 && value.len() == 4,
                "invalid occurrence entry"
            );
            let found = u32::from_be_bytes(value.try_into()?);
            if height.is_some_and(|height| height != found) {
                return Ok(());
            }
            ensure!(
                result.len() < limit,
                "occurrence response limit; specify height"
            );
            let hash = BlockHash::from_slice(&key[33..65])?;
            ensure!(self.record(found)?.hash == hash, "noncanonical occurrence");
            result.push(Occurrence {
                txid,
                block_hash: hash,
                height: found,
                position: u32::from_be_bytes(key[65..69].try_into()?),
            });
            Ok(())
        })?;
        result.sort_by_key(|occurrence| (occurrence.height, occurrence.position));
        Ok(result)
    }
    /// Defaults to latest canonical occurrence; explicit height disambiguates BIP30.
    pub fn transaction(
        &self,
        txid: Txid,
        height: Option<u32>,
        max_bytes: usize,
    ) -> Result<(Occurrence, Vec<u8>)> {
        ensure!(
            max_bytes <= MAX_BLOCK_BYTES,
            "transaction response budget outside supported range"
        );
        let occurrence = self
            .occurrences(txid, height, 16)?
            .pop()
            .context("transaction not indexed")?;
        let block = self.block(occurrence.height)?;
        let transaction = block
            .txdata
            .get(occurrence.position as usize)
            .context("transaction position missing")?;
        ensure!(
            transaction.compute_txid() == txid,
            "transaction occurrence mismatch"
        );
        let bytes = serialize(transaction);
        ensure!(bytes.len() <= max_bytes, "transaction response limit");
        Ok((occurrence, bytes))
    }
    pub fn proof(&self, txid: Txid, height: u32) -> Result<Proof> {
        let occurrence = self
            .occurrences(txid, Some(height), 16)?
            .pop()
            .context("transaction not indexed at height")?;
        let block = self.block(height)?;
        let mut hashes: Vec<_> = block
            .txdata
            .iter()
            .map(|tx| tx.compute_txid().to_byte_array())
            .collect();
        ensure!(
            hashes.get(occurrence.position as usize) == Some(&txid.to_byte_array()),
            "transaction position mismatch"
        );
        let mut position = occurrence.position as usize;
        let mut branch = Vec::new();
        while hashes.len() > 1 {
            if !hashes.len().is_multiple_of(2) {
                hashes.push(*hashes.last().context("empty Merkle layer")?);
            }
            branch.push(hashes[position ^ 1]);
            hashes = hashes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|layer| pair(layer[0], layer[1]))
                .collect();
            position /= 2;
        }
        let proof = Proof {
            occurrence,
            header: block.header,
            branch,
        };
        ensure!(proof.verify(), "Merkle proof does not match header");
        Ok(proof)
    }
}
