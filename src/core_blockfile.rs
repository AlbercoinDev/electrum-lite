//! Read-only inspection of Bitcoin Core block files; never authorizes pruning.
//!
//! Core stores each record as network magic, little-endian length and a
//! serialized block. Newer Core versions XOR the file with blocks/xor.dat.
//! This parser only extracts header hashes. Core remains authoritative about
//! block heights, file eligibility, prune locks and actual deletion.
use anyhow::{ensure, Context, Result};
use bitcoin::{consensus::deserialize, BlockHash, Network};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::SystemTime,
};

const MAX_BLOCK_BYTES: u64 = 4_000_000;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 1_000_000;

/// Properties used to reject files changed or replaced during inspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fingerprint {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change: (i64, i64),
}

pub(crate) fn fingerprint(meta: &fs::Metadata) -> Result<Fingerprint> {
    ensure!(
        meta.file_type().is_file(),
        "block file must be a regular file"
    );
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Fingerprint {
        len: meta.len(),
        modified: meta.modified()?,
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
        #[cfg(unix)]
        change: (meta.ctime(), meta.ctime_nsec()),
    })
}

pub fn file_fingerprint(path: &Path) -> Result<Fingerprint> {
    fingerprint(&fs::symlink_metadata(path)?)
}

/// Inspect one file without trusting it to authorize deletion. Resolution must
/// include all records, including stale-chain blocks, just as Core's file
/// metadata does. Any missing header or mutation stops the inspection.
pub fn inspect_file<F>(
    path: &Path,
    network: Network,
    key: &[u8; 8],
    mut resolve: F,
) -> Result<(Fingerprint, u32)>
where
    F: FnMut(&[BlockHash]) -> Result<Vec<u32>>,
{
    let before = file_fingerprint(path)?;
    let mut file = fs::File::open(path)?;
    ensure!(
        fingerprint(&file.metadata()?)? == before,
        "block file changed before inspection"
    );
    let hashes = header_hashes(&mut file, before.len, network, key)?;
    ensure!(!hashes.is_empty(), "closed block file has no records");
    let heights = resolve(&hashes)?;
    ensure!(
        heights.len() == hashes.len(),
        "not all block heights were resolved"
    );
    ensure!(
        fingerprint(&file.metadata()?)? == before && file_fingerprint(path)? == before,
        "block file changed during inspection"
    );
    Ok((before, *heights.iter().max().context("no block heights")?))
}

pub fn eligible(max_height: u32, tip: u32) -> bool {
    tip >= 288 && max_height <= tip - 288
}

/// Parse Core's raw eight-byte blocks/xor.dat key, without inventing a default.
pub fn xor_key(bytes: &[u8]) -> Result<[u8; 8]> {
    bytes
        .try_into()
        .context("Core block XOR key must be eight bytes")
}

fn read_decoded<R: Read>(reader: &mut R, buf: &mut [u8], key: &[u8; 8], offset: u64) -> Result<()> {
    reader.read_exact(buf)?;
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte ^= key[((offset % 8) as usize + i) % 8];
    }
    Ok(())
}

/// Enumerate header hashes in a stable closed block file, including side chains.
/// The caller must verify file stability and resolve *every* hash with Core;
/// this function does not infer file heights or decide whether a file is closed.
pub fn header_hashes<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    network: Network,
    key: &[u8; 8],
) -> Result<Vec<BlockHash>> {
    ensure!(
        file_len <= MAX_FILE_BYTES,
        "Core block file exceeds inspection limit"
    );
    reader.seek(SeekFrom::Start(0))?;
    let mut pos = 0u64;
    let mut hashes = Vec::new();
    let magic = network.magic().to_bytes();
    while pos < file_len {
        let available = file_len - pos;
        let mut prefix = [0u8; 8];
        let read_len = available.min(8) as usize;
        reader.read_exact(&mut prefix[..read_len])?;
        let raw_zero = prefix[..read_len].iter().all(|b| *b == 0);
        for (i, byte) in prefix[..read_len].iter_mut().enumerate() {
            *byte ^= key[((pos % 8) as usize + i) % 8];
        }
        let decoded_zero = prefix[..read_len].iter().all(|b| *b == 0);
        if (raw_zero || decoded_zero) && (read_len < 4 || prefix[..4] != magic) {
            // Core preallocates unused space. Validate all of it, rather than
            // interpreting a malformed record as an early end of the file.
            pos += read_len as u64;
            let mut padding = [0u8; 8192];
            while pos < file_len {
                let n = (file_len - pos).min(padding.len() as u64) as usize;
                reader.read_exact(&mut padding[..n])?;
                ensure!(
                    padding[..n]
                        .iter()
                        .enumerate()
                        .all(|(i, b)| { *b == 0 || *b == key[((pos % 8) as usize + i) % 8] }),
                    "nonzero block file padding"
                );
                pos += n as u64;
            }
            break;
        }
        ensure!(read_len == 8, "truncated block record prefix");
        ensure!(prefix[..4] == magic, "unexpected block file network magic");
        let size = u32::from_le_bytes(prefix[4..].try_into()?) as u64;
        ensure!(
            (80..=MAX_BLOCK_BYTES).contains(&size),
            "invalid Core block record size"
        );
        ensure!(size <= file_len - pos - 8, "truncated Core block record");
        ensure!(
            hashes.len() < MAX_RECORDS,
            "Core block record count exceeds limit"
        );
        let mut bytes = [0u8; 80];
        read_decoded(reader, &mut bytes, key, pos + 8)?;
        let header: bitcoin::block::Header = deserialize(&bytes).context("invalid block header")?;
        hashes.push(header.block_hash());
        pos += 8 + size;
        reader.seek(SeekFrom::Start(pos))?;
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{blockdata::constants::genesis_block, consensus::serialize};
    use std::io::Cursor;

    #[test]
    fn reads_xor_records_and_padding() -> Result<()> {
        let network = Network::Signet;
        let block = genesis_block(network);
        let data = serialize(&block);
        let mut file = Vec::new();
        for _ in 0..2 {
            file.extend(network.magic().to_bytes());
            file.extend((data.len() as u32).to_le_bytes());
            file.extend(&data);
        }
        file.extend([0u8; 31]);
        let key = *b"12345678";
        for (i, b) in file.iter_mut().enumerate() {
            *b ^= key[i % 8];
        }
        let len = file.len() as u64;
        assert_eq!(
            header_hashes(&mut Cursor::new(&file), len, network, &key)?,
            vec![block.block_hash(); 2]
        );
        // Core's filesystem preallocation can also leave literal zero bytes.
        let padding_start = file.len() - 31;
        file[padding_start..].fill(0);
        assert_eq!(
            header_hashes(&mut Cursor::new(file), len, network, &key)?,
            vec![block.block_hash(); 2]
        );
        Ok(())
    }

    #[test]
    fn rejects_truncation_wrong_magic_and_garbage_padding() {
        let network = Network::Regtest;
        let mut record = Vec::from(network.magic().to_bytes());
        record.extend(100u32.to_le_bytes());
        record.extend([0u8; 80]);
        assert!(header_hashes(
            &mut Cursor::new(&record),
            record.len() as u64,
            network,
            &[0; 8]
        )
        .is_err());
        assert!(header_hashes(
            &mut Cursor::new(&record),
            record.len() as u64,
            Network::Signet,
            &[0; 8]
        )
        .is_err());
        let padding = [0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert!(header_hashes(
            &mut Cursor::new(padding),
            padding.len() as u64,
            network,
            &[0; 8]
        )
        .is_err());
        assert!(xor_key(&[0; 7]).is_err());
    }

    #[test]
    fn resolves_all_heights_and_detects_file_mutation() -> Result<()> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "el-core-file-inspect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        fs::create_dir(&dir)?;
        let path = dir.join("blk00000.dat");
        let network = Network::Regtest;
        let block = genesis_block(network);
        let data = serialize(&block);
        let mut bytes = Vec::new();
        for _ in 0..2 {
            bytes.extend(network.magic().to_bytes());
            bytes.extend((data.len() as u32).to_le_bytes());
            bytes.extend(&data);
        }
        fs::write(&path, bytes)?;
        let (_, height) = inspect_file(&path, network, &[0; 8], |hashes| {
            assert_eq!(hashes, [block.block_hash(); 2]);
            Ok(vec![100, 200])
        })?;
        assert_eq!(height, 200);
        assert!(!eligible(height, 487));
        assert!(eligible(height, 488));
        assert!(!eligible(0, 287));
        assert!(inspect_file(&path, network, &[0; 8], |_| Ok(vec![100])).is_err());
        assert!(inspect_file(&path, network, &[0; 8], |_| {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(&[0])?;
            Ok(vec![100, 200])
        })
        .is_err());
        fs::remove_file(&path)?;
        fs::remove_dir(&dir)?;
        Ok(())
    }
}
