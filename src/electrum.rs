//! Private Electrum 1.4 TCP service. One bounded worker owns SQLite/Core/archive state.
use crate::{
    archive::Archive,
    mempool::Mempool,
    rpc::Core,
    store::{Limits, Store},
    sync, Chain, MAX_BLOCK_BYTES,
};
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, sha256d, Hash},
    Block, BlockHash, Transaction, Txid,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Semaphore},
    time,
};

const MAX_MESSAGE: usize = 8_100_000;
const MAX_RESPONSE: usize = 16_200_000;
#[derive(Clone, Copy, serde::Serialize)]
pub struct TransportLimits {
    pub connections: usize,
    pub subscriptions: usize,
}

pub struct Options {
    pub transport: TransportLimits,
    pub chain: Chain,
    pub data_dir: PathBuf,
    pub core: Core,
    pub limits: Limits,
    pub query_limit: usize,
    pub listen: SocketAddr,
    pub poll_seconds: u64,
    pub min_free_bytes: u64,
    pub mempool_budget: crate::mempool::Budget,
}

#[derive(Debug)]
struct Fault(i64, &'static str);
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.1)
    }
}
impl std::error::Error for Fault {}
fn invalid() -> anyhow::Error {
    Fault(-32602, "invalid parameters").into()
}
fn argument<'a>(params: &'a Value, index: usize, name: &str) -> Option<&'a Value> {
    if params.is_array() {
        params.get(index)
    } else {
        params.get(name)
    }
}
fn text<'a>(params: &'a Value, index: usize, name: &str) -> Result<&'a str> {
    argument(params, index, name)
        .and_then(Value::as_str)
        .ok_or_else(invalid)
}
fn number(params: &Value, index: usize, name: &str) -> Result<u32> {
    argument(params, index, name)
        .and_then(Value::as_u64)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(invalid)
}
fn optional_number(params: &Value, index: usize, name: &str, default: u32) -> Result<u32> {
    if argument(params, index, name).is_some() {
        number(params, index, name)
    } else {
        Ok(default)
    }
}
fn optional_bool(params: &Value, index: usize, name: &str) -> Result<bool> {
    argument(params, index, name).map_or(Ok(false), |v| v.as_bool().ok_or_else(invalid))
}
fn script(params: &Value) -> Result<String> {
    let hash = text(params, 0, "scripthash")?;
    ensure!(
        hex::decode(hash).map(|b| b.len() == 32).unwrap_or(false),
        Fault(-32602, "invalid scripthash")
    );
    Ok(hash.to_ascii_lowercase())
}

pub fn merkle(mut hashes: Vec<[u8; 32]>, mut position: usize) -> Result<(Vec<String>, String)> {
    ensure!(position < hashes.len(), "Merkle position out of range");
    let mut branch = Vec::new();
    while hashes.len() > 1 {
        if hashes.len() % 2 == 1 {
            hashes.push(*hashes.last().unwrap());
        }
        branch.push(BlockHash::from_byte_array(hashes[position ^ 1]).to_string());
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let mut data = [0u8; 64];
                data[..32].copy_from_slice(&pair[0]);
                data[32..].copy_from_slice(&pair[1]);
                sha256d::Hash::hash(&data).to_byte_array()
            })
            .collect();
        position /= 2;
    }
    Ok((branch, BlockHash::from_byte_array(hashes[0]).to_string()))
}

enum Request {
    Call(String, Value, oneshot::Sender<Result<Value>>),
    Poll(Vec<String>, oneshot::Sender<Result<Value>>),
    Stop,
}

struct State {
    options: Options,
    archive: Archive,
    store: Store,
    pool: Mempool,
    ready: bool,
    last_error: Option<&'static str>,
    requests: u64,
    recovery_required: bool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    core_info: Value,
    core_network: Value,
    observed_at: Option<Instant>,
    catching_up: bool,
}

// Only expose predefined diagnostics; underlying errors may include local paths or RPC data.
fn diagnostic(error: &anyhow::Error) -> &'static str {
    let details = format!("{error:#}");
    if details.contains("historical blocks were already pruned") {
        "required historical blocks are already pruned; restore trusted history"
    } else if details.contains("free-space threshold") {
        "free-space threshold reached"
    } else if details.contains("mempool")
        && (details.contains("budget") || details.contains("response exceeds"))
    {
        "mempool budget exceeded; increase limits only if resources permit"
    } else if details.contains("mempool snapshot warming") {
        "mempool snapshot warming; retrying"
    } else if details.contains("reorg exceeds") || details.contains("undo") {
        "reorg exceeds available undo; separate chain recovery required"
    } else if details.contains("network mismatch") || details.contains("genesis mismatch") {
        "Core network or genesis mismatch"
    } else if details.contains("RPC cookie") {
        "cannot read or validate Core RPC cookie"
    } else if details.contains("RPC transport") {
        "Core RPC unavailable; check endpoint and Core"
    } else if details.contains("Core RPC failed") {
        "Core RPC failed; check Core status and historical block availability"
    } else if details.contains("Core changed") || details.contains("Core reorg") {
        "Core chain changed during synchronization; retrying"
    } else {
        "synchronization or storage error; preserve data and check Core and disk"
    }
}

impl State {
    fn progress(
        &self,
        previous: Option<(u32, Instant)>,
        now: Instant,
    ) -> Result<(String, Option<(u32, Instant)>)> {
        let tip = self.store.stats()?.tip.map(|(height, _)| height);
        let current = self.core_info["blocks"].as_u64();
        let age = self.observed_at.map(|t| t.elapsed().as_secs());
        let rate = tip.zip(previous).and_then(|(height, (old, when))| {
            let elapsed = now.saturating_duration_since(when).as_secs_f64();
            height
                .checked_sub(old)
                .filter(|_| elapsed > 0.0)
                .map(|delta| f64::from(delta) / elapsed)
        });
        let number = |value: Option<u64>| value.map_or_else(|| "?".to_owned(), |n| n.to_string());
        let status = if self.last_error.is_some() {
            "esperando o error"
        } else if self.ready {
            "listo"
        } else if self.catching_up {
            "indexando"
        } else {
            "esperando Core o mempool"
        };
        let lag = current
            .zip(tip)
            .map(|(core, index)| core.saturating_sub(u64::from(index)));
        let floor = self.core_info["pruneheight"].as_u64();
        let next = tip.map_or(0, |height| u64::from(height) + 1);
        let mut line = format!("Estado: {status} | Core: {} | Electrum Lite: {} | retraso observado: {} | bloques/s: {} | poda Core: {} | siguiente: {} | observación Core: {} s | libre: {} MiB",
            number(current), number(tip.map(u64::from)), number(lag), rate.map_or_else(|| "?".to_owned(), |v| format!("{v:.1}")), number(floor), next, number(age), fs2::available_space(self.archive.directory())? / (1024 * 1024));
        if self.core_info["automatic_pruning"] == true {
            line.push_str(" | AVISO: poda automática puede eliminar bloques necesarios");
        }
        if floor.is_some_and(|floor| next.max(1) < floor) {
            line.push_str(
                " | ERROR: faltan bloques históricos; conservar datos y usar fuente confiable",
            );
        }
        if let Some(error) = self.last_error {
            line.push_str(" | causa: ");
            line.push_str(error);
        }
        Ok((line, tip.map(|height| (height, now))))
    }

    fn refresh(&mut self) -> Result<()> {
        self.ready = false;
        if self.recovery_required {
            self.store.recover(&mut self.archive)?;
            self.recovery_required = false;
        }
        ensure!(
            fs2::available_space(self.archive.directory())? >= self.options.min_free_bytes,
            "free-space threshold reached"
        );
        self.core_info = self.options.core.verify(self.options.chain)?;
        self.observed_at = Some(Instant::now());
        self.core_network = self.options.core.call("getnetworkinfo", json!([]))?;
        if let Err(error) = sync::batch(
            &mut self.store,
            &mut self.archive,
            &self.options.core,
            self.options.chain,
            20,
            None,
            Some(&self.stop),
        ) {
            self.recovery_required = true;
            return Err(error);
        }
        let info = self.options.core.verify(self.options.chain)?;
        self.core_info = info.clone();
        self.observed_at = Some(Instant::now());
        self.core_info["mempool"] = self.options.core.call("getmempoolinfo", json!([]))?;
        self.core_network = self.options.core.call("getnetworkinfo", json!([]))?;
        let tip = self.store.stats()?.tip.context("index empty")?;
        self.catching_up = info["blocks"]
            .as_u64()
            .is_some_and(|h| h > u64::from(tip.0));
        self.last_error = None;
        if info["blocks"].as_u64() != Some(u64::from(tip.0))
            || info["initialblockdownload"] != false
        {
            return Ok(());
        }
        ensure!(self.options.core.hash(tip.0)? == tip.1, "Core changed tip");
        self.pool.reconcile(
            &self.options.core,
            &self.store,
            self.options.mempool_budget,
            Some(&self.stop),
        )?;
        self.ready = true;
        self.last_error = None;
        Ok(())
    }

    fn guard(&self) -> Result<()> {
        ensure!(
            self.ready,
            Fault(
                1,
                "index not ready; synchronization or reconciliation in progress"
            )
        );
        let current: String = self.options.core.call("getbestblockhash", json!([]))?;
        ensure!(
            self.store
                .stats()?
                .tip
                .is_some_and(|(_, hash)| hash.to_string() == current),
            Fault(1, "chain changed; retry after synchronization")
        );
        Ok(())
    }

    fn block(&self, height: u32) -> Result<Block> {
        let (hash, location) = self.store.record(height)?.context("block not indexed")?;
        let (h, block) = self.archive.read(location)?;
        ensure!(
            h == height && block.block_hash() == hash,
            "archive reference mismatch"
        );
        Ok(block)
    }
    fn header(&self) -> Result<Value> {
        let (height, _) = self.store.stats()?.tip.context("index empty")?;
        Ok(json!({"height":height,"hex":hex::encode(serialize(&self.store.header(height)?))}))
    }
    fn history(&self, hash: &str) -> Result<Value> {
        let view = self.view(hash)?;
        let mut result: Vec<Value> = view
            .history
            .into_iter()
            .filter(|o| o.height > 0)
            .map(|o| json!({"tx_hash":o.txid,"height":o.height}))
            .collect();
        if let Some(pending) = self.pool.history.get(hash) {
            for p in pending {
                result.push(serde_json::to_value(p)?);
            }
        }
        ensure!(
            result.len() <= self.options.query_limit,
            Fault(10001, "history too large")
        );
        Ok(json!(result))
    }
    fn view(&self, hash: &str) -> Result<crate::index::ScriptView> {
        self.store
            .view(hash, self.options.query_limit)
            .map_err(|error| {
                if error.to_string().contains("--max-query-results") {
                    Fault(10001, "script history or UTXO list too large").into()
                } else {
                    error
                }
            })
    }
    fn status(&self, hash: &str) -> Result<Value> {
        let history = self.history(hash)?;
        let entries = history.as_array().unwrap();
        if entries.is_empty() {
            return Ok(Value::Null);
        }
        let mut joined = String::new();
        for entry in entries {
            joined.push_str(entry["tx_hash"].as_str().context("invalid history txid")?);
            joined.push(':');
            joined.push_str(&entry["height"].to_string());
            joined.push(':');
        }
        Ok(json!(sha256::Hash::hash(joined.as_bytes()).to_string()))
    }

    fn call(&mut self, method: &str, params: &Value) -> Result<Value> {
        self.requests += 1;
        match method {
            "server.version" => {
                let version = argument(params, 1, "protocol_version")
                    .cloned()
                    .unwrap_or(json!("1.4"));
                let (low, high) = if let Some(v) = version.as_str() {
                    (v, v)
                } else {
                    let a = version.as_array().ok_or_else(invalid)?;
                    ensure!(a.len() == 2, Fault(-32602, "invalid version range"));
                    (
                        a[0].as_str().ok_or_else(invalid)?,
                        a[1].as_str().ok_or_else(invalid)?,
                    )
                };
                ensure!(
                    version_tuple(low)? <= version_tuple("1.4")?
                        && version_tuple(high)? >= version_tuple("1.4")?,
                    Fault(1, "unsupported protocol version")
                );
                return Ok(json!([
                    format!("ElectrumLite {}", env!("CARGO_PKG_VERSION")),
                    "1.4"
                ]));
            }
            "server.ping" => return Ok(Value::Null),
            "server.banner" => {
                return Ok(json!(
                    "Electrum Lite private beta; no production guarantees"
                ))
            }
            "server.donation_address" => return Ok(json!("")),
            "server.peers.subscribe" => return Ok(json!([])),
            "server.add_peer" => return Ok(json!(false)),
            "server.features" => {
                return Ok(
                    json!({"server_version":format!("ElectrumLite {}",env!("CARGO_PKG_VERSION")),"protocol_min":"1.4","protocol_max":"1.4","genesis_hash":self.options.chain.genesis(),"hash_function":"sha256","hosts":{"localhost":{"tcp_port":self.options.listen.port()}},"pruning":null}),
                )
            }
            "server.health" => {
                let stats = self.store.stats()?;
                let next = stats.tip.map_or(0, |(height, _)| u64::from(height) + 1);
                let floor = self.core_info["pruneheight"].as_u64();
                let missing = floor.is_some_and(|floor| next.max(1) < floor);
                let status = if self.last_error.is_some() {
                    "error"
                } else if self.ready {
                    "ready"
                } else if self.catching_up {
                    "indexing"
                } else {
                    "waiting_for_core_or_mempool"
                };
                return Ok(json!({
                    "version":env!("CARGO_PKG_VERSION"),"network":self.options.chain,
                    "ready":self.guard().is_ok(),
                    "indexed_tip":stats.tip.map(|(height,hash)|json!({"height":height,"hash":hash})),
                    "core_tip":self.core_info["bestblockhash"],"core_height":self.core_info["blocks"],
                    "core_initialblockdownload":self.core_info["initialblockdownload"],
                    "core_pruned":self.core_info["pruned"],"core_automatic_pruning":self.core_info["automatic_pruning"],
                    "core_prune_height":floor,"next_required_height":next,"historical_blocks_missing":missing,
                    "core_observation_age_seconds":self.observed_at.map(|t|t.elapsed().as_secs()),"status":status,
                    "core_network_active":self.core_network["networkactive"],"core_connections":self.core_network["connections"],
                    "core_mempool_transactions":self.core_info["mempool"]["size"],"core_mempool_vbytes":self.core_info["mempool"]["bytes"],
                    "transactions":stats.transactions,"utxos":stats.utxos,"history_entries":stats.history,
                    "mempool_cached_transactions":self.pool.raw.len(),"mempool_budget":self.options.mempool_budget,"transport_limits":self.options.transport,
                    "requests":self.requests,"archive_bytes":self.archive.size_bytes(),
                    "index_bytes":std::fs::metadata(self.archive.directory().join("index.sqlite"))?.len(),
                    "free_bytes":fs2::available_space(self.archive.directory())?,"last_error":self.last_error,"automatic_pruning":false
                }));
            }
            _ => {}
        }
        ensure!(names(method).is_some(), Fault(-32601, "method not found"));
        self.guard()?;
        match method {
            "blockchain.headers.subscribe" => self.header(),
            "blockchain.block.header" => {
                let h = number(params, 0, "height")?;
                let cp = optional_number(params, 1, "cp_height", 0)?;
                let header = hex::encode(serialize(&self.store.header(h)?));
                if cp == 0 {
                    return Ok(json!(header));
                }
                ensure!(h <= cp, Fault(-32602, "height after checkpoint"));
                let (branch, root) = merkle(self.store.header_hashes(cp)?, h as usize)?;
                Ok(json!({"header":header,"root":root,"branch":branch}))
            }
            "blockchain.block.headers" => {
                let start = number(params, 0, "start_height")?;
                let requested = number(params, 1, "count")?.min(2016);
                let cp = optional_number(params, 2, "cp_height", 0)?;
                let tip = self.store.stats()?.tip.unwrap().0;
                ensure!(
                    start <= tip.saturating_add(1),
                    Fault(-32602, "start height after tip")
                );
                let count = requested.min(tip.saturating_add(1).saturating_sub(start));
                let bytes = self.store.headers(start, count)?;
                let mut result = json!({"count":count,"max":2016,"hex":hex::encode(bytes)});
                if cp > 0 && count > 0 {
                    let final_height = start + count - 1;
                    ensure!(
                        final_height <= cp,
                        Fault(-32602, "headers after checkpoint")
                    );
                    let (branch, root) =
                        merkle(self.store.header_hashes(cp)?, final_height as usize)?;
                    result["root"] = json!(root);
                    result["branch"] = json!(branch);
                }
                Ok(result)
            }
            "blockchain.scripthash.get_history" => self.history(&script(params)?),
            "blockchain.scripthash.subscribe" => self.status(&script(params)?),
            "blockchain.scripthash.unsubscribe" => Ok(json!(false)),
            "blockchain.scripthash.get_mempool" => {
                let pending = self
                    .pool
                    .history
                    .get(&script(params)?)
                    .cloned()
                    .unwrap_or_default();
                ensure!(
                    pending.len() <= self.options.query_limit,
                    Fault(10001, "mempool history too large")
                );
                Ok(json!(pending))
            }
            "blockchain.scripthash.get_balance" => {
                let hash = script(params)?;
                let view = self.view(&hash)?;
                Ok(
                    json!({"confirmed":view.confirmed_sats,"unconfirmed":self.pool.delta.get(&hash).copied().unwrap_or(0)}),
                )
            }
            "blockchain.scripthash.listunspent" => {
                let hash = script(params)?;
                let view = self.view(&hash)?;
                let mut confirmed = Vec::new();
                for (point, coin) in view.utxos {
                    if self.pool.spent.contains(&point) {
                        continue;
                    }
                    let position = self
                        .store
                        .occurrences(point.txid, Some(coin.height))?
                        .last()
                        .context("UTXO occurrence absent")?
                        .position;
                    confirmed.push((coin.height,position,point.vout,json!({"tx_hash":point.txid,"tx_pos":point.vout,"height":coin.height,"value":coin.value})));
                }
                confirmed.sort_by_key(|(h, p, v, _)| (*h, *p, *v));
                let mut result: Vec<_> = confirmed.into_iter().map(|(_, _, _, v)| v).collect();
                let mut pending: Vec<_> = self
                    .pool
                    .outputs
                    .iter()
                    .filter(|(point, coin)| {
                        coin.scripthash == hash && !self.pool.spent.contains(point)
                    })
                    .collect();
                pending.sort_by_key(|(point, _)| (point.txid.to_string(), point.vout));
                for (point, coin) in pending {
                    result.push(json!({"tx_hash":point.txid,"tx_pos":point.vout,"height":0,"value":coin.value}));
                }
                ensure!(
                    result.len() <= self.options.query_limit,
                    Fault(10001, "UTXO response too large")
                );
                Ok(json!(result))
            }
            "blockchain.transaction.get" => {
                let txid: Txid = text(params, 0, "tx_hash")?.parse().map_err(|_| invalid())?;
                let mut confirmed = None;
                let transaction = if let Some(tx) = self.pool.raw.get(&txid) {
                    tx.clone()
                } else {
                    let occurrences = self.store.occurrences(txid, None)?;
                    let occurrence = occurrences.last().context("transaction absent")?;
                    let block = self.block(occurrence.height)?;
                    confirmed = Some((occurrence.height, block.header));
                    block
                        .txdata
                        .get(occurrence.position)
                        .context("transaction position missing")?
                        .clone()
                };
                ensure!(transaction.compute_txid() == txid, "TXID mismatch");
                let raw = hex::encode(serialize(&transaction));
                if !optional_bool(params, 1, "verbose")? {
                    return Ok(json!(raw));
                }
                let mut decoded: Value = self
                    .options
                    .core
                    .call("decoderawtransaction", json!([raw]))?;
                decoded["hex"] = json!(hex::encode(serialize(&transaction)));
                if let Some((height, header)) = confirmed {
                    decoded["blockhash"] = json!(header.block_hash());
                    decoded["blocktime"] = json!(header.time);
                    decoded["time"] = json!(header.time);
                    decoded["confirmations"] =
                        json!(self.store.stats()?.tip.unwrap().0 - height + 1);
                } else {
                    decoded["confirmations"] = json!(0);
                }
                Ok(decoded)
            }
            "blockchain.transaction.get_merkle" | "blockchain.transaction.id_from_pos" => {
                let (height, position, proof) = if method.ends_with("get_merkle") {
                    let txid: Txid = text(params, 0, "tx_hash")?.parse().map_err(|_| invalid())?;
                    let height = number(params, 1, "height")?;
                    let entries = self.store.occurrences(txid, Some(height))?;
                    (
                        height,
                        entries
                            .last()
                            .context("transaction absent at height")?
                            .position,
                        true,
                    )
                } else {
                    (
                        number(params, 0, "height")?,
                        number(params, 1, "tx_pos")? as usize,
                        optional_bool(params, 2, "merkle")?,
                    )
                };
                let block = self.block(height)?;
                let txid = block
                    .txdata
                    .get(position)
                    .context("transaction position out of range")?
                    .compute_txid();
                if !proof {
                    return Ok(json!(txid));
                }
                let (branch, _) = merkle(
                    block
                        .txdata
                        .iter()
                        .map(|tx| tx.compute_txid().to_byte_array())
                        .collect(),
                    position,
                )?;
                if method.ends_with("get_merkle") {
                    Ok(json!({"block_height":height,"pos":position,"merkle":branch}))
                } else {
                    Ok(json!({"tx_hash":txid,"merkle":branch}))
                }
            }
            "blockchain.transaction.broadcast" => {
                let raw = text(params, 0, "raw_tx")?;
                ensure!(
                    raw.len() <= MAX_BLOCK_BYTES * 2,
                    Fault(-32602, "transaction too large")
                );
                let tx: Transaction = deserialize(&hex::decode(raw).map_err(|_| invalid())?)
                    .map_err(|_| invalid())?;
                let txid: String = self.options.core.call("sendrawtransaction", json!([raw]))?;
                ensure!(
                    txid == tx.compute_txid().to_string(),
                    "broadcast TXID mismatch"
                );
                Ok(json!(txid))
            }
            "blockchain.estimatefee" => {
                let blocks = number(params, 0, "number")?;
                ensure!(blocks > 0, Fault(-32602, "invalid block target"));
                let fee: Value = self
                    .options
                    .core
                    .call("estimatesmartfee", json!([blocks]))?;
                Ok(fee.get("feerate").cloned().unwrap_or(json!(-1)))
            }
            "blockchain.relayfee" => {
                let info: Value = self.options.core.call("getnetworkinfo", json!([]))?;
                Ok(info["relayfee"].clone())
            }
            "mempool.get_fee_histogram" => Ok(self.pool.fee_histogram()),
            _ => Err(Fault(-32601, "method not found").into()),
        }
    }

    fn snapshot(&self, scripts: Vec<String>) -> Result<Value> {
        self.guard()?;
        let mut statuses = serde_json::Map::new();
        for hash in scripts {
            statuses.insert(hash.clone(), self.status(&hash)?);
        }
        Ok(json!({"header":self.header()?,"statuses":statuses}))
    }
}

fn version_tuple(text: &str) -> Result<[u32; 3]> {
    let parts: Vec<_> = text.split('.').take(4).collect();
    ensure!(
        (2..=3).contains(&parts.len()),
        Fault(-32602, "invalid protocol version")
    );
    let mut result = [0; 3];
    for (i, part) in parts.iter().enumerate() {
        result[i] = part.parse().map_err(|_| invalid())?;
    }
    Ok(result)
}

async fn ask(sender: &mpsc::SyncSender<Request>, method: String, params: Value) -> Result<Value> {
    let (tx, rx) = oneshot::channel();
    sender
        .try_send(Request::Call(method, params, tx))
        .map_err(|_| anyhow::anyhow!("server busy"))?;
    time::timeout(Duration::from_secs(90), rx)
        .await
        .context("worker timeout")?
        .context("worker stopped")?
}
async fn poll(sender: &mpsc::SyncSender<Request>, scripts: Vec<String>) -> Result<Value> {
    let (tx, rx) = oneshot::channel();
    sender
        .try_send(Request::Poll(scripts, tx))
        .map_err(|_| anyhow::anyhow!("server busy"))?;
    time::timeout(Duration::from_secs(90), rx)
        .await
        .context("worker timeout")?
        .context("worker stopped")?
}

async fn frame(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    partial: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            ensure!(partial.is_empty(), "truncated JSON frame");
            return Ok(None);
        }
        let end = bytes.iter().position(|b| *b == b'\n');
        let n = end.map_or(bytes.len(), |i| i + 1);
        ensure!(partial.len() + n <= MAX_MESSAGE, "message too large");
        partial.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if end.is_some() {
            return Ok(Some(std::mem::take(partial)));
        }
    }
}
fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn names(method: &str) -> Option<&'static [&'static str]> {
    Some(match method {
        "server.version" => &["client_name", "protocol_version"],
        "server.add_peer" => &["features"],
        "server.ping"
        | "server.banner"
        | "server.donation_address"
        | "server.peers.subscribe"
        | "server.features"
        | "server.health"
        | "blockchain.headers.subscribe"
        | "blockchain.relayfee"
        | "mempool.get_fee_histogram" => &[],
        "blockchain.block.header" => &["height", "cp_height"],
        "blockchain.block.headers" => &["start_height", "count", "cp_height"],
        "blockchain.scripthash.get_history"
        | "blockchain.scripthash.get_mempool"
        | "blockchain.scripthash.get_balance"
        | "blockchain.scripthash.listunspent"
        | "blockchain.scripthash.subscribe"
        | "blockchain.scripthash.unsubscribe" => &["scripthash"],
        "blockchain.transaction.get" => &["tx_hash", "verbose"],
        "blockchain.transaction.get_merkle" => &["tx_hash", "height"],
        "blockchain.transaction.id_from_pos" => &["height", "tx_pos", "merkle"],
        "blockchain.transaction.broadcast" => &["raw_tx"],
        "blockchain.estimatefee" => &["number"],
        _ => return None,
    })
}

fn response(id: Value, result: Result<Value>, legacy: bool) -> Value {
    let mut value = match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => {
            let (code, message) = error
                .downcast_ref::<Fault>()
                .map_or((1, "request unavailable or index not ready"), |f| {
                    (f.0, f.1)
                });
            self::error(id, code, message)
        }
    };
    if legacy {
        value.as_object_mut().unwrap().remove("jsonrpc");
        if value.get("error").is_some() {
            value["result"] = Value::Null;
        } else {
            value["error"] = Value::Null;
        }
    }
    value
}

async fn request(
    value: Value,
    sender: &mpsc::SyncSender<Request>,
    scripts: &mut HashMap<String, Value>,
    headers: &mut Option<Value>,
    subscription_limit: usize,
) -> Option<Value> {
    let Some(object) = value.as_object() else {
        return Some(error(Value::Null, -32600, "invalid request"));
    };
    let id = object.get("id").cloned();
    let method = object.get("method").and_then(Value::as_str);
    let valid_id = id
        .as_ref()
        .is_none_or(|id| id.is_null() || id.is_string() || id.is_number());
    if method.is_none()
        || !valid_id
        || object
            .get("jsonrpc")
            .is_some_and(|v| v != "2.0" && v != "1.0")
    {
        return Some(error(Value::Null, -32600, "invalid request"));
    }
    let method = method.unwrap();
    let legacy = object.get("jsonrpc") != Some(&json!("2.0"));
    let params = object.get("params").cloned().unwrap_or(json!([]));
    if !params.is_array() && !params.is_object() {
        return id.map(|id| response(id, Err(invalid()), legacy));
    }
    if let Some(names) = names(method) {
        let valid = params
            .as_array()
            .is_none_or(|array| array.len() <= names.len())
            && params
                .as_object()
                .is_none_or(|object| object.keys().all(|key| names.contains(&key.as_str())));
        if !valid {
            return id.map(|id| response(id, Err(invalid()), legacy));
        }
    }
    let target = if method.starts_with("blockchain.scripthash.") {
        script(&params).ok()
    } else {
        None
    };
    let result = if method == "blockchain.scripthash.subscribe"
        && target.as_ref().is_some_and(|h| !scripts.contains_key(h))
        && scripts.len() >= subscription_limit
    {
        Err(Fault(1, "subscription limit reached").into())
    } else {
        ask(sender, method.to_owned(), params).await
    };
    let result = match result {
        Ok(mut result) => {
            if method == "blockchain.scripthash.subscribe" {
                if let Some(hash) = target {
                    scripts.insert(hash, result.clone());
                }
            } else if method == "blockchain.scripthash.unsubscribe" {
                result = json!(target.is_some_and(|h| scripts.remove(&h).is_some()));
            } else if method == "blockchain.headers.subscribe" {
                *headers = Some(result.clone());
            }
            Ok(result)
        }
        Err(e) => Err(e),
    };
    id.map(|id| response(id, result, legacy))
}

async fn write(writer: &mut tokio::net::tcp::OwnedWriteHalf, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_RESPONSE,
        "response exceeds resource limit"
    );
    bytes.push(b'\n');
    time::timeout(Duration::from_secs(15), writer.write_all(&bytes)).await??;
    Ok(())
}

async fn connection(
    stream: TcpStream,
    sender: mpsc::SyncSender<Request>,
    subscription_limit: usize,
    processing: std::sync::Arc<Semaphore>,
) -> Result<()> {
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut partial = Vec::new();
    let mut scripts = HashMap::new();
    let mut headers = None;
    let mut last_chain = None;
    let mut tick = time::interval(Duration::from_secs(2));
    let mut last = Instant::now();
    let mut window = Instant::now();
    let mut count = 0;
    loop {
        tokio::select! {
            data=frame(&mut reader,&mut partial)=>{
                let Some(data)=data?else{return Ok(());};last=Instant::now();
                let _processing=processing.acquire().await?;
                if window.elapsed()>=Duration::from_secs(10){window=Instant::now();count=0;}
                count+=1;ensure!(count<=200,"request rate limit exceeded");
                let value:Value=match serde_json::from_slice(&data){Ok(v)=>v,Err(_)=>{write(&mut writer,&error(Value::Null,-32700,"parse error")).await?;continue;}};
                let weight=value.as_array().map_or(1,Vec::len);count+=weight.saturating_sub(1);ensure!(count<=200,"request rate limit exceeded");
                if let Value::Array(batch)=value{
                    if batch.is_empty()||batch.len()>64{write(&mut writer,&error(Value::Null,-32600,"invalid batch size")).await?;continue;}
                    let mut responses=Vec::new();for value in batch{if let Some(response)=request(value,&sender,&mut scripts,&mut headers,subscription_limit).await{responses.push(response);}}
                    if !responses.is_empty(){write(&mut writer,&json!(responses)).await?;}
                }else if let Some(response)=request(value,&sender,&mut scripts,&mut headers,subscription_limit).await{write(&mut writer,&response).await?;}
            }
            _=tick.tick()=>{
                ensure!(last.elapsed()<Duration::from_secs(120),"idle connection timeout");
                if scripts.is_empty()&&headers.is_none(){continue;}
                if let Ok(snapshot)=poll(&sender,scripts.keys().cloned().collect()).await{
                    let changed_chain=last_chain.as_ref().is_some_and(|old|old!=&snapshot["header"]);last_chain=Some(snapshot["header"].clone());
                    let changed_header=headers.as_ref().is_some_and(|old|old!=&snapshot["header"]);
                    if changed_header{headers=Some(snapshot["header"].clone());write(&mut writer,&json!({"jsonrpc":"2.0","method":"blockchain.headers.subscribe","params":[snapshot["header"].clone()]})).await?;}
                    for(hash,old)in &mut scripts{if let Some(status)=snapshot["statuses"].get(hash){if *old!=*status||changed_chain{*old=status.clone();write(&mut writer,&json!({"jsonrpc":"2.0","method":"blockchain.scripthash.subscribe","params":[hash,status]})).await?;}}}
                }
            }
        }
    }
}

pub async fn serve(options: Options) -> Result<()> {
    ensure!(options.listen.ip().is_loopback(),"Electrum must bind loopback; use SSH/WireGuard or a verified TLS reverse proxy for remote access");
    ensure!(options.poll_seconds > 0, "poll interval must be positive");
    ensure!(
        (1..=128).contains(&options.transport.connections)
            && (1..=10_000).contains(&options.transport.subscriptions),
        "transport limits out of range"
    );
    let transport = options.transport;
    ensure!(
        options.mempool_budget.transactions > 0 && options.mempool_budget.transactions<=1_000_000 && options.mempool_budget.bytes > 0 && options.mempool_budget.bytes<=1024*1024*1024 && options.mempool_budget.entries>0 && options.mempool_budget.entries<=5_000_000,
        "mempool budgets must be positive and at most 1,000,000 transactions / 1 GiB / 5,000,000 entries"
    );
    let listener = TcpListener::bind(options.listen).await?;
    let listen = listener.local_addr()?;
    let (tx, rx) = mpsc::sync_channel::<Request>(64);
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_stop = stopping.clone();
    let (boot, boot_rx) = oneshot::channel();
    eprintln!(
        "Electrum Lite: verificando archivo e índice; todavía no se ha comprobado Bitcoin Core."
    );
    let worker = std::thread::spawn(move || -> Result<()> {
        let mut archive = Archive::open(&options.data_dir, options.chain, true)?;
        let mut store = Store::open(
            &archive.directory().join("index.sqlite"),
            options.chain,
            options.limits,
        )?;
        store.recover(&mut archive)?;
        let mut state = State {
            options,
            archive,
            store,
            pool: Mempool::default(),
            ready: false,
            last_error: None,
            requests: 0,
            recovery_required: false,
            stop: worker_stop,
            core_info: Value::Null,
            core_network: Value::Null,
            observed_at: None,
            catching_up: false,
        };
        state.options.listen = listen;
        let _ = boot.send(());
        let period = Duration::from_secs(state.options.poll_seconds);
        let mut updated = Instant::now() - period;
        let mut last_progress = Instant::now() - Duration::from_secs(5);
        let mut previous_progress = None;
        let mut displayed_error = None;
        loop {
            if state.stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if updated.elapsed() >= period {
                if let Err(error) = state.refresh() {
                    state.ready = false;
                    state.catching_up = false;
                    state.last_error = Some(diagnostic(&error));
                }
                updated = Instant::now();
                if last_progress.elapsed() >= Duration::from_secs(5)
                    || displayed_error != state.last_error
                {
                    let now = Instant::now();
                    match state.progress(previous_progress, now) {
                        Ok((line, next)) => {
                            eprintln!("{line}");
                            previous_progress = next;
                        }
                        Err(_) => eprintln!(
                            "Estado: no se puede leer el progreso; conservar datos y revisar disco"
                        ),
                    }
                    displayed_error = state.last_error;
                    last_progress = now;
                }
            }
            let wait = if state.catching_up {
                Duration::ZERO
            } else {
                period.saturating_sub(updated.elapsed())
            };
            match rx.recv_timeout(wait) {
                Ok(Request::Call(method, params, reply)) => {
                    let _ = reply.send(state.call(&method, &params));
                }
                Ok(Request::Poll(scripts, reply)) => {
                    let _ = reply.send(state.snapshot(scripts));
                }
                Ok(Request::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if state.catching_up {
                        updated = Instant::now() - period;
                    }
                }
            }
        }
        Ok(())
    });
    boot_rx
        .await
        .context("storage worker failed during startup; inspect database/schema/archive")?;
    eprintln!(
        "{}",
        json!({"version":env!("CARGO_PKG_VERSION"),"electrum_listen":listen.to_string(),"transport":"private-loopback-tcp"})
    );
    let semaphore = std::sync::Arc::new(Semaphore::new(transport.connections));
    let processing = std::sync::Arc::new(Semaphore::new(2));
    let mut supervision = time::interval(Duration::from_secs(1));
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted=listener.accept()=>{let(stream,_)=accepted?;if let Ok(permit)=semaphore.clone().try_acquire_owned(){let sender=tx.clone();let processing=processing.clone();tokio::spawn(async move{let _permit=permit;let _=connection(stream,sender,transport.subscriptions,processing).await;});}},
            _=supervision.tick()=>{ensure!(!worker.is_finished(),"storage worker terminated unexpectedly");},
            _=&mut shutdown=>break,
            _=async {#[cfg(unix)] {terminate.recv().await;} #[cfg(not(unix))] {std::future::pending::<()>().await;}}=>break,
        }
    }
    stopping.store(true, std::sync::atomic::Ordering::Relaxed);
    let sender = tx.clone();
    tokio::task::spawn_blocking(move || {
        let _ = sender.send(Request::Stop);
    })
    .await?;
    tokio::task::spawn_blocking(move || {
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("storage worker panicked"))?
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_specific_without_leaking_error_details() {
        let error = anyhow::anyhow!(
            "required historical blocks were already pruned; restore trusted history"
        );
        assert!(diagnostic(&error).contains("already pruned"));
        let secret = anyhow::anyhow!("sensitive-cookie-and-wallet-data");
        assert!(!diagnostic(&secret).contains("sensitive"));
        let error = anyhow::anyhow!("mempool byte budget exceeded");
        assert!(diagnostic(&error).contains("mempool budget"));
    }

    #[test]
    fn progress_shows_prune_hazard_and_does_not_echo_rpc_contents() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.core_info =
            json!({"blocks":100,"pruneheight":2,"automatic_pruning":true,"untrusted":"secret"});
        state.observed_at = Some(Instant::now());
        state.last_error =
            Some("required historical blocks are already pruned; restore trusted history");
        let (text, previous) = state.progress(None, Instant::now()).unwrap();
        assert!(text.contains("Core: 100") && text.contains("Electrum Lite: 0"));
        assert!(text.contains("poda automática") && text.contains("faltan bloques históricos"));
        assert!(!text.contains("secret"));
        assert_eq!(previous.unwrap().0, 0);
    }

    fn state(root: &std::path::Path) -> State {
        let chain = Chain::Regtest;
        let mut archive = Archive::open(root, chain, true).unwrap();
        let limits = Limits {
            transactions: 100,
            entries: 1000,
        };
        let mut store =
            Store::open(&archive.directory().join("index.sqlite"), chain, limits).unwrap();
        store.recover(&mut archive).unwrap();
        store
            .commit(
                &mut archive,
                0,
                &bitcoin::blockdata::constants::genesis_block(chain.network()),
            )
            .unwrap();
        State {
            options: Options {
                transport: TransportLimits {
                    connections: 16,
                    subscriptions: 1000,
                },
                chain,
                data_dir: root.to_owned(),
                core: Core::new("http://127.0.0.1:1/", std::path::Path::new("unused-cookie"))
                    .unwrap(),
                limits,
                query_limit: 100,
                listen: "127.0.0.1:50001".parse().unwrap(),
                poll_seconds: 1,
                min_free_bytes: 0,
                mempool_budget: crate::mempool::Budget {
                    transactions: 100,
                    bytes: 1_000_000,
                    entries: 1000,
                },
            },
            archive,
            store,
            pool: Mempool::default(),
            ready: false,
            last_error: None,
            requests: 0,
            recovery_required: false,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            core_info: Value::Null,
            core_network: Value::Null,
            observed_at: None,
            catching_up: false,
        }
    }

    #[test]
    fn negotiation_genesis_status_and_not_ready_are_explicit() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        assert_eq!(
            state
                .call("server.version", &json!(["test", ["1.4", "1.4.2"]]))
                .unwrap()[1],
            "1.4"
        );
        assert!(state
            .call("server.version", &json!(["test", "2.0"]))
            .is_err());
        assert_eq!(
            state
                .call("unknown.method", &json!([]))
                .unwrap_err()
                .downcast_ref::<Fault>()
                .unwrap()
                .0,
            -32601
        );
        assert!(state
            .call("blockchain.headers.subscribe", &json!([]))
            .is_err());
        let genesis = bitcoin::blockdata::constants::genesis_block(Chain::Regtest.network());
        let hash = crate::index::script_hash(&genesis.txdata[0].output[0].script_pubkey);
        assert_eq!(state.status(&hash).unwrap(), Value::Null);
        assert!(version_tuple("1.4.1.2").is_err());
        assert!(version_tuple("x.y").is_err());
    }

    #[test]
    fn odd_merkle_branches_match_rust_bitcoin() {
        let mut block = bitcoin::blockdata::constants::genesis_block(Chain::Regtest.network());
        let tx = block.txdata[0].clone();
        block.txdata = (0..5)
            .map(|i| {
                let mut tx = tx.clone();
                tx.lock_time = bitcoin::absolute::LockTime::from_consensus(i);
                tx
            })
            .collect();
        let expected = block.compute_merkle_root().unwrap().to_string();
        for position in 0..5 {
            let (branch, root) = merkle(
                block
                    .txdata
                    .iter()
                    .map(|t| t.compute_txid().to_byte_array())
                    .collect(),
                position,
            )
            .unwrap();
            assert_eq!(root, expected);
            assert_eq!(branch.len(), 3);
        }
        assert!(merkle(Vec::new(), 0).is_err());
    }

    #[tokio::test]
    async fn notifications_legacy_ids_and_named_parameter_validation() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let worker = std::thread::spawn(move || {
            for message in receiver {
                if let Request::Call(_, _, reply) = message {
                    let _ = reply.send(Ok(Value::Null));
                }
            }
        });
        let mut scripts = HashMap::new();
        let mut headers = None;
        assert!(request(
            json!({"method":"server.ping"}),
            &sender,
            &mut scripts,
            &mut headers,
            1000
        )
        .await
        .is_none());
        let legacy = request(
            json!({"id":7,"method":"server.ping"}),
            &sender,
            &mut scripts,
            &mut headers,
            1000,
        )
        .await
        .unwrap();
        assert_eq!(legacy, json!({"id":7,"result":null,"error":null}));
        let invalid=request(json!({"jsonrpc":"2.0","id":8,"method":"blockchain.block.header","params":{"unknown":1}}),&sender,&mut scripts,&mut headers,1000).await.unwrap();
        assert_eq!(invalid["error"]["code"], -32602);
        let invalid = request(
            json!({"id":true,"method":"server.ping"}),
            &sender,
            &mut scripts,
            &mut headers,
            1000,
        )
        .await
        .unwrap();
        assert_eq!(invalid["error"]["code"], -32600);
        let first = format!("{:064x}", 1);
        let second = format!("{:064x}", 2);
        let subscribed=request(json!({"jsonrpc":"2.0","id":9,"method":"blockchain.scripthash.subscribe","params":[first]}),&sender,&mut scripts,&mut headers,1).await.unwrap();
        assert!(subscribed.get("error").is_none());
        let denied=request(json!({"jsonrpc":"2.0","id":10,"method":"blockchain.scripthash.subscribe","params":[second]}),&sender,&mut scripts,&mut headers,1).await.unwrap();
        assert_eq!(denied["error"]["code"], 1);
        drop(sender);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn fragmented_frames_survive_poll_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (read, _write) = server.into_split();
        let mut reader = BufReader::new(read);
        let mut partial = Vec::new();
        client.write_all(b"{\"id\":1,").await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(100), frame(&mut reader, &mut partial))
                .await
                .is_err()
        );
        assert_eq!(partial, b"{\"id\":1,");
        client
            .write_all(b"\"method\":\"server.ping\"}\n")
            .await
            .unwrap();
        let bytes = frame(&mut reader, &mut partial).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["method"],
            "server.ping"
        );
        assert!(partial.is_empty());
        client.write_all(b"{\"id\":2").await.unwrap();
        client.shutdown().await.unwrap();
        assert!(frame(&mut reader, &mut partial).await.is_err());
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_without_unbounded_read() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let writer = tokio::spawn(async move {
            let _ = client.write_all(&vec![b'x'; MAX_MESSAGE + 1]).await;
        });
        let (read, write) = server.into_split();
        let mut reader = BufReader::new(read);
        let mut partial = Vec::new();
        assert!(frame(&mut reader, &mut partial).await.is_err());
        assert!(partial.len() <= MAX_MESSAGE);
        drop(reader);
        drop(write);
        writer.await.unwrap();
    }
}
