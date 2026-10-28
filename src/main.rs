use std::{path::PathBuf, str::FromStr};

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
    about = "Electrum Lite beta: block archive and durable SQLite index. No Electrum service or pruning."
)]
struct Cli {
    #[arg(long, value_enum)]
    network: Chain,
    /// Root directory; a network-specific subdirectory is always added.
    #[arg(long, default_value = "./data", global = true)]
    data_dir: PathBuf,
    /// Cumulative transaction safety cap during new indexing/import (not an RSS limit).
    #[arg(long, default_value_t = 100_000, global = true)]
    max_index_transactions: usize,
    /// Combined persistent UTXO + history occurrence cap during new indexing/import.
    #[arg(long, default_value_t = 500_000, global = true)]
    max_index_entries: usize,
    /// Maximum history or UTXO entries returned for one script. Never silently truncates.
    #[arg(long, default_value_t = 10_000, global = true)]
    max_query_results: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
            let info = core.verify(cli.network)?;
            let core_height: u32 = info["blocks"]
                .as_u64()
                .context("Core missing blocks height")?
                .try_into()?;
            let mut archive = Archive::open(&cli.data_dir, cli.network, true)?;
            let mut index = Store::open(
                &archive.directory().join("index.sqlite"),
                cli.network,
                limits,
            )?;
            index.recover(&mut archive)?;
            let tip = index.stats()?.tip;
            if let Some((height, hash)) = tip {
                ensure!(height <= core_height && core.hash(height)? == hash, "archive differs from Core active chain; beta has no rollback, preserve archive and rebuild in a separate directory");
            }
            let start = tip.map_or(0, |(h, _)| h + 1);
            let end = to_height
                .unwrap_or(core_height)
                .min(core_height)
                .min(start.saturating_add(max_blocks - 1));
            if start <= end {
                if info["pruned"].as_bool() == Some(true) {
                    let floor = info["pruneheight"]
                        .as_u64()
                        .context("Core missing pruneheight")?;
                    ensure!(u64::from(start.max(1)) >= floor || end == 0, "required historical blocks are already pruned; restore a trusted archive or use a node with complete history");
                }
                let anchor = core.hash(end)?;
                for height in start..=end {
                    // Refuse a moving branch. A concurrent reorg still requires explicit recovery.
                    ensure!(
                        core.hash(end)? == anchor,
                        "Core reorg during sync; stopped without pruning"
                    );
                    let hash = core.hash(height)?;
                    let block = core.block(hash)?;
                    ensure!(
                        core.hash(height)? == hash,
                        "Core reorg while fetching block"
                    );
                    index.commit(&mut archive, height, &block)?;
                }
                ensure!(
                    core.hash(end)? == anchor,
                    "Core reorg at batch end; archive needs recovery before further syncing"
                );
            }
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
            "index_backend":"sqlite-schema-1",
            "scope":"local archived prefix only; not necessarily synchronized with Core",
            "electrum_server":false, "pruning_enabled":false
        }))?
    );
    Ok(())
}
