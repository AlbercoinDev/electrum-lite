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

    /// Query header heights in bounded batches. All replies must be present,
    /// uniquely identified and successful; a partial result is never accepted.
    pub fn header_heights(&self, hashes: &[bitcoin::BlockHash]) -> Result<Vec<u32>> {
        let mut heights = Vec::with_capacity(hashes.len());
        for chunk in hashes.chunks(128) {
            let requests: Vec<_> = chunk.iter().enumerate().map(|(id, hash)| {
                json!({"jsonrpc":"1.0", "id":id, "method":"getblockheader", "params":[hash.to_string(), true]})
            }).collect();
            let auth = STANDARD.encode(fs::read_to_string(&self.cookie)?.trim());
            let response = match self
                .agent
                .post(&self.url)
                .set("Authorization", &format!("Basic {auth}"))
                .send_json(json!(requests))
            {
                Ok(r) | Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(e.into()),
            };
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 2 * 1024 * 1024,
                "header batch response too large"
            );
            let replies: Vec<Value> = serde_json::from_slice(&bytes)?;
            ensure!(replies.len() == chunk.len(), "incomplete header batch");
            let mut batch: Vec<Option<u32>> = vec![None; chunk.len()];
            for reply in replies {
                let id: usize = reply["id"]
                    .as_u64()
                    .context("missing header batch id")?
                    .try_into()?;
                ensure!(
                    id < chunk.len() && batch[id].is_none(),
                    "invalid header batch id"
                );
                ensure!(reply["error"].is_null(), "Core rejected header batch");
                ensure!(
                    reply["result"]["hash"].as_str() == Some(&chunk[id].to_string()),
                    "header batch hash mismatch"
                );
                batch[id] = Some(
                    reply["result"]["height"]
                        .as_u64()
                        .context("missing header height")?
                        .try_into()?,
                );
            }
            heights.extend(
                batch
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .context("incomplete header batch")?,
            );
        }
        Ok(heights)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
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

    #[test]
    fn header_batch_accepts_reordering_and_rejects_duplicate_ids() -> Result<()> {
        let hash =
            bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest).block_hash();
        let cookie = std::env::temp_dir().join(format!(
            "electrum-core-cookie-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::write(&cookie, "user:password")?;
        for duplicate in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let addr = listener.local_addr()?;
            let worker = std::thread::spawn(move || -> Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let header_end = loop {
                    let mut byte = [0];
                    stream.read_exact(&mut byte)?;
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break request.len();
                    }
                    ensure!(request.len() < 16384, "test request headers too large");
                };
                let headers = std::str::from_utf8(&request[..header_end])?;
                let len: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                    .context("test missing content length")?
                    .parse()?;
                let mut body = vec![0; len];
                stream.read_exact(&mut body)?;
                let batch: Vec<Value> = serde_json::from_slice(&body)?;
                ensure!(batch.len() == 2, "test expected two headers");
                let response = json!([
                    {"id":1,"error":null,"result":{"hash":hash.to_string(),"height":7}},
                    {"id":if duplicate {1} else {0},"error":null,"result":{"hash":hash.to_string(),"height":3}}
                ]).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )?;
                Ok(())
            });
            let core = Core::new(format!("http://{addr}"), cookie.clone())?;
            let result = core.header_heights(&[hash, hash]);
            worker.join().expect("test RPC worker")?;
            if duplicate {
                assert!(result.is_err());
            } else {
                assert_eq!(result?, vec![3, 7]);
            }
        }
        fs::remove_file(cookie)?;
        Ok(())
    }
}
