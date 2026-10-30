//! Experimental Linux supervisor for an explicitly dedicated test Core.
//! Never targets an existing RPC endpoint: it signals only children it started.
use crate::Chain;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

pub struct Options {
    pub lsm: bool,
    pub chain: Chain,
    pub data_dir: PathBuf,
    pub core_dir: PathBuf,
    pub bitcoind: PathBuf,
    pub rpc_port: u16,
    pub listen: SocketAddr,
    pub offline: bool,
    pub auto_prune: Option<AutoPrune>,
}

pub struct AutoPrune {
    pub backup_root: PathBuf,
    pub interval_blocks: u32,
}

fn prune_due(height: u64, checkpoint: u32, interval: u32) -> bool {
    height
        .checked_sub(288)
        .is_some_and(|eligible| eligible >= u64::from(checkpoint) + u64::from(interval))
}

/// Runs only after the owned daemon stopped, with its exclusive archive lock.
/// Any failure leaves networking disabled; the caller stops the owned Core.
fn automatic_maintenance(
    data: &Path,
    cookie: PathBuf,
    endpoint: String,
    chain: Chain,
    backup_root: &Path,
    lsm: bool,
) -> Result<Value> {
    let core = crate::rpc::Core::new(&endpoint, &cookie)?;
    #[cfg(feature = "experimental-lsm")]
    if lsm {
        let mut store = crate::experimental_lsm::BitcoinStore::open(data, chain)?;
        store.prune_checkpoint()?;
        let active = core.call::<Value>("getnetworkinfo", json!([]))?["networkactive"]
            .as_bool()
            .context("missing network state")?;
        let _: Value = core.call("setnetworkactive", json!([false]))?;
        let parent = tempfile::Builder::new()
            .prefix("lsm-prune-")
            .tempdir_in(backup_root)?
            .keep();
        let result = store.prune_offline(
            &core,
            &parent.join("backup"),
            &parent.join("verified"),
            512 * 1024 * 1024,
            true,
        )?;
        if active {
            let _: Value = core.call("setnetworkactive", json!([true]))?;
        }
        return Ok(result);
    }
    ensure!(!lsm, "LSM feature required");
    let mut archive = crate::archive::Archive::open(data, chain, true)?;
    let mut store = crate::store::Store::open(
        &archive.directory().join("index.sqlite"),
        chain,
        crate::store::Limits {
            transactions: 10_000_000_000,
            entries: 100_000_000_000,
        },
    )?;
    store.recover(&mut archive)?;
    store.prune_checkpoint()?;
    let network: Value = core.call("getnetworkinfo", json!([]))?;
    let active = network["networkactive"]
        .as_bool()
        .context("missing Core network state")?;
    let _: Value = core.call("setnetworkactive", json!([false]))?;
    // Random NEW destination; retained on every failure, never reused/deleted.
    let backup = tempfile::Builder::new()
        .prefix("auto-prune-")
        .tempdir_in(backup_root)?;
    let parent = backup.keep();
    let result = crate::operations::prune(
        &store,
        &mut archive,
        &core,
        chain,
        &parent.join("backup"),
        512 * 1024 * 1024,
    )?;
    if active {
        let _: Value = core.call("setnetworkactive", json!([true]))?;
    }
    Ok(result)
}

struct OwnedChild(Child);
impl std::ops::Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            // Best effort on every error path, without signalling any unrelated process.
            if let Ok(pid) = libc::pid_t::try_from(self.0.id()) {
                eprintln!("Supervisor: salida inesperada; solicitando parada del proceso dedicado {pid} (sin confirmación)");
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }
}

fn signal_on_parent_death(cmd: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    // Only async-signal-safe libc calls are made between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(125);
            }
            Ok(())
        });
    }
}

fn check_executable(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o6000 == 0,
        "executable must be regular and not setuid/setgid"
    );
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let attribute = c"security.capability";
    // Capabilities on exec clear the Linux parent-death signal. Do not assume
    // protection if their absence cannot be verified.
    let count =
        unsafe { libc::getxattr(name.as_ptr(), attribute.as_ptr(), std::ptr::null_mut(), 0) };
    if count >= 0 {
        anyhow::bail!("executable has file capabilities; parent-death protection is unavailable");
    }
    let error = std::io::Error::last_os_error();
    ensure!(
        matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)),
        "cannot verify executable capabilities: {error}"
    );
    Ok(())
}

fn stop_owned(child: &mut Child, label: &str) -> Result<()> {
    if child.try_wait()?.is_some() {
        eprintln!("{label}: proceso ya detenido");
        return Ok(());
    }
    let pid: libc::pid_t = child.id().try_into()?;
    eprintln!("{label}: solicitando parada ordenada del proceso dedicado {pid}");
    // The Child remains unreaped: its pid cannot have been recycled.
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    ensure!(
        result == 0,
        "could not signal owned child: {}",
        std::io::Error::last_os_error()
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait()? {
            eprintln!("{label}: parada confirmada ({status})");
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "{label}: parada no confirmada; revise el proceso dedicado"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn probe(address: SocketAddr, chain: Chain) -> Result<Value> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server.health\",\"params\":[]}\n",
    )?;
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .take(65536)
        .read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    ensure!(
        response["id"] == 1 && response.get("error").is_none(),
        "invalid health response"
    );
    ensure!(
        response["result"]["network"] == chain.name(),
        "health network mismatch"
    );
    Ok(response["result"].clone())
}

fn prepare_core_root(path: &Path, chain: Chain) -> Result<PathBuf> {
    // Never adopt an existing directory or node. A marker from this command is
    // required on restart, and the configuration must remain exactly ours.
    let marker = path.join("electrum-lite-dedicated.json");
    let conf = path.join("electrum-lite-core.conf");
    let identity = json!({"owner":"electrum-lite-dedicated-v1","network":chain.name()}).to_string();
    if !path.exists() {
        fs::create_dir(path)
            .context("dedicated Core root must be new and its parent must exist")?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        file.write_all(identity.as_bytes())?;
        file.sync_all()?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&conf)?;
        file.sync_all()?;
        std::fs::File::open(path)?.sync_all()?;
        std::fs::File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
    }
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "dedicated Core root must be a real directory"
    );
    ensure!(
        fs::symlink_metadata(&marker)?.file_type().is_file()
            && fs::metadata(&marker)?.len() <= 256
            && fs::read_to_string(&marker)? == identity,
        "Core root is not this supervisor's dedicated directory"
    );
    ensure!(
        fs::symlink_metadata(&conf)?.file_type().is_file() && fs::metadata(&conf)?.len() == 0,
        "dedicated Core configuration changed; refusing startup"
    );
    Ok(path.canonicalize()?)
}

pub async fn run(options: Options) -> Result<()> {
    ensure!(
        !options.lsm || cfg!(feature = "experimental-lsm"),
        "LSM feature required"
    );
    ensure!(
        options.chain != Chain::Mainnet,
        "managed Core is experimental and limited to test networks"
    );
    ensure!(
        options.listen.ip().is_loopback() && options.listen.port() != 0 && options.rpc_port != 0,
        "explicit nonzero loopback ports are required"
    );
    let executable = options
        .bitcoind
        .canonicalize()
        .context("bitcoind executable missing")?;
    check_executable(&executable)?;
    let app_executable = std::env::current_exe()?.canonicalize()?;
    check_executable(&app_executable)?;
    let root = prepare_core_root(&options.core_dir, options.chain)?;
    let app_root = if options.data_dir.exists() {
        options.data_dir.canonicalize()?
    } else {
        let parent = options
            .data_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        parent.canonicalize()?.join(
            options
                .data_dir
                .file_name()
                .context("data root requires a filename")?,
        )
    };
    ensure!(
        !root.starts_with(&app_root) && !app_root.starts_with(&root),
        "Core and Electrum data roots must be separate"
    );
    let mut prune_checkpoint = 0;
    if let Some(policy) = &options.auto_prune {
        ensure!(
            policy.interval_blocks >= 288,
            "automatic prune interval must be >=288 blocks"
        );
        let backup = policy
            .backup_root
            .canonicalize()
            .context("backup root must already exist")?;
        ensure!(
            backup.is_dir() && !backup.starts_with(&app_root) && !backup.starts_with(&root),
            "backup root must be a directory outside Core and application data"
        );
        #[cfg(feature = "experimental-lsm")]
        if options.lsm {
            let store = if app_root.exists() {
                crate::experimental_lsm::BitcoinStore::open(&app_root, options.chain)?
            } else {
                crate::experimental_lsm::BitcoinStore::create(&app_root, options.chain)?
            };
            prune_checkpoint = store.prune_checkpoint()?.map_or(0, |audit| audit.requested);
        }
        if !options.lsm {
            let mut archive = crate::archive::Archive::open(&app_root, options.chain, true)?;
            let mut store = crate::store::Store::open(
                &archive.directory().join("index.sqlite"),
                options.chain,
                crate::store::Limits {
                    transactions: 10_000_000_000,
                    entries: 100_000_000_000,
                },
            )?;
            store.recover(&mut archive)?;
            prune_checkpoint = store.prune_checkpoint()?;
        }
    }
    // Refuse occupied ports before launching either child; the applications also
    // independently bind them, and failures stop the owned Core.
    drop(TcpListener::bind(options.listen).context("Electrum listen port already in use")?);
    drop(
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, options.rpc_port))
            .context("Core RPC port already in use")?,
    );
    let cookie = root.join(options.chain.name()).join(".cookie");
    let endpoint = format!("http://127.0.0.1:{}/", options.rpc_port);
    let mut app_command = Command::new(app_executable);
    let app = &mut app_command;
    app.arg("--network")
        .arg(options.chain.name())
        .arg("--data-dir")
        .arg(&options.data_dir)
        .arg(if options.lsm { "lsm-serve" } else { "serve" })
        .arg("--cookie-file")
        .arg(&cookie)
        .arg("--rpc-url")
        .arg(&endpoint)
        .arg("--listen")
        .arg(options.listen.to_string());
    signal_on_parent_death(app);
    let mut app = OwnedChild(
        app_command
            .spawn()
            .context("failed to start Electrum Lite")?,
    );
    eprintln!("Supervisor: esperando verificación de Electrum Lite; Core aún no se ha iniciado.");
    let startup = Instant::now();
    let ctrl = tokio::signal::ctrl_c();
    tokio::pin!(ctrl);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        if let Some(status) = app.try_wait()? {
            anyhow::bail!("Electrum Lite stopped before Core startup: {status}");
        }
        let address = options.listen;
        let chain = options.chain;
        if let Ok(Ok(health)) = tokio::task::spawn_blocking(move || probe(address, chain)).await {
            if !options.lsm || (health["backend"] == "lsm" && health["generation"].is_number()) {
                break;
            }
        }
        ensure!(
            startup.elapsed() < Duration::from_secs(600),
            "index verification did not complete; Core was not started"
        );
        tokio::select! { _=&mut ctrl=>{stop_owned(&mut app,"Electrum Lite")?;return Ok(());}, _=terminate.recv()=>{stop_owned(&mut app,"Electrum Lite")?;return Ok(());}, _=tokio::time::sleep(Duration::from_secs(1))=>{} }
    }
    let flag = match options.chain {
        Chain::Signet => "-signet",
        Chain::Testnet4 => "-testnet4",
        Chain::Regtest => "-regtest",
        Chain::Mainnet => unreachable!(),
    };
    let mut command = Command::new(executable);
    command
        .arg(flag)
        .arg(format!("-datadir={}", root.display()))
        .arg(format!(
            "-conf={}",
            root.join("electrum-lite-core.conf").display()
        ))
        .arg(format!("-rpcport={}", options.rpc_port))
        .args([
            "-server=1",
            "-daemon=0",
            "-prune=1",
            "-txindex=0",
            "-rpcbind=127.0.0.1",
            "-rpcallowip=127.0.0.1",
        ]);
    if options.offline {
        command.args(["-listen=0", "-networkactive=0", "-dnsseed=0", "-connect=0"]);
    }
    signal_on_parent_death(&mut command);
    let mut core = match command.spawn() {
        Ok(child) => OwnedChild(child),
        Err(error) => {
            let _ = stop_owned(&mut app, "Electrum Lite");
            return Err(error.into());
        }
    };
    eprintln!(
        "Supervisor: Core dedicado iniciado en poda manual (prune=1); prune=550 no está validado."
    );
    if let Some(policy) = &options.auto_prune {
        eprintln!("Supervisor: poda automática coordinada habilitada; intervalo={} alturas elegibles, margen=288; auditoría y backup completo antes de borrar. No es una garantía de espacio ni velocidad.", policy.interval_blocks);
    }
    let mut last_health = Instant::now();
    let core_started = Instant::now();
    let mut core_seen = false;
    let mut errors = 0u32;
    let (reason, failed) = loop {
        if let Some(status) = app.try_wait()? {
            break (format!("Electrum Lite terminó: {status}; causa detallada no disponible si terminó abruptamente"), true);
        }
        if let Some(status) = core.try_wait()? {
            break (format!("Core dedicado terminó: {status}"), true);
        }
        let address = options.listen;
        let chain = options.chain;
        match tokio::task::spawn_blocking(move || probe(address, chain)).await {
            Ok(Ok(health)) => {
                last_health = Instant::now();
                if health["core_height"].is_number() {
                    core_seen = true;
                }
                if let Some(policy) = &options.auto_prune {
                    if health["last_error"].is_null()
                        && health["indexed_tip"]["height"]
                            .as_u64()
                            .is_some_and(|height| {
                                prune_due(height, prune_checkpoint, policy.interval_blocks)
                            })
                    {
                        eprintln!("Poda automática: ventana de mantenimiento; deteniendo el escritor antes de auditar y respaldar.");
                        if let Err(error) = stop_owned(&mut app, "Electrum Lite") {
                            break (
                                format!("automatic prune could not stop writer: {error}"),
                                true,
                            );
                        }
                        let data = app_root.clone();
                        let cookie = cookie.clone();
                        let endpoint = endpoint.clone();
                        let backup_root = policy.backup_root.clone();
                        let lsm = options.lsm;
                        let result = tokio::task::spawn_blocking(move || {
                            automatic_maintenance(&data, cookie, endpoint, chain, &backup_root, lsm)
                        })
                        .await;
                        match result {
                            Ok(Ok(report)) => {
                                prune_checkpoint = report["requested"]
                                    .as_u64()
                                    .context("automatic prune report missing cursor")?
                                    .try_into()?;
                                eprintln!("Poda automática completada: {report}");
                                match app_command.spawn() {
                                    Ok(child) => app = OwnedChild(child),
                                    Err(error) => {
                                        break (
                                            format!(
                                                "failed to restart after automatic prune: {error}"
                                            ),
                                            true,
                                        )
                                    }
                                }
                                last_health = Instant::now();
                                errors = 0;
                                continue;
                            }
                            error => {
                                break (format!("automatic prune failed closed: {error:?}"), true)
                            }
                        }
                    }
                }
                if core_seen || core_started.elapsed() > Duration::from_secs(120) {
                    if let Some(error) = health["last_error"].as_str() {
                        if !error.contains("mempool snapshot warming")
                            && !(options.lsm && error == "confirmed index catching up")
                        {
                            errors += 1;
                        } else {
                            errors = 0;
                        }
                        if errors >= 3 {
                            break (format!("Electrum Lite no puede continuar: {error}"), true);
                        }
                    } else {
                        errors = 0;
                    }
                }
            }
            _ => {
                if last_health.elapsed() > Duration::from_secs(45) {
                    break (
                        "Electrum Lite no responde al control de salud; causa desconocida"
                            .to_owned(),
                        true,
                    );
                }
            }
        }
        tokio::select! { _=&mut ctrl=>break ("parada solicitada por usuario".to_owned(), false), _=terminate.recv()=>break ("parada solicitada por SIGTERM".to_owned(), false), _=tokio::time::sleep(Duration::from_secs(2))=>{} }
    };
    eprintln!("Supervisor: {reason}");
    let core_result = stop_owned(&mut core, "Bitcoin Core dedicado");
    let app_result = stop_owned(&mut app, "Electrum Lite");
    core_result?;
    app_result?;
    ensure!(
        !failed,
        "supervised service failed; Core stop was confirmed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unowned_or_changed_core_directory_without_modifying_it() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("user-data"), b"preserve").unwrap();
        assert!(prepare_core_root(&existing, Chain::Regtest).is_err());
        assert_eq!(fs::read(existing.join("user-data")).unwrap(), b"preserve");
        let own = root.path().join("owned");
        prepare_core_root(&own, Chain::Regtest).unwrap();
        assert!(prepare_core_root(&own, Chain::Signet).is_err());
        fs::write(own.join("electrum-lite-core.conf"), b"prune=550").unwrap();
        assert!(prepare_core_root(&own, Chain::Regtest).is_err());
        assert_eq!(
            fs::read(own.join("electrum-lite-core.conf")).unwrap(),
            b"prune=550"
        );
    }

    #[test]
    fn automatic_prune_schedule_keeps_margin_and_uses_persistent_cursor() {
        assert!(!prune_due(287, 0, 288));
        assert!(!prune_due(575, 0, 288));
        assert!(prune_due(576, 0, 288));
        assert!(!prune_due(863, 288, 288));
        assert!(prune_due(864, 288, 288));
        assert!(!prune_due(100, 1000, 288)); // Reorg must not reset the cursor.
    }
}
