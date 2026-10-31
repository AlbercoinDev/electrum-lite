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
    /// Revalidate immediately before pruning. Caller must own the index lock,
    /// preserve prune intent and ensure there are no other chain writers.
    pub fn verify_prune_preconditions(
        &self,
        expected_height: u32,
        expected_hash: bitcoin::BlockHash,
    ) -> Result<()> {
        let chain = self.call("getblockchaininfo", json!([]))?;
        let network = self.call("getnetworkinfo", json!([]))?;
        let mempool = self.call("getrawmempool", json!([]))?;
        let tip = self.call("getbestblockhash", json!([]))?;
        validate_prune_preconditions(
            &chain,
            &network,
            &mempool,
            &tip,
            expected_height,
            expected_hash,
        )
    }

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

    /// Resolve a small consecutive range as one RPC batch; never accept a
    /// missing, duplicate, malformed, or unsuccessful reply.
    pub fn block_hashes(&self, start: u32, count: u32) -> Result<Vec<bitcoin::BlockHash>> {
        ensure!((1..=16).contains(&count), "invalid block hash batch size");
        ensure!(start.checked_add(count - 1).is_some(), "height overflow");
        let requests: Vec<_> = (0..count)
            .map(|id| json!({"jsonrpc":"1.0", "id":id, "method":"getblockhash", "params":[start + id]}))
            .collect();
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
        response.into_reader().take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "hash batch response too large");
        parse_hash_batch(&bytes, count as usize)
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

fn validate_prune_preconditions(
    chain: &Value,
    network: &Value,
    mempool: &Value,
    tip: &Value,
    expected_height: u32,
    expected_hash: bitcoin::BlockHash,
) -> Result<()> {
    ensure!(
        chain["pruned"] == true && chain["automatic_pruning"] == false,
        "Core manual pruning mode changed"
    );
    ensure!(
        chain["blocks"].as_u64() == Some(u64::from(expected_height)),
        "Core height changed before prune"
    );
    ensure!(
        chain["bestblockhash"].as_str() == Some(expected_hash.to_string().as_str()),
        "Core chain tip changed before prune"
    );
    ensure!(
        tip.as_str() == Some(expected_hash.to_string().as_str()),
        "Core best block changed before prune"
    );
    ensure!(
        network["networkactive"] == false,
        "Core network became active before prune"
    );
    ensure!(
        mempool.as_array().is_some_and(Vec::is_empty),
        "Core mempool is not empty before prune"
    );
    Ok(())
}

fn parse_hash_batch(bytes: &[u8], count: usize) -> Result<Vec<bitcoin::BlockHash>> {
    let replies: Vec<Value> = serde_json::from_slice(bytes)?;
    ensure!(replies.len() == count, "incomplete hash batch");
    let mut hashes = vec![None; count];
    for reply in replies {
        let id: usize = reply["id"]
            .as_u64()
            .context("missing hash batch id")?
            .try_into()?;
        ensure!(id < count && hashes[id].is_none(), "invalid hash batch id");
        ensure!(
            reply.get("error").is_some_and(Value::is_null),
            "Core getblockhash batch failed"
        );
        hashes[id] = Some(
            reply["result"]
                .as_str()
                .context("missing block hash")?
                .parse()?,
        );
    }
    hashes
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .context("incomplete hash batch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn prune_preconditions_reject_changed_or_missing_state() {
        let hash =
            bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest).block_hash();
        let chain = json!({"pruned":true,"automatic_pruning":false,"blocks":1000,"bestblockhash":hash.to_string()});
        let network = json!({"networkactive":false});
        let mempool = json!([]);
        let tip = json!(hash.to_string());
        let check = |c: &Value, n: &Value, m: &Value, t: &Value| {
            validate_prune_preconditions(c, n, m, t, 1000, hash).is_ok()
        };
        assert!(check(&chain, &network, &mempool, &tip));
        for (field, value) in [
            ("pruned", json!(false)),
            ("automatic_pruning", json!(true)),
            ("blocks", json!(1001)),
            ("bestblockhash", json!("wrong")),
        ] {
            let mut changed = chain.clone();
            changed[field] = value;
            assert!(!check(&changed, &network, &mempool, &tip));
        }
        assert!(!check(
            &chain,
            &json!({"networkactive":true}),
            &mempool,
            &tip
        ));
        assert!(!check(&chain, &json!({}), &mempool, &tip));
        assert!(!check(&chain, &network, &json!(["tx"]), &tip));
        assert!(!check(&chain, &network, &Value::Null, &tip));
        assert!(!check(&chain, &network, &mempool, &json!("wrong")));
    }
    #[test]
    fn hash_batch_validates_all_responses() -> Result<()> {
        let hash =
            bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Regtest).block_hash();
        let valid = json!([{"id":1,"error":null,"result":hash.to_string()},{"id":0,"error":null,"result":hash.to_string()}]);
        assert_eq!(
            parse_hash_batch(&serde_json::to_vec(&valid)?, 2)?,
            vec![hash, hash]
        );
        for invalid in [
            json!([{"id":0,"error":null,"result":hash.to_string()}]),
            json!([{"id":0,"error":null,"result":hash.to_string()},{"id":0,"error":null,"result":hash.to_string()}]),
            json!([{"id":0,"error":null,"result":hash.to_string()},{"id":1,"error":{"code":-1},"result":null}]),
            json!([{"id":0,"error":null,"result":hash.to_string()},{"id":1,"error":null,"result":"invalid"}]),
            json!([{"id":0,"result":hash.to_string()},{"id":1,"error":null,"result":hash.to_string()}]),
        ] {
            assert!(parse_hash_batch(&serde_json::to_vec(&invalid)?, 2).is_err());
        }
        Ok(())
    }
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
