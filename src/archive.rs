use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256d, Hash},
    Block, BlockHash,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{validate_block, Chain, MAX_BLOCK_BYTES};

const MAGIC: &[u8; 8] = b"ELBETA01";
const HEADER: usize = 116;
const MAX_COMPRESSED: usize = MAX_BLOCK_BYTES + 65536;
const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    pub segment: u32,
    pub offset: u64,
    pub length: u32,
}

pub struct Archive {
    dir: PathBuf,
    chain: Chain,
    _lock: File,
    segments: Vec<PathBuf>,
    tip: Option<(u32, BlockHash)>,
    scanned: bool,
    branches: bool,
    bytes: u64,
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
            // Persist directory entries too, including a newly created data root.
            let mut directory = Some(dir.as_path());
            while let Some(path) = directory {
                File::open(if path.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    path
                })?
                .sync_all()?;
                directory = path.parent();
            }
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
        let bytes = segments.iter().try_fold(0u64, |sum, path| -> Result<u64> {
            sum.checked_add(fs::metadata(path)?.len())
                .context("archive size overflow")
        })?;
        Ok(Self {
            dir,
            chain,
            _lock: lock,
            segments,
            tip: None,
            scanned: false,
            branches: false,
            bytes,
        })
    }

    /// Replay all records before appending. A torn tail is rejected, never silently discarded.
    pub fn scan(&mut self, mut visit: impl FnMut(u32, &Block) -> Result<()>) -> Result<()> {
        self.scan_records(|height, block, _| visit(height, block))
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Schema-2 coordinator validates active-chain transitions and durable rewind markers.
    pub fn enable_branches(&mut self) {
        self.branches = true;
    }

    pub fn size_bytes(&self) -> u64 {
        self.bytes
    }

    /// An unsuccessful append poisons the handle until a complete scan succeeds.
    pub(crate) fn requires_scan(&self) -> bool {
        !self.scanned
    }

    pub fn scan_records(
        &mut self,
        mut visit: impl FnMut(u32, &Block, Location) -> Result<()>,
    ) -> Result<()> {
        self.tip = None;
        self.scanned = false;
        self.bytes = self
            .segments
            .iter()
            .try_fold(0u64, |sum, path| -> Result<u64> {
                sum.checked_add(fs::metadata(path)?.len())
                    .context("archive size overflow")
            })?;
        File::open(&self.dir)?.sync_all()?;
        for (segment, path) in self.segments.iter().enumerate() {
            let mut file = File::open(path)?;
            // Recovery can import a complete tail or a copied legacy archive: ensure
            // its bytes are durable before publishing new database references.
            file.sync_all()?;
            loop {
                let offset = file.stream_position()?;
                let Some((height, block)) = read_record(&mut file, self.chain)? else {
                    break;
                };
                let location = Location {
                    segment: segment.try_into()?,
                    offset,
                    length: (file.stream_position()? - offset).try_into()?,
                };
                self.check_next(height, &block)?;
                visit(height, &block, location)?;
                self.tip = Some((height, block.block_hash()));
            }
        }
        self.scanned = true;
        Ok(())
    }

    fn check_next(&self, height: u32, block: &Block) -> Result<()> {
        if self.branches && self.tip.is_some() {
            return Ok(());
        }
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

    pub fn read(&self, location: Location) -> Result<(u32, Block)> {
        let path = self
            .segments
            .get(location.segment as usize)
            .context("referenced archive segment missing")?;
        ensure!(
            location.length as usize <= HEADER + MAX_COMPRESSED,
            "invalid archive reference length"
        );
        let mut file = File::open(path)?;
        ensure!(
            location
                .offset
                .checked_add(u64::from(location.length))
                .is_some_and(|end| end <= file.metadata().map(|m| m.len()).unwrap_or(0)),
            "archive reference outside file"
        );
        file.seek(SeekFrom::Start(location.offset))?;
        let result =
            read_record(&mut file, self.chain)?.context("archive reference points at EOF")?;
        ensure!(
            file.stream_position()? - location.offset == u64::from(location.length),
            "archive reference length mismatch"
        );
        Ok(result)
    }

    pub fn append(&mut self, height: u32, block: &Block) -> Result<Location> {
        self.append_record(height, block, true, true)
    }

    // Only Store's coordinator calls this immediately after prepare validates this block.
    pub(crate) fn append_staged(&mut self, height: u32, block: &Block) -> Result<Location> {
        self.append_record(height, block, false, false)
    }

    pub(crate) fn sync_staged(&mut self, first: u32) -> Result<()> {
        self.scanned = false;
        for path in self.segments.iter().skip(first as usize) {
            File::open(path)?.sync_all()?;
        }
        File::open(&self.dir)?.sync_all()?;
        self.scanned = true;
        Ok(())
    }

    pub(crate) fn require_scan(&mut self) {
        self.scanned = false;
    }

    fn append_record(
        &mut self,
        height: u32,
        block: &Block,
        sync: bool,
        verify: bool,
    ) -> Result<Location> {
        ensure!(
            self.scanned,
            "scan the complete archive successfully before appending"
        );
        self.check_next(height, block)?;
        if verify {
            validate_block(block)?;
        }
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
        let location = Location {
            segment: (self.segments.len() - 1).try_into()?,
            offset: file.metadata()?.len(),
            length: (header.len() + compressed.len()).try_into()?,
        };
        file.write_all(&header)?;
        file.write_all(&compressed)?;
        if sync {
            file.sync_all()?;
        }
        self.bytes = self
            .bytes
            .checked_add(u64::from(location.length))
            .context("archive size overflow")?;
        self.tip = Some((height, block.block_hash()));
        self.scanned = true;
        Ok(location)
    }
}

fn read_record(file: &mut File, chain: Chain) -> Result<Option<(u32, Block)>> {
    let mut header = [0u8; HEADER];
    if file.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    file.read_exact(&mut header[1..])
        .context("incomplete archive header; preserve files and restore a backup")?;
    ensure!(&header[..8] == MAGIC, "unsupported archive format");
    ensure!(
        header[8..40] == chain.genesis().to_byte_array(),
        "archive network mismatch"
    );
    let height = u32::from_le_bytes(header[40..44].try_into()?);
    let expected_hash = BlockHash::from_slice(&header[44..76])?;
    let raw_len = u32::from_le_bytes(header[76..80].try_into()?) as usize;
    let compressed_len = u32::from_le_bytes(header[80..84].try_into()?) as usize;
    ensure!(
        (1..=MAX_BLOCK_BYTES).contains(&raw_len) && (1..=MAX_COMPRESSED).contains(&compressed_len),
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
    Ok(Some((height, block)))
}
