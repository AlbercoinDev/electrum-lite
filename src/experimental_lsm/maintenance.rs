//! Maintenance runs with exclusive store ownership and an offline dedicated Core.
//! A requested record is never automatically cleared after uncertain RPC results.
use super::BitcoinStore;
use crate::{rpc::Core, Chain};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct PruneAudit {
    pub requested: u32,
    pub tip_height: u32,
    pub tip_hash: bitcoin::BlockHash,
    pub backup: std::path::PathBuf,
    pub actual: Option<i64>,
}

impl BitcoinStore {
    pub fn prune_checkpoint(&self) -> Result<Option<PruneAudit>> {
        ensure!(self.ready, "LSM recovery required");
        let path = self.archive.directory().join("prune-audit.jsonl");
        let mut audit = None;
        match File::open(&path) {
            Ok(file) => {
                ensure!(!path.is_symlink(), "prune audit symlink refused");
                let mut bytes = Vec::new();
                file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= 4 * 1024 * 1024 && bytes.last() == Some(&b'\n'),
                    "oversized or torn prune audit; review required"
                );
                for line in bytes
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                {
                    audit = Some(serde_json::from_slice::<PruneAudit>(line)?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            audit.as_ref().is_none_or(|audit| audit.actual.is_some()),
            "unfinished LSM prune audit; review and recover into a new root, never clear to bypass"
        );
        Ok(audit)
    }

    fn write_prune_audit(&mut self, audit: &PruneAudit) -> Result<()> {
        let path = self.archive.directory().join("prune-audit.jsonl");
        ensure!(!path.is_symlink(), "prune audit symlink refused");
        if path.exists() {
            ensure!(
                fs::metadata(&path)?.len() < 4 * 1024 * 1024 - 65536,
                "prune audit capacity reached; preserve and recover into new root"
            );
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut bytes = serde_json::to_vec(audit)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(self.archive.directory())?.sync_all()?;
        Ok(())
    }

    /// No networking toggles here: the supervisor must stop service, disable P2P
    /// and exclude other chain writers before taking exclusive ownership.
    pub fn prune_offline(
        &mut self,
        core: &Core,
        backup: &Path,
        verified_restore: &Path,
        min_free: u64,
        confirm_isolated: bool,
    ) -> Result<Value> {
        ensure!(
            self.chain != Chain::Mainnet,
            "mainnet pruning is gated; no unsafe override"
        );
        ensure!(
            confirm_isolated,
            "explicit isolated dedicated Core maintenance confirmation required"
        );
        self.prune_checkpoint()?;
        ensure!(
            fs2::available_space(self.archive.directory())? >= min_free,
            "insufficient maintenance free space"
        );
        let info = core.verify(self.chain)?;
        ensure!(
            info["pruned"] == true && info["automatic_pruning"] == false,
            "manual prune=1 required"
        );
        ensure!(
            core.call::<Value>("getnetworkinfo", json!([]))?["networkactive"] == false,
            "Core must be explicitly offline"
        );
        let _: Value = core.call("syncwithvalidationinterfacequeue", json!([]))?;
        let tip = self.state()?.tip.context("empty LSM index")?;
        ensure!(
            info["blocks"].as_u64() == Some(u64::from(tip.height))
                && core.hash(tip.height)? == tip.hash,
            "LSM must exactly match observed Core tip"
        );
        let requested = tip
            .height
            .checked_sub(288)
            .context("insufficient reorg margin")?;
        self.backup(backup)?;
        // Full manifest verification, DB recovery and semantic replay in a NEW
        // destination. Preserve both backup and verified generation for rollback.
        let restored = BitcoinStore::restore(backup, verified_restore, self.chain)?;
        ensure!(
            restored.state()? == self.state()?,
            "verified backup generation mismatch"
        );
        drop(restored);
        ensure!(
            core.call::<Value>("getnetworkinfo", json!([]))?["networkactive"] == false,
            "Core networking changed during preparation"
        );
        let before = core.verify(self.chain)?;
        ensure!(
            before["blocks"].as_u64() == Some(u64::from(tip.height))
                && core.hash(tip.height)? == tip.hash,
            "Core changed during backup preparation"
        );
        let mut audit = PruneAudit {
            requested,
            tip_height: tip.height,
            tip_hash: tip.hash,
            backup: backup.canonicalize()?,
            actual: None,
        };
        self.write_prune_audit(&audit)?;
        let actual: i64 = core.call("pruneblockchain", json!([requested]))?;
        ensure!(
            actual >= -1 && actual <= i64::from(requested),
            "unexpected prune result; audit remains unfinished"
        );
        let after = core.verify(self.chain)?;
        ensure!(
            after["blocks"].as_u64() == Some(u64::from(tip.height))
                && core.hash(tip.height)? == tip.hash,
            "Core changed during prune; audit remains unfinished"
        );
        ensure!(
            core.call::<Value>("getnetworkinfo", json!([]))?["networkactive"] == false,
            "Core networking changed during prune"
        );
        if actual >= 1 {
            let height = u32::try_from(actual)?;
            ensure!(
                after["pruneheight"]
                    .as_u64()
                    .is_some_and(|value| value > u64::from(height)),
                "prune height does not prove removal"
            );
            let hash = core.hash(height)?;
            let error = core
                .block(hash)
                .err()
                .context("Core still serves reported removed block")?;
            ensure!(
                error
                    .downcast_ref::<crate::rpc::RpcFailure>()
                    .is_some_and(|error| error.code == -1),
                "block unavailability not proven by Core; audit remains unfinished"
            );
            // Historical query must still come entirely from local archive.
            let snapshot = self.snapshot()?;
            let block = snapshot.block(height)?;
            ensure!(
                block.block_hash() == hash,
                "local historical branch mismatch"
            );
            for tx in &block.txdata {
                ensure!(
                    snapshot
                        .transaction(tx.compute_txid(), Some(height), crate::MAX_BLOCK_BYTES)?
                        .1
                        == bitcoin::consensus::serialize(tx),
                    "historical transaction bytes changed"
                );
                ensure!(
                    snapshot.proof(tx.compute_txid(), height)?.verify(),
                    "historical Merkle proof failed"
                );
            }
        }
        audit.actual = Some(actual);
        self.write_prune_audit(&audit)?;
        Ok(
            json!({"requested":requested,"actual":actual,"observed_pruneheight":after["pruneheight"],"backup":backup,"verified_restore":verified_restore}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unfinished_intent_survives_reopen_and_blocks_further_pruning() -> Result<()> {
        let root = tempfile::tempdir()?;
        let data = root.path().join("data");
        let mut store = BitcoinStore::create(&data, Chain::Regtest)?;
        store.commit(
            0,
            &bitcoin::blockdata::constants::genesis_block(Chain::Regtest.network()),
        )?;
        assert!(store.prune_checkpoint()?.is_none());
        let audit = PruneAudit {
            requested: 1,
            tip_height: 289,
            tip_hash: Chain::Regtest.genesis(),
            backup: root.path().join("backup"),
            actual: None,
        };
        store.write_prune_audit(&audit)?;
        assert!(store.prune_checkpoint().is_err());
        drop(store);
        let mut store = BitcoinStore::open(&data, Chain::Regtest)?;
        assert!(store.prune_checkpoint().is_err());
        let core = Core::new("http://127.0.0.1:1/", &root.path().join("missing-cookie"))?;
        assert!(store
            .prune_offline(
                &core,
                &root.path().join("backup"),
                &root.path().join("restore"),
                0,
                true
            )
            .unwrap_err()
            .to_string()
            .contains("unfinished"));
        assert!(!root.path().join("backup").exists());
        let backup = root.path().join("preserved-backup");
        store.backup(&backup)?;
        let restored =
            BitcoinStore::restore(&backup, &root.path().join("restored"), Chain::Regtest)?;
        assert!(restored.prune_checkpoint().is_err());
        drop(restored);
        let path = store.archive.directory().join("prune-audit.jsonl");
        OpenOptions::new()
            .append(true)
            .open(path)?
            .write_all(b"{torn")?;
        assert!(store
            .prune_checkpoint()
            .unwrap_err()
            .to_string()
            .contains("torn"));
        Ok(())
    }
}
