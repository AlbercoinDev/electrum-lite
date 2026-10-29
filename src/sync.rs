use crate::{
    archive::Archive,
    rpc::Core,
    store::{Store, UNDO_WINDOW},
    Chain,
};
use anyhow::{ensure, Context, Result};
use bitcoin::{Block, BlockHash};
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

// One fixed worker pool per synchronization call. Groups preserve order; no
// thread is spawned per block and no result is published before the whole
// group succeeds.
const FETCH_GROUP: u32 = 4;
const PIPELINE_DEPTH: usize = 1;

fn fetch_workers() -> usize {
    std::thread::available_parallelism().map_or(4, |workers| workers.get().clamp(2, 8))
}

#[derive(Debug)]
struct Fetched {
    height: u32,
    block: Block,
    size: usize,
}

/// Fetch a contiguous group and reject a Core branch change inside it using the
/// local `prev_blockhash` chain. Returns the consensus size measured while
/// decoding, so callers never re-serialize the block for accounting.
fn fetch_group(core: &Core, start: u32, end: u32) -> Result<Vec<Fetched>> {
    let mut fetched = Vec::with_capacity((end - start + 1) as usize);
    let mut previous: Option<BlockHash> = None;
    for (height, (block, size)) in (start..=end).zip(core.block_group(start, end)?) {
        let hash = block.block_hash();
        if let Some(expected) = previous {
            ensure!(
                block.header.prev_blockhash == expected,
                "Core block chain discontinuity while fetching"
            );
        }
        previous = Some(hash);
        fetched.push(Fetched {
            height,
            block,
            size,
        });
    }
    Ok(fetched)
}

#[derive(Default)]
struct CommitMetrics {
    time: Duration,
    blocks: usize,
    bytes: usize,
}

/// Ordered publisher: fetched groups arrive out of order, but every batch is
/// committed in height order and only after Core still reports the anchor.
struct Publisher<'a> {
    store: &'a mut Store,
    archive: &'a mut Archive,
    core: &'a Core,
    anchor: BlockHash,
    end: u32,
    pending: Vec<(u32, Block)>,
    pending_bytes: usize,
    metrics: CommitMetrics,
}

impl Publisher<'_> {
    fn commit(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        ensure!(
            self.core.hash(self.end)? == self.anchor,
            "Core changed branch during sync; retry safely"
        );
        let started = Instant::now();
        self.store.commit_batch(self.archive, &self.pending)?;
        self.metrics.time += started.elapsed();
        self.metrics.blocks += self.pending.len();
        self.metrics.bytes += self.pending_bytes;
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }

    fn push(&mut self, item: Fetched) -> Result<()> {
        if !self.pending.is_empty()
            && (self.pending.len() >= 16
                || self.pending_bytes.saturating_add(item.size) > 16_000_000)
        {
            self.commit()?;
        }
        self.pending_bytes = self
            .pending_bytes
            .checked_add(item.size)
            .context("batch byte count overflow")?;
        self.pending.push((item.height, item.block));
        Ok(())
    }
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
    let started = Instant::now();
    let mut fetch_wait = Duration::ZERO;
    let mut metrics = CommitMetrics::default();
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
        let groups = (u64::from(end - start + 1)).div_ceil(u64::from(FETCH_GROUP)) as usize;
        let failed = AtomicBool::new(false);
        metrics = std::thread::scope(|scope| -> Result<CommitMetrics> {
            let workers = fetch_workers();
            // One bounded queue per worker, consumed in deterministic order.
            // A slow first group cannot grow an unbounded reorder buffer.
            // Receivers live inside the scope closure so errors drop them before
            // scoped threads are joined, waking blocked senders.
            let mut receivers = Vec::with_capacity(workers);
            for worker in 0..workers {
                let (sender, receiver) = mpsc::sync_channel::<Result<Vec<Fetched>>>(PIPELINE_DEPTH);
                receivers.push(receiver);
                let failed = &failed;
                scope.spawn(move || {
                    for index in (worker..groups).step_by(workers) {
                        if failed.load(Ordering::Relaxed) {
                            break;
                        }
                        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
                            let _ = sender.send(Err(anyhow::anyhow!("shutdown requested")));
                            break;
                        }
                        let group_start =
                            u32::try_from(u64::from(start) + index as u64 * u64::from(FETCH_GROUP))
                                .expect("group starts at or before end");
                        let group_end = group_start.saturating_add(FETCH_GROUP - 1).min(end);
                        let result = fetch_group(core, group_start, group_end);
                        if result.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        if sender.send(result).is_err() {
                            break;
                        }
                    }
                });
            }
            let mut publisher = Publisher {
                store,
                archive,
                core,
                anchor,
                end,
                pending: Vec::new(),
                pending_bytes: 0,
                metrics: CommitMetrics::default(),
            };
            for expected in 0..groups {
                let waiting = Instant::now();
                let group = receivers[expected % workers]
                    .recv()
                    .context("block fetch workers stopped unexpectedly")??;
                fetch_wait += waiting.elapsed();
                for item in group {
                    publisher.push(item)?;
                }
            }
            publisher.commit()?;
            ensure!(
                core.hash(end)? == anchor,
                "Core changed branch during sync; retry safely"
            );
            Ok(publisher.metrics)
        })?;
    }
    let mut info = info;
    info["electrum_sync_metrics"] = serde_json::json!({
        "batch_blocks": metrics.blocks,
        "batch_bytes": metrics.bytes,
        "total_ms": started.elapsed().as_millis(),
        "fetch_wait_ms": fetch_wait.as_millis(),
        "commit_ms": metrics.time.as_millis()
    });
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
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).unwrap();
                let body = r#"[{"id":0,"error":{"code":-28,"message":"secret"},"result":null},{"id":1,"error":null,"result":null},{"id":2,"error":null,"result":null},{"id":3,"error":null,"result":null}]"#;
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
