//! Finding which blocks of the target a local file already holds.
//!
//! The scan slides a window over the local file one byte at a time. Each
//! position's rolling checksum is looked up in an index of the target's
//! blocks; a hit is confirmed with the block's MD4. A confirmed hit jumps
//! forward a whole block, because those bytes cannot start another match.
//!
//! With `seq_matches == 2` a lone hit is not trusted: the index is keyed by
//! *pairs* of consecutive blocks, so a lookup only fires when two blocks in
//! a row line up. The target's last block has no successor to pair with, so
//! it is never in the index; it is picked up by extending a run that
//! reaches it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use md4::{Digest, Md4};

use super::control::ControlFile;
use super::rsum::Rsum;
use crate::{Error, Result};

/// Largest block size the scanner accepts. A header naming a bigger power
/// of two is describing something no local file should be scanned for.
const MAX_BLOCKSIZE: u64 = 64 * 1024 * 1024;

/// How much new file content is read per pass. The buffer also holds the
/// lookahead windows, so nothing here grows with the file size.
const CHUNK: usize = 1024 * 1024;

/// A block not found in the local file.
const NOT_FOUND: u64 = u64::MAX;

/// Which target blocks the local file holds, and at which file offset.
#[derive(Debug, Clone)]
pub struct SourceMap {
    offsets: Vec<u64>,
    matched: usize,
}

impl SourceMap {
    fn new(blocks: usize) -> Self {
        Self {
            offsets: vec![NOT_FOUND; blocks],
            matched: 0,
        }
    }

    pub fn block_count(&self) -> usize {
        self.offsets.len()
    }

    pub fn matched(&self) -> usize {
        self.matched
    }

    pub fn is_complete(&self) -> bool {
        self.matched == self.offsets.len()
    }

    /// File offset where `block` was found, if any.
    pub fn offset_of(&self, block: usize) -> Option<u64> {
        self.offsets
            .get(block)
            .and_then(|&o| (o != NOT_FOUND).then_some(o))
    }

    /// First offset wins: a block found twice is identical bytes either way.
    fn record(&mut self, block: usize, offset: u64) {
        if self.offsets[block] == NOT_FOUND {
            self.offsets[block] = offset;
            self.matched += 1;
        }
    }
}

/// Index of target blocks by rolling checksum, for O(1) candidate lookup.
///
/// The key uses only the `b` halves: with `seq_matches == 1` it is the
/// window's `b`; with 2 it pairs the `b` of two consecutive windows. Full
/// verification (masked `a` plus MD4) happens on the candidates.
struct BlockIndex {
    seq_matches: u8,
    /// Combined key -> block numbers. For seq 2 the blocks are pair starts,
    /// covering 0..n-1; the last block is found by run extension.
    by_key: HashMap<u32, Vec<usize>>,
}

impl BlockIndex {
    fn build(control: &ControlFile) -> Self {
        let n = control.block_count();
        let seq = control.hash_lengths.seq_matches;
        let mut by_key: HashMap<u32, Vec<usize>> = HashMap::new();
        let indexable = if seq > 1 { n.saturating_sub(1) } else { n };
        for block in 0..indexable {
            let key = if seq > 1 {
                let b0 = control.blocks[block].rsum & 0xffff;
                let b1 = control.blocks[block + 1].rsum & 0xffff;
                (b0 << 16) | b1
            } else {
                control.blocks[block].rsum & 0xffff
            };
            by_key.entry(key).or_default().push(block);
        }
        Self {
            seq_matches: seq,
            by_key,
        }
    }

    fn candidates(&self, r1: Rsum, r2: Rsum) -> Option<&Vec<usize>> {
        let key = if self.seq_matches > 1 {
            ((r1.b as u32) << 16) | r2.b as u32
        } else {
            r1.b as u32
        };
        self.by_key.get(&key)
    }
}

/// MD4 of `data`, for strong-checksum comparison.
fn md4_of(data: &[u8]) -> [u8; 16] {
    let mut h = Md4::new();
    h.update(data);
    h.finalize().into()
}

/// Scan `local` for the target's blocks. Returns where each block was found.
pub fn scan(local: &Path, control: &ControlFile) -> Result<SourceMap> {
    let n = control.block_count();
    let bs = control.blocksize;
    if bs == 0 || !bs.is_power_of_two() || bs > MAX_BLOCKSIZE {
        return Err(Error::Zsync(format!("unusable block size {bs}")));
    }
    let bs = bs as usize;
    let shift = bs.trailing_zeros();
    let seq = control.hash_lengths.seq_matches as usize;
    let strong_len = control.hash_lengths.checksum_bytes as usize;

    let mut map = SourceMap::new(n);
    if n == 0 {
        return Ok(map);
    }
    let index = BlockIndex::build(control);

    let mut file = File::open(local).map_err(|e| Error::Zsync(e.to_string()))?;
    let file_len = file
        .metadata()
        .map_err(|e| Error::Zsync(e.to_string()))?
        .len();

    // Buffer holds one chunk plus the lookahead windows; zeroed each pass
    // so the tail past EOF is the zero padding zsync assumes.
    let lookahead = bs * seq;
    let mut buf = vec![0u8; CHUNK + lookahead];

    let mut base: u64 = 0;
    while base < file_len {
        buf.fill(0);
        file.seek(SeekFrom::Start(base))
            .map_err(|e| Error::Zsync(e.to_string()))?;
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(r) => filled += r,
                Err(e) => return Err(Error::Zsync(e.to_string())),
            }
        }

        // New offsets introduced by this chunk.
        let fresh = (CHUNK as u64).min(file_len - base) as usize;
        let mut i = 0;
        // Rolling checksums of the windows at i and i+bs.
        let mut r1 = Rsum::of(&buf[0..bs]);
        let mut r2 = if seq > 1 {
            Rsum::of(&buf[bs..2 * bs])
        } else {
            Rsum::default()
        };

        while i < fresh {
            let mut jumped = false;
            if let Some(cands) = index.candidates(r1, r2) {
                // Clone the candidate list: recording borrows map mutably.
                for &block in cands.clone().iter() {
                    if control.hash_lengths.truncate(r1) != control.blocks[block].rsum {
                        continue;
                    }
                    let win = &buf[i..i + bs];
                    if md4_of(win)[..strong_len] != control.blocks[block].checksum[..strong_len] {
                        continue;
                    }
                    if seq > 1 {
                        let win2 = &buf[i + bs..i + 2 * bs];
                        if md4_of(win2)[..strong_len]
                            != control.blocks[block + 1].checksum[..strong_len]
                        {
                            continue;
                        }
                        map.record(block, base + i as u64);
                        map.record(block + 1, base + i as u64 + bs as u64);
                        i += 2 * bs;
                        jumped = true;
                        break;
                    } else {
                        map.record(block, base + i as u64);
                        i += bs;
                        jumped = true;
                        break;
                    }
                }
            }
            if jumped {
                // Recompute rolling state at the new position from scratch;
                // jumps are rare, rolling is for the byte-by-byte crawl.
                // The buffer is zero-padded past EOF, so this is correct
                // even when the jump lands in the padding.
                let end1 = (i + bs).min(buf.len());
                let mut w1 = vec![0u8; bs];
                w1[..end1 - i].copy_from_slice(&buf[i..end1]);
                r1 = Rsum::of(&w1);
                if seq > 1 {
                    let s2 = i + bs;
                    let end2 = (s2 + bs).min(buf.len());
                    let mut w2 = vec![0u8; bs];
                    w2[..end2 - s2].copy_from_slice(&buf[s2..end2]);
                    r2 = Rsum::of(&w2);
                }
                continue;
            }
            // Roll one byte forward, unless the next offset belongs to the
            // next chunk (which recomputes from scratch).
            if i + 1 < fresh {
                r1.roll(buf[i], buf[i + bs], shift);
                if seq > 1 {
                    r2.roll(buf[i + bs], buf[i + 2 * bs], shift);
                }
            }
            i += 1;
        }
        base += CHUNK as u64;
    }

    // Run extension: a confirmed block's successor sits one blocksize on,
    // which is how the last block (never in the pair index) is found.
    if seq > 1 {
        extend_runs(local, control, &mut map)?;
    }

    Ok(map)
}

/// Extend confirmed runs block by block: if block `b` was found at `o` and
/// `b+1` is still missing, check the bytes at `o + blocksize`.
fn extend_runs(local: &Path, control: &ControlFile, map: &mut SourceMap) -> Result<()> {
    let n = control.block_count();
    let bs = control.blocksize;
    let strong_len = control.hash_lengths.checksum_bytes as usize;
    let mut file = File::open(local).map_err(|e| Error::Zsync(e.to_string()))?;
    let file_len = file
        .metadata()
        .map_err(|e| Error::Zsync(e.to_string()))?
        .len();

    let mut window = vec![0u8; bs as usize];
    for b in 0..n {
        let Some(off) = map.offset_of(b) else {
            continue;
        };
        let next = b + 1;
        if next >= n || map.offset_of(next).is_some() {
            continue;
        }
        let at = off + bs;
        // Bytes past EOF are zeros, per zsync's padding.
        window.fill(0);
        if at < file_len {
            file.seek(SeekFrom::Start(at))
                .map_err(|e| Error::Zsync(e.to_string()))?;
            let mut filled = 0;
            while filled < window.len() {
                match file.read(&mut window[filled..]) {
                    Ok(0) => break,
                    Ok(r) => filled += r,
                    Err(e) => return Err(Error::Zsync(e.to_string())),
                }
            }
        }
        if md4_of(&window)[..strong_len] == control.blocks[next].checksum[..strong_len] {
            map.record(next, at);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zsync::control::HashLengths;

    /// Build a synthetic control file: blocks of `blocksize` bytes with
    /// known content, checksummed per the format.
    fn synth(blocks: &[Vec<u8>], blocksize: usize, seq: u8) -> ControlFile {
        let lengths = HashLengths {
            seq_matches: seq,
            rsum_bytes: 2,
            checksum_bytes: 4,
        };
        let sums = blocks
            .iter()
            .map(|b| {
                let mut padded = vec![0u8; blocksize];
                padded[..b.len().min(blocksize)].copy_from_slice(&b[..b.len().min(blocksize)]);
                let r = Rsum::of(&padded);
                // rsum_bytes is 2 here: the table keeps the low 16 bits,
                // which is what truncate yields.
                let v = lengths.truncate(r);
                debug_assert_eq!(v, v & 0xffff);
                let mut checksum = [0u8; 16];
                checksum[..4].copy_from_slice(&md4_of(&padded)[..4]);
                super::super::control::BlockSum { rsum: v, checksum }
            })
            .collect();
        ControlFile {
            header: super::super::control::Header {
                filename: None,
                length: (blocks.len() * blocksize) as u64,
                sha1: None,
                url: None,
                blocksize: Some(blocksize as u64),
                hash_lengths: lengths,
            },
            blocksize: blocksize as u64,
            hash_lengths: lengths,
            blocks: sums,
        }
    }

    fn write_temp(data: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("dropin-scan-test-{}", rand_suffix()));
        std::fs::write(&p, data).unwrap();
        p
    }

    fn rand_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{n}-{}", std::process::id())
    }

    fn block_data(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i * 31) as u8))
            .collect()
    }

    #[test]
    fn finds_blocks_in_order_seq1() {
        let bs = 64;
        let blocks: Vec<Vec<u8>> = (0..4).map(|s| block_data(s, bs)).collect();
        let control = synth(&blocks, bs, 1);
        let mut file: Vec<u8> = Vec::new();
        for b in &blocks {
            file.extend_from_slice(b);
        }
        let p = write_temp(&file);
        let map = scan(&p, &control).unwrap();
        assert!(map.is_complete());
        for (i, b) in blocks.iter().enumerate() {
            let off = map.offset_of(i).unwrap();
            assert_eq!(off, (i * bs) as u64);
            let _ = b;
        }
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn finds_shifted_blocks_seq2() {
        let bs = 64;
        let blocks: Vec<Vec<u8>> = (0..5).map(|s| block_data(s + 10, bs)).collect();
        let control = synth(&blocks, bs, 2);
        // Local file: junk prefix, then the blocks in order, junk suffix.
        let mut file = vec![0xAAu8; 100];
        for b in &blocks {
            file.extend_from_slice(b);
        }
        file.extend_from_slice(&[0xBBu8; 50]);
        let p = write_temp(&file);
        let map = scan(&p, &control).unwrap();
        assert!(
            map.is_complete(),
            "matched {}/{}",
            map.matched(),
            map.block_count()
        );
        for i in 0..blocks.len() {
            assert_eq!(map.offset_of(i), Some((100 + i * bs) as u64));
        }
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn missing_block_stays_missing() {
        let bs = 64;
        let blocks: Vec<Vec<u8>> = (0..4).map(|s| block_data(s + 20, bs)).collect();
        let control = synth(&blocks, bs, 2);
        // Local file has blocks 0,1,3 but not 2. With seq_matches=2 an
        // isolated block whose predecessor is missing cannot be reached:
        // it is in no pair, and no run extends to it.
        let mut file: Vec<u8> = Vec::new();
        file.extend_from_slice(&blocks[0]);
        file.extend_from_slice(&blocks[1]);
        file.extend_from_slice(&blocks[3]);
        let p = write_temp(&file);
        let map = scan(&p, &control).unwrap();
        assert!(!map.is_complete());
        assert!(map.offset_of(0).is_some());
        assert!(map.offset_of(1).is_some());
        assert!(map.offset_of(2).is_none());
        assert!(map.offset_of(3).is_none());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn run_extension_reaches_the_last_block() {
        let bs = 64;
        let blocks: Vec<Vec<u8>> = (0..4).map(|s| block_data(s + 30, bs)).collect();
        let control = synth(&blocks, bs, 2);
        // All blocks present and in order: the last block is in no pair,
        // but the run extending from block 2 reaches it.
        let mut file: Vec<u8> = Vec::new();
        for b in &blocks {
            file.extend_from_slice(b);
        }
        let p = write_temp(&file);
        let map = scan(&p, &control).unwrap();
        assert!(map.is_complete());
        assert_eq!(map.offset_of(3), Some((3 * bs) as u64));
        std::fs::remove_file(p).ok();
    }
}
