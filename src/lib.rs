//! Beta archive/index primitives. Not an Electrum server or a pruning controller.
pub mod archive;
pub mod electrum;
#[cfg(feature = "experimental-lsm")]
pub mod experimental_lsm;
pub mod index;
pub mod mempool;
pub mod operations;
pub mod rpc;
pub mod store;
#[cfg(target_os = "linux")]
pub mod supervisor;
pub mod sync;

use anyhow::{ensure, Result};
use bitcoin::{blockdata::constants::genesis_block, Block, BlockHash, Network, Txid};
use clap::ValueEnum;
use serde::Serialize;

pub const MAX_BLOCK_BYTES: usize = 4_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Mainnet,
    Signet,
    Testnet4,
    Regtest,
}

impl Chain {
    pub fn network(self) -> Network {
        match self {
            Self::Mainnet => Network::Bitcoin,
            Self::Signet => Network::Signet,
            Self::Testnet4 => Network::Testnet4,
            Self::Regtest => Network::Regtest,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Signet => "signet",
            Self::Testnet4 => "testnet4",
            Self::Regtest => "regtest",
        }
    }

    pub fn rpc_chain(self) -> &'static str {
        if self == Self::Mainnet {
            "main"
        } else {
            self.name()
        }
    }

    pub fn port(self) -> u16 {
        match self {
            Self::Mainnet => 8332,
            Self::Signet => 38332,
            Self::Testnet4 => 48332,
            Self::Regtest => 18443,
        }
    }

    pub fn genesis(self) -> BlockHash {
        genesis_block(self.network()).block_hash()
    }
}

/// Validate consensus-visible block commitments once and return the transaction IDs
/// so callers never hash the same transaction twice (Merkle root plus indexing).
pub fn validate_block_txids(block: &Block) -> Result<Vec<bitcoin::Txid>> {
    ensure!(!block.txdata.is_empty(), "empty transaction list");
    ensure!(
        block.txdata[0].is_coinbase(),
        "first transaction is not coinbase"
    );
    ensure!(
        block.txdata.iter().skip(1).all(|tx| !tx.is_coinbase()),
        "multiple coinbase transactions"
    );
    let txids: Vec<bitcoin::Txid> = block.txdata.iter().map(|tx| tx.compute_txid()).collect();
    ensure!(
        bitcoin::merkle_tree::calculate_root(txids.iter().copied())
            == Some(Txid::from_raw_hash(block.header.merkle_root.to_raw_hash())),
        "invalid transaction Merkle root"
    );
    ensure!(
        block.check_witness_commitment(),
        "invalid witness commitment"
    );
    Ok(txids)
}

pub fn validate_block(block: &Block) -> Result<()> {
    validate_block_txids(block).map(|_| ())
}
