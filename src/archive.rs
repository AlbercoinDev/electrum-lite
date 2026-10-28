use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256d, Hash},
    Block, BlockHash,
};
use fs2::FileExt;

use crate::{validate_block, Chain, MAX_BLOCK_BYTES};

const MAGIC: &[u8; 8] = b"ELBETA01";
const HEADER: usize = 116;
const MAX_COMPRESSED: usize = MAX_BLOCK_BYTES + 65536;
const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

pub struct Archive {
    dir: PathBuf,
    chain: Chain,
    _lock: File,
    segments: Vec<PathBuf>,
    tip: Option<(u32, BlockHash)>,
    scanned: bool,
}

impl Drop for Archive {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._lock);
    }
}

impl Archive {
    pub fn open(root: &Path, chain: Chain, create: bool) -> Result<Self> {
        let dir = root.join(chain.name());
        if create {
            fs::create_dir_all(&dir)?;
        }
        ensure!(dir.is_dir(), "archive does not exist: {}", dir.display());
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("archive.lock"))?;
        lock.try_lock_exclusive()
            .context("archive is already in use")?;
        let mut segments = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "elb") {
                segments.push(path);
            }
        }
        segments.sort();
        for (n, path) in segments.iter().enumerate() {
            ensure!(
                path.file_name().unwrap().to_str() == Some(&format!("blocks-{n:08}.elb")),
                "invalid segment sequence"
            );
        }
        Ok(Self {
            dir,
            chain,
            _lock: lock,
            segments,
            tip: None,
            scanned: false,
        })
    }

    /// Replay all records before appending. A torn tail is rejected, never silently discarded.
    pub fn scan(&mut self, mut visit: impl FnMut(u32, &Block) -> Result<()>) -> Result<()> {
        self.tip = None;
        self.scanned = false;
        for path in &self.segments {
            let mut file = File::open(path)?;
            loop {
                let mut header = [0u8; HEADER];
                if file.read(&mut header[..1])? == 0 {
                    break;
                }
                file.read_exact(&mut header[1..])
                    .context("incomplete archive header; preserve files and restore a backup")?;
                ensure!(&header[..8] == MAGIC, "unsupported archive format");
                ensure!(
                    header[8..40] == self.chain.genesis().to_byte_array(),
                    "archive network mismatch"
                );
                let height = u32::from_le_bytes(header[40..44].try_into()?);
                let expected_hash = BlockHash::from_slice(&header[44..76])?;
                let raw_len = u32::from_le_bytes(header[76..80].try_into()?) as usize;
                let compressed_len = u32::from_le_bytes(header[80..84].try_into()?) as usize;
                ensure!(
                    (1..=MAX_BLOCK_BYTES).contains(&raw_len)
                        && (1..=MAX_COMPRESSED).contains(&compressed_len),
                    "invalid record size"
                );
                let mut compressed = vec![0; compressed_len];
                file.read_exact(&mut compressed)
                    .context("incomplete archive payload; preserve files and restore a backup")?;
                ensure!(
                    sha256d::Hash::hash(&compressed).as_byte_array() == &header[84..116],
                    "archive checksum mismatch"
                );
                let mut decoder = zstd::stream::read::Decoder::new(compressed.as_slice())?;
                decoder.window_log_max(23)?;
                let mut raw = Vec::new();
                decoder
                    .take((MAX_BLOCK_BYTES + 1) as u64)
                    .read_to_end(&mut raw)?;
                ensure!(raw.len() == raw_len, "decompressed size mismatch");
                let block: Block = deserialize(&raw)?;
                ensure!(
                    block.block_hash() == expected_hash,
                    "archived block hash mismatch"
                );
                validate_block(&block)?;
                self.check_next(height, &block)?;
                visit(height, &block)?;
                self.tip = Some((height, expected_hash));
            }
        }
        self.scanned = true;
        Ok(())
    }

    fn check_next(&self, height: u32, block: &Block) -> Result<()> {
        match self.tip {
            None => ensure!(
                height == 0 && block.block_hash() == self.chain.genesis(),
                "archive must start at network genesis"
            ),
            Some((h, hash)) => ensure!(
                height == h + 1 && block.header.prev_blockhash == hash,
                "archive discontinuity or reorg"
            ),
        }
        Ok(())
    }

    pub fn append(&mut self, height: u32, block: &Block) -> Result<()> {
        ensure!(
            self.scanned,
            "scan the complete archive successfully before appending"
        );
        self.check_next(height, block)?;
        validate_block(block)?;
        let raw = serialize(block);
        ensure!(raw.len() <= MAX_BLOCK_BYTES, "block too large");
        let compressed = zstd::stream::encode_all(raw.as_slice(), 3)?;
        ensure!(
            compressed.len() <= MAX_COMPRESSED,
            "compressed block too large"
        );
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(self.chain.genesis().as_byte_array());
        header.extend_from_slice(&height.to_le_bytes());
        header.extend_from_slice(block.block_hash().as_byte_array());
        header.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        header.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        header.extend_from_slice(sha256d::Hash::hash(&compressed).as_byte_array());
        let rotate = match self.segments.last() {
            None => true,
            Some(path) => {
                fs::metadata(path)?.len() + (HEADER + compressed.len()) as u64 > SEGMENT_BYTES
            }
        };
        if rotate {
            let path = self
                .dir
                .join(format!("blocks-{:08}.elb", self.segments.len()));
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            file.sync_all()?;
            File::open(&self.dir)?.sync_all()?;
            self.segments.push(path);
        }
        // Any I/O failure poisons this handle until a successful full scan.
        self.scanned = false;
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.segments.last().unwrap())?;
        file.write_all(&header)?;
        file.write_all(&compressed)?;
        file.sync_all()?;
        self.tip = Some((height, block.block_hash()));
        self.scanned = true;
        Ok(())
    }
}
