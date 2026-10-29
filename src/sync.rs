use crate::{
    archive::Archive,
    rpc::Core,
    store::{Store, UNDO_WINDOW},
    Chain,
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

// Four independent RPCs at most. Results retain height order and are never published
// until the entire group succeeds; SQLite/archive remain on the single writer thread.
fn fetch_group(core: &Core, start: u32, end: u32) -> Result<Vec<(u32, bitcoin::Block)>> {
    std::thread::scope(|scope| {
        let workers: Vec<_> = (start..=end)
            .map(|height| {
                scope.spawn(move || -> Result<_> {
                    let hash = core.hash(height)?;
                    let block = core.block(hash)?;
                    ensure!(
                        core.hash(height)? == hash,
                        "Core reorg while fetching block"
                    );
                    Ok((height, block))
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("block fetch worker panicked"))?
            })
            .collect()
    })
}

pub fn batch(
    store: &mut Store,
    archive: &mut Archive,
    core: &Core,
    chain: Chain,
    maximum: u32,
    to_height: Option<u32>,
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Value> {
    ensure!(maximum > 0, "batch size must be positive");
    let info = core.verify(chain)?;
    let core_height: u32 = info["blocks"]
        .as_u64()
        .context("missing Core height")?
        .try_into()?;
    if let Some((height, hash)) = store.stats()?.tip {
        if height > core_height || core.hash(height)? != hash {
            let mut ancestor = height.min(core_height);
            loop {
                ensure!(
                    !stop.is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Relaxed)),
                    "shutdown requested"
                );
                ensure!(
                    height - ancestor <= UNDO_WINDOW,
                    "reorg exceeds undo window; safe reconstruction required"
                );
                let core_hash = core.hash(ancestor)?;
                if store
                    .record(ancestor)?
                    .is_some_and(|(hash, _)| hash == core_hash)
                {
                    break;
                }
                ensure!(ancestor > 0, "Core genesis differs");
                ancestor -= 1;
            }
            store.rewind(archive, ancestor)?;
        }
    }
    let start = store.stats()?.tip.map_or(0, |(h, _)| h + 1);
    let end = to_height
        .unwrap_or(core_height)
        .min(core_height)
        .min(start.saturating_add(maximum - 1));
    if start <= end {
        if info["pruned"] == true {
            let floor = info["pruneheight"]
                .as_u64()
                .context("missing Core pruneheight")?;
            ensure!(
                u64::from(start.max(1)) >= floor || end == 0,
                "required historical blocks were already pruned; restore trusted history"
            );
        }
        let anchor = core.hash(end)?;
        let mut pending = Vec::new();
        let mut pending_bytes = 0usize;
        let mut height = start;
        loop {
            ensure!(
                !stop.is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Relaxed)),
                "shutdown requested"
            );
            ensure!(
                core.hash(end)? == anchor,
                "Core changed branch during sync; retry safely"
            );
            let last = height.saturating_add(3).min(end);
            for (block_height, block) in fetch_group(core, height, last)? {
                let size = bitcoin::consensus::serialize(&block).len();
                if !pending.is_empty()
                    && (pending.len() == 64 || pending_bytes.saturating_add(size) > 16_000_000)
                {
                    ensure!(
                        core.hash(end)? == anchor,
                        "Core changed branch during sync; retry safely"
                    );
                    store.commit_batch(archive, &pending)?;
                    pending.clear();
                    pending_bytes = 0;
                }
                pending_bytes = pending_bytes
                    .checked_add(size)
                    .context("batch byte count overflow")?;
                pending.push((block_height, block));
            }
            if last == end {
                break;
            }
            height = last + 1;
        }
        ensure!(
            core.hash(end)? == anchor,
            "Core reorg at batch end; retry safely"
        );
        if !pending.is_empty() {
            store.commit_batch(archive, &pending)?;
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn concurrent_rpc_failures_are_propagated_without_interpreting_a_fork() {
        let root = tempfile::tempdir().unwrap();
        let cookie = root.path().join("cookie");
        std::fs::write(&cookie, "user:secret").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).unwrap();
                let body = r#"{"id":"electrum-lite","error":{"code":-28,"message":"secret"},"result":null}"#;
                write!(stream, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        let core = Core::new(&url, &cookie).unwrap();
        let error = fetch_group(&core, 1, 4).unwrap_err();
        assert!(error
            .downcast_ref::<crate::rpc::RpcFailure>()
            .is_some_and(|e| e.code == -28));
        assert!(!format!("{error:#}").contains("secret"));
        server.join().unwrap();
    }
}
