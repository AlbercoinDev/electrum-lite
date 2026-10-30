//! Supervisor for an explicitly dedicated Core. Never adopts an existing process.
use crate::core::Core;
use anyhow::{bail, ensure, Context, Result};
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
    pub high: u32,
    pub low: u32,
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
    eprintln!("supervisor: {name} {extra}");
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
    fn server(&self, cookie: &Path) -> Result<OwnedChild> {
        OwnedChild::spawn(self.base_command(cookie)?.arg("serve"))
    }
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
            !root.join("prune-pending").exists(),
            "unfinished prune blocks automatic startup; preserve audit and recover"
        );
    }
    let maintenance = root.join("supervisor");
    ensure!(!maintenance.is_symlink(), "maintenance symlink refused");
    fs::create_dir_all(&maintenance)?;
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
            "-dbcache=64",
            "-listenonion=0",
            "-discover=0",
        ])
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
    let mut daemon = c.server(&cookie)?;
    let mut started = Instant::now();
    let mut paused = false;
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
        if !paused && lag >= u64::from(c.high) {
            core.call("setnetworkactive", json!([false]))?;
            paused = true;
            event(
                &events,
                "backpressure_pause",
                json!({"lag":lag,"core_height":tip,"indexed_height":indexed}),
            )?;
        }
        if paused && lag <= u64::from(c.low) {
            core.call("setnetworkactive", json!([true]))?;
            paused = false;
            event(&events, "backpressure_resume", json!({"lag":lag}))?;
        }
        if c.auto_prune
            && tip >= crate::TESTNET_PRUNE_MIN_HEIGHT
            && tip.saturating_sub(last_maintenance) >= c.prune_interval
            && lag == 0
            && state["ready"] == true
            && state["mempool_transactions"] == 0
        {
            daemon.stop()?;
            let block_dir = network_dir.join("blocks");
            let before = block_files(&block_dir)?;
            core.call("setnetworkactive", json!([false]))?;
            event(
                &events,
                "maintenance_offline",
                json!({"height":tip,"electrum_stopped":true,"networkactive":false}),
            )?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let audit = maintenance.join(format!("audit-{tip}-{stamp}"));
            let backup = maintenance.join(format!("backup-{tip}-{stamp}"));
            let mut pruning = OwnedChild::spawn(
                c.base_command(&cookie)?
                    .args(["--confirm-prune", "--audit-dir"])
                    .arg(audit)
                    .arg("--backup-dir")
                    .arg(backup)
                    .arg("prune"),
            )?;
            let status = loop {
                if let Some(status) = pruning.0.try_wait()? {
                    break status;
                }
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "maintenance interrupted; owned Core will stop without resuming"
                );
                thread::sleep(Duration::from_millis(100));
            };
            ensure!(
                status.success(),
                "coordinated prune failed; Core will stop, no automatic resume"
            );
            last_maintenance = tip;
            let after = block_files(&block_dir)?;
            let deleted: Vec<_> = before
                .keys()
                .filter(|name| !after.contains_key(*name))
                .cloned()
                .collect();
            event(
                &events,
                "maintenance_complete",
                json!({"height":tip,"pruneheight":core.call("getblockchaininfo",json!([]))?["pruneheight"],"deleted_block_files":deleted,"block_file_bytes_before":before.values().sum::<u64>(),"block_file_bytes_after":after.values().sum::<u64>()}),
            )?;
            daemon = c.server(&cookie)?;
            started = Instant::now();
            health_seen = false;
            core.call("setnetworkactive", json!([true]))?;
            paused = false;
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
            high: 200,
            low: 20,
            auto_prune: false,
            prune_interval: 2016,
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
}
