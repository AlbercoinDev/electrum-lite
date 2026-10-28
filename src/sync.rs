use crate::{
    archive::Archive,
    rpc::Core,
    store::{Store, UNDO_WINDOW},
    Chain,
};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

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
        for height in start..=end {
            ensure!(
                !stop.is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Relaxed)),
                "shutdown requested"
            );
            ensure!(
                core.hash(end)? == anchor,
                "Core changed branch during sync; retry safely"
            );
            let hash = core.hash(height)?;
            let block = core.block(hash)?;
            ensure!(
                core.hash(height)? == hash,
                "Core reorg while fetching block"
            );
            store.commit(archive, height, &block)?;
        }
        ensure!(
            core.hash(end)? == anchor,
            "Core reorg at batch end; retry safely"
        );
    }
    Ok(info)
}
