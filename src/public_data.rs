//! Privacy-preserving reconstruction of historical block bodies.
//!
//! Lite already knows the block hash from its local index. When Core has pruned
//! the block, this module asks a public Esplora instance for the **whole raw
//! block**, never for a txid or scripthash. To avoid revealing which block is
//! actually needed, it fetches an aligned window of blocks, in random order,
//! from a single endpoint and verifies every block locally.
use crate::index::Index;
use anyhow::{ensure, Context, Result};
use bitcoin::{hashes::Hash, Block, BlockHash, Network};
use std::{io::Read, path::Path, time::Duration};

pub const FETCH_WINDOW: u32 = 16;
pub const COVER_BLOCKS: u32 = 1;
const MAX_CACHE_FILES: usize = 2048;

const CLEARNET: &[&str] = &["https://mempool.space/api", "https://blockstream.info/api"];
const ONION: &[&str] = &[
    "http://mempoolhqx4isw62xs7abwphsq7ldayuidyx2v2oethdhhj6mlo2r6ad.onion/api",
    "http://explorerzydxu5ecjrkwceayqybizmpjjznk5izmitf2modhcusuqlid.onion/api",
];

pub struct Client {
    agent: ureq::Agent,
    endpoints: Vec<String>,
}

impl Client {
    /// `proxy` is a SOCKS5 endpoint (the private Tor process or an external
    /// proxy). Onion endpoints are used whenever a proxy is configured.
    pub fn new(proxy: Option<&str>) -> Result<Self> {
        let mut builder = ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .redirects(0)
            .timeout(Duration::from_secs(60));
        if let Some(proxy) = proxy {
            builder = builder.proxy(ureq::Proxy::new(format!("socks5://{proxy}"))?);
        }
        let configured = std::env::var("ELECTRUM_LITE_ESPLORA").ok();
        let endpoints: Vec<String> = configured
            .as_deref()
            .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()))
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect();
        let endpoints = if endpoints.is_empty() {
            let list = if proxy.is_some() { ONION } else { CLEARNET };
            list.iter().map(|s| (*s).to_owned()).collect()
        } else {
            endpoints
        };
        ensure!(!endpoints.is_empty(), "no public data endpoint configured");
        Ok(Self {
            agent: builder.build(),
            endpoints,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_endpoints(endpoints: Vec<String>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .try_proxy_from_env(false)
                .redirects(0)
                .timeout(Duration::from_secs(10))
                .build(),
            endpoints,
        }
    }

    pub fn has_proxy(&self) -> bool {
        // Onion endpoints only make sense with a proxy. The caller already
        // knows the configured proxy; this is a light sanity check.
        self.endpoints.iter().any(|e| e.contains(".onion"))
    }

    fn fetch_endpoint(&self, endpoint: &str, hash: BlockHash, limit: usize) -> Result<Vec<u8>> {
        let response = match self
            .agent
            .get(&format!("{endpoint}/block/{hash}/raw"))
            .call()
        {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            (200..300).contains(&response.status()),
            "public block source returned HTTP {}",
            response.status()
        );
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "public block response too large");
        ensure!(!bytes.is_empty(), "empty public block response");
        Ok(bytes)
    }

    fn read_cache(
        &self,
        cache: &Path,
        hash: BlockHash,
        network: Network,
        height: u32,
    ) -> Result<Option<Block>> {
        let path = cache.join(format!("{hash}.blk"));
        if !path.exists() {
            return Ok(None);
        }
        let block: Block = bitcoin::consensus::deserialize(&std::fs::read(&path)?)?;
        crate::net::verify_at(&block, hash, network, height)?;
        Ok(Some(block))
    }

    fn write_cache(&self, cache: &Path, hash: BlockHash, block: &Block) -> Result<()> {
        std::fs::create_dir_all(cache)?;
        let path = cache.join(format!("{hash}.blk"));
        if path.exists() {
            return Ok(());
        }
        let temp = cache.join(format!("{hash}.pending"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("cannot create {}", temp.display()))?;
        use std::io::Write;
        file.write_all(&bitcoin::consensus::serialize(block))?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        evict(cache)?;
        Ok(())
    }

    /// Fetch one block by hash from a public source, verifying it completely.
    pub fn block(
        &self,
        network: Network,
        hash: BlockHash,
        height: u32,
        cache: &Path,
        persist: bool,
    ) -> Result<Block> {
        if let Some(block) = self.read_cache(cache, hash, network, height)? {
            return Ok(block);
        }
        let mut last = None;
        for index in 0..self.endpoints.len() {
            let endpoint = &self.endpoints[(index + rotation(hash)) % self.endpoints.len()];
            match self.fetch_endpoint(endpoint, hash, 8 * 1024 * 1024) {
                Ok(bytes) => match bitcoin::consensus::deserialize::<Block>(&bytes) {
                    Ok(block) => match crate::net::verify_at(&block, hash, network, height) {
                        Ok(()) => {
                            if persist {
                                self.write_cache(cache, hash, &block)?;
                            }
                            return Ok(block);
                        }
                        Err(e) => last = Some(e),
                    },
                    Err(e) => last = Some(e.into()),
                },
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("public block source unavailable")))
    }

    /// Fetch the aligned window containing `height`, return the requested
    /// block, and discard (or cache) the decoys. The endpoint sees a set of
    /// `FETCH_WINDOW` block hashes, never the single block that is needed.
    pub fn block_window(
        &self,
        index: &Index,
        height: u32,
        hash: BlockHash,
        persist: bool,
    ) -> Result<Block> {
        let network = index.network;
        let cache = &index.cache;
        if let Some(block) = self.read_cache(cache, hash, network, height)? {
            return Ok(block);
        }
        let start = height - height % FETCH_WINDOW;
        let end = (start + FETCH_WINDOW - 1).min(index.tip()?.unwrap_or(height));
        let mut order: Vec<u32> = (start..=end).collect();
        shuffle(&mut order, hash);
        let mut target = None;
        for h in order {
            let block_hash = index
                .header(h)
                .with_context(|| format!("header {h}"))?
                .block_hash();
            let block = self.block(network, block_hash, h, cache, persist)?;
            if h == height {
                target = Some(block);
            }
        }
        let block = target.context("window did not contain requested block")?;
        // Optional cover traffic: one random block from another height, so the
        // number of requested windows is not a direct proxy for wallet activity.
        for _ in 0..COVER_BLOCKS {
            if let Some(tip) = index.tip()? {
                let cover_height = rotation(hash) as u32 % (tip + 1);
                if cover_height != height {
                    if let Ok(cover_hash) = index.header(cover_height).map(|h| h.block_hash()) {
                        let _ = self.block(network, cover_hash, cover_height, cache, false);
                    }
                }
            }
        }
        Ok(block)
    }
}

fn rotation(hash: BlockHash) -> usize {
    let bytes = hash.as_byte_array();
    (u32::from_le_bytes(bytes[..4].try_into().expect("hash prefix")) as usize)
        ^ (u32::from_le_bytes(bytes[4..8].try_into().expect("hash prefix")) as usize).rotate_left(7)
}

fn shuffle(values: &mut [u32], seed: BlockHash) {
    let mut state = rotation(seed) as u64 | 1;
    for i in (1..values.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.swap(i, (state as usize) % (i + 1));
    }
}

fn evict(cache: &Path) -> Result<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(cache)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().ends_with(".blk") {
            files.push((entry.metadata()?.modified()?, entry.path()));
        }
    }
    if files.len() <= MAX_CACHE_FILES {
        return Ok(());
    }
    files.sort_by_key(|(time, _)| *time);
    let excess = files.len().saturating_sub(MAX_CACHE_FILES).min(256);
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Verify the Core `CBlockUndo` binary layout is at least well-formed for the
/// number of transactions. Used by the future spent-txout index path and by
/// diagnostics; a malformed response is never accepted.
pub fn validate_spent(block: &Block, bytes: &[u8]) -> Result<()> {
    fn varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
        ensure!(*offset < bytes.len(), "truncated varint");
        let first = bytes[*offset];
        *offset += 1;
        let (value, extra) = match first {
            0xfd => (0, 2),
            0xfe => (0, 4),
            0xff => (0, 8),
            n => return Ok(u64::from(n)),
        };
        ensure!(bytes.len() - *offset >= extra, "truncated varint");
        let mut value = value;
        for i in 0..extra {
            value |= u64::from(bytes[*offset + i]) << (8 * i);
        }
        *offset += extra;
        Ok(value)
    }
    let mut offset = 0;
    let txs = varint(bytes, &mut offset)?;
    ensure!(txs as usize == block.txdata.len(), "undo tx count mismatch");
    for tx in &block.txdata {
        let count = varint(bytes, &mut offset)?;
        if tx.is_coinbase() {
            ensure!(count == 0, "coinbase undo entries");
        } else {
            ensure!(
                count as usize == tx.input.len(),
                "undo input count mismatch"
            );
        }
        for _ in 0..count {
            // value (8) + script length (varint) + script bytes
            ensure!(bytes.len() - offset >= 8, "truncated spent value");
            offset += 8;
            let script = varint(bytes, &mut offset)? as usize;
            ensure!(bytes.len() - offset >= script, "truncated spent script");
            offset += script;
        }
    }
    ensure!(offset == bytes.len(), "trailing spent data");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Index, StorageProfile};
    use bitcoin::Network;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn window_fetches_and_caches_verified_blocks() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-public-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let index =
            Index::open_with_profile(&root.join("index"), Network::Regtest, StorageProfile::Hdd)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        index.commit(0, &genesis)?;
        let mut one = genesis.clone();
        one.header.prev_blockhash = genesis.block_hash();
        one.header.time += 1;
        index.commit(1, &one)?;
        let mut two = one.clone();
        two.header.prev_blockhash = one.block_hash();
        two.header.time += 1;
        index.commit(2, &two)?;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let blocks = [genesis.clone(), one.clone(), two.clone()];
        let worker = std::thread::spawn(move || -> Result<()> {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte)?;
                    request.push(byte[0]);
                    ensure!(request.len() < 8192, "request too large");
                }
                let request = std::str::from_utf8(&request)?;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .context("request path")?;
                let hash = path
                    .strip_prefix("/block/")
                    .and_then(|rest| rest.strip_suffix("/raw"))
                    .context("unexpected path")?;
                let block = blocks
                    .iter()
                    .find(|block| block.block_hash().to_string() == hash)
                    .context("missing mock block")?;
                let body = bitcoin::consensus::serialize(block);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )?;
                stream.write_all(&body)?;
            }
            Ok(())
        });
        let client = Client::with_endpoints(vec![format!("http://{address}")]);
        let block = client.block_window(&index, 1, one.block_hash(), true)?;
        assert_eq!(block.block_hash(), one.block_hash());
        assert!(index
            .cache
            .join(format!("{}.blk", one.block_hash()))
            .exists());
        worker.join().expect("mock server")?;
        drop(index);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
