//! Machine-derived resource budget.
//!
//! Defaults are intentionally conservative: one core and ~20% of RAM are left
//! outside the indexer so Core, Tor, the OS and query bursts are not starved.
use crate::index::StorageProfile;
use anyhow::Result;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub cpus: usize,
    pub ram_mib: u64,
    pub core_par: u32,
    pub index_workers: usize,
    pub rocks_jobs: u32,
    pub rocks_subcompactions: u32,
    pub core_dbcache_mib: u32,
    pub core_mempool_mib: u32,
    pub block_cache_mib: u64,
    pub write_buffer_mib: u64,
    pub memtables: u32,
    pub pipeline_mib: u64,
    pub min_free_mib: u64,
    pub storage: StorageProfile,
}

fn cgroup_cpu_limit() -> Option<usize> {
    let text = std::fs::read_to_string("/sys/fs/cgroup/cpu.max").ok()?;
    let mut parts = text.split_whitespace();
    let quota = parts.next()?;
    let period = parts.next()?.parse::<u64>().ok()?;
    if quota == "max" || period == 0 {
        return None;
    }
    let quota = quota.parse::<u64>().ok()?;
    Some(quota.div_ceil(period) as usize)
}

fn cgroup_memory_mib() -> Option<u64> {
    let path = if Path::new("/sys/fs/cgroup/memory.max").exists() {
        "/sys/fs/cgroup/memory.max"
    } else {
        "/sys/fs/cgroup/memory/memory.limit_in_bytes"
    };
    let text = std::fs::read_to_string(path).ok()?;
    let bytes = text.trim().parse::<u64>().ok()?;
    // v1 uses a huge sentinel when unlimited.
    if bytes == 0 || bytes >= u64::MAX / 2 || bytes > (1u64 << 50) {
        return None;
    }
    Some(bytes / (1024 * 1024))
}

fn machine_ram_mib() -> u64 {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(line) = text.lines().find(|line| line.starts_with("MemTotal:")) {
            if let Some(kb) = line.split_whitespace().nth(1) {
                if let Ok(kb) = kb.parse::<u64>() {
                    return kb / 1024;
                }
            }
        }
    }
    4096
}

fn available_disk_mib(path: &Path) -> Option<u64> {
    let path = path.to_str()?;
    let cpath = std::ffi::CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let bytes = (stat.f_bavail as u128) * (stat.f_frsize as u128);
    Some((bytes / (1024 * 1024)) as u64)
}

impl Budget {
    pub fn detect(data_dir: &Path, storage: StorageProfile) -> Result<Self> {
        let cpus = cgroup_cpu_limit()
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(2)
            })
            .max(1);
        let ram_mib = cgroup_memory_mib().unwrap_or_else(machine_ram_mib).max(512);
        let disk_mib = available_disk_mib(data_dir).unwrap_or(0);
        Ok(Self::from_resources(cpus, ram_mib, disk_mib, storage))
    }

    pub fn from_resources(
        cpus: usize,
        ram_mib: u64,
        disk_mib: u64,
        storage: StorageProfile,
    ) -> Self {
        let reserved_cpu = ((cpus as f64 * 0.15).ceil() as usize).max(1);
        let usable_cpu = cpus.saturating_sub(reserved_cpu).max(1);
        let core_par = (usable_cpu / 2).clamp(1, 4) as u32;
        let lite_cpu = usable_cpu.saturating_sub(core_par as usize).max(1);
        let index_workers = lite_cpu.clamp(1, 3);
        let rocks_jobs = lite_cpu.div_ceil(2).clamp(1, 2) as u32;
        let rocks_subcompactions = if storage == StorageProfile::Ssd {
            rocks_jobs
        } else {
            1
        };

        let reserve_os = ((ram_mib as f64 * 0.20) as u64).max(1024);
        let core_dbcache_mib = ((ram_mib as f64 * 0.08) as u64).clamp(128, 1024) as u32;
        let core_mempool_mib = ((ram_mib as f64 * 0.02) as u64).clamp(32, 256) as u32;
        let other = 300 + core_dbcache_mib as u64 + core_mempool_mib as u64;
        let lite_budget = ram_mib.saturating_sub(reserve_os + other).max(512);
        let block_cache_mib = ((lite_budget as f64 * 0.40) as u64).clamp(256, 4096);
        let write_buffer_mib = ((lite_budget as f64 * 0.06) as u64).clamp(32, 256);
        let memtables = if ram_mib >= 8192 { 4 } else { 3 };
        let pipeline_mib = ((lite_budget as f64 * 0.15) as u64).clamp(64, 512);
        let min_free_mib = ((disk_mib as f64 * 0.10) as u64).clamp(2048, 20480);

        Self {
            cpus,
            ram_mib,
            core_par,
            index_workers,
            rocks_jobs,
            rocks_subcompactions,
            core_dbcache_mib,
            core_mempool_mib,
            block_cache_mib,
            write_buffer_mib,
            memtables,
            pipeline_mib,
            min_free_mib,
            storage,
        }
    }

    pub fn override_resources(
        &mut self,
        cpus: Option<usize>,
        ram_mib: Option<u64>,
        workers: Option<usize>,
        core_par: Option<u32>,
    ) {
        if let Some(cpus) = cpus {
            self.cpus = cpus.max(1);
        }
        if let Some(ram) = ram_mib {
            self.ram_mib = ram.max(512);
        }
        if let Some(workers) = workers {
            self.index_workers = workers.clamp(1, 8);
        }
        if let Some(core_par) = core_par {
            self.core_par = core_par.clamp(1, 16);
        }
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.cpus >= 1, "CPU budget must be at least 1");
        anyhow::ensure!(self.ram_mib >= 512, "RAM budget must be at least 512 MiB");
        anyhow::ensure!(
            self.block_cache_mib + self.write_buffer_mib * u64::from(self.memtables) < self.ram_mib,
            "RocksDB budget exceeds total RAM"
        );
        anyhow::ensure!(
            self.core_dbcache_mib >= 64,
            "Core dbcache below its minimum"
        );
        Ok(())
    }

    pub fn log(&self) {
        eprintln!(
            "budget: cpus={} ram={}MiB core_par={} index_workers={} rocks_jobs={} cache={}MiB write_buffer={}MiB pipeline={}MiB core_dbcache={}MiB mempool={}MiB min_free={}MiB storage={:?}",
            self.cpus,
            self.ram_mib,
            self.core_par,
            self.index_workers,
            self.rocks_jobs,
            self.block_cache_mib,
            self.write_buffer_mib,
            self.pipeline_mib,
            self.core_dbcache_mib,
            self.core_mempool_mib,
            self.min_free_mib,
            self.storage
        );
    }

    pub fn detect_or_default(data_dir: &Path, storage: StorageProfile) -> Self {
        Self::detect(data_dir, storage).unwrap_or_else(|error| {
            eprintln!("budget detection failed: {error:#}; using 2 CPU / 4096 MiB defaults");
            Self::from_resources(2, 4096, 0, storage)
        })
    }

    pub fn ensure_fits_disk(&self, data_dir: &Path) -> Result<()> {
        if let Some(free) = available_disk_mib(data_dir) {
            anyhow::ensure!(
                free > self.min_free_mib,
                "free disk {free} MiB is below the reserve {} MiB",
                self.min_free_mib
            );
        }
        Ok(())
    }
}

pub fn parse_bool_env(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_machine_keeps_a_core_and_memory_in_reserve() {
        let b = Budget::from_resources(2, 4096, 300_000, StorageProfile::Hdd);
        assert_eq!(b.core_par, 1);
        assert_eq!(b.index_workers, 1);
        assert_eq!(b.rocks_jobs, 1);
        assert!(b.block_cache_mib >= 256);
        assert!(b.block_cache_mib + b.write_buffer_mib * u64::from(b.memtables) < b.ram_mib);
        assert_eq!(b.min_free_mib, 20_480);
        b.validate().expect("budget valid");
    }

    #[test]
    fn larger_machine_uses_more_parallelism_but_not_all_cores() {
        let b = Budget::from_resources(8, 8192, 500_000, StorageProfile::Ssd);
        assert!(b.core_par >= 2);
        assert!(b.index_workers >= 2 && b.index_workers <= 3);
        assert_eq!(b.memtables, 4);
        assert!(b.block_cache_mib >= 1024);
        b.validate().expect("budget valid");
    }
}
