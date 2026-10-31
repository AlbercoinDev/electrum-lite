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

#[derive(Clone)]
struct PreparedVerification {
    snapshot: PathBuf,
    audit: PathBuf,
    backup: PathBuf,
    height: u32,
    hash: bitcoin::BlockHash,
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

fn prepared_generations(value: &Value, allowed: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let mut paths = Vec::new();
    for (field, prefix) in [
        ("snapshot", "snapshot-"),
        ("audit", "audit-"),
        ("backup", "backup-"),
    ] {
        let path = PathBuf::from(value[field].as_str().context("prepared path missing")?);
        ensure!(
            std::fs::symlink_metadata(&path)?.file_type().is_dir(),
            "prepared generation must be a real directory"
        );
        let canonical = std::fs::canonicalize(&path)?;
        ensure!(
            canonical.parent() == Some(allowed),
            "prepared generation outside supervisor directory"
        );
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid generation name")?;
        let suffix = name
            .strip_prefix(prefix)
            .context("invalid generation name")?;
        let (height, stamp) = suffix.split_once('-').context("invalid generation name")?;
        ensure!(
            !height.is_empty()
                && height.bytes().all(|b| b.is_ascii_digit())
                && !stamp.is_empty()
                && stamp.bytes().all(|b| b.is_ascii_digit()),
            "invalid generation name"
        );
        height.parse::<u32>()?;
        stamp.parse::<u128>()?;
        paths.push((canonical, suffix.to_owned()));
    }
    ensure!(
        paths[0].1 == paths[1].1 && paths[1].1 == paths[2].1,
        "prepared generation names differ"
    );
    Ok((paths[0].0.clone(), paths[1].0.clone(), paths[2].0.clone()))
}

fn check_expected_core_tip(core: &Core, request: &Value) -> Result<()> {
    if let Some(expected) = request.get("expected_tip") {
        let expected: bitcoin::BlockHash = expected
            .as_str()
            .context("invalid expected Core tip")?
            .parse()?;
        let current = core.call("getbestblockhash", json!([]))?;
        let current: bitcoin::BlockHash = current.as_str().context("invalid Core tip")?.parse()?;
        if current != expected {
            return Err(crate::index::AuditTipChanged.into());
        }
    }
    Ok(())
}

fn verify_prune_handoff(
    proof: &Path,
    allowed: &Path,
    snapshot: &Path,
    audit: &Path,
    backup: &Path,
) -> Result<()> {
    let private = proof.parent().context("prepared proof parent missing")?;
    ensure!(
        std::fs::symlink_metadata(private)?.file_type().is_dir()
            && std::fs::canonicalize(private)?.parent() == Some(allowed),
        "prepared proof directory invalid"
    );
    let meta = std::fs::symlink_metadata(proof)?;
    ensure!(
        meta.file_type().is_file() && meta.len() <= 16384,
        "prepared proof invalid"
    );
    let expected = format!(
        "snapshot={}\naudit={}\nbackup={}\n",
        snapshot.display(),
        audit.display(),
        backup.display()
    );
    ensure!(
        std::fs::read_to_string(proof)? == expected,
        "prepared proof mismatch"
    );
    Ok(())
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
        matches!(
            value["method"].as_str(),
            Some(
                "snapshot"
                    | "verify-prepared"
                    | "verification-status"
                    | "release-verification"
                    | "prune-prepared"
            )
        ),
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
        let index = std::sync::Arc::new(index);
        let mut synced = Instant::now() - Duration::from_secs(2);
        let mut ready = false;
        let mut sync_warning: Option<Instant> = None;
        let mut mempool = Mempool::default();
        let mut verification: Option<mpsc::Receiver<Result<(u32, bitcoin::BlockHash)>>> = None;
        let mut verification_hold: Option<Instant> = None;
        let mut prepared: Option<PreparedVerification> = None;
        let mut pruning: Option<mpsc::Receiver<Result<Value>>> = None;
        let mut prune_failed = false;
        let mut verification_status = json!({"state":"idle"});
        while !worker_stop.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(pending) = &verification {
                match pending.try_recv() {
                    Ok(Ok((height, hash))) => {
                        // This experimental lease does not authorize pruning.
                        // It expires if the private controller disappears.
                        verification_status = json!({"state":"verified", "height":height, "hash":hash.to_string(), "prune_authorized":false, "lease_seconds":30});
                        verification = None;
                        verification_hold = Some(Instant::now() + Duration::from_secs(30));
                    }
                    Ok(Err(error)) => {
                        verification_status =
                            json!({"state":"failed", "error":format!("{error:#}")});
                        verification = None;
                        prepared = None;
                        ready = false;
                        synced = Instant::now() - Duration::from_secs(1);
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        verification_status =
                            json!({"state":"failed", "error":"verification worker stopped"});
                        verification = None;
                        prepared = None;
                        ready = false;
                        synced = Instant::now() - Duration::from_secs(1);
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if verification_hold.is_some_and(|deadline| Instant::now() >= deadline) {
                verification_hold = None;
                prepared = None;
                verification_status = json!({"state":"expired", "prune_authorized":false});
                ready = false;
                synced = Instant::now() - Duration::from_secs(1);
            }
            if let Some(pending) = &pruning {
                match pending.try_recv() {
                    Ok(Ok(result)) => {
                        verification_status = json!({"state":"pruned", "result":result});
                        pruning = None;
                        prepared = None;
                        ready = false;
                        synced = Instant::now() - Duration::from_secs(1);
                    }
                    Ok(Err(error)) => {
                        verification_status =
                            json!({"state":"prune-failed", "error":format!("{error:#}")});
                        pruning = None;
                        prepared = None;
                        prune_failed = true;
                        ready = false;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        verification_status =
                            json!({"state":"prune-failed", "error":"prune worker stopped"});
                        pruning = None;
                        prepared = None;
                        prune_failed = true;
                        ready = false;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if verification.is_none()
                && verification_hold.is_none()
                && pruning.is_none()
                && !prune_failed
                && synced.elapsed() >= Duration::from_secs(1)
            {
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
                        if sync_warning.take().is_some() {
                            eprintln!(
                                "{}",
                                crate::i18n::text(
                                    "Synchronization available again",
                                    "Sincronización disponible de nuevo"
                                )
                            );
                        }
                        true
                    }
                    Err(e) => {
                        if e.to_string() == "catching up" {
                            let heights = (|| -> Result<(u64, u64)> {
                                let indexed = u64::from(index.tip()?.context("empty index")?);
                                let info = core.call("getblockchaininfo", json!([]))?;
                                let core_height =
                                    info["blocks"].as_u64().context("missing Core height")?;
                                Ok((indexed, core_height))
                            })();
                            match heights {
                                Ok((indexed, core_height)) => {
                                    let lag = core_height.saturating_sub(indexed);
                                    if (lag > 100 || core_height <= indexed)
                                        && sync_warning
                                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
                                    {
                                        if core_height > indexed {
                                            eprintln!(
                                                "{}: {lag} {}",
                                                crate::i18n::text("Bitcoin Core is ahead by", "Bitcoin Core va por delante"),
                                                crate::i18n::text("blocks", "bloques")
                                            );
                                        } else {
                                            eprintln!(
                                                "{}",
                                                crate::i18n::text("Synchronization unavailable: chain tips differ; checking for a reorganization", "Sincronización no disponible: las puntas de cadena difieren; comprobando posible reorganización")
                                            );
                                        }
                                        sync_warning = Some(Instant::now());
                                    }
                                }
                                Err(error) => eprintln!(
                                    "{}: {error:#}",
                                    crate::i18n::text("Sync unavailable", "Sincronización no disponible")
                                ),
                            }
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
                            if method == "verification-status" {
                                return Ok(verification_status.clone());
                            }
                            if method == "release-verification" {
                                ensure!(
                                    verification.is_none() && pruning.is_none() && !prune_failed,
                                    "verification or prune still running or failed"
                                );
                                ensure!(
                                    verification_hold.take().is_some(),
                                    "no verification lease"
                                );
                                verification_status =
                                    json!({"state":"released", "prune_authorized":false});
                                prepared = None;
                                ready = false;
                                synced = Instant::now() - Duration::from_secs(1);
                                return Ok(verification_status.clone());
                            }
                            if method == "prune-prepared" {
                                ensure!(
                                    verification.is_none() && pruning.is_none() && !prune_failed,
                                    "verification or prune unavailable"
                                );
                                let deadline =
                                    verification_hold.context("no verification lease")?;
                                ensure!(Instant::now() < deadline, "verification lease expired");
                                let state = prepared
                                    .as_ref()
                                    .context("prepared verification missing")?
                                    .clone();
                                ensure!(
                                    index.network != bitcoin::Network::Bitcoin,
                                    "mainnet pruning is blocked"
                                );
                                ensure!(
                                    state.height >= crate::TESTNET_PRUNE_MIN_HEIGHT,
                                    "Core test-network pruning requires at least 1000 blocks"
                                );
                                let dir = snapshot_dir
                                    .as_ref()
                                    .context("private control unavailable")?;
                                let root = dir.parent().context("supervisor data root missing")?;
                                ensure!(
                                    dir.file_name().and_then(|n| n.to_str()) == Some("supervisor")
                                        && std::fs::canonicalize(
                                            index.cache.parent().context("index cache parent")?
                                        )? == root,
                                    "prepared prune requires the managed index root"
                                );
                                let proof = PathBuf::from(
                                    r.value["proof"]
                                        .as_str()
                                        .context("prepared proof missing")?,
                                );
                                verify_prune_handoff(
                                    &proof,
                                    dir,
                                    &state.snapshot,
                                    &state.audit,
                                    &state.backup,
                                )?;
                                ensure!(
                                    index.tip()? == Some(state.height)
                                        && index.header(state.height)?.block_hash() == state.hash,
                                    "index changed after verification"
                                );
                                core.verify_prune_preconditions(state.height, state.hash)?;
                                ensure!(
                                    Instant::now() < deadline,
                                    "verification lease expired before prune"
                                );
                                let (done, pending) = mpsc::sync_channel(1);
                                let frozen = index.clone();
                                let core = core.clone();
                                let stop = worker_stop.clone();
                                let root = root.to_path_buf();
                                verification_hold = None;
                                pruning = Some(pending);
                                verification_status =
                                    json!({"state":"pruning", "height":state.height});
                                thread::spawn(move || {
                                    let result = (|| -> Result<Value> {
                                        ensure!(
                                            frozen.verify_prepared_audit(
                                                &state.snapshot,
                                                &state.audit
                                            )? == (state.height, state.hash),
                                            "prepared state changed before prune"
                                        );
                                        ensure!(
                                            frozen.verify_snapshot_matches(&state.backup)?
                                                == (state.height, state.hash),
                                            "prepared backup changed before prune"
                                        );
                                        crate::prune::commit_verified_prune(
                                            &frozen,
                                            &core,
                                            &root,
                                            &state.audit,
                                            &state.backup,
                                            &stop,
                                        )
                                    })();
                                    let _ = done.send(result);
                                });
                                return Ok(verification_status.clone());
                            }
                            if method == "verify-prepared" {
                                ensure!(
                                    verification.is_none()
                                        && verification_hold.is_none()
                                        && pruning.is_none()
                                        && !prune_failed,
                                    "prepared verification already running"
                                );
                                check_expected_core_tip(&core, &r.value)?;
                                ensure!(ready && mempool.raw.is_empty(), "prepared verification requires a ready index and empty mempool");
                                let dir = snapshot_dir
                                    .as_ref()
                                    .context("private control unavailable")?;
                                let (snapshot, audit, backup) =
                                    prepared_generations(&r.value, dir)?;
                                let height = index.tip()?.context("empty index")?;
                                let named_height: u32 = snapshot
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .and_then(|name| name.strip_prefix("snapshot-"))
                                    .and_then(|name| name.split_once('-'))
                                    .context("invalid snapshot name")?
                                    .0
                                    .parse()?;
                                ensure!(
                                    named_height == height,
                                    "prepared generation height differs from index"
                                );
                                let hash = index.header(height)?.block_hash();
                                ensure!(
                                    core.call("getbestblockhash", json!([]))? == hash.to_string(),
                                    "Core tip changed before verification"
                                );
                                ensure!(
                                    core.call("getrawmempool", json!([]))?
                                        .as_array()
                                        .is_some_and(|x| x.is_empty()),
                                    "mempool not empty before verification"
                                );
                                let (done, pending) = mpsc::sync_channel(1);
                                let frozen = index.clone();
                                let core = core.clone();
                                prepared = Some(PreparedVerification {
                                    snapshot: snapshot.clone(),
                                    audit: audit.clone(),
                                    backup: backup.clone(),
                                    height,
                                    hash,
                                });
                                verification_status = json!({"state":"running"});
                                verification = Some(pending);
                                *published.lock().map_err(|_| {
                                    anyhow::anyhow!("health publication poisoned")
                                })? = json!({"pid":std::process::id(),"ready":false,"indexed_height":index.tip()?,"network":index.network.to_string(),"transport":if index.fetcher.proxy.is_some(){"socks5"}else{"direct"},"mempool_transactions":mempool.raw.len(),"experimental":true,"verification_in_progress":true});
                                thread::spawn(move || {
                                    let result = (|| -> Result<(u32, bitcoin::BlockHash)> {
                                        let verified =
                                            frozen.verify_prepared_audit(&snapshot, &audit)?;
                                        ensure!(
                                            verified == (height, hash),
                                            "live index changed during verification"
                                        );
                                        ensure!(
                                            frozen.verify_snapshot_matches(&backup)? == verified,
                                            "backup differs from verified state"
                                        );
                                        ensure!(
                                            core.call("getbestblockhash", json!([]))?
                                                == hash.to_string(),
                                            "Core tip changed during verification"
                                        );
                                        ensure!(
                                            core.call("getrawmempool", json!([]))?
                                                .as_array()
                                                .is_some_and(|x| x.is_empty()),
                                            "mempool changed during verification"
                                        );
                                        Ok(verified)
                                    })();
                                    let _ = done.send(result);
                                });
                                return Ok(verification_status.clone());
                            }
                            ensure!(method == "snapshot", "unsupported private method");
                            ensure!(
                                verification.is_none()
                                    && verification_hold.is_none()
                                    && pruning.is_none()
                                    && !prune_failed,
                                "prepared verification in progress"
                            );
                            check_expected_core_tip(&core, &r.value)?;
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
                        ensure!(
                            method != "blockchain.transaction.broadcast"
                                || (verification.is_none()
                                    && verification_hold.is_none()
                                    && pruning.is_none()),
                            "broadcast temporarily unavailable during coordinated maintenance"
                        );
                        if (verification.is_some()
                            || verification_hold.is_some()
                            || pruning.is_some())
                            && !method.starts_with("server.")
                        {
                            let height = index.tip()?.context("index empty")?;
                            ensure!(
                                core.call("getbestblockhash", json!([]))?
                                    == index.header(height)?.block_hash().to_string(),
                                "chain changed; retry"
                            );
                        }
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
                            let code = if r.internal
                                && e.downcast_ref::<crate::index::AuditTipChanged>().is_some()
                            {
                                i64::from(crate::AUDIT_STALE_EXIT_CODE)
                            } else {
                                1
                            };
                            json!({"jsonrpc":"2.0","id":r.value["id"],"error":{"code":code,"message":e.to_string()}})
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
