//! Binary REST client for a loopback Bitcoin Core (Core 30+).
//!
//! The REST interface is unauthenticated in Core and is only used against a
//! loopback address. It avoids the hex/JSON overhead of `getblock` and exposes
//! `/rest/spenttxouts/` and `/rest/blockpart/`, which the JSON-RPC interface
//! does not provide.
use anyhow::{ensure, Context, Result};
use bitcoin::{consensus::deserialize, hashes::Hash, Block, BlockHash};
use std::{io::Read, time::Duration};

pub struct Rest {
    url: String,
    agent: ureq::Agent,
}

fn read_limited(response: ureq::Response, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "REST response too large");
    Ok(bytes)
}

impl Rest {
    pub fn new(rpc_url: &str) -> Self {
        let url = rpc_url.trim_end_matches('/').to_owned();
        let agent = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .redirects(0)
            .timeout(Duration::from_secs(30))
            .build();
        Self { url, agent }
    }

    fn get(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        let response = match self.agent.get(&format!("{}{path}", self.url)).call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            (200..300).contains(&response.status()),
            "Core REST unavailable (HTTP {})",
            response.status()
        );
        read_limited(response, limit)
    }

    /// Returns whether this Core exposes the binary REST endpoints we need.
    pub fn probe(&self) -> Result<()> {
        let bytes = self.get("/rest/chaininfo.json", 4 * 1024 * 1024)?;
        let info: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(info["chain"].is_string(), "invalid REST chain info");
        Ok(())
    }

    pub fn block(&self, hash: BlockHash) -> Result<Block> {
        let bytes = self.get(&format!("/rest/block/{hash}.bin"), 8 * 1024 * 1024)?;
        let block: Block = deserialize(&bytes)?;
        ensure!(block.block_hash() == hash, "REST block hash mismatch");
        Ok(block)
    }

    pub fn block_hash_by_height(&self, height: u32) -> Result<BlockHash> {
        let bytes = self.get(&format!("/rest/blockhashbyheight/{height}.bin"), 64)?;
        ensure!(bytes.len() == 32, "invalid REST block hash");
        Ok(BlockHash::from_byte_array(
            bytes.try_into().expect("checked"),
        ))
    }

    /// Fetch up to `count` consecutive headers starting at `hash`.
    pub fn headers(&self, hash: BlockHash, count: u32) -> Result<Vec<bitcoin::block::Header>> {
        ensure!((1..=2000).contains(&count), "invalid REST header count");
        let bytes = self.get(
            &format!("/rest/headers/{hash}.bin?count={count}"),
            2000 * 80,
        )?;
        ensure!(
            !bytes.is_empty() && bytes.len() % 80 == 0,
            "invalid REST headers response"
        );
        let mut headers = Vec::with_capacity(bytes.len() / 80);
        for chunk in bytes.as_chunks::<80>().0 {
            headers.push(deserialize(chunk).context("invalid REST header")?);
        }
        ensure!(headers.len() <= count as usize, "too many REST headers");
        Ok(headers)
    }

    /// Raw spent txouts (Core `CBlockUndo`) for a block. The caller owns the
    /// parser because the format is also useful for building history rows.
    pub fn spent_txouts(&self, hash: BlockHash) -> Result<Vec<u8>> {
        // A 4 MiB block has at most ~4 MiB of undo data for normal blocks.
        self.get(&format!("/rest/spenttxouts/{hash}.bin"), 16 * 1024 * 1024)
    }
}
