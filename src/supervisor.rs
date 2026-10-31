//! Supervisor for an explicitly dedicated Core. Never adopts an existing process.
use crate::{core::Core, core_blockfile};
use anyhow::{bail, ensure, Context, Result};
use bitcoin::Network;

// If Core did not delete anything (for example because of an internal prune
// lock), retry unchanged candidates after a bounded delay, not on every block.
const NO_DELETION_RETRY_BLOCKS: u32 = 144;
const NO_DELETION_RETRY_TIME: Duration = Duration::from_secs(15 * 60);

fn no_deletion_retry_due(same_file: bool, tip: u32, attempted_tip: u32, elapsed: Duration) -> bool {
    !same_file
        || tip < attempted_tip
        || tip - attempted_tip >= NO_DELETION_RETRY_BLOCKS
        || elapsed >= NO_DELETION_RETRY_TIME
}

// GNU df reports space available to the current user, including filesystem
// reservations. Errors are fatal: continuing without the guard is unsafe.
fn available_bytes(path: &Path) -> Result<u64> {
    let output = Command::new("df")
        .env("LC_ALL", "C")
        .args(["-B1", "--output=avail", "--"])
        .arg(path)
        .output()
        .context("cannot query free disk space")?;
    ensure!(output.status.success(), "cannot query free disk space");
    let text = std::str::from_utf8(&output.stdout)?;
    let mut lines = text.lines();
    ensure!(lines.next().is_some(), "missing disk space header");
    let free = lines
        .next()
        .context("missing disk space value")?
        .trim()
        .parse()?;
    ensure!(lines.next().is_none(), "unexpected disk space output");
    Ok(free)
}

fn pause_needed(paused: bool, lag: u64, high: u32, low: u32, free: u64, minimum: u64) -> bool {
    if paused {
        lag > u64::from(low) || free < minimum.saturating_add(512 * 1024 * 1024)
    } else {
        lag >= u64::from(high) || free < minimum
    }
}

fn current_core_tip(core: &Core) -> Result<bitcoin::BlockHash> {
    let value = core.call("getbestblockhash", json!([]))?;
    value
        .as_str()
        .context("Core current tip missing")?
        .parse()
        .context("invalid Core current tip")
}
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub struct Config {
    pub network: String,
    pub data_dir: PathBuf,
    pub core_dir: PathBuf,
    pub bitcoind: PathBuf,
    pub core_rpc: String,
    pub listen: SocketAddr,
    pub p2p_port: u16,
    pub core_dbcache_mib: u32,
    pub high: u32,
    pub low: u32,
    pub min_free_mib: u32,
    pub auto_prune: bool,
    pub prune_interval: u32,
    pub peers: Vec<String>,
    pub proxy: Option<String>,
}
struct OwnedChild(Child);
impl OwnedChild {
    fn spawn(c: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        Ok(Self(c.stdin(Stdio::null()).spawn()?))
    }
    fn live(&mut self) -> Result<()> {
        ensure!(
            self.0.try_wait()?.is_none(),
            "owned child exited unexpectedly"
        );
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        if let Some(status) = self.0.try_wait()? {
            ensure!(status.success(), "child failed: {status}");
            return Ok(());
        }
        #[cfg(unix)]
        {
            ensure!(
                Command::new("kill")
                    .args(["-TERM", &self.0.id().to_string()])
                    .status()?
                    .success(),
                "cannot signal owned child"
            );
        }
        #[cfg(not(unix))]
        self.0.kill()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(45) {
            if let Some(status) = self.0.try_wait()? {
                ensure!(
                    status.success(),
                    "child did not shut down cleanly: {status}"
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("owned child shutdown timed out; maintenance aborted")
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() && self.stop().is_err() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
fn durable_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::File::open(path.parent().context("parent")?)?.sync_all()?;
    Ok(())
}
fn event(path: &Path, name: &str, extra: Value) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    writeln!(f, "{}", json!({"event":name,"details":extra}))?;
    f.sync_all()?;
    fs::File::open(path.parent().context("event parent")?)?.sync_all()?;
    let human = match name {
        "core_owned" => crate::i18n::text("dedicated Core started", "Core dedicado iniciado"),
        "backpressure_pause" => crate::i18n::text(
            "Core network paused while indexing catches up",
            "red de Core pausada mientras el índice se pone al día",
        ),
        "backpressure_resume" => crate::i18n::text("Core network resumed", "red de Core reanudada"),
        "maintenance_offline" => crate::i18n::text(
            "offline maintenance started",
            "mantenimiento sin red iniciado",
        ),
        "maintenance_complete" => {
            crate::i18n::text("maintenance completed", "mantenimiento finalizado")
        }
        "retention_complete" => crate::i18n::text(
            "old maintenance generations removed",
            "generaciones antiguas de mantenimiento eliminadas",
        ),
        "retention_skipped" => crate::i18n::text(
            "retention skipped; previous generations preserved",
            "limpieza omitida; generaciones anteriores conservadas",
        ),
        "electrum_resuming" => crate::i18n::text(
            "Electrum restarting before maintenance cleanup",
            "Electrum reiniciándose antes de la limpieza de mantenimiento",
        ),
        "maintenance_waiting" => crate::i18n::text(
            "maintenance postponed: no eligible closed block file found",
            "mantenimiento aplazado: no hay archivos cerrados aptos para podar",
        ),
        "maintenance_stale" => crate::i18n::text(
            "maintenance postponed: Core chain changed during preparation",
            "mantenimiento aplazado: la cadena de Core cambió durante la preparación",
        ),
        "shutdown" => crate::i18n::text("shutdown complete", "cierre completado"),
        _ => name,
    };
    eprintln!("supervisor: {human} {extra}");
    Ok(())
}
fn health(listen: SocketAddr) -> Result<Value> {
    let mut s = TcpStream::connect_timeout(&listen, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    s.write_all(b"{\"id\":1,\"method\":\"server.health\",\"params\":[]}\n")?;
    let mut line = String::new();
    BufReader::new(s).take(65537).read_line(&mut line)?;
    ensure!(line.len() < 65536, "health response limit");
    let r: Value = serde_json::from_str(&line)?;
    ensure!(r["error"].is_null(), "health request failed");
    Ok(r["result"].clone())
}

#[cfg(unix)]
fn private_call(control: &Path, request: Value) -> Result<Value> {
    use std::os::unix::net::UnixStream;
    let mut stream =
        UnixStream::connect(control).context("private snapshot control unavailable")?;
    stream.set_read_timeout(Some(Duration::from_secs(130)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(65537)
        .read_line(&mut response)?;
    ensure!(response.len() < 65536, "private response too large");
    let response: Value = serde_json::from_str(&response)?;
    if response["error"]["code"].as_i64() == Some(i64::from(crate::AUDIT_STALE_EXIT_CODE)) {
        return Err(crate::index::AuditTipChanged.into());
    }
    ensure!(
        response.get("error").is_none_or(Value::is_null),
        "private request failed: {}",
        response["error"]
    );
    Ok(response
        .get("result")
        .context("private response missing result")?
        .clone())
}

#[cfg(not(unix))]
fn private_call(_control: &Path, _request: Value) -> Result<Value> {
    bail!("private supervisor control requires Unix")
}

fn request_snapshot(
    control: &Path,
    snapshot: &Path,
    expected_height: u32,
    expected_hash: bitcoin::BlockHash,
) -> Result<bitcoin::BlockHash> {
    let result = private_call(
        control,
        json!({"method":"snapshot", "path":snapshot, "expected_tip":expected_hash.to_string()}),
    )?;
    ensure!(
        result["height"].as_u64() == Some(u64::from(expected_height)),
        "snapshot height mismatch"
    );
    let hash: bitcoin::BlockHash = result["hash"]
        .as_str()
        .context("snapshot hash missing")?
        .parse()?;
    ensure!(
        hash == expected_hash,
        "snapshot hash differs from requested tip"
    );
    Ok(hash)
}

impl Config {
    fn base_command(&self, cookie: &Path) -> Result<Command> {
        let mut c = Command::new(std::env::current_exe()?);
        c.args(["--network", &self.network, "--data-dir"])
            .arg(&self.data_dir)
            .arg("--core-cookie")
            .arg(cookie)
            .args([
                "--core-rpc",
                &self.core_rpc,
                "--listen",
                &self.listen.to_string(),
            ]);
        for peer in &self.peers {
            c.args(["--block-peer", peer]);
        }
        if let Some(proxy) = &self.proxy {
            c.args(["--socks-proxy", proxy]);
        }
        Ok(c)
    }
    fn server(&self, cookie: &Path, control: Option<&Path>) -> Result<OwnedChild> {
        let mut command = self.base_command(cookie)?;
        if let Some(control) = control {
            command.arg("--control-socket").arg(control);
        }
        OwnedChild::spawn(command.arg("serve"))
    }
}

/// Allocate a new private capability directory for this supervisor run.
/// Never adopt or remove an existing directory or socket after a crash.
#[cfg(unix)]
fn private_control_path(maintenance: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let name = format!("c-{:x}-{:x}", std::process::id(), stamp as u32);
    let private = fs::canonicalize(maintenance)?.join(name);
    let path = private.join("s");
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.as_os_str().as_bytes().len() < 108,
        "private control socket path too long; use a shorter data directory"
    );
    fs::DirBuilder::new().mode(0o700).create(&private)?;
    Ok(path)
}

#[cfg(not(unix))]
fn private_control_path(_maintenance: &Path) -> Result<PathBuf> {
    bail!("private supervisor control requires Unix")
}
pub fn run(mut c: Config, stop: Arc<AtomicBool>) -> Result<()> {
    ensure!(
        c.listen.ip().is_loopback(),
        "managed listener must be loopback; tunnel for remote access"
    );
    ensure!(
        c.high > c.low && c.high > 0 && c.prune_interval > 0,
        "invalid supervisor thresholds"
    );
    ensure!(
        (256..=1048576).contains(&c.min_free_mib),
        "invalid free-space reserve"
    );
    ensure!(
        (64..=1024).contains(&c.core_dbcache_mib),
        "Core database cache must be 64..=1024 MiB"
    );
    ensure!(
        !c.auto_prune || c.network != "mainnet",
        "mainnet automatic pruning is blocked"
    );
    ensure!(
        !c.core_dir.is_symlink(),
        "dedicated Core root symlink refused"
    );
    let core_dir = if c.core_dir.exists() {
        fs::canonicalize(&c.core_dir)?
    } else {
        fs::create_dir_all(&c.core_dir)?;
        fs::canonicalize(&c.core_dir)?
    };
    let rpc_address: SocketAddr = c
        .core_rpc
        .strip_prefix("http://")
        .context("loopback RPC URL required")?
        .trim_end_matches('/')
        .parse()?;
    ensure!(
        rpc_address.ip().is_loopback(),
        "Core RPC must bind loopback"
    );
    let root = c.data_dir.join(&c.network);
    ensure!(!root.is_symlink(), "data root symlink refused");
    fs::create_dir_all(&root)?;
    let absolute_root = fs::canonicalize(&root)?;
    ensure!(
        !absolute_root.starts_with(&core_dir) && !core_dir.starts_with(&absolute_root),
        "Core and Lite roots must be separate, not nested"
    );
    let identity = format!(
        "ELTHIN-DEDICATED-1\nnetwork={}\nrpc={}\np2p={}\nindex={}\n",
        c.network,
        rpc_address.port(),
        c.p2p_port,
        absolute_root.display()
    );
    let marker = core_dir.join("electrum-lite.owner");
    if marker.exists() {
        ensure!(
            fs::read_to_string(&marker)? == identity,
            "dedicated Core configuration mismatch"
        );
    } else {
        ensure!(
            fs::read_dir(&core_dir)?.next().is_none(),
            "refusing existing Core data without ownership marker"
        );
        durable_new(&marker, identity.as_bytes())?;
    }
    let conf = core_dir.join("electrum-lite.conf");
    let conf_bytes = b"# Dedicated Core owned by Electrum Lite; settings supplied on CLI.\n";
    if conf.exists() {
        ensure!(
            fs::read(&conf)? == conf_bytes,
            "managed Core configuration was modified"
        );
    } else {
        durable_new(&conf, conf_bytes)?;
    }
    if c.auto_prune {
        ensure!(
            fs::symlink_metadata(root.join("prune-pending"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "unfinished prune blocks automatic startup; preserve audit and recover"
        );
    }
    let maintenance = root.join("supervisor");
    ensure!(!maintenance.is_symlink(), "maintenance symlink refused");
    fs::create_dir_all(&maintenance)?;
    let maintenance = fs::canonicalize(&maintenance)?;
    let control = if c.auto_prune {
        Some(private_control_path(&maintenance)?)
    } else {
        None
    };
    let events = maintenance.join("events.jsonl");
    let network_dir = match c.network.as_str() {
        "mainnet" => core_dir.clone(),
        name => core_dir.join(name),
    };
    let cookie = network_dir.join(".cookie");
    let core = Core::new(c.core_rpc.clone(), cookie.clone())?;
    let pid = core_dir.join("electrum-lite.pid");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(maintenance.join("core.log"))?;
    let mut command = Command::new(&c.bitcoind);
    command
        .arg(format!("-datadir={}", core_dir.display()))
        .arg(format!("-conf={}", conf.display()))
        .arg(format!("-pid={}", pid.display()))
        .args([
            "-server=1",
            "-listen=1",
            "-daemon=0",
            "-nosettings",
            "-prune=1",
            "-txindex=0",
            "-listenonion=0",
            "-discover=0",
        ])
        .arg(format!("-dbcache={}", c.core_dbcache_mib))
        .arg(format!("-rpcport={}", rpc_address.port()))
        .arg(format!("-port={}", c.p2p_port))
        .arg(format!("-bind=127.0.0.1:{}", c.p2p_port))
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    if c.network != "mainnet" {
        command.arg(format!("-{}", c.network));
    }
    if c.network == "regtest" {
        command.args(["-connect=0", "-dnsseed=0", "-fallbackfee=0.00001"]);
    }
    let mut node = OwnedChild::spawn(&mut command)?;
    let startup = Instant::now();
    loop {
        node.live()?;
        ensure!(!stop.load(Ordering::Relaxed), "startup interrupted");
        ensure!(
            startup.elapsed() < Duration::from_secs(60),
            "dedicated Core startup timed out"
        );
        // Do not authenticate to or stop an old Core using the same port/root.
        if fs::read_to_string(&pid)
            .ok()
            .is_some_and(|p| p.trim() == node.0.id().to_string())
            && core.call("getblockchaininfo", json!([])).is_ok()
        {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    event(
        &events,
        "core_owned",
        json!({"pid":node.0.id(),"manual_pruning":true}),
    )?;
    let mut daemon = c.server(&cookie, control.as_deref())?;
    let started = Instant::now();
    let mut paused = false;
    let minimum_free = u64::from(c.min_free_mib) * 1024 * 1024;
    let mut free_bytes = available_bytes(&core_dir)?.min(available_bytes(&root)?);
    let mut free_checked = Instant::now();
    let mut last_maintenance = 0u32;
    for entry in fs::read_dir(&root)? {
        let name = entry?.file_name();
        if let Some(height) = name.to_string_lossy().strip_prefix("prune-complete-") {
            last_maintenance = last_maintenance.max(
                height
                    .parse::<u32>()
                    .context("invalid prune record filename")?,
            );
        }
    }
    let mut health_seen = false;
    let mut waiting_for_files = false;
    // Inspection is read-only and runs off the supervisor loop: Core may be
    // downloading while header heights are resolved. Only stable, closed files
    // are cached, and Core still decides whether deletion is permitted.
    let mut inspected = std::collections::BTreeMap::new();
    let mut unsuccessful = std::collections::BTreeMap::new();
    let mut inspection: Option<
        std::sync::mpsc::Receiver<Result<(String, core_blockfile::Fingerprint, u32)>>,
    > = None;
    let mut inspection_retry = Instant::now();
    let network = match c.network.as_str() {
        "mainnet" => Network::Bitcoin,
        "signet" => Network::Signet,
        "testnet4" => Network::Testnet4,
        "regtest" => Network::Regtest,
        _ => bail!("unsupported network"),
    };
    let discover_sources = c.peers.is_empty();
    while !stop.load(Ordering::Relaxed) {
        node.live()?;
        daemon.live()?;
        let state = match health(c.listen) {
            Ok(v) => {
                health_seen = true;
                v
            }
            Err(e) => {
                ensure!(
                    !health_seen && started.elapsed() < Duration::from_secs(60),
                    "Electrum health unavailable: {e}"
                );
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        ensure!(
            state["pid"].as_u64() == Some(u64::from(daemon.0.id())),
            "Electrum listener belongs to another process; refusing adoption"
        );
        let info = core.call("getblockchaininfo", json!([]))?;
        ensure!(
            info["pruned"] == true && info["automatic_pruning"] == false,
            "dedicated Core must stay in manual pruning mode"
        );
        let tip: u32 = info["blocks"].as_u64().context("Core height")?.try_into()?;
        if discover_sources {
            let peers = core.call("getpeerinfo", json!([]))?;
            for peer in peers.as_array().context("Core peer list")? {
                let services = peer["services"]
                    .as_str()
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
                    .unwrap_or(0);
                let Some(address) = peer["addr"].as_str() else {
                    continue;
                };
                if services & 9 == 9 && c.peers.len() < 8 && !c.peers.iter().any(|p| p == address) {
                    c.peers.push(address.to_owned());
                }
            }
        }
        let indexed = state["indexed_height"].as_u64();
        let lag = (u64::from(tip) + 1).saturating_sub(indexed.map_or(0, |n| n + 1));
        if free_checked.elapsed() >= Duration::from_secs(5) {
            free_bytes = available_bytes(&core_dir)?.min(available_bytes(&root)?);
            free_checked = Instant::now();
        }
        let should_pause = pause_needed(paused, lag, c.high, c.low, free_bytes, minimum_free);
        if !paused && should_pause {
            core.call("setnetworkactive", json!([false]))?;
            paused = true;
            event(
                &events,
                "backpressure_pause",
                json!({"lag":lag,"core_height":tip,"indexed_height":indexed,"free_bytes":free_bytes,"minimum_free_bytes":minimum_free,"reason":if free_bytes < minimum_free {"disk"} else {"lag"}}),
            )?;
        }
        if paused && !should_pause {
            core.call("setnetworkactive", json!([true]))?;
            paused = false;
            event(&events, "backpressure_resume", json!({"lag":lag}))?;
        }
        let maintenance_due = c.auto_prune
            && tip >= crate::TESTNET_PRUNE_MIN_HEIGHT
            && tip.saturating_sub(last_maintenance) >= c.prune_interval
            && lag == 0
            && state["ready"] == true
            && state["mempool_transactions"] == 0;
        if let Some(rx) = &inspection {
            match rx.try_recv() {
                Ok(Ok((name, fingerprint, height))) => {
                    inspected.insert(name, (fingerprint, height));
                    inspection = None;
                }
                Ok(Err(e)) => {
                    eprintln!("block-file inspection unavailable: {e:#}");
                    inspection_retry = Instant::now() + Duration::from_secs(30);
                    inspection = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!("block-file inspection worker stopped unexpectedly");
                    inspection_retry = Instant::now() + Duration::from_secs(30);
                    inspection = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let block_dir = network_dir.join("blocks");
        let mut eligible = Vec::new();
        if maintenance_due {
            let files = block_files(&block_dir)?;
            // Core does not prune its current (highest numbered) file. Ignore
            // all unexpected names rather than guessing their ordering.
            let mut closed: Vec<_> = files
                .keys()
                .map(|name| {
                    let digits = name
                        .strip_prefix("blk")
                        .and_then(|n| n.strip_suffix(".dat"))
                        .context("invalid Core block filename")?;
                    ensure!(
                        digits.len() >= 5 && digits.bytes().all(|b| b.is_ascii_digit()),
                        "invalid Core block filename"
                    );
                    Ok((digits.parse::<u32>()?, name.clone()))
                })
                .collect::<Result<Vec<_>>>()?;
            closed.sort_unstable();
            closed.pop(); // last/current file is never an eligible candidate
            for (_, name) in &closed {
                let path = block_dir.join(name);
                let fp = core_blockfile::file_fingerprint(&path)?;
                if let Some((old, height)) = inspected.get(name) {
                    if *old == fp && core_blockfile::eligible(*height, tip) {
                        let retry = unsuccessful.get(name).is_none_or(
                            |(previous, previous_tip, previous_time): &(
                                core_blockfile::Fingerprint,
                                u32,
                                Instant,
                            )| {
                                no_deletion_retry_due(
                                    *previous == fp,
                                    tip,
                                    *previous_tip,
                                    previous_time.elapsed(),
                                )
                            },
                        );
                        if retry {
                            eligible.push((name.clone(), fp));
                        }
                    }
                }
            }
            if eligible.is_empty() && inspection.is_none() && Instant::now() >= inspection_retry {
                for (_, name) in &closed {
                    let path = block_dir.join(name);
                    let fp = core_blockfile::file_fingerprint(&path)?;
                    if inspected.get(name).is_some_and(|(old, _)| *old == fp) {
                        continue;
                    }
                    let key_path = block_dir.join("xor.dat");
                    let name = name.clone();
                    let core = core.clone();
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    thread::spawn(move || {
                        let result = (|| {
                            let key = core_blockfile::xor_key(&fs::read(key_path)?)?;
                            let (fp, height) =
                                core_blockfile::inspect_file(&path, network, &key, |hashes| {
                                    core.header_heights(hashes)
                                })?;
                            Ok((name, fp, height))
                        })();
                        let _ = tx.send(result);
                    });
                    inspection = Some(rx);
                    break;
                }
            }
        }
        if maintenance_due && eligible.is_empty() && !waiting_for_files {
            event(&events, "maintenance_waiting", json!({"height":tip}))?;
            waiting_for_files = true;
        }
        if maintenance_due && !eligible.is_empty() {
            waiting_for_files = false;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let snapshot = maintenance.join(format!("snapshot-{tip}-{stamp}"));
            let audit = maintenance.join(format!("audit-{tip}-{stamp}"));
            let backup = maintenance.join(format!("backup-{tip}-{stamp}"));
            let control = control.as_deref().context("private control missing")?;
            event(&events, "audit_preparing", json!({"height":tip}))?;
            let expected_hash: bitcoin::BlockHash = info["bestblockhash"]
                .as_str()
                .context("Core best hash missing")?
                .parse()
                .context("invalid Core best hash")?;
            let snapshot_hash = match request_snapshot(control, &snapshot, tip, expected_hash) {
                Ok(hash) => hash,
                Err(error)
                    if error
                        .downcast_ref::<crate::index::AuditTipChanged>()
                        .is_some() =>
                {
                    ensure!(
                        current_core_tip(&core)? != expected_hash,
                        "snapshot reported a changed chain but Core tip did not change"
                    );
                    ensure!(
                        fs::symlink_metadata(&snapshot)
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                        "failed snapshot exists; preserve for inspection"
                    );
                    event(
                        &events,
                        "maintenance_stale",
                        json!({"snapshot_height":tip,"phase":"snapshot"}),
                    )?;
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut auditing = OwnedChild::spawn(
                c.base_command(&cookie)?
                    .arg("--snapshot-dir")
                    .arg(&snapshot)
                    .arg("--audit-dir")
                    .arg(&audit)
                    .arg("audit"),
            )?;
            event(
                &events,
                "audit_started",
                json!({"height":tip,"pid":auditing.0.id(),"snapshot":snapshot,"audit":audit}),
            )?;
            let mut reserve_checked = Instant::now();
            let audit_status = loop {
                if let Some(status) = auditing.0.try_wait()? {
                    break status;
                }
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "online audit interrupted; partial generations preserved"
                );
                node.live()?;
                daemon.live()?;
                if reserve_checked.elapsed() >= Duration::from_secs(5) {
                    let free = available_bytes(&core_dir)?.min(available_bytes(&root)?);
                    ensure!(
                        free >= minimum_free,
                        "disk reserve exhausted during online audit; generations preserved"
                    );
                    reserve_checked = Instant::now();
                }
                thread::sleep(Duration::from_millis(100));
            };
            if audit_status.code() == Some(i32::from(crate::AUDIT_STALE_EXIT_CODE)) {
                let current = current_core_tip(&core)?;
                ensure!(
                    current != snapshot_hash,
                    "audit reported a changed chain but Core tip did not change"
                );
                crate::retention::discard_stale_preparation(
                    &root,
                    &maintenance,
                    &snapshot,
                    &audit,
                    false,
                )?;
                event(&events, "maintenance_stale", json!({"snapshot_height":tip}))?;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            ensure!(
                audit_status.success(),
                "online audit failed; generations preserved and no pruning performed"
            );
            event(&events, "audit_prepared", json!({"height":tip}))?;
            ensure!(
                available_bytes(&core_dir)?.min(available_bytes(&root)?) >= minimum_free,
                "disk reserve exhausted before online backup; generations preserved"
            );
            event(&events, "backup_preparing", json!({"height":tip}))?;
            let mut backing_up = OwnedChild::spawn(
                c.base_command(&cookie)?
                    .arg("--snapshot-dir")
                    .arg(&snapshot)
                    .arg("--backup-dir")
                    .arg(&backup)
                    .arg("backup"),
            )?;
            let mut reserve_checked = Instant::now();
            let backup_status = loop {
                if let Some(status) = backing_up.0.try_wait()? {
                    break status;
                }
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "online backup interrupted; partial generations preserved"
                );
                node.live()?;
                daemon.live()?;
                if reserve_checked.elapsed() >= Duration::from_secs(5) {
                    let free = available_bytes(&core_dir)?.min(available_bytes(&root)?);
                    ensure!(
                        free >= minimum_free,
                        "disk reserve exhausted during online backup; generations preserved"
                    );
                    reserve_checked = Instant::now();
                }
                thread::sleep(Duration::from_millis(100));
            };
            ensure!(
                backup_status.success(),
                "online backup failed; generations preserved and no pruning performed"
            );
            event(&events, "backup_prepared", json!({"height":tip}))?;
            if current_core_tip(&core)? != snapshot_hash {
                crate::retention::discard_stale_preparation(
                    &root,
                    &maintenance,
                    &snapshot,
                    &audit,
                    true,
                )?;
                event(
                    &events,
                    "maintenance_stale",
                    json!({"snapshot_height":tip,"phase":"backup"}),
                )?;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            let before = block_files(&block_dir)?;
            core.call("setnetworkactive", json!([false]))?;
            paused = true;
            event(
                &events,
                "maintenance_offline",
                json!({"height":tip,"electrum_stopped":false,"networkactive":false}),
            )?;
            if current_core_tip(&core)? != snapshot_hash {
                crate::retention::discard_stale_preparation(
                    &root,
                    &maintenance,
                    &snapshot,
                    &audit,
                    true,
                )?;
                event(
                    &events,
                    "maintenance_stale",
                    json!({"snapshot_height":tip,"phase":"network_pause"}),
                )?;
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            let started_verify = match private_call(
                control,
                json!({"method":"verify-prepared","snapshot":snapshot,"audit":audit,"backup":backup,"expected_tip":snapshot_hash.to_string()}),
            ) {
                Ok(result) => result,
                Err(error)
                    if error
                        .downcast_ref::<crate::index::AuditTipChanged>()
                        .is_some() =>
                {
                    ensure!(
                        current_core_tip(&core)? != snapshot_hash,
                        "verification reported a changed chain but Core tip did not change"
                    );
                    crate::retention::discard_stale_preparation(
                        &root,
                        &maintenance,
                        &snapshot,
                        &audit,
                        true,
                    )?;
                    event(
                        &events,
                        "maintenance_stale",
                        json!({"snapshot_height":tip,"phase":"verification"}),
                    )?;
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(error) => return Err(error),
            };
            ensure!(
                started_verify["state"] == "running",
                "prepared verification did not start"
            );
            loop {
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "verification interrupted; owned Core will stop without resuming"
                );
                node.live()?;
                daemon.live()?;
                let state = private_call(control, json!({"method":"verification-status"}))?;
                match state["state"].as_str() {
                    Some("running") => {}
                    Some("verified") => {
                        ensure!(
                            state["height"].as_u64() == Some(u64::from(tip)),
                            "verified height changed"
                        );
                        break;
                    }
                    _ => bail!("prepared verification failed or expired: {state}"),
                }
                thread::sleep(Duration::from_millis(100));
            }
            let proof = control
                .parent()
                .context("private control parent")?
                .join(format!("audit-{stamp}.ok"));
            durable_new(
                &proof,
                format!(
                    "snapshot={}\naudit={}\nbackup={}\n",
                    fs::canonicalize(&snapshot)?.display(),
                    fs::canonicalize(&audit)?.display(),
                    fs::canonicalize(&backup)?.display()
                )
                .as_bytes(),
            )?;
            let started_prune =
                private_call(control, json!({"method":"prune-prepared","proof":proof}))?;
            ensure!(
                started_prune["state"] == "pruning",
                "coordinated prune did not start"
            );
            let prune_result = loop {
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "maintenance interrupted; owned Core will stop without resuming"
                );
                node.live()?;
                daemon.live()?;
                let state = private_call(control, json!({"method":"verification-status"}))?;
                match state["state"].as_str() {
                    Some("pruning") => {}
                    Some("pruned") => break state["result"].clone(),
                    _ => bail!("coordinated prune failed; Core will stop: {state}"),
                }
                thread::sleep(Duration::from_millis(100));
            };
            let after = block_files(&block_dir)?;
            let deleted: Vec<_> = before
                .keys()
                .filter(|name| !after.contains_key(*name))
                .cloned()
                .collect();
            let post_info = core.call("getblockchaininfo", json!([]))?;
            ensure!(
                post_info["pruneheight"] == prune_result["pruneheight"],
                "Core prune height changed after maintenance"
            );
            last_maintenance = post_info["blocks"]
                .as_u64()
                .context("Core height after prune")?
                .try_into()?;
            ensure!(
                last_maintenance == tip,
                "Core height changed during maintenance"
            );
            if deleted.is_empty() {
                let now = Instant::now();
                for (name, fp) in eligible {
                    unsuccessful.insert(name, (fp, last_maintenance, now));
                }
            } else {
                unsuccessful.clear();
            }
            event(
                &events,
                "maintenance_complete",
                json!({"height":tip,"pruneheight":post_info["pruneheight"],"deleted_block_files":deleted,"block_file_bytes_before":before.values().sum::<u64>(),"block_file_bytes_after":after.values().sum::<u64>()}),
            )?;
            // Retention operates on closed generations while the same server
            // continues serving; Core remains offline until cleanup finishes.
            match crate::retention::clean(&root, &maintenance, &audit, &backup, network) {
                Ok(count) => {
                    event(
                        &events,
                        "retention_complete",
                        json!({"removed_pairs":count}),
                    )?;
                    if let Err(e) = crate::retention::clean_prepared(
                        &root,
                        &maintenance,
                        &snapshot,
                        &proof,
                        network,
                    ) {
                        event(
                            &events,
                            "retention_skipped",
                            json!({"reason":format!("temporary generation preserved: {e:#}")}),
                        )?;
                    }
                }
                Err(e) => event(
                    &events,
                    "retention_skipped",
                    json!({"reason":format!("{e:#}")}),
                )?,
            }
            daemon.live()?;
            // Refresh the reserve before deciding whether networking may resume.
            free_bytes = available_bytes(&core_dir)?.min(available_bytes(&root)?);
            free_checked = Instant::now();
            paused = true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    daemon.stop()?;
    // Stop only the dedicated child whose PID we verified, never an adopted node.
    core.call("stop", json!([]))?;
    let deadline = Instant::now();
    loop {
        if let Some(status) = node.0.try_wait()? {
            ensure!(status.success(), "Core shutdown failed: {status}");
            break;
        }
        ensure!(
            deadline.elapsed() < Duration::from_secs(45),
            "Core shutdown timed out"
        );
        thread::sleep(Duration::from_millis(100));
    }
    event(&events, "shutdown", json!({"clean":true}))?;
    Ok(())
}
fn block_files(path: &Path) -> Result<std::collections::BTreeMap<String, u64>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("blk") && name.ends_with(".dat") {
            ensure!(
                entry.file_type()?.is_file(),
                "unexpected non-file in owned Core block directory"
            );
            files.insert(name, entry.metadata()?.len());
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(root: &Path) -> Config {
        Config {
            network: "regtest".to_owned(),
            data_dir: root.join("lite"),
            core_dir: root.join("core"),
            bitcoind: PathBuf::from("must-not-be-launched"),
            core_rpc: "http://127.0.0.1:18443".to_owned(),
            listen: "127.0.0.1:50001".parse().expect("address"),
            p2p_port: 18444,
            core_dbcache_mib: 64,
            high: 200,
            low: 20,
            min_free_mib: 2048,
            auto_prune: false,
            prune_interval: 1,
            peers: Vec::new(),
            proxy: None,
        }
    }
    #[test]
    fn never_adopts_unowned_core() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("el-supervisor-unowned-{}", std::process::id()));
        fs::create_dir_all(root.join("core"))?;
        fs::write(root.join("core/wallet.dat"), b"preserve")?;
        let result = run(config(&root), Arc::new(AtomicBool::new(false)));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("refusing existing Core data"));
        assert_eq!(fs::read(root.join("core/wallet.dat"))?, b"preserve");
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn mainnet_auto_prune_hard_gate() {
        let root =
            std::env::temp_dir().join(format!("el-mainnet-prune-gate-{}", std::process::id()));
        let mut c = config(&root);
        c.network = "mainnet".to_owned();
        c.auto_prune = true;
        let e = run(c, Arc::new(AtomicBool::new(false))).unwrap_err();
        assert!(e
            .to_string()
            .contains("mainnet automatic pruning is blocked"));
        assert!(!root.exists());
    }

    #[test]
    fn core_cache_bounds_are_checked_before_node_start() {
        let root = std::env::temp_dir().join(format!("el-cache-gate-{}", std::process::id()));
        for cache in [0, 63, 1025, u32::MAX] {
            let mut c = config(&root);
            c.core_dbcache_mib = cache;
            let error = run(c, Arc::new(AtomicBool::new(false))).unwrap_err();
            assert!(error.to_string().contains("Core database cache"));
            assert!(!root.exists());
        }
    }

    #[test]
    fn backpressure_hysteresis_protects_both_resources() {
        let mib = 1024 * 1024;
        let minimum = 2048 * mib;
        assert!(!pause_needed(false, 1999, 2000, 200, minimum, minimum));
        assert!(pause_needed(false, 2000, 2000, 200, minimum, minimum));
        assert!(pause_needed(false, 0, 2000, 200, minimum - 1, minimum));
        assert!(pause_needed(true, 0, 2000, 200, minimum, minimum));
        assert!(pause_needed(
            true,
            201,
            2000,
            200,
            minimum + 512 * mib,
            minimum
        ));
        assert!(!pause_needed(
            true,
            200,
            2000,
            200,
            minimum + 512 * mib,
            minimum
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_control_rejects_failed_or_mismatched_replies() -> Result<()> {
        use std::os::unix::net::UnixListener;
        let root = std::env::temp_dir().join(format!(
            "el-control-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&root)?;
        let hash = bitcoin::blockdata::constants::genesis_block(Network::Regtest).block_hash();
        for (i, response, valid) in [
            (
                0,
                json!({"result":{"height":7,"hash":hash.to_string()}}),
                true,
            ),
            (1, json!({"error":{"message":"refused"}}), false),
            (
                2,
                json!({"result":{"height":8,"hash":hash.to_string()}}),
                false,
            ),
            (3, json!({"result":{"height":7,"hash":"invalid"}}), false),
        ] {
            let path = root.join(format!("s{i}"));
            let listener = UnixListener::bind(&path)?;
            let worker = thread::spawn(move || -> Result<()> {
                let (mut stream, _) = listener.accept()?;
                let mut request = String::new();
                BufReader::new(stream.try_clone()?).read_line(&mut request)?;
                let request: Value = serde_json::from_str(&request)?;
                ensure!(
                    request["method"] == "snapshot",
                    "unexpected control request"
                );
                writeln!(stream, "{response}")?;
                Ok(())
            });
            assert_eq!(
                request_snapshot(&path, &root.join("snapshot-7-1"), 7, hash).is_ok(),
                valid
            );
            worker.join().expect("control test worker")?;
            fs::remove_file(path)?;
        }
        fs::remove_dir(root)?;
        Ok(())
    }

    #[test]
    fn no_deletion_retries_are_bounded() {
        assert!(!no_deletion_retry_due(
            true,
            1001,
            1000,
            Duration::from_secs(10)
        ));
        assert!(!no_deletion_retry_due(
            true,
            1143,
            1000,
            Duration::from_secs(899)
        ));
        assert!(no_deletion_retry_due(
            true,
            1144,
            1000,
            Duration::from_secs(10)
        ));
        assert!(no_deletion_retry_due(
            true,
            1001,
            1000,
            Duration::from_secs(900)
        ));
        assert!(no_deletion_retry_due(false, 1001, 1000, Duration::ZERO));
        assert!(no_deletion_retry_due(true, 999, 1000, Duration::ZERO));
    }
}
