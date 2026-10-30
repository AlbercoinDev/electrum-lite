use anyhow::{ensure, Result};
use bitcoin::Network;
use clap::{CommandFactory, FromArgMatches, Parser};
use electrum_lite::{core::Core, index::Index};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Experimental thin Electrum server — not production ready"
)]
struct Args {
    #[arg(long, default_value="regtest", value_parser=["mainnet","signet","testnet4","regtest"])]
    network: String,
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    core_cookie: Option<PathBuf>,
    #[arg(long, default_value = "http://127.0.0.1:18443")]
    core_rpc: String,
    #[arg(long, default_value = "127.0.0.1:50001")]
    listen: SocketAddr,
    #[arg(long)]
    allow_lan: bool,
    /// Explicit historical P2P sources (host:port); otherwise reuse Core peer addresses.
    #[arg(long)]
    block_peer: Vec<String>,
    /// Start a private Tor subprocess; requires the tor executable. No direct fallback.
    #[arg(long, conflicts_with = "socks_proxy")]
    tor: bool,
    /// Use an existing SOCKS5 proxy for historical P2P, with remote name resolution.
    #[arg(long)]
    socks_proxy: Option<String>,
    #[arg(long, default_value = "127.0.0.1:19050")]
    tor_socks: String,
    /// NEW destination for a verified RocksDB checkpoint before test-network pruning.
    #[arg(long)]
    backup_dir: Option<PathBuf>,
    #[arg(long)]
    confirm_prune: bool,
    /// NEW semantic replay destination; retained for inspection.
    #[arg(long)]
    audit_dir: Option<PathBuf>,
    #[arg(long)]
    core_dir: Option<PathBuf>,
    #[arg(long, default_value = "bitcoind")]
    bitcoind: PathBuf,
    #[arg(long, default_value_t = 18444)]
    core_p2p_port: u16,
    /// Dedicated Bitcoin Core database cache in MiB; does not limit total RAM.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u32).range(64..=1024))]
    core_dbcache_mib: u32,
    #[arg(long)]
    confirm_dedicated: bool,
    #[arg(long)]
    auto_prune: bool,
    #[arg(long, default_value_t = 200)]
    pause_lag: u32,
    #[arg(long, default_value_t = 20)]
    resume_lag: u32,
    #[arg(long, default_value_t = 1)]
    prune_interval: u32,
    /// Private supervisor control socket; never exposed over Electrum TCP.
    #[arg(long, hide = true)]
    control_socket: Option<PathBuf>,
    #[arg(value_parser=["sync","serve","managed-serve","health","backup","audit","prune"], default_value="serve")]
    command: String,
}
fn main() -> Result<()> {
    let mut command = Args::command();
    if electrum_lite::i18n::language() == electrum_lite::i18n::Language::Spanish {
        command = command
            .about("Servidor Electrum thin experimental: no preparado para producción")
            .help_template("{about-with-newline}\nUso: {usage}\n\n{all-args}{after-help}")
            .mut_arg("network", |a| a.help("Red Bitcoin"))
            .mut_arg("data_dir", |a| {
                a.help("Directorio de datos de Electrum Lite")
            })
            .mut_arg("core_cookie", |a| {
                a.help("Archivo cookie RPC de Bitcoin Core")
            })
            .mut_arg("core_rpc", |a| a.help("Dirección RPC local de Core"))
            .mut_arg("core_dir", |a| {
                a.help("Directorio nuevo para Core dedicado")
            })
            .mut_arg("bitcoind", |a| a.help("Ruta al ejecutable de Bitcoin Core"))
            .mut_arg("core_dbcache_mib", |a| {
                a.help("Caché de base de datos de Core en MiB; no limita la RAM total")
            })
            .mut_arg("confirm_dedicated", |a| {
                a.help("Confirmar uso de un Core dedicado")
            })
            .mut_arg("auto_prune", |a| {
                a.help("Activar poda coordinada en redes de prueba")
            })
            .mut_arg("block_peer", |a| {
                a.help("Fuente P2P de bloques históricos (host:puerto)")
            })
            .mut_arg("tor", |a| {
                a.help(
                    "Iniciar Tor privado para bloques históricos, sin conexión directa de respaldo",
                )
            })
            .mut_arg("socks_proxy", |a| {
                a.help("Proxy SOCKS5 existente para bloques históricos")
            })
            .mut_arg("backup_dir", |a| {
                a.help("Destino NUEVO para una copia de seguridad verificada")
            })
            .mut_arg("audit_dir", |a| {
                a.help("Destino NUEVO para auditoría semántica")
            });
    }
    let matches = command.get_matches();
    let a = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Relaxed))?;
    let network = match a.network.as_str() {
        "mainnet" => Network::Bitcoin,
        "signet" => Network::Signet,
        "testnet4" => Network::Testnet4,
        _ => Network::Regtest,
    };
    let root = a.data_dir.join(&a.network);
    ensure!(!root.is_symlink(), "data root symlink refused");
    if root.exists() {
        for entry in std::fs::read_dir(&root)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            ensure!(
                matches!(
                    name.as_ref(),
                    "index" | "cache" | "tor" | "prune-pending" | "supervisor"
                ) || name.starts_with("prune-complete-"),
                "unrecognized/legacy data root; choose a NEW root, no migration performed"
            );
        }
    }
    let _tor = if a.tor {
        Some(electrum_lite::net::Tor::start(
            &root.join("tor"),
            &a.tor_socks,
        )?)
    } else {
        None
    };
    let proxy = if a.tor {
        Some(a.tor_socks)
    } else {
        a.socks_proxy
    };
    if a.command == "managed-serve" {
        ensure!(
            a.control_socket.is_none(),
            "managed-serve owns its private control socket"
        );
        ensure!(
            a.confirm_dedicated,
            "managed-serve requires --confirm-dedicated; never adopts an existing Core"
        );
        ensure!(
            a.core_cookie.is_none(),
            "managed-serve derives its private cookie; omit --core-cookie"
        );
        return electrum_lite::supervisor::run(
            electrum_lite::supervisor::Config {
                network: a.network,
                data_dir: a.data_dir,
                core_dir: a
                    .core_dir
                    .ok_or_else(|| anyhow::anyhow!("--core-dir NEW dedicated root required"))?,
                bitcoind: a.bitcoind,
                core_rpc: a.core_rpc,
                listen: a.listen,
                p2p_port: a.core_p2p_port,
                core_dbcache_mib: a.core_dbcache_mib,
                high: a.pause_lag,
                low: a.resume_lag,
                auto_prune: a.auto_prune,
                prune_interval: a.prune_interval,
                peers: a.block_peer,
                proxy,
            },
            stop,
        );
    }
    ensure!(!a.auto_prune, "--auto-prune only applies to managed-serve");
    ensure!(
        a.control_socket.is_none() || a.command == "serve",
        "private control socket only applies to serve"
    );
    let mut index = Index::open(&root.join("index"), network)?;
    index.fetcher.peers = a.block_peer;
    index.fetcher.proxy = proxy;
    let core = Core::new(
        a.core_rpc,
        a.core_cookie
            .ok_or_else(|| anyhow::anyhow!("--core-cookie is required outside managed-serve"))?,
    )?;
    match a.command.as_str() {
        "health" => println!(
            "{}",
            serde_json::json!({"indexed_height":index.tip()?,"network":a.network,"engine":"thin-v1","experimental":true})
        ),
        "sync" => index.sync_batch_with_cancel(&core, u32::MAX, &stop)?,
        "backup" => index.backup(
            a.backup_dir
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--backup-dir NEW is required"))?,
        )?,
        "audit" => index.audit_with_cancel(
            &core,
            a.audit_dir
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--audit-dir NEW is required"))?,
            &stop,
        )?,
        "prune" => {
            ensure!(
                a.confirm_prune,
                "prune requires --confirm-prune and a dedicated offline Core"
            );
            ensure!(network != Network::Bitcoin, "mainnet pruning is blocked");
            let info = core.call("getblockchaininfo", serde_json::json!([]))?;
            ensure!(
                info["pruned"] == true && info["automatic_pruning"] == false,
                "Core must use manual prune=1"
            );
            ensure!(
                core.call("getnetworkinfo", serde_json::json!([]))?["networkactive"] == false,
                "disable Core P2P explicitly before pruning"
            );
            ensure!(
                core.call("getrawmempool", serde_json::json!([]))?
                    .as_array()
                    .is_some_and(|m| m.is_empty()),
                "mempool must be empty"
            );
            index.sync(&core)?;
            let height = index.tip()?.ok_or_else(|| anyhow::anyhow!("empty index"))?;
            ensure!(height >= electrum_lite::TESTNET_PRUNE_MIN_HEIGHT,"Core test-network pruning requires at least 1000 blocks with its normal parameters");
            let audit = root.join("prune-pending");
            let completed = root.join(format!("prune-complete-{height}"));
            ensure!(
                std::fs::symlink_metadata(&audit)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "unfinished prune: inspect Core and preserve audit before recovery"
            );
            ensure!(
                std::fs::symlink_metadata(&completed)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "this height already has a completed prune record"
            );
            let base = electrum_lite::retention::verified_audit_base(&root, network, height)?;
            index.audit_with_verified_base(
                &core,
                a.audit_dir
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--audit-dir NEW is required before pruning"))?,
                &stop,
                base.as_deref(),
            )?;
            index.backup(
                a.backup_dir
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--backup-dir NEW is required"))?,
            )?;
            ensure!(
                !stop.load(std::sync::atomic::Ordering::Relaxed),
                "maintenance interrupted before pruning"
            );
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&audit)?;
            use std::io::Write;
            writeln!(file, "requested_height={}", height - 288)?;
            writeln!(
                file,
                "backup={}",
                a.backup_dir.as_deref().expect("checked backup").display()
            )?;
            writeln!(
                file,
                "audit={}",
                a.audit_dir.as_deref().expect("checked audit").display()
            )?;
            file.sync_all()?;
            std::fs::File::open(&root)?.sync_all()?;
            ensure!(
                !stop.load(std::sync::atomic::Ordering::Relaxed),
                "maintenance interrupted; intent preserved"
            );
            let result = core.call("pruneblockchain", serde_json::json!([height - 288]))?;
            let after = core.call("getblockchaininfo", serde_json::json!([]))?;
            let pruneheight = after["pruneheight"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Core pruneheight missing"))?;
            let mut deleted = false;
            if pruneheight > 0 {
                let hash = index
                    .header(u32::try_from(pruneheight - 1)?)?
                    .block_hash()
                    .to_string();
                match core.block(&hash) {
                    Err(e)
                        if e.downcast_ref::<electrum_lite::core::RpcError>()
                            .is_some_and(|e| e.code == -1) =>
                    {
                        deleted = true
                    }
                    Err(e) => return Err(e),
                    Ok(_) => anyhow::bail!(
                        "Core reports pruning but removed historical block remains available"
                    ),
                };
                core.call("getblockchaininfo", serde_json::json!([]))?;
            }
            writeln!(
                file,
                "actual_pruneheight={pruneheight}\nmissing_block_verified={deleted}"
            )?;
            file.sync_all()?;
            std::fs::rename(&audit, completed)?;
            std::fs::File::open(&root)?.sync_all()?;
            println!(
                "{}",
                serde_json::json!({"pruned_to":result,"pruneheight":pruneheight,"missing_block_verified":deleted})
            );
        }
        _ => {
            ensure!(
                a.listen.ip().is_loopback() || a.allow_lan,
                "LAN requires --allow-lan; prefer SSH/WireGuard"
            );
            electrum_lite::server::serve(index, core, a.listen, stop, a.control_socket)?;
        }
    }
    Ok(())
}
