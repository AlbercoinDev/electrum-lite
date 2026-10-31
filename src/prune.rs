//! Durable prune RPC after a separately verified audit and backup.
use crate::{core::Core, index::Index};
use anyhow::{ensure, Context, Result};
use bitcoin::Network;
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// Perform the irreversible phase only. The caller MUST own/freeze the sole
/// index writer, verify the independent semantic audit and backup against that
/// exact index, and prevent any other chain writers for the entire operation.
/// This function rechecks Core immediately before the RPC and never clears an
/// incomplete durable intent. It does not itself establish audit provenance.
pub fn commit_verified_prune(
    index: &Index,
    core: &Core,
    root: &Path,
    audit: &Path,
    backup: &Path,
    stop: &AtomicBool,
) -> Result<Value> {
    ensure!(
        index.network != Network::Bitcoin,
        "mainnet pruning is blocked"
    );
    let height = index.tip()?.context("empty index")?;
    ensure!(
        height >= crate::TESTNET_PRUNE_MIN_HEIGHT,
        "Core test-network pruning requires at least 1000 blocks with its normal parameters"
    );
    let pending = root.join("prune-pending");
    let completed = root.join(format!("prune-complete-{height}"));
    ensure!(
        std::fs::symlink_metadata(&pending)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "unfinished prune: inspect Core and preserve audit before recovery"
    );
    ensure!(
        std::fs::symlink_metadata(&completed)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "this height already has a completed prune record"
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "maintenance interrupted before pruning"
    );
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending)?;
    writeln!(file, "requested_height={}", height - 288)?;
    writeln!(file, "backup={}", backup.display())?;
    writeln!(file, "audit={}", audit.display())?;
    file.sync_all()?;
    std::fs::File::open(root)?.sync_all()?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "maintenance interrupted; intent preserved"
    );
    let expected_hash = index.header(height)?.block_hash();
    ensure!(
        index.tip()? == Some(height),
        "index tip changed before prune; intent preserved"
    );
    core.verify_prune_preconditions(height, expected_hash)?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "maintenance interrupted; intent preserved"
    );
    let result = core.call("pruneblockchain", json!([height - 288]))?;
    let after = core.call("getblockchaininfo", json!([]))?;
    let pruneheight = after["pruneheight"]
        .as_u64()
        .context("Core pruneheight missing")?;
    let mut deleted = false;
    if pruneheight > 0 {
        let hash = index
            .header(u32::try_from(pruneheight - 1)?)?
            .block_hash()
            .to_string();
        match core.block(&hash) {
            Err(e)
                if e.downcast_ref::<crate::core::RpcError>()
                    .is_some_and(|e| e.code == -1) =>
            {
                deleted = true
            }
            Err(e) => return Err(e),
            Ok(_) => {
                anyhow::bail!("Core reports pruning but removed historical block remains available")
            }
        }
        core.call("getblockchaininfo", json!([]))?;
    }
    writeln!(
        file,
        "actual_pruneheight={pruneheight}\nmissing_block_verified={deleted}"
    )?;
    file.sync_all()?;
    std::fs::rename(&pending, completed)?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(json!({"pruned_to":result,"pruneheight":pruneheight,"missing_block_verified":deleted}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn rpc_failure_preserves_durable_intent_before_prune() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "el-prune-rpc-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let index = Index::open(&root.join("index"), Network::Regtest)?;
        let genesis = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        let mut meta = bitcoin::consensus::serialize(&genesis.header);
        meta.extend(bitcoin::consensus::serialize(
            &genesis.txdata[0].compute_txid(),
        ));
        index.db.put(crate::index::height_key(b'B', 1000), meta)?;
        index.db.put(b"tip", 1000u32.to_be_bytes())?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let worker = std::thread::spawn(move || -> Result<String> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                headers.push(byte[0]);
                ensure!(headers.len() < 16384, "oversized request");
            }
            let headers = std::str::from_utf8(&headers)?;
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .context("missing content length")?
                .parse()?;
            let mut body = vec![0; len];
            stream.read_exact(&mut body)?;
            let body: Value = serde_json::from_slice(&body)?;
            let method = body["method"]
                .as_str()
                .context("missing method")?
                .to_owned();
            let response = json!({"id":1,"result":null,"error":{"code":-28}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )?;
            Ok(method)
        });
        let cookie = root.join("cookie");
        std::fs::write(&cookie, "user:password")?;
        let core = Core::new(format!("http://{address}"), cookie)?;
        let result = commit_verified_prune(
            &index,
            &core,
            &root,
            &root.join("audit"),
            &root.join("backup"),
            &AtomicBool::new(false),
        );
        assert!(result.is_err());
        assert_eq!(
            worker.join().expect("mock RPC thread")?,
            "getblockchaininfo"
        );
        assert!(root.join("prune-pending").is_file());
        assert!(!root.join("prune-complete-1000").exists());
        drop(index);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
