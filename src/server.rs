use crate::mempool::Mempool;
use crate::{core::Core, index::Index};
use anyhow::{bail, ensure, Context, Result};
use bitcoin::{
    consensus::serialize,
    hashes::{sha256, sha256d, Hash},
    Txid,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn script(v: &Value) -> Result<[u8; 32]> {
    let mut b = hex::decode(v.as_str().context("missing scripthash")?)?;
    ensure!(b.len() == 32, "invalid scripthash");
    b.reverse();
    Ok(b.try_into().expect("checked length"))
}
fn number(v: &Value) -> Result<u32> {
    Ok(v.as_u64()
        .context("unsigned integer required")?
        .try_into()?)
}
fn status(index: &Index, mempool: &Mempool, s: &[u8; 32]) -> Result<Value> {
    let h = mempool.history(index, s)?;
    let rows = h.as_array().context("history")?;
    if rows.is_empty() {
        return Ok(Value::Null);
    }
    let mut data = String::new();
    for row in rows {
        data.push_str(&format!(
            "{}:{}:",
            row["tx_hash"].as_str().context("txid")?,
            row["height"]
        ));
    }
    Ok(json!(sha256::Hash::hash(data.as_bytes()).to_string()))
}
fn header(index: &Index) -> Result<Value> {
    let h = index.tip()?.context("index empty")?;
    Ok(json!({"height":h,"hex":hex::encode(serialize(&index.header(h)?))}))
}
pub fn dispatch(
    index: &Index,
    core: &Core,
    mempool: &Mempool,
    method: &str,
    p: &Value,
) -> Result<Value> {
    match method {
        "server.version" => {
            fn parse(v: &Value) -> Result<(u16, u16, u16)> {
                let parts = v
                    .as_str()
                    .context("protocol version must be a string")?
                    .split('.')
                    .map(str::parse::<u16>)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ensure!((2..=3).contains(&parts.len()), "invalid protocol version");
                Ok((parts[0], parts[1], parts.get(2).copied().unwrap_or(0)))
            }
            if !p[1].is_null() {
                let (min, max) = if p[1].is_array() {
                    (parse(&p[1][0])?, parse(&p[1][1])?)
                } else {
                    let v = parse(&p[1])?;
                    (v, v)
                };
                ensure!(
                    min <= (1, 4, 0) && (1, 4, 0) <= max,
                    "unsupported protocol: server supports only 1.4"
                );
            }
            Ok(json!([
                "electrum-lite 0.0.1-beta.1 thin-experimental",
                "1.4"
            ]))
        }
        "server.ping" => Ok(Value::Null),
        "server.banner" => Ok(json!(
            "Experimental local thin indexer. No production guarantees."
        )),
        "server.features" => Ok(
            json!({"genesis_hash":bitcoin::blockdata::constants::genesis_block(index.network).block_hash().to_string(),"protocol_min":"1.4","protocol_max":"1.4","hash_function":"sha256","hosts":{}}),
        ),
        "server.peers.subscribe" => Ok(json!([])),
        "server.health" => Ok(json!({"indexed_height":index.tip()?,"experimental":true})),
        "blockchain.headers.subscribe" => header(index),
        "blockchain.block.header" => {
            ensure!(
                p[1].as_u64().unwrap_or(0) == 0,
                "header checkpoints not implemented"
            );
            Ok(json!(hex::encode(serialize(
                &index.header(number(&p[0])?)?
            ))))
        }
        "blockchain.block.headers" => {
            let start = number(&p[0])?;
            let count = number(&p[1])?.min(2016);
            ensure!(
                p[2].as_u64().unwrap_or(0) == 0,
                "header checkpoints not implemented"
            );
            let end = start
                .saturating_add(count)
                .min(index.tip()?.context("empty")?.saturating_add(1));
            let mut bytes = Vec::new();
            for h in start..end {
                bytes.extend(serialize(&index.header(h)?));
            }
            Ok(json!({"count":end.saturating_sub(start),"hex":hex::encode(bytes),"max":2016}))
        }
        "blockchain.scripthash.get_history" => mempool.history(index, &script(&p[0])?),
        "blockchain.scripthash.subscribe" => status(index, mempool, &script(&p[0])?),
        "blockchain.scripthash.unsubscribe" => Ok(json!(true)),
        "blockchain.scripthash.listunspent" => mempool.unspent(index, &script(&p[0])?),
        "blockchain.scripthash.get_balance" => {
            let s = script(&p[0])?;
            let v = index.unspent(&s)?;
            let sum: u64 = v
                .as_array()
                .context("unspent")?
                .iter()
                .map(|x| x["value"].as_u64().unwrap_or(0))
                .sum();
            Ok(json!({"confirmed":sum,"unconfirmed":mempool.delta.get(&s).copied().unwrap_or(0)}))
        }
        "blockchain.scripthash.get_mempool" => Ok(json!(mempool
            .history
            .get(&script(&p[0])?)
            .cloned()
            .unwrap_or_default())),
        "blockchain.transaction.broadcast" => core.call("sendrawtransaction", json!([p[0]])),
        "blockchain.transaction.get" => {
            let txid: Txid = p[0].as_str().context("txid")?.parse()?;
            let tx = match mempool.raw.get(&txid) {
                Some(t) => t.clone(),
                None => index.transaction(core, txid)?,
            };
            let raw = hex::encode(serialize(&tx));
            if p[1].as_bool().unwrap_or(false) {
                let mut decoded = core.call("decoderawtransaction", json!([raw]))?;
                decoded["hex"] = json!(raw);
                if let Ok((h, _)) = index.locate(txid) {
                    let head = index.header(h)?;
                    decoded["blockhash"] = json!(head.block_hash().to_string());
                    decoded["confirmations"] =
                        json!(index.tip()?.context("empty index")?.saturating_sub(h) + 1);
                    decoded["blocktime"] = json!(head.time);
                    decoded["time"] = json!(head.time);
                }
                Ok(decoded)
            } else {
                Ok(json!(raw))
            }
        }
        "blockchain.transaction.get_merkle" | "blockchain.transaction.id_from_pos" => {
            let (h, pos) = if method.ends_with("id_from_pos") {
                (number(&p[0])?, number(&p[1])?)
            } else {
                let t: Txid = p[0].as_str().context("txid")?.parse()?;
                let found = index.locate_at(
                    t,
                    if p[1].is_null() {
                        None
                    } else {
                        Some(number(&p[1])?)
                    },
                )?;
                ensure!(
                    p[1].is_null() || number(&p[1])? == found.0,
                    "height mismatch"
                );
                found
            };
            let b = index.block_meta(h)?;
            let mut hashes: Vec<[u8; 32]> = b[80..].as_chunks::<32>().0.to_vec();
            ensure!((pos as usize) < hashes.len(), "position outside block");
            let txid = Txid::from_byte_array(hashes[pos as usize]).to_string();
            if method.ends_with("id_from_pos") && !p[2].as_bool().unwrap_or(false) {
                return Ok(json!(txid));
            }
            let mut n = pos as usize;
            let mut branch = Vec::new();
            while hashes.len() > 1 {
                if hashes.len() % 2 == 1 {
                    hashes.push(*hashes.last().expect("nonempty"));
                }
                branch.push(sha256d::Hash::from_byte_array(hashes[n ^ 1]).to_string());
                hashes = hashes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| {
                        let mut b = p[0].to_vec();
                        b.extend(p[1]);
                        sha256d::Hash::hash(&b).to_byte_array()
                    })
                    .collect();
                n /= 2;
            }
            if method.ends_with("id_from_pos") {
                Ok(json!({"tx_hash":txid,"merkle":branch}))
            } else {
                Ok(json!({"block_height":h,"pos":pos,"merkle":branch}))
            }
        }
        "blockchain.estimatefee" => {
            let r = core.call("estimatesmartfee", json!([p[0]]))?;
            Ok(r.get("feerate").cloned().unwrap_or(json!(-1)))
        }
        "blockchain.relayfee" => Ok(core.call("getnetworkinfo", json!([]))?["relayfee"].clone()),
        "mempool.get_fee_histogram" => mempool.fee_histogram(),
        _ => bail!("unsupported method"),
    }
}
struct Request {
    value: Value,
    reply: mpsc::SyncSender<Value>,
    internal: bool,
}

fn snapshot_destination(path: &Path, allowed: &Path) -> Result<u32> {
    let parent = path.parent().context("snapshot parent missing")?;
    ensure!(
        std::fs::canonicalize(parent)? == allowed,
        "snapshot must be inside the supervisor directory"
    );
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid snapshot name")?;
    let suffix = name
        .strip_prefix("snapshot-")
        .context("invalid snapshot name")?;
    let (height, stamp) = suffix.split_once('-').context("invalid snapshot name")?;
    let parsed_height = height.parse::<u32>()?;
    stamp.parse::<u128>()?;
    ensure!(
        height.bytes().all(|b| b.is_ascii_digit()) && stamp.bytes().all(|b| b.is_ascii_digit()),
        "invalid snapshot name"
    );
    ensure!(
        std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "snapshot destination must be NEW"
    );
    Ok(parsed_height)
}

#[cfg(unix)]
struct PrivateControl {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl PrivateControl {
    fn bind(path: PathBuf) -> Result<(Self, PathBuf)> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let private = path
            .parent()
            .context("control socket needs a private parent")?;
        let meta = std::fs::symlink_metadata(private)?;
        ensure!(
            meta.file_type().is_dir() && meta.mode() & 0o077 == 0,
            "control directory must be a real private directory"
        );
        let allowed = private.parent().context("supervisor directory missing")?;
        ensure!(
            std::fs::symlink_metadata(allowed)?.file_type().is_dir(),
            "supervisor directory must be real"
        );
        let allowed = std::fs::canonicalize(allowed)?;
        ensure!(
            std::fs::symlink_metadata(&path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "control socket path must be NEW"
        );
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let meta = std::fs::symlink_metadata(&path)?;
        Ok((
            Self {
                listener,
                path,
                device: meta.dev(),
                inode: meta.ino(),
            },
            allowed,
        ))
    }
}

#[cfg(unix)]
impl Drop for PrivateControl {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| {
            m.file_type().is_socket() && m.dev() == self.device && m.ino() == self.inode
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn private_request(
    mut stream: std::os::unix::net::UnixStream,
    tx: mpsc::SyncSender<Request>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = Vec::new();
    BufReader::new(stream.try_clone()?)
        .take(4097)
        .read_until(b'\n', &mut line)?;
    ensure!(
        line.len() <= 4096 && line.ends_with(b"\n"),
        "invalid private request"
    );
    let value: Value = serde_json::from_slice(&line)?;
    ensure!(
        value["method"] == "snapshot" && value["path"].is_string(),
        "invalid private request"
    );
    let (reply, rx) = mpsc::sync_channel(1);
    tx.try_send(Request {
        value,
        reply,
        internal: true,
    })
    .context("index worker queue full")?;
    let result = rx.recv_timeout(Duration::from_secs(120))?;
    serde_json::to_writer(&mut stream, &result)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn serve(
    index: Index,
    core: Core,
    listen: SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    control_socket: Option<PathBuf>,
) -> Result<()> {
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let (tx, rx) = mpsc::sync_channel::<Request>(64);
    #[cfg(unix)]
    let (control, snapshot_dir) = if let Some(path) = control_socket {
        let (control, allowed) = PrivateControl::bind(path)?;
        (Some(control), Some(allowed))
    } else {
        (None, None)
    };
    #[cfg(not(unix))]
    let snapshot_dir: Option<PathBuf> = {
        ensure!(
            control_socket.is_none(),
            "private supervisor control requires Unix"
        );
        None
    };
    let health = std::sync::Arc::new(std::sync::Mutex::new(
        json!({"pid":std::process::id(),"ready":false,"indexed_height":index.tip()?,"network":index.network.to_string(),"transport":if index.fetcher.proxy.is_some(){"socks5"}else{"direct"},"mempool_transactions":0,"experimental":true}),
    ));
    let published = health.clone();
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || -> Result<()> {
        let mut synced = Instant::now() - Duration::from_secs(2);
        let mut ready = false;
        let mut mempool = Mempool::default();
        while !worker_stop.load(std::sync::atomic::Ordering::Relaxed) {
            if synced.elapsed() >= Duration::from_secs(1) {
                ready = match index
                    .sync_batch_with_cancel(&core, 64, &worker_stop)
                    .and_then(|()| {
                        let h = index.tip()?.context("empty index")?;
                        ensure!(
                            core.call("getbestblockhash", json!([]))?
                                == index.header(h)?.block_hash().to_string(),
                            "catching up"
                        );
                        Mempool::load(&index, &core)
                    }) {
                    Ok(m) => {
                        mempool = m;
                        true
                    }
                    Err(e) => {
                        if e.to_string() == "catching up" {
                            eprintln!(
                                "{}",
                                crate::i18n::text(
                                    "Sync unavailable: catching up",
                                    "Sincronización no disponible: alcanzando a Bitcoin Core"
                                )
                            );
                        } else {
                            eprintln!(
                                "{}: {e:#}",
                                crate::i18n::text(
                                    "Sync unavailable",
                                    "Sincronización no disponible"
                                )
                            );
                        }
                        false
                    }
                };
                synced = if ready {
                    Instant::now()
                } else {
                    Instant::now() - Duration::from_secs(1)
                };
                let state = json!({"pid":std::process::id(),"ready":ready,"indexed_height":index.tip()?,"network":index.network.to_string(),"transport":if index.fetcher.proxy.is_some(){"socks5"}else{"direct"},"mempool_transactions":mempool.raw.len(),"experimental":true});
                *published
                    .lock()
                    .map_err(|_| anyhow::anyhow!("health publication poisoned"))? = state;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => {
                    let method = r.value["method"].as_str().unwrap_or("");
                    let params = &r.value["params"];
                    let result = (|| {
                        if r.internal {
                            ensure!(method == "snapshot", "unsupported private method");
                            ensure!(
                                ready && mempool.raw.is_empty(),
                                "snapshot requires a ready index and empty mempool"
                            );
                            let dir = snapshot_dir
                                .as_ref()
                                .context("private control unavailable")?;
                            let path =
                                PathBuf::from(r.value["path"].as_str().context("snapshot path")?);
                            let expected_height = snapshot_destination(&path, dir)?;
                            let height = index.tip()?.context("empty index")?;
                            ensure!(
                                height == expected_height,
                                "snapshot requested at a different height"
                            );
                            ensure!(
                                core.call("getbestblockhash", json!([]))?
                                    == index.header(height)?.block_hash().to_string(),
                                "Core tip changed before snapshot"
                            );
                            ensure!(
                                core.call("getrawmempool", json!([]))?
                                    .as_array()
                                    .is_some_and(|x| x.is_empty()),
                                "Core mempool changed before snapshot"
                            );
                            let (height, hash) = index.create_snapshot(&path)?;
                            return Ok(json!({"height":height,"hash":hash.to_string()}));
                        }
                        if method == "server.health" {
                            return Ok(
                                json!({"pid":std::process::id(),"ready":ready,"indexed_height":index.tip()?,"network":index.network.to_string(),"transport":if index.fetcher.proxy.is_some(){"socks5"}else{"direct"},"mempool_transactions":mempool.raw.len(),"experimental":true}),
                            );
                        }
                        ensure!(
                            ready || method.starts_with("server."),
                            "index not synchronized"
                        );
                        if method.starts_with("blockchain.scripthash.") {
                            let tip = index.tip()?.context("index empty")?;
                            ensure!(
                                core.call("getbestblockhash", json!([]))?
                                    == index.header(tip)?.block_hash().to_string(),
                                "chain changed; retry"
                            );
                            let ids = core.call("getrawmempool", json!([]))?;
                            let ids: std::collections::BTreeSet<String> = ids
                                .as_array()
                                .context("mempool")?
                                .iter()
                                .map(|v| v.as_str().unwrap_or("").to_owned())
                                .collect();
                            ensure!(
                                ids == mempool.raw.keys().map(ToString::to_string).collect(),
                                "mempool changed; retry"
                            );
                        }
                        dispatch(&index, &core, &mempool, method, params)
                    })();
                    let response = match result {
                        Ok(v) => json!({"jsonrpc":"2.0","id":r.value["id"],"result":v}),
                        Err(e) => {
                            json!({"jsonrpc":"2.0","id":r.value["id"],"error":{"code":1,"message":e.to_string()}})
                        }
                    };
                    let _ = r.reply.send(response);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        index.db.flush_wal(true)?;
        Ok(())
    });
    eprintln!(
        "{} {listen}; {}",
        crate::i18n::text("Electrum listening on", "Electrum escuchando en"),
        crate::i18n::text("experimental thin-v1", "thin-v1 experimental")
    );
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(unix)]
    let private_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        #[cfg(unix)]
        if let Some(control) = &control {
            match control.listener.accept() {
                Ok((stream, _)) => {
                    if !private_active.swap(true, std::sync::atomic::Ordering::AcqRel) {
                        let flag = private_active.clone();
                        let tx = tx.clone();
                        thread::spawn(move || {
                            if let Err(e) = private_request(stream, tx) {
                                eprintln!("private control request failed: {e:#}");
                            }
                            flag.store(false, std::sync::atomic::Ordering::Release);
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if active.load(std::sync::atomic::Ordering::Relaxed) >= 32 {
            drop(stream);
            continue;
        }
        active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let active = active.clone();
        let tx = tx.clone();
        let health = health.clone();
        thread::spawn(move || {
            if let Err(e) = connection(stream, tx, health) {
                eprintln!(
                    "{}: {e}",
                    crate::i18n::text("Connection closed", "Conexión cerrada")
                );
            }
            active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        });
    }
    drop(tx);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("storage worker panicked"))??;
    Ok(())
}
fn connection(
    mut stream: TcpStream,
    tx: mpsc::SyncSender<Request>,
    health: std::sync::Arc<std::sync::Mutex<Value>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = Vec::new();
    let mut scripts: Vec<(Value, Value)> = Vec::new();
    let mut last_header = Value::Null;
    fn ask(tx: &mpsc::SyncSender<Request>, v: Value) -> Result<Value> {
        let (a, b) = mpsc::sync_channel(1);
        tx.send(Request {
            value: v,
            reply: a,
            internal: false,
        })?;
        Ok(b.recv_timeout(Duration::from_secs(120))?)
    }
    loop {
        let read = reader.fill_buf();
        match read {
            Ok([]) => return Ok(()),
            Ok(b) => {
                let n = b
                    .iter()
                    .position(|c| *c == b'\n')
                    .map_or(b.len(), |n| n + 1);
                line.extend_from_slice(&b[..n]);
                reader.consume(n);
                ensure!(line.len() <= 8_100_000, "message too large");
                if !line.ends_with(b"\n") {
                    continue;
                }
                let v: Value = serde_json::from_slice(&line)?;
                line.clear();
                let batch = v.is_array();
                let requests = if batch {
                    v.as_array().context("batch")?.clone()
                } else {
                    vec![v]
                };
                ensure!(
                    !requests.is_empty() && requests.len() <= 100,
                    "batch size must be 1..100"
                );
                let mut responses = Vec::new();
                for v in requests {
                    ensure!(
                        v.is_object()
                            && v["method"].is_string()
                            && (v["params"].is_array() || v["params"].is_null()),
                        "invalid JSON-RPC request"
                    );
                    let response = if v["method"] == "server.health" {
                        json!({"jsonrpc":"2.0","id":v["id"],"result":health.lock().map_err(|_|anyhow::anyhow!("health publication poisoned"))?.clone()})
                    } else {
                        ask(&tx, v.clone())?
                    };
                    if v["method"] == "blockchain.scripthash.subscribe"
                        && response["error"].is_null()
                    {
                        ensure!(scripts.len() < 1000, "subscription limit");
                        scripts.retain(|(s, _)| s != &v["params"][0]);
                        scripts.push((v["params"][0].clone(), response["result"].clone()));
                    }
                    if v["method"] == "blockchain.scripthash.unsubscribe" {
                        scripts.retain(|(s, _)| s != &v["params"][0]);
                    }
                    if v["method"] == "blockchain.headers.subscribe" {
                        last_header = response["result"].clone();
                    }
                    if v.get("id").is_some() {
                        responses.push(response);
                    }
                }
                if !responses.is_empty() {
                    let response = if batch {
                        json!(responses)
                    } else {
                        responses.remove(0)
                    };
                    serde_json::to_writer(&mut stream, &response)?;
                    stream.write_all(b"\n")?;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !last_header.is_null() {
                    let r = ask(
                        &tx,
                        json!({"method":"blockchain.headers.subscribe","params":[],"id":0}),
                    )?;
                    if r["error"].is_null() && r["result"] != last_header {
                        last_header = r["result"].clone();
                        serde_json::to_writer(
                            &mut stream,
                            &json!({"jsonrpc":"2.0","method":"blockchain.headers.subscribe","params":[last_header]}),
                        )?;
                        stream.write_all(b"\n")?;
                    }
                }
                for (s, last) in &mut scripts {
                    let r = ask(
                        &tx,
                        json!({"method":"blockchain.scripthash.subscribe","params":[s],"id":0}),
                    )?;
                    if r["error"].is_null() && r["result"] != *last {
                        *last = r["result"].clone();
                        serde_json::to_writer(
                            &mut stream,
                            &json!({"jsonrpc":"2.0","method":"blockchain.scripthash.subscribe","params":[s,last]}),
                        )?;
                        stream.write_all(b"\n")?;
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}
