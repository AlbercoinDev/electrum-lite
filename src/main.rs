use std::{path::PathBuf, str::FromStr};

use anyhow::{ensure, Context, Result};
use bitcoin::{consensus::serialize, Txid};
use clap::{Parser, Subcommand};
use electrum_lite::{archive::Archive, index::Index, rpc::Core, Chain};
use serde_json::json;

#[derive(Parser)]
#[command(
    version,
    about = "Electrum Lite beta: block archive and in-memory index. No Electrum service or pruning."
)]
struct Cli {
    #[arg(long, value_enum)]
    network: Chain,
    /// Root directory; a network-specific subdirectory is always added.
    #[arg(long, default_value = "./data", global = true)]
    data_dir: PathBuf,
    /// Hard transaction-count guard, not a guaranteed RAM budget. Replay also obeys it.
    #[arg(long, default_value_t = 100_000, global = true)]
    max_index_transactions: usize,
    /// Combined UTXO + history occurrence guard (not a byte-exact RSS limit).
    #[arg(long, default_value_t = 500_000, global = true)]
    max_index_entries: usize,
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
    /// Replay archive then fetch a bounded batch from Core. Never calls pruning RPCs.
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
    /// Verify the entire local archive and reconstruct in-memory indexes.
    Inspect,
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
            let mut index = Index::with_limits(
                cli.network,
                cli.max_index_transactions,
                cli.max_index_entries,
            );
            archive.scan(|height, block| index.apply(height, block))?;
            if let Some((height, hash)) = index.tip {
                ensure!(height <= core_height && core.hash(height)? == hash, "archive differs from Core active chain; beta has no rollback, preserve archive and rebuild in a separate directory");
            }
            let start = index.tip.map_or(0, |(h, _)| h + 1);
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
                    index.apply(height, &block)?;
                    archive.append(height, &block)?;
                }
                ensure!(
                    core.hash(end)? == anchor,
                    "Core reorg at batch end; archive needs recovery before further syncing"
                );
            }
            print_stats(&index)?;
        }
        Command::Inspect | Command::History { .. } | Command::Transaction { .. } => {
            let mut archive = Archive::open(&cli.data_dir, cli.network, false)?;
            let mut index = Index::with_limits(
                cli.network,
                cli.max_index_transactions,
                cli.max_index_entries,
            );
            let query = if let Command::Transaction { ref txid, height } = cli.command {
                Some((Txid::from_str(txid)?, height))
            } else {
                None
            };
            let mut found = Vec::new();
            archive.scan(|height, block| {
                index.apply(height, block)?;
                if let Some((txid, requested_height)) = query {
                    if requested_height.is_none_or(|h| h == height) {
                        for (position, tx) in block.txdata.iter().enumerate() {
                            if tx.compute_txid() == txid {
                                // Keep output bounded even with pathological duplicate fixtures.
                                ensure!(found.len() < 16, "too many occurrences; specify --height");
                                found.push(json!({"height":height, "position":position, "hex":hex::encode(serialize(tx))}));
                            }
                        }
                    }
                }
                Ok(())
            })?;
            match cli.command {
                Command::History { scripthash } => println!(
                    "{}",
                    serde_json::to_string_pretty(&index.view(&scripthash)?)?
                ),
                Command::Transaction { .. } => {
                    ensure!(
                        !found.is_empty(),
                        "transaction absent from this partial archive"
                    );
                    println!("{}", serde_json::to_string_pretty(&found)?);
                }
                _ => print_stats(&index)?,
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

fn print_stats(index: &Index) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "version":env!("CARGO_PKG_VERSION"), "network":index.chain,
            "indexed_tip": index.tip.map(|(height, hash)| json!({"height":height,"hash":hash})),
            "transactions":index.transactions, "utxos":index.utxos.len(),
            "scope":"local archived prefix only; not necessarily synchronized with Core",
            "electrum_server":false, "pruning_enabled":false
        }))?
    );
    Ok(())
}
