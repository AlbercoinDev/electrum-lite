use std::{fs, io::Read, net::IpAddr, path::Path, time::Duration};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use bitcoin::{consensus::deserialize, Block, BlockHash};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use url::Url;

use crate::{Chain, MAX_BLOCK_BYTES};

pub struct Core {
    agent: ureq::Agent,
    url: String,
    cookie: std::path::PathBuf,
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
        })
    }

    pub fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        // Re-read to support Core cookie rotation. Never include credentials in errors.
        let cookie = fs::read_to_string(&self.cookie).context("cannot read RPC cookie")?;
        let cookie = cookie.trim();
        ensure!(
            cookie.contains(':') && !cookie.contains(['\n', '\r']),
            "invalid RPC cookie"
        );
        let response = self
            .agent
            .post(&self.url)
            .set(
                "Authorization",
                &format!("Basic {}", STANDARD.encode(cookie)),
            )
            .send_json(
                json!({"jsonrpc":"2.0", "id":"electrum-lite", "method":method, "params":params}),
            );
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(_) => anyhow::bail!("RPC transport failed; check Core and the loopback endpoint"),
        };
        let status = response.status();
        let limit = MAX_BLOCK_BYTES * 2 + 65536;
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
            anyhow::bail!(
                "Core RPC {method} failed (code {}); historical block may be unavailable or pruned",
                value["error"]["code"]
            );
        }
        ensure!(status == 200, "Core HTTP status {status}");
        serde_json::from_value(value.get("result").context("RPC missing result")?.clone())
            .context("unexpected RPC result")
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

    pub fn block(&self, hash: BlockHash) -> Result<Block> {
        let encoded: String = self.call("getblock", json!([hash.to_string(), 0]))?;
        ensure!(
            encoded.len() <= MAX_BLOCK_BYTES * 2,
            "Core block exceeds size limit"
        );
        let block: Block = deserialize(&hex::decode(encoded)?)?;
        ensure!(
            block.block_hash() == hash,
            "Core returned a different block"
        );
        Ok(block)
    }
}
