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
    /// Storage profile: auto, hdd (default) or ssd.
    #[arg(long, default_value = "auto", value_parser = ["auto", "hdd", "ssd"])]
    storage: String,
    /// Initial-sync mode: WAL/compactions off, batched writes, hash-only Core verification.
    #[arg(long)]
    fast_sync: bool,
    /// Verify Merkle/witness for Core blocks: hash (default in fast-sync) or full.
    #[arg(long, default_value = "hash", value_parser = ["hash", "full"])]
    verify_core: String,
    /// Generate transaction offsets (txpos) during indexing.
    #[arg(long, default_value = "off", value_parser = ["off", "on"])]
    txpos: String,
    /// Override detected index workers.
    #[arg(long)]
    index_threads: Option<usize>,
    /// Override detected CPU count for budgeting.
    #[arg(long)]
    cpu_budget: Option<usize>,
    /// Override detected RAM in MiB for budgeting.
    #[arg(long)]
    memory_budget_mib: Option<u64>,
    /// Optional Esplora endpoint(s), comma-separated; only block hashes are sent.
    #[arg(long)]
    esplora: Option<String>,
    /// Allow public clearnet block reconstruction without Tor (privacy trade-off).
    #[arg(long)]
    allow_public_data: bool,
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
    /// Explicitly allow manual mainnet pruning (recovery depends on Lite's index).
    #[arg(long)]
    confirm_pruned_mainnet: bool,
    /// NEW semantic replay destination; retained for inspection.
    #[arg(long)]
    audit_dir: Option<PathBuf>,
    #[arg(long)]
    core_dir: Option<PathBuf>,
    #[arg(long, default_value = "bitcoind")]
    bitcoind: PathBuf,
    #[arg(long, default_value_t = 18444)]
    core_p2p_port: u16,
    /// Dedicated Bitcoin Core database cache in MiB; defaults to the RAM budget.
    #[arg(long, value_parser = clap::value_parser!(u32).range(64..=1024))]
    core_dbcache_mib: Option<u32>,
    /// Dedicated Core script-verification threads; defaults to the CPU budget.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=16))]
    core_par: Option<u32>,
    #[arg(long)]
    confirm_dedicated: bool,
    #[arg(long)]
    auto_prune: bool,
    #[arg(long, default_value_t = 64)]
    pause_lag: u32,
    #[arg(long, default_value_t = 8)]
    resume_lag: u32,
    /// Pause Core networking below this free-space reserve on either filesystem (MiB).
    #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u32).range(256..=1048576))]
    min_free_mib: u32,
    #[arg(long, default_value_t = 1)]
    prune_interval: u32,
    /// Private supervisor control socket; never exposed over Electrum TCP.
    #[arg(long, hide = true)]
    control_socket: Option<PathBuf>,
    /// Supervisor-owned checkpoint to audit independently from the live index.
    #[arg(long, hide = true)]
    snapshot_dir: Option<PathBuf>,
    /// Internal handoff from the managed supervisor after its audit child exits.
    #[arg(long, hide = true)]
    prepared_snapshot: Option<PathBuf>,
    #[arg(long, hide = true)]
    prepared_proof: Option<PathBuf>,
    #[arg(value_parser=["sync","serve","managed-serve","health","backup","audit","prune"], default_value="serve")]
    command: String,
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            if error
                .downcast_ref::<electrum_lite::index::AuditTipChanged>()
                .is_some()
            {
                std::process::ExitCode::from(electrum_lite::AUDIT_STALE_EXIT_CODE)
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn run() -> Result<()> {
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
            .mut_arg("storage", |a| {
                a.help("Perfil de almacenamiento: auto, hdd o ssd")
            })
            .mut_arg("esplora", |a| {
                a.help("Endpoint(s) Esplora separados por comas; solo se envían hashes de bloque")
            })
            .mut_arg("allow_public_data", |a| {
                a.help("Permitir reconstrucción pública sin Tor; reduce la privacidad")
            })
            .mut_arg("confirm_pruned_mainnet", |a| {
                a.help("Confirmar poda mainnet; la recuperación depende del índice de Lite")
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
    if let Some(endpoints) = &a.esplora {
        std::env::set_var("ELECTRUM_LITE_ESPLORA", endpoints);
    }
    if a.allow_public_data {
        std::env::set_var("ELECTRUM_LITE_ALLOW_PUBLIC_DATA", "1");
    }
    if a.storage != "auto" {
        std::env::set_var("ELECTRUM_LITE_STORAGE", &a.storage);
    }
    let storage_arg = if a.storage == "auto" {
        std::env::var("ELECTRUM_LITE_STORAGE").unwrap_or_else(|_| "auto".to_owned())
    } else {
        a.storage.clone()
    };
    let storage = match storage_arg.as_str() {
        "ssd" => electrum_lite::index::StorageProfile::Ssd,
        "hdd" => electrum_lite::index::StorageProfile::Hdd,
        _ => electrum_lite::index::detect_storage_profile(&a.data_dir),
    };
    let mut budget = electrum_lite::budget::Budget::detect_or_default(&a.data_dir, storage);
    budget.override_resources(
        a.cpu_budget,
        a.memory_budget_mib,
        a.index_threads,
        a.core_par,
    );
    budget.validate()?;
    budget.log();
    if let Err(error) = budget.ensure_fits_disk(&a.data_dir) {
        eprintln!("budget warning: {error:#}");
    }
    let fast_sync = a.fast_sync
        || electrum_lite::budget::parse_bool_env("ELECTRUM_LITE_FAST_SYNC").unwrap_or(false);
    let verify_core =
        std::env::var("ELECTRUM_LITE_VERIFY_CORE").unwrap_or_else(|_| a.verify_core.clone());
    let txpos = std::env::var("ELECTRUM_LITE_TXPOS").unwrap_or_else(|_| a.txpos.clone());
    let index_workers = std::env::var("ELECTRUM_LITE_INDEX_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| a.index_threads.unwrap_or(budget.index_workers));
    if fast_sync {
        std::env::set_var("ELECTRUM_LITE_FAST_SYNC", "1");
    }
    std::env::set_var("ELECTRUM_LITE_VERIFY_CORE", &verify_core);
    std::env::set_var("ELECTRUM_LITE_TXPOS", &txpos);
    std::env::set_var("ELECTRUM_LITE_INDEX_THREADS", index_workers.to_string());
    if let Some(cpu) = a.cpu_budget {
        std::env::set_var("ELECTRUM_LITE_CPU_BUDGET", cpu.to_string());
    }
    if let Some(ram) = a.memory_budget_mib {
        std::env::set_var("ELECTRUM_LITE_MEMORY_BUDGET_MIB", ram.to_string());
    }
    if a.command == "managed-serve" {
        ensure!(
            a.control_socket.is_none(),
            "managed-serve owns its private control socket"
        );
        ensure!(
            a.snapshot_dir.is_none(),
            "managed-serve cannot use an audit snapshot"
        );
        ensure!(
            a.prepared_snapshot.is_none() && a.prepared_proof.is_none(),
            "managed-serve owns prepared pruning"
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
                core_dbcache_mib: a.core_dbcache_mib.unwrap_or(budget.core_dbcache_mib),
                core_par: a.core_par.unwrap_or(budget.core_par),
                core_mempool_mib: budget.core_mempool_mib,
                high: a.pause_lag,
                low: a.resume_lag,
                min_free_mib: a.min_free_mib.max(budget.min_free_mib as u32),
                auto_prune: a.auto_prune,
                prune_interval: a.prune_interval,
                confirm_pruned_mainnet: a.confirm_pruned_mainnet,
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
    ensure!(
        a.snapshot_dir.is_none() || matches!(a.command.as_str(), "audit" | "backup"),
        "snapshot only applies to audit or backup"
    );
    ensure!(
        a.prepared_snapshot.is_some() == a.prepared_proof.is_some(),
        "prepared prune requires both snapshot and proof"
    );
    ensure!(
        a.prepared_snapshot.is_none() || a.command == "prune",
        "prepared snapshot only applies to prune"
    );
    let options = electrum_lite::index::IndexOptions {
        storage,
        block_cache_mib: budget.block_cache_mib,
        write_buffer_mib: budget.write_buffer_mib,
        memtables: budget.memtables,
        rocks_jobs: budget.rocks_jobs,
        rocks_subcompactions: budget.rocks_subcompactions,
        index_workers,
        fast_sync,
        verify_core: if verify_core == "full" {
            electrum_lite::index::VerifyCore::Full
        } else {
            electrum_lite::index::VerifyCore::Hash
        },
        txpos: txpos == "on",
    };
    let mut index = if let Some(snapshot) = &a.snapshot_dir {
        let metadata = std::fs::symlink_metadata(snapshot)?;
        ensure!(
            metadata.file_type().is_dir(),
            "snapshot must be a real directory"
        );
        let supervisor = std::fs::canonicalize(root.join("supervisor"))?;
        ensure!(
            std::fs::canonicalize(snapshot)?.parent() == Some(supervisor.as_path()),
            "snapshot must be inside supervisor directory"
        );
        let name = snapshot
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid snapshot name"))?;
        let (height, stamp) = name
            .strip_prefix("snapshot-")
            .and_then(|s| s.split_once('-'))
            .ok_or_else(|| anyhow::anyhow!("invalid snapshot name"))?;
        ensure!(
            !height.is_empty()
                && height.bytes().all(|b| b.is_ascii_digit())
                && !stamp.is_empty()
                && stamp.bytes().all(|b| b.is_ascii_digit()),
            "invalid snapshot name"
        );
        let height: u32 = height.parse()?;
        let _: u128 = stamp.parse()?;
        ensure!(
            std::fs::symlink_metadata(snapshot.join("CURRENT"))?
                .file_type()
                .is_file(),
            "snapshot is not an existing RocksDB checkpoint"
        );
        {
            let check =
                rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), snapshot, false)?;
            let identity = format!("{}:{network}", electrum_lite::index::IDENTITY_PREFIX);
            ensure!(
                check.get(b"identity")?.as_deref() == Some(identity.as_bytes()),
                "snapshot network identity mismatch"
            );
            ensure!(
                check.get(b"tip")?.as_deref() == Some(height.to_be_bytes().as_slice()),
                "snapshot name does not match its tip"
            );
        }
        let checkpoint = Index::open_with_options(snapshot, network, options)?;
        ensure!(
            checkpoint.tip()? == Some(height),
            "snapshot name does not match its tip"
        );
        checkpoint
    } else {
        Index::open_with_options(&root.join("index"), network, options)?
    };
    index.fetcher.peers = a.block_peer;
    index.fetcher.proxy = proxy.clone();
    let allow_public =
        a.allow_public_data || std::env::var_os("ELECTRUM_LITE_ALLOW_PUBLIC_DATA").is_some();
    if proxy.is_some() || a.esplora.is_some() || allow_public {
        index.public = Some(electrum_lite::public_data::Client::new(proxy.as_deref())?);
    }
    if a.snapshot_dir.is_some() {
        index.cache = root.join("cache");
    }
    let mut core = Core::new(
        a.core_rpc,
        a.core_cookie
            .ok_or_else(|| anyhow::anyhow!("--core-cookie is required outside managed-serve"))?,
    )?;
    if let Err(error) = core.enable_rest() {
        eprintln!("Core REST unavailable; using slower JSON-RPC transport: {error:#}");
    } else {
        eprintln!(
            "{}",
            electrum_lite::i18n::text(
                "Core REST transport enabled",
                "Transporte REST de Core activado"
            )
        );
    }
    match a.command.as_str() {
        "health" => println!(
            "{}",
            serde_json::json!({"indexed_height":index.tip()?,"network":a.network,"engine":"thin-v3-txnum","experimental":true})
        ),
        "sync" => index.sync_batch_with_cancel(&core, u32::MAX, &stop)?,
        "backup" => index.backup(
            a.backup_dir
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--backup-dir NEW is required"))?,
        )?,
        "audit" => {
            let base = if a.snapshot_dir.is_some() {
                electrum_lite::retention::verified_audit_base(
                    &root,
                    network,
                    index
                        .tip()?
                        .ok_or_else(|| anyhow::anyhow!("empty snapshot"))?,
                )?
            } else {
                None
            };
            index.audit_with_verified_base(
                &core,
                a.audit_dir
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("--audit-dir NEW is required"))?,
                &stop,
                base.as_deref(),
            )?
        }
        "prune" => {
            ensure!(
                a.confirm_prune,
                "prune requires --confirm-prune and a dedicated offline Core"
            );
            ensure!(
                network != Network::Bitcoin || a.confirm_pruned_mainnet,
                "mainnet pruning requires --confirm-pruned-mainnet and an external backup policy"
            );
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
            let audit_path = a
                .audit_dir
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--audit-dir is required before pruning"))?;
            let backup_path = a
                .backup_dir
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--backup-dir is required before pruning"))?;
            if let Some(snapshot) = a.prepared_snapshot.as_deref() {
                let proof = a.prepared_proof.as_deref().expect("checked prepared proof");
                let maintenance = std::fs::canonicalize(root.join("supervisor"))?;
                let private = proof
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("prepared proof parent missing"))?;
                ensure!(
                    std::fs::symlink_metadata(private)?.file_type().is_dir()
                        && std::fs::canonicalize(private)?.parent() == Some(maintenance.as_path()),
                    "prepared proof directory invalid"
                );
                let proof_meta = std::fs::symlink_metadata(proof)?;
                ensure!(
                    proof_meta.file_type().is_file() && proof_meta.len() <= 16384,
                    "prepared proof invalid"
                );
                for path in [snapshot, audit_path, backup_path] {
                    ensure!(
                        std::fs::symlink_metadata(path)?.file_type().is_dir()
                            && std::fs::canonicalize(path)?.parent() == Some(maintenance.as_path()),
                        "prepared generation outside supervisor directory"
                    );
                }
                let snapshot = std::fs::canonicalize(snapshot)?;
                let audit_path = std::fs::canonicalize(audit_path)?;
                let backup_path = std::fs::canonicalize(backup_path)?;
                let suffix = snapshot
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("snapshot-"))
                    .ok_or_else(|| anyhow::anyhow!("invalid prepared snapshot name"))?;
                ensure!(
                    audit_path.file_name().and_then(|n| n.to_str())
                        == Some(format!("audit-{suffix}").as_str())
                        && backup_path.file_name().and_then(|n| n.to_str())
                            == Some(format!("backup-{suffix}").as_str()),
                    "prepared generations do not match"
                );
                let expected = format!(
                    "snapshot={}\naudit={}\nbackup={}\n",
                    snapshot.display(),
                    audit_path.display(),
                    backup_path.display()
                );
                ensure!(
                    std::fs::read_to_string(proof)? == expected,
                    "prepared proof mismatch"
                );
                let (prepared_height, _) = index.verify_prepared_audit(&snapshot, &audit_path)?;
                ensure!(prepared_height == height, "prepared audit height changed");
                let (backup_height, _) = index.verify_snapshot_matches(&backup_path)?;
                ensure!(backup_height == height, "prepared backup height changed");
            } else {
                let base = electrum_lite::retention::verified_audit_base(&root, network, height)?;
                index.audit_with_verified_base(&core, audit_path, &stop, base.as_deref())?;
                index.backup(backup_path)?;
            }
            let result = electrum_lite::prune::commit_verified_prune(
                &index,
                &core,
                &root,
                audit_path,
                backup_path,
                &stop,
            )?;
            println!("{result}");
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
