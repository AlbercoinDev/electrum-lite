use crate::core::Core;
use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256, Hash},
    Block, Network, Transaction, Txid,
};
use rocksdb::{Direction, IteratorMode, Options, WriteBatch, WriteOptions, DB};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

const WINDOW: u32 = 288;
pub struct Index {
    pub db: DB,
    pub network: Network,
    pub fetcher: crate::net::Fetcher,
    pub cache: std::path::PathBuf,
}
pub fn script_hash(script: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(script).to_byte_array()
}
pub fn key(prefix: u8, bytes: &[u8]) -> Vec<u8> {
    let mut k = vec![prefix];
    k.extend(bytes);
    k
}
pub fn height_key(prefix: u8, h: u32) -> Vec<u8> {
    key(prefix, &h.to_be_bytes())
}
fn write_options() -> WriteOptions {
    let mut w = WriteOptions::default();
    w.set_sync(true);
    w
}
impl Index {
    /// Full semantic replay into a NEW generation. Source and audit copy are preserved.
    pub fn audit(&self, core: &Core, path: &Path) -> Result<()> {
        self.audit_with_cancel(core, path, &std::sync::atomic::AtomicBool::new(false))
    }
    pub fn audit_with_cancel(
        &self,
        core: &Core,
        path: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        ensure!(!path.exists(), "audit destination must be NEW");
        let other = Self::open(path, self.network)?;
        let tip = self.tip()?.context("empty index")?;
        for h in 0..=tip {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "semantic audit interrupted; partial audit generation preserved"
            );
            let hash = self.header(h)?.block_hash();
            ensure!(
                core.call("getblockhash", json!([h]))? == hash.to_string(),
                "audit chain differs from Core"
            );
            let block = self.block_source(core, h, false)?;
            other.commit(h, &block)?;
        }
        ensure!(
            core.call("getbestblockhash", json!([]))? == self.header(tip)?.block_hash().to_string(),
            "chain changed during audit"
        );
        // A rewind can leave fewer than 288 retained undos; this is not a
        // confirmed-state difference. Compare every retained undo separately.
        let mut a = self
            .db
            .iterator(IteratorMode::Start)
            .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
        let mut b = other
            .db
            .iterator(IteratorMode::Start)
            .filter(|r| !r.as_ref().is_ok_and(|(k, _)| k.starts_with(b"U")));
        loop {
            match (a.next(), b.next()) {
                (None, None) => break,
                (Some(x), Some(y)) => {
                    ensure!(x? == y?, "semantic replay differs from index");
                }
                _ => anyhow::bail!("semantic replay key count mismatch"),
            }
        }
        for (k, v) in self.rows(b"U", WINDOW as usize)? {
            ensure!(
                other.db.get(k)?.as_deref() == Some(v.as_slice()),
                "retained undo differs from semantic replay"
            );
        }
        println!(
            "{} {tip}: {}",
            crate::i18n::text(
                "Semantic replay verified at height",
                "Reproducción semántica verificada en la altura"
            ),
            path.display()
        );
        Ok(())
    }
    pub fn backup(&self, path: &Path) -> Result<()> {
        ensure!(!path.exists(), "backup destination must be NEW");
        rocksdb::checkpoint::Checkpoint::new(&self.db)?.create_checkpoint(path)?;
        let check = DB::open_for_read_only(&Options::default(), path, false)?;
        ensure!(
            check.get(b"identity")? == self.db.get(b"identity")?
                && check.get(b"tip")? == self.db.get(b"tip")?,
            "backup verification failed"
        );
        for item in check.iterator(IteratorMode::Start) {
            let (k, v) = item?;
            ensure!(
                self.db.get(&k)?.as_deref() == Some(v.as_ref()),
                "backup content mismatch"
            );
        }
        std::fs::File::open(path)?.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?
            .sync_all()?;
        }
        println!(
            "{}: {} ({})",
            crate::i18n::text("Verified checkpoint", "Copia de seguridad verificada"),
            path.display(),
            crate::i18n::text(
                "historical cache is replaceable and not included",
                "la caché histórica es reemplazable y no está incluida"
            )
        );
        Ok(())
    }
    pub fn open(path: &Path, network: Network) -> Result<Self> {
        ensure!(!path.is_symlink(), "index symlink refused");
        std::fs::create_dir_all(path)?;
        let mut o = Options::default();
        o.create_if_missing(true);
        o.set_write_buffer_size(32 * 1024 * 1024);
        let db = DB::open(&o, path)?;
        let identity = format!("thin-v1:{network}");
        if let Some(v) = db.get(b"identity")? {
            ensure!(v == identity.as_bytes(), "wrong network or database format");
        } else {
            ensure!(
                db.iterator(IteratorMode::Start).next().is_none(),
                "unrecognized nonempty database"
            );
            db.put_opt(b"identity", identity, &write_options())?;
        }
        Ok(Self {
            db,
            network,
            fetcher: crate::net::Fetcher {
                peers: Vec::new(),
                proxy: None,
            },
            cache: path.parent().context("index parent")?.join("cache"),
        })
    }
    pub fn tip(&self) -> Result<Option<u32>> {
        self.db
            .get(b"tip")?
            .map(|b| Ok(u32::from_be_bytes(b.as_slice().try_into()?)))
            .transpose()
    }
    pub fn rows(&self, prefix: &[u8], max: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for row in self
            .db
            .iterator(IteratorMode::From(prefix, Direction::Forward))
        {
            let (k, v) = row?;
            if !k.starts_with(prefix) {
                break;
            }
            ensure!(out.len() < max, "query limit exceeded; no partial response");
            out.push((k.to_vec(), v.to_vec()));
        }
        Ok(out)
    }
    pub fn block_meta(&self, h: u32) -> Result<Vec<u8>> {
        let meta = self
            .db
            .get(height_key(b'B', h))?
            .context("height not indexed")?;
        ensure!(
            meta.len() >= 112 && (meta.len() - 80) % 32 == 0,
            "corrupt block metadata"
        );
        Ok(meta)
    }
    pub fn header(&self, h: u32) -> Result<bitcoin::block::Header> {
        Ok(deserialize(&self.block_meta(h)?[..80])?)
    }
    pub fn commit(&self, h: u32, block: &Block) -> Result<()> {
        self.commit_with_sync(h, block, true)
    }
    fn commit_with_sync(&self, h: u32, block: &Block, durable: bool) -> Result<()> {
        crate::net::verify_at(block, block.block_hash(), self.network, h)?;
        match self.tip()? {
            Some(t) => ensure!(
                h == t + 1 && block.header.prev_blockhash == self.header(t)?.block_hash(),
                "chain discontinuity"
            ),
            None => ensure!(
                h == 0
                    && block.block_hash()
                        == bitcoin::blockdata::constants::genesis_block(self.network).block_hash(),
                "wrong genesis"
            ),
        }
        let mut delta: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut meta = serialize(&block.header);
        for (pos, tx) in block.txdata.iter().enumerate() {
            let txid = tx.compute_txid();
            meta.extend(txid.as_byte_array());
            let mut occurrence = h.to_be_bytes().to_vec();
            occurrence.extend((pos as u32).to_be_bytes());
            let mut tk = key(b'T', &txid.as_byte_array()[..8]);
            tk.extend(&occurrence);
            delta.insert(tk, Some(Vec::new()));
            let mut scripts = std::collections::BTreeSet::new();
            if !tx.is_coinbase() {
                for input in &tx.input {
                    let outpoint = serialize(&input.previous_output);
                    let ck = key(b'C', &outpoint);
                    let coin = match delta.get(&ck) {
                        Some(v) => v.clone(),
                        None => self.db.get(&ck)?,
                    }
                    .context("missing prevout")?;
                    ensure!(coin.len() == 48, "corrupt coin");
                    let script: [u8; 32] = coin[..32].try_into()?;
                    scripts.insert(script);
                    let mut sk = key(b'S', &script);
                    ensure!(coin.len() == 48, "corrupt coin");
                    sk.extend(&coin[40..48]);
                    sk.extend(input.previous_output.vout.to_be_bytes());
                    delta.insert(ck, None);
                    delta.insert(sk, None);
                }
            }
            for (vout, output) in tx.output.iter().enumerate() {
                let script = script_hash(output.script_pubkey.as_bytes());
                scripts.insert(script);
                // History includes unspendable outputs; only the UTXO set excludes them.
                if h == 0
                    || output.script_pubkey.is_op_return()
                    || output.script_pubkey.len() > 10_000
                {
                    continue;
                }
                let op = bitcoin::OutPoint::new(txid, vout as u32);
                let bytes = serialize(&op);
                let ck = key(b'C', &bytes);
                let previous = match delta.get(&ck) {
                    Some(v) => v.clone(),
                    None => self.db.get(&ck)?,
                };
                if let Some(old) = previous {
                    ensure!(old.len() == 48, "corrupt previous coin");
                    let mut old_script = key(b'S', &old[..32]);
                    old_script.extend(&old[40..48]);
                    old_script.extend((vout as u32).to_be_bytes());
                    delta.insert(old_script, None);
                }
                let mut coin = script.to_vec();
                coin.extend(output.value.to_sat().to_le_bytes());
                coin.extend(h.to_be_bytes());
                coin.extend((pos as u32).to_be_bytes());
                delta.insert(ck, Some(coin.clone()));
                let mut sk = key(b'S', &script);
                sk.extend(&occurrence);
                sk.extend((vout as u32).to_be_bytes());
                delta.insert(sk, Some(coin[32..40].to_vec()));
            }
            if h > 0 {
                for script in scripts {
                    let mut hk = key(b'H', &script);
                    hk.extend(&occurrence);
                    delta.insert(hk, Some(Vec::new()));
                }
            }
        }
        delta.insert(height_key(b'B', h), Some(meta));
        delta.insert(b"tip".to_vec(), Some(h.to_be_bytes().to_vec()));
        let mut undo = Vec::new();
        for k in delta.keys() {
            undo.extend((k.len() as u32).to_le_bytes());
            undo.extend(k);
            match self.db.get(k)? {
                Some(v) => {
                    undo.extend((v.len() as u32).to_le_bytes());
                    undo.extend(v);
                }
                None => undo.extend(u32::MAX.to_le_bytes()),
            }
        }
        let mut batch = WriteBatch::default();
        for (k, v) in delta {
            match v {
                Some(v) => batch.put(k, v),
                None => batch.delete(k),
            }
        }
        batch.put(height_key(b'U', h), undo);
        if h >= WINDOW {
            batch.delete(height_key(b'U', h - WINDOW));
        }
        let mut options = write_options();
        options.set_sync(durable);
        self.db.write_opt(batch, &options)?;
        Ok(())
    }
    pub fn rewind_one(&self) -> Result<()> {
        let h = self.tip()?.context("empty index")?;
        let undo = self
            .db
            .get(height_key(b'U', h))?
            .context("undo unavailable: restore backup or rebuild into NEW root")?;
        let mut bytes = undo.as_slice();
        let mut batch = WriteBatch::default();
        fn number(b: &mut &[u8]) -> Result<u32> {
            ensure!(b.len() >= 4, "corrupt undo");
            let n = u32::from_le_bytes(b[..4].try_into()?);
            *b = &b[4..];
            Ok(n)
        }
        while !bytes.is_empty() {
            let n = number(&mut bytes)? as usize;
            ensure!(bytes.len() >= n, "corrupt undo key");
            let k = bytes[..n].to_vec();
            bytes = &bytes[n..];
            let n = number(&mut bytes)?;
            if n == u32::MAX {
                batch.delete(k);
            } else {
                let n = n as usize;
                ensure!(bytes.len() >= n, "corrupt undo value");
                batch.put(k, &bytes[..n]);
                bytes = &bytes[n..];
            }
        }
        batch.delete(height_key(b'U', h));
        self.db.write_opt(batch, &write_options())?;
        Ok(())
    }
    pub fn sync(&self, core: &Core) -> Result<()> {
        self.sync_batch(core, u32::MAX)
    }
    pub fn sync_batch(&self, core: &Core, limit: u32) -> Result<()> {
        self.sync_batch_with_cancel(core, limit, &std::sync::atomic::AtomicBool::new(false))
    }
    pub fn sync_batch_with_cancel(
        &self,
        core: &Core,
        limit: u32,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        let info = core.call("getblockchaininfo", json!([]))?;
        let expected = match self.network {
            Network::Bitcoin => "main",
            Network::Testnet4 => "testnet4",
            Network::Signet => "signet",
            Network::Regtest => "regtest",
            _ => "unsupported",
        };
        ensure!(info["chain"] == expected, "Core network mismatch");
        // Detect deep reorg before changing anything.
        let mut ancestor = self.tip()?;
        let mut depth = 0;
        while let Some(h) = ancestor {
            if h as u64 <= info["blocks"].as_u64().context("missing Core height")?
                && core.call("getblockhash", json!([h]))?
                    == self.header(h)?.block_hash().to_string()
            {
                break;
            }
            depth += 1;
            ensure!(depth <= WINDOW, "reorg exceeds undo window");
            ancestor = h.checked_sub(1);
        }
        while self.tip()? != ancestor {
            self.rewind_one()?;
        }
        let end = info["blocks"].as_u64().context("missing height")? as u32;
        let start = self.tip()?.map_or(0, |h| h + 1);
        let end = end.min(start.saturating_add(limit.saturating_sub(1)));
        for h in start..=end {
            ensure!(
                !cancel.load(std::sync::atomic::Ordering::Relaxed),
                "synchronization interrupted"
            );
            let hash = core.call("getblockhash", json!([h]))?;
            let hash = hash.as_str().context("invalid hash")?;
            let block = core.block(hash).context(
                "Core pruned required history: stop and recover missing blocks; never skip heights",
            )?;
            ensure!(
                block.block_hash().to_string() == hash,
                "RPC block hash mismatch"
            );
            self.commit_with_sync(h, &block, h == end || (h - start + 1) % 64 == 0)?;
            if h % 1000 == 0 {
                eprintln!("{} {h}/{end}", crate::i18n::text("indexed", "indexados"));
            }
        }
        Ok(())
    }
    pub fn history(&self, script: &[u8; 32]) -> Result<Value> {
        let rows = self.rows(&key(b'H', script), 100_000)?;
        let mut result = Vec::new();
        let mut previous = None;
        let mut meta = Vec::new();
        for (k, _) in rows {
            ensure!(k.len() == 41, "corrupt history key");
            let h = u32::from_be_bytes(k[33..37].try_into()?);
            let pos = u32::from_be_bytes(k[37..41].try_into()?);
            if previous != Some(h) {
                meta = self.block_meta(h)?;
                previous = Some(h);
            }
            let offset = 80 + pos as usize * 32;
            let txid = Txid::from_slice(
                meta.get(offset..offset + 32)
                    .context("corrupt history position")?,
            )?;
            result.push(json!({"height":h,"tx_hash":txid.to_string()}));
        }
        Ok(json!(result))
    }
    pub fn unspent(&self, script: &[u8; 32]) -> Result<Value> {
        let mut out = Vec::new();
        let mut previous = None;
        let mut meta = Vec::new();
        for (k, v) in self.rows(&key(b'S', script), 100_000)? {
            ensure!(k.len() == 45 && v.len() == 8, "corrupt unspent entry");
            let h = u32::from_be_bytes(k[33..37].try_into()?);
            let pos = u32::from_be_bytes(k[37..41].try_into()?);
            let vout = u32::from_be_bytes(k[41..45].try_into()?);
            if previous != Some(h) {
                meta = self.block_meta(h)?;
                previous = Some(h);
            }
            let offset = 80 + pos as usize * 32;
            let txid = Txid::from_slice(
                meta.get(offset..offset + 32)
                    .context("corrupt unspent position")?,
            )?;
            out.push(json!({"tx_hash":txid.to_string(),"tx_pos":vout,"value":u64::from_le_bytes(v[..8].try_into()?),"height":h}));
        }
        Ok(Value::Array(out))
    }
    pub fn locate(&self, txid: Txid) -> Result<(u32, u32)> {
        self.locate_at(txid, None)
    }
    pub fn locate_at(&self, txid: Txid, height: Option<u32>) -> Result<(u32, u32)> {
        let rows = self.rows(&key(b'T', &txid.as_byte_array()[..8]), 100)?;
        for (k, _) in rows.into_iter().rev() {
            ensure!(k.len() == 17, "corrupt txid entry");
            let h = u32::from_be_bytes(k[9..13].try_into()?);
            if height.is_some_and(|wanted| wanted != h) {
                continue;
            }
            let pos = u32::from_be_bytes(k[13..17].try_into()?);
            let meta = self.block_meta(h)?;
            let offset = 80 + pos as usize * 32;
            if meta.get(offset..offset + 32) == Some(txid.as_byte_array().as_slice()) {
                return Ok((h, pos));
            }
        }
        anyhow::bail!("transaction not indexed")
    }
    pub fn transaction(&self, core: &Core, txid: Txid) -> Result<Transaction> {
        let (h, _) = self.locate(txid)?;
        self.block_at(core, h)?
            .txdata
            .into_iter()
            .find(|t| t.compute_txid() == txid)
            .context("transaction absent from block")
    }
    fn block_at(&self, core: &Core, h: u32) -> Result<Block> {
        self.block_source(core, h, true)
    }
    fn block_source(&self, core: &Core, h: u32, persist: bool) -> Result<Block> {
        let hash = self.header(h)?.block_hash();
        if self.cache.join(format!("{hash}.blk")).exists() {
            let block = self.fetcher.get(self.network, hash, h, &self.cache)?;
            return Ok(block);
        }
        let block = match core.block(&hash.to_string()) {
            Ok(b) => {
                crate::net::verify_at(&b, hash, self.network, h)?;
                b
            }
            Err(_) => {
                if self.fetcher.peers.is_empty() {
                    let peers = core.call("getpeerinfo", json!([]))?;
                    let peers = peers
                        .as_array()
                        .context("peer list")?
                        .iter()
                        .filter_map(|p| p["addr"].as_str().map(str::to_owned))
                        .collect();
                    crate::net::Fetcher {
                        peers,
                        proxy: self.fetcher.proxy.clone(),
                    }
                    .retrieve_for_index(
                        self.network,
                        hash,
                        h,
                        &self.cache,
                        persist,
                    )?
                } else {
                    self.fetcher
                        .retrieve_for_index(self.network, hash, h, &self.cache, persist)?
                }
            }
        };
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genesis_and_rewind() -> Result<()> {
        let path = std::env::temp_dir().join(format!("el-thin-test-{}", std::process::id()));
        let i = Index::open(&path, Network::Regtest)?;
        let b = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        i.commit(0, &b)?;
        assert_eq!(i.tip()?, Some(0));
        assert_eq!(i.header(0)?, b.header);
        assert!(i.commit(2, &b).is_err());
        i.rewind_one()?;
        assert_eq!(i.tip()?, None);
        i.commit(0, &b)?;
        let mut b1 = b.clone();
        b1.header.prev_blockhash = b.block_hash();
        b1.header.time += 1;
        i.commit(1, &b1)?;
        let mut b2 = b1.clone();
        b2.header.prev_blockhash = b1.block_hash();
        b2.header.time += 1;
        i.commit(2, &b2)?;
        let script = script_hash(b.txdata[0].output[0].script_pubkey.as_bytes());
        assert_eq!(i.history(&script)?.as_array().expect("array").len(), 2);
        let coins = i.unspent(&script)?;
        assert_eq!(coins.as_array().expect("array").len(), 1);
        assert_eq!(coins[0]["height"], 2);
        assert_eq!(i.locate(b.txdata[0].compute_txid())?, (2, 0));
        i.rewind_one()?;
        assert_eq!(i.unspent(&script)?[0]["height"], 1);
        drop(i);
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
