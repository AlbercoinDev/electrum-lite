use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{ensure, Context, Result};
use bitcoin::{consensus::serialize, Txid};
use clap::{Parser, Subcommand};
use electrum_lite::{
    archive::Archive,
    rpc::Core,
    store::{Limits, Store},
    Chain,
};
use serde_json::json;

#[derive(Parser)]
#[command(
    version,
    about = "Electrum Lite beta: private Electrum server, durable Bitcoin archive and indexes."
)]
struct Cli {
    #[arg(long, value_enum)]
    network: Chain,
    /// Root directory; a network-specific subdirectory is always added.
    #[arg(long, default_value = "./data", global = true)]
    data_dir: PathBuf,
    /// Cumulative transaction safety cap during new indexing/import (not an RSS limit).
    #[arg(long, default_value_t = 10_000_000_000usize, global = true)]
    max_index_transactions: usize,
    /// Combined persistent UTXO + history occurrence cap during new indexing/import.
    #[arg(long, default_value_t = 100_000_000_000usize, global = true)]
    max_index_entries: usize,
    /// Maximum history or UTXO entries returned for one script. Never silently truncates.
    #[arg(long, default_value_t = 10_000, global = true)]
    max_query_results: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recover a deep reorg into a NEW root using matching archived blocks and Core's new branch.
    RecoverChain {
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long)]
        output_root: PathBuf,
        #[arg(long, default_value_t = 512)]
        min_free_mib: u64,
    },
    /// Validate a backup manifest and checksums without modifying data.
    VerifyBackup {
        #[arg(long)]
        directory: PathBuf,
    },
    /// Restore a verified backup into a NEW data root; never overwrite current data.
    Restore {
        #[arg(long)]
        backup_dir: PathBuf,
        #[arg(long)]
        output_root: PathBuf,
    },
    /// Run the private Electrum server with continuous synchronization and mempool reconciliation.
    Serve {
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long, default_value = "127.0.0.1:50001")]
        listen: SocketAddr,
        #[arg(long, default_value_t = 2)]
        poll_seconds: u64,
        #[arg(long, default_value_t = 512)]
        min_free_mib: u64,
        #[arg(long, default_value_t = 50_000)]
        max_mempool_transactions: usize,
        #[arg(long, default_value_t = 64)]
        max_mempool_mib: usize,
        #[arg(long, default_value_t = 500_000)]
        max_mempool_entries: usize,
        #[arg(long, default_value_t = 16)]
        max_connections: usize,
        #[arg(long, default_value_t = 1000)]
        max_subscriptions: usize,
    },
    /// Ask a running local daemon for readiness and counters.
    Health {
        #[arg(long, default_value = "127.0.0.1:50001")]
        address: SocketAddr,
    },
    /// Copy a closed matching archive/index generation into a NEW backup directory.
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    /// Reconstruct and compare all semantic index data against the archived intent log.
    Audit,
    /// Explicit schema-1 -> schema-2 migration, with a mandatory NEW backup directory.
    MigrateIndex {
        #[arg(long)]
        backup_dir: PathBuf,
    },
    /// Audited manual Core pruning on test networks only; never enabled by the daemon.
    Prune {
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long)]
        backup_dir: PathBuf,
        #[arg(long)]
        confirm: bool,
        #[arg(long, default_value_t = 512)]
        min_free_mib: u64,
    },
    /// Check Core identity without downloading blocks.
    CheckCore {
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(long)]
        cookie_file: PathBuf,
    },
    /// Recover durable indexes and fetch a bounded batch. Never calls pruning RPCs.
    Sync {
        #[arg(long)]
        rpc_url: Option<String>,
        #[arg(long)]
        cookie_file: PathBuf,
        #[arg(long, default_value_t = 100)]
        max_blocks: u32,
        #[arg(long)]
        to_height: Option<u32>,
    },
    /// Verify the entire archive, SQLite consistency and committed references.
    Inspect,
    /// Build a separate new SQLite index from the archive, without replacing existing data.
    RebuildIndex {
        #[arg(long)]
        output: PathBuf,
    },
    /// Confirmed history and UTXOs; no mempool, coinbase maturity or Electrum service.
    History { scripthash: String },
    /// Retrieve raw transaction hex from an archived block (offline).
    Transaction {
        txid: String,
        /// Disambiguate historical duplicate TXIDs.
        #[arg(long)]
        height: Option<u32>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    ensure!(
        cli.max_index_transactions > 0,
        "transaction limit must be positive"
    );
    ensure!(cli.max_index_entries > 0, "entry limit must be positive");
    ensure!(cli.max_query_results > 0, "query limit must be positive");
    let limits = Limits {
        transactions: cli.max_index_transactions,
        entries: cli.max_index_entries,
    };
    match cli.command {
        Command::RecoverChain {
            rpc_url,
            cookie_file,
            output_root,
            min_free_mib,
        } => {
            let core = core(cli.network, rpc_url, cookie_file)?;
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut store = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            store.recover(&mut archive)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&electrum_lite::operations::recover_chain(
                    &store,
                    &archive,
                    &core,
                    cli.network,
                    limits,
                    &output_root,
                    min_free_mib
                        .checked_mul(1024 * 1024)
                        .context("free-space limit overflow")?
                )?)?
            );
        }
        Command::VerifyBackup { directory } => {
            let files = electrum_lite::operations::verify_backup(&directory, cli.network)?;
            println!(
                "{}",
                json!({"verified_files":files.len(),"network":cli.network})
            );
        }
        Command::Restore {
            backup_dir,
            output_root,
        } => {
            electrum_lite::operations::restore(&backup_dir, &output_root, cli.network)?;
            println!(
                "{}",
                json!({"restored":output_root,"next":"run inspect; schema-1 backups require migration or the previous binary"})
            );
        }
        Command::Serve {
            rpc_url,
            cookie_file,
            listen,
            poll_seconds,
            min_free_mib,
            max_mempool_transactions,
            max_mempool_mib,
            max_mempool_entries,
            max_connections,
            max_subscriptions,
        } => {
            let options = electrum_lite::electrum::Options {
                transport: electrum_lite::electrum::TransportLimits {
                    connections: max_connections,
                    subscriptions: max_subscriptions,
                },
                chain: cli.network,
                data_dir: cli.data_dir,
                core: core(cli.network, rpc_url, cookie_file)?,
                limits,
                query_limit: cli.max_query_results,
                listen,
                poll_seconds,
                min_free_bytes: min_free_mib
                    .checked_mul(1024 * 1024)
                    .context("free-space limit overflow")?,
                mempool_budget: electrum_lite::mempool::Budget {
                    transactions: max_mempool_transactions,
                    entries: max_mempool_entries,
                    bytes: max_mempool_mib
                        .checked_mul(1024 * 1024)
                        .context("mempool size overflow")?,
                },
            };
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
                .block_on(electrum_lite::electrum::serve(options))?;
        }
        Command::Health { address } => {
            use std::io::{BufRead, Read, Write};
            ensure!(
                address.ip().is_loopback(),
                "health endpoint must be loopback"
            );
            let mut stream =
                std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(5))?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(90)))?;
            stream.write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server.health\",\"params\":[]}\n",
            )?;
            let mut line = String::new();
            std::io::BufReader::new(stream)
                .take(65536)
                .read_line(&mut line)?;
            let result: serde_json::Value = serde_json::from_str(&line)?;
            ensure!(result.get("error").is_none(), "health request failed");
            ensure!(
                result["result"]["network"] == cli.network.name(),
                "health endpoint network mismatch"
            );
            println!("{}", serde_json::to_string_pretty(&result["result"])?);
        }
        Command::MigrateIndex { backup_dir } => {
            let archive = Archive::open(&cli.data_dir, cli.network, false)?;
            electrum_lite::operations::migrate(&archive, cli.network, &backup_dir)?;
            println!(
                "{}",
                json!({"schema":2,"backup":backup_dir,"next":"run inspect before serving"})
            );
        }
        Command::Backup { output } => {
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut store = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            store.recover(&mut archive)?;
            electrum_lite::operations::backup(&archive, &output)?;
            println!("{}", json!({"backup":output}));
        }
        Command::Audit => {
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut store = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            store.recover(&mut archive)?;
            store.audit(&mut archive)?;
            println!("{}", json!({"audit":"ok"}));
        }
        Command::Prune {
            rpc_url,
            cookie_file,
            backup_dir,
            confirm,
            min_free_mib,
        } => {
            ensure!(confirm,"pruning is destructive: explicit --confirm and a new backup directory are required");
            ensure!(
                cli.network != Chain::Mainnet,
                "mainnet pruning remains gated on sustained validation; no unsafe override"
            );
            let core = core(cli.network, rpc_url, cookie_file)?;
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut store = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            store.recover(&mut archive)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&electrum_lite::operations::prune(
                    &store,
                    &mut archive,
                    &core,
                    cli.network,
                    &backup_dir,
                    min_free_mib
                        .checked_mul(1024 * 1024)
                        .context("free-space limit overflow")?
                )?)?
            );
        }
        Command::CheckCore {
            rpc_url,
            cookie_file,
        } => {
            let core = core(cli.network, rpc_url, cookie_file)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&core.verify(cli.network)?)?
            );
        }
        Command::Sync {
            rpc_url,
            cookie_file,
            max_blocks,
            to_height,
        } => {
            ensure!(max_blocks > 0, "max-blocks must be positive");
            let core = core(cli.network, rpc_url, cookie_file)?;
            let mut archive = Archive::open(&cli.data_dir, cli.network, true)?;
            let mut index = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            index.recover(&mut archive)?;
            electrum_lite::sync::batch(
                &mut index,
                &mut archive,
                &core,
                cli.network,
                max_blocks,
                to_height,
                None,
            )?;
            print_stats(&index, cli.network)?;
        }
        Command::RebuildIndex { output } => {
            ensure!(
                output
                    .extension()
                    .is_some_and(|extension| extension == "sqlite"),
                "rebuild output must use .sqlite, never an archive segment or lock filename"
            );
            ensure!(
                !output.exists(),
                "rebuild output already exists; choose a new file, never overwrite a backup"
            );
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            // Atomic refusal of an existing destination, not just an exists() check.
            std::fs::File::create_new(&output)
                .context("cannot create a new rebuild output file")?
                .sync_all()?;
            let mut store = Store::open(&output, cli.network, limits)?;
            store.recover(&mut archive)?;
            store.integrity_check()?;
            print_stats(&store, cli.network)?;
            eprintln!("Built separate index {}; existing index and archive unchanged. See recovery instructions before replacement.", output.display());
        }
        Command::Inspect | Command::History { .. } | Command::Transaction { .. } => {
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut index = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            index.recover(&mut archive)?;
            match cli.command {
                Command::History { scripthash } => println!(
                    "{}",
                    serde_json::to_string_pretty(&index.view(&scripthash, cli.max_query_results)?)?
                ),
                Command::Transaction { txid, height } => {
                    let txid = Txid::from_str(&txid)?;
                    let mut found = Vec::new();
                    for occurrence in index.occurrences(txid, height)? {
                        let (hash, location) = index
                            .record(occurrence.height)?
                            .context("transaction block missing from index")?;
                        let (height, block) = archive.read(location)?;
                        ensure!(
                            height == occurrence.height && block.block_hash() == hash,
                            "transaction archive reference mismatch"
                        );
                        let tx = block
                            .txdata
                            .get(occurrence.position)
                            .context("transaction position outside block")?;
                        ensure!(tx.compute_txid() == txid, "indexed transaction ID mismatch");
                        found.push(json!({"height":height,"position":occurrence.position,"hex":hex::encode(serialize(tx))}));
                    }
                    ensure!(
                        !found.is_empty(),
                        "transaction absent from this partial archive"
                    );
                    println!("{}", serde_json::to_string_pretty(&found)?);
                }
                _ => print_stats(&index, cli.network)?,
            }
        }
    }
    Ok(())
}

fn core(chain: Chain, endpoint: Option<String>, cookie: PathBuf) -> Result<Core> {
    Core::new(
        &endpoint.unwrap_or_else(|| format!("http://127.0.0.1:{}/", chain.port())),
        &cookie,
    )
}

fn print_stats(index: &Store, chain: Chain) -> Result<()> {
    let stats = index.stats()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "version":env!("CARGO_PKG_VERSION"), "network":chain,
            "indexed_tip": stats.tip.map(|(height, hash)| json!({"height":height,"hash":hash})),
            "durable_tip":stats.tip.map(|(height,hash)| json!({"height":height,"hash":hash})),
            "transactions":stats.transactions, "utxos":stats.utxos,"history_entries":stats.history,
            "index_backend":"sqlite-schema-2",
            "scope":"local archived prefix only; not necessarily synchronized with Core",
            "electrum_server":false, "pruning_enabled":false
        }))?
    );
    Ok(())
}
