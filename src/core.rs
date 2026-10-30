use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use bitcoin::{consensus::deserialize, Block};
use serde_json::{json, Value};
use std::{fs, io::Read, path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct Core {
    pub url: String,
    pub cookie: PathBuf,
    agent: ureq::Agent,
}
#[derive(Debug)]
pub struct RpcError {
    pub method: String,
    pub code: i64,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Core RPC {} failed (code {})", self.method, self.code)
    }
}
impl std::error::Error for RpcError {}

impl Core {
    pub fn new(url: String, cookie: PathBuf) -> Result<Self> {
        let address: std::net::SocketAddr = url
            .strip_prefix("http://")
            .context("Core RPC must be loopback HTTP")?
            .trim_end_matches('/')
            .parse()
            .context("Core RPC requires numeric loopback IP:port, no userinfo/path")?;
        ensure!(address.ip().is_loopback(), "Core RPC must be loopback HTTP");
        Ok(Self {
            url,
            cookie,
            agent: ureq::AgentBuilder::new()
                .try_proxy_from_env(false)
                .redirects(0)
                .timeout(Duration::from_secs(30))
                .build(),
        })
    }
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let auth = STANDARD.encode(fs::read_to_string(&self.cookie)?.trim());
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &format!("Basic {auth}"))
            .send_json(json!({"jsonrpc":"1.0","id":1,"method":method,"params":params}));
        let response = match response {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 32 * 1024 * 1024, "RPC response too large");
        let v: Value = serde_json::from_slice(&bytes)?;
        if !v["error"].is_null() {
            return Err(RpcError {
                method: method.to_owned(),
                code: v["error"]["code"]
                    .as_i64()
                    .context("invalid RPC error code")?,
            }
            .into());
        }
        Ok(v["result"].clone())
    }
    pub fn block(&self, hash: &str) -> Result<Block> {
        let v = self.call("getblock", json!([hash, 0]))?;
        deserialize(&hex::decode(
            v.as_str()
                .context("missing block bytes; historical block may be pruned")?,
        )?)
        .context("invalid Core block")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpc_must_be_numeric_loopback() {
        for url in [
            "http://127.0.0.1:8332@evil.invalid",
            "http://192.168.1.2:8332",
            "https://127.0.0.1:8332",
        ] {
            assert!(Core::new(url.to_owned(), PathBuf::from("missing")).is_err());
        }
        assert!(Core::new("http://127.0.0.1:8332".to_owned(), PathBuf::from("missing")).is_ok());
    }
}
