use std::{
    fs,
    io::Read,
    net::IpAddr,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use bitcoin::{consensus::deserialize, Block, BlockHash};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use url::Url;

use crate::{Chain, MAX_BLOCK_BYTES};

#[derive(Debug)]
pub struct RpcFailure {
    pub code: i64,
}
impl std::fmt::Display for RpcFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Core RPC failed (code {})", self.code)
    }
}
impl std::error::Error for RpcFailure {}

#[derive(Default)]
struct RpcTiming {
    hash_calls: AtomicU64,
    hash_us: AtomicU64,
    block_calls: AtomicU64,
    block_us: AtomicU64,
    decode_us: AtomicU64,
}

#[derive(Clone)]
pub struct Core {
    agent: ureq::Agent,
    url: String,
    cookie: std::path::PathBuf,
    authorization: Arc<Mutex<Option<(u64, u64, String)>>>,
    timing: Arc<RpcTiming>,
}

impl Core {
    pub fn new(endpoint: &str, cookie: &Path) -> Result<Self> {
        let url = Url::parse(endpoint).context("invalid RPC URL")?;
        ensure!(
            url.scheme() == "http"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "RPC must be a plain HTTP root URL without credentials, query or fragment"
        );
        let host = url.host_str().unwrap_or_default();
        let ip: IpAddr = host
            .trim_matches(['[', ']'])
            .parse()
            .context("use a numeric loopback RPC address (127.0.0.1 or [::1])")?;
        ensure!(
            ip.is_loopback(),
            "beta RPC is restricted to loopback; use a local secure tunnel for remote Core"
        );
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(60))
            .build();
        Ok(Self {
            agent,
            url: url.to_string(),
            cookie: cookie.to_owned(),
            authorization: Arc::new(Mutex::new(None)),
            timing: Arc::new(RpcTiming::default()),
        })
    }

    /// Cache the Basic header and revalidate it with metadata so cookie rotation
    /// still works without reading and base64-encoding the file on every RPC.
    fn authorization(&self) -> Result<String> {
        let metadata = fs::metadata(&self.cookie).context("cannot read RPC cookie")?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let length = metadata.len();
        let mut cache = self
            .authorization
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_modified, cached_length, header)) = cache.as_ref() {
            if *cached_modified == modified && *cached_length == length {
                return Ok(header.clone());
            }
        }
        // Re-read to support Core cookie rotation. Never include credentials in errors.
        let cookie = fs::read_to_string(&self.cookie).context("cannot read RPC cookie")?;
        let cookie = cookie.trim();
        ensure!(
            cookie.contains(':') && !cookie.contains(['\n', '\r']),
            "invalid RPC cookie"
        );
        let header = format!("Basic {}", STANDARD.encode(cookie));
        *cache = Some((modified, length, header.clone()));
        Ok(header)
    }

    pub fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        self.call_with_limit(method, params, MAX_BLOCK_BYTES * 2 + 65536)
    }

    pub fn call_with_limit<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        limit: usize,
    ) -> Result<T> {
        ensure!(
            limit > 0 && limit <= 64 * 1024 * 1024,
            "invalid RPC response budget"
        );
        let authorization = self.authorization()?;
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &authorization)
            .send_json(
                json!({"jsonrpc":"2.0", "id":"electrum-lite", "method":method, "params":params}),
            );
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(_) => anyhow::bail!("RPC transport failed; check Core and the loopback endpoint"),
        };
        let status = response.status();
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "RPC response exceeds size limit");
        let value: Value = serde_json::from_slice(&bytes)
            .context("invalid RPC JSON (check authentication and Core version)")?;
        ensure!(value["id"] == "electrum-lite", "RPC response ID mismatch");
        if !value["error"].is_null() {
            // Codes suffice for diagnostics without echoing server-provided secrets.
            return Err(RpcFailure {
                code: value["error"]["code"]
                    .as_i64()
                    .context("invalid RPC error code")?,
            }
            .into());
        }
        ensure!(status == 200, "Core HTTP status {status}");
        // Move the result out instead of cloning a multi-megabyte block string.
        let mut value = value;
        let result = std::mem::take(value.get_mut("result").context("RPC missing result")?);
        serde_json::from_value(result).context("unexpected RPC result")
    }

    pub fn verify(&self, chain: Chain) -> Result<Value> {
        let info: Value = self.call("getblockchaininfo", json!([]))?;
        ensure!(
            info["chain"].as_str() == Some(chain.rpc_chain()),
            "Core network mismatch"
        );
        ensure!(self.hash(0)? == chain.genesis(), "Core genesis mismatch");
        Ok(info)
    }

    pub fn hash(&self, height: u32) -> Result<BlockHash> {
        let hash: String = self.call("getblockhash", json!([height]))?;
        Ok(hash.parse()?)
    }

    /// Small bounded batches amortize HTTP latency; IDs are checked and results
    /// reordered explicitly (JSON-RPC does not promise response ordering).
    fn batch(&self, method: &str, parameters: Vec<Value>, limit: usize) -> Result<Vec<Value>> {
        ensure!(
            !parameters.is_empty() && parameters.len() <= 8,
            "invalid RPC batch size"
        );
        ensure!(limit <= 64 * 1024 * 1024, "invalid RPC batch budget");
        let count = parameters.len();
        let requests: Vec<Value> = parameters
            .into_iter()
            .enumerate()
            .map(|(id, params)| json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .collect();
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.authorization()?)
            .send_json(json!(requests));
        let response = match response {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(_) => anyhow::bail!("RPC transport failed; check Core and the loopback endpoint"),
        };
        let status = response.status();
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "RPC response exceeds size limit");
        let responses: Vec<Value> =
            serde_json::from_slice(&bytes).context("invalid RPC batch JSON")?;
        ensure!(
            responses.len() == count,
            "RPC batch response count mismatch"
        );
        let mut results = vec![None; count];
        for mut response in responses {
            let id: usize = response["id"]
                .as_u64()
                .context("RPC batch ID missing")?
                .try_into()?;
            ensure!(
                id < count && results[id].is_none(),
                "invalid or duplicate RPC batch ID"
            );
            if !response["error"].is_null() {
                return Err(RpcFailure {
                    code: response["error"]["code"]
                        .as_i64()
                        .context("invalid RPC error code")?,
                }
                .into());
            }
            results[id] = Some(std::mem::take(
                response.get_mut("result").context("RPC missing result")?,
            ));
        }
        ensure!(status == 200, "Core HTTP status {status}");
        results
            .into_iter()
            .map(|value| value.context("RPC batch missing result"))
            .collect()
    }

    pub fn block_group(&self, start: u32, end: u32) -> Result<Vec<(Block, usize)>> {
        ensure!(end >= start && end - start < 8, "invalid block group");
        let started = Instant::now();
        let hashes = self.batch(
            "getblockhash",
            (start..=end).map(|h| json!([h])).collect(),
            65536,
        )?;
        self.timing.hash_calls.fetch_add(1, Ordering::Relaxed);
        self.timing
            .hash_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        let hashes: Vec<BlockHash> = hashes
            .into_iter()
            .map(|h| {
                h.as_str()
                    .context("invalid block hash")?
                    .parse()
                    .context("invalid block hash")
            })
            .collect::<Result<_>>()?;
        let started = Instant::now();
        let blocks = self.batch(
            "getblock",
            hashes.iter().map(|h| json!([h.to_string(), 0])).collect(),
            hashes.len() * (MAX_BLOCK_BYTES * 2 + 65536),
        )?;
        self.timing.block_calls.fetch_add(1, Ordering::Relaxed);
        self.timing
            .block_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        let started = Instant::now();
        let decoded = blocks
            .into_iter()
            .zip(hashes)
            .map(|(encoded, hash)| {
                let encoded = encoded.as_str().context("invalid block hex")?;
                ensure!(
                    encoded.len() <= MAX_BLOCK_BYTES * 2,
                    "Core block exceeds size limit"
                );
                let raw = hex::decode(encoded)?;
                let block: Block = deserialize(&raw)?;
                ensure!(
                    block.block_hash() == hash,
                    "Core returned a different block"
                );
                Ok((block, raw.len()))
            })
            .collect();
        self.timing
            .decode_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        decoded
    }

    /// Successful group-call totals, summed across workers (not wall time).
    pub fn group_timing(&self) -> [u64; 5] {
        [
            self.timing.hash_calls.load(Ordering::Relaxed),
            self.timing.hash_us.load(Ordering::Relaxed),
            self.timing.block_calls.load(Ordering::Relaxed),
            self.timing.block_us.load(Ordering::Relaxed),
            self.timing.decode_us.load(Ordering::Relaxed),
        ]
    }

    pub fn block(&self, hash: BlockHash) -> Result<Block> {
        Ok(self.block_with_size(hash)?.0)
    }

    /// Return the deserialized block plus its consensus byte length so callers
    /// never re-serialize a block just to account for its size.
    pub fn block_with_size(&self, hash: BlockHash) -> Result<(Block, usize)> {
        let encoded: String = self.call("getblock", json!([hash.to_string(), 0]))?;
        ensure!(
            encoded.len() <= MAX_BLOCK_BYTES * 2,
            "Core block exceeds size limit"
        );
        let raw = hex::decode(encoded)?;
        ensure!(
            raw.len() <= MAX_BLOCK_BYTES,
            "Core block exceeds size limit"
        );
        let block: Block = deserialize(&raw)?;
        ensure!(
            block.block_hash() == hash,
            "Core returned a different block"
        );
        Ok((block, raw.len()))
    }
}
