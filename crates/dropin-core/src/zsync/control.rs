//! Parsing a `.zsync` control file: the text header and the binary block table.
//!
//! Layout, following zsync 0.6.2:
//!
//! ```text
//! zsync: 0.6.2
//! Filename: app-x86_64.AppImage
//! MTime: Thu, 31 Dec 2020 11:50:34 +0000
//! Blocksize: 2048
//! Length: 2172096
//! Hash-Lengths: 2,2,4
//! URL: app-x86_64.AppImage
//! SHA-1: 3c7d3061b7f2372d314fee553f3e37d2c2e5c03b
//!
//! <binary table>
//! ```
//!
//! `Hash-Lengths` is `seq_matches,rsum_bytes,checksum_bytes`: how many
//! consecutive blocks must match for a hit to count, how many bytes of the
//! rolling checksum the table keeps, how many bytes of the MD4. When the
//! line is absent the historical default `1,4,16` applies.
//!
//! The table holds one entry per block, in order. Each entry is the last
//! `rsum_bytes` of the big-endian rolling checksum value `(a << 16) | b`,
//! followed by the first `checksum_bytes` of the block's MD4. The final
//! block is checksummed zero-padded to the full block size.

use super::rsum::Rsum;
use crate::{Error, Result};

/// Cap on a control file: at ~0.2% of the payload it describes, this covers
/// multi-gigabyte AppImages while refusing absurd allocations.
const CONTROL_MAX: usize = 16 * 1024 * 1024;

/// How many bytes of the header a "does an update exist" check fetches.
/// Real headers are a few hundred bytes; anything past this is block table.
pub const HEADER_REQUEST: usize = 8 * 1024;

/// The three lengths from the `Hash-Lengths` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashLengths {
    /// Consecutive blocks that must match for a hit to count: 1 or 2.
    pub seq_matches: u8,
    /// Bytes of the rolling checksum kept per block: 1 to 4.
    pub rsum_bytes: u8,
    /// Bytes of the MD4 kept per block: 3 to 16.
    pub checksum_bytes: u8,
}

impl HashLengths {
    /// What a header without the line means: the untruncated checksums
    /// zsync used before the field existed.
    pub const DEFAULT: Self = Self { seq_matches: 1, rsum_bytes: 4, checksum_bytes: 16 };

    /// Bytes one block occupies in the table.
    pub fn entry_size(self) -> usize {
        self.rsum_bytes as usize + self.checksum_bytes as usize
    }

    /// Which bits of a computed rolling checksum a stored one can be
    /// compared against. `b` is always compared whole; `a` is masked to
    /// nothing below three bytes, one byte at three, both at four. At one
    /// byte this keeps zsync's quirk: a whole `b` is compared against a
    /// stored value that only ever held eight bits of it.
    pub fn rsum_mask(self) -> u32 {
        let a_mask: u32 = match self.rsum_bytes {
            0..=2 => 0,
            3 => 0xff,
            _ => 0xffff,
        };
        (a_mask << 16) | 0xffff
    }

    /// Reduce a computed checksum to what the table holds.
    pub fn truncate(self, rsum: Rsum) -> u32 {
        rsum.value() & self.rsum_mask()
    }

    fn parse(value: &str) -> std::result::Result<Self, String> {
        let mut fields = value.split(',').map(str::trim);
        let nums: Vec<u8> = fields
            .by_ref()
            .take(4)
            .map(|f| f.parse::<u8>())
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| format!("{value:?} is not a usable Hash-Lengths line"))?;
        if nums.len() != 3 || fields.next().is_some() {
            return Err(format!("{value:?} is not a usable Hash-Lengths line"));
        }
        let [seq_matches, rsum_bytes, checksum_bytes] = [nums[0], nums[1], nums[2]];
        if !(1..=2).contains(&seq_matches)
            || !(1..=4).contains(&rsum_bytes)
            || !(3..=16).contains(&checksum_bytes)
        {
            return Err(format!("{value:?} is not a usable Hash-Lengths line"));
        }
        Ok(Self { seq_matches, rsum_bytes, checksum_bytes })
    }
}

/// The text header: everything a check needs, without the block table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub filename: Option<String>,
    pub length: u64,
    /// Lowercase hex SHA-1 of the complete file.
    pub sha1: Option<String>,
    /// The payload URL as written; may be relative to the .zsync URL.
    pub url: Option<String>,
    pub blocksize: Option<u64>,
    pub hash_lengths: HashLengths,
}

/// One block's checksums from the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSum {
    /// Rolling checksum in the low `rsum_bytes` of the value; compare with
    /// [`HashLengths::truncate`].
    pub rsum: u32,
    /// MD4 of the block, only the first `checksum_bytes` are meaningful.
    pub checksum: [u8; 16],
}

/// A fully parsed control file.
#[derive(Debug, Clone)]
pub struct ControlFile {
    pub header: Header,
    pub blocksize: u64,
    pub hash_lengths: HashLengths,
    pub blocks: Vec<BlockSum>,
}

impl ControlFile {
    /// log2 of the block size, for [`Rsum::roll`].
    pub fn blockshift(&self) -> u32 {
        self.blocksize.trailing_zeros()
    }

    /// The published bytes of a block's strong checksum.
    pub fn checksum(&self, block: usize) -> Option<&[u8]> {
        self.blocks
            .get(block)
            .map(|s| &s.checksum[..self.hash_lengths.checksum_bytes as usize])
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

/// Fetch just the header of a `.zsync` URL, for update checks.
pub fn fetch_header(zsync_url: &str) -> Result<Header> {
    use super::fetch::RangeBody;
    let body = super::fetch::get_range(zsync_url, 0, HEADER_REQUEST as u64)
        .map_err(|e| Error::Zsync(format!("fetching header from {zsync_url}: {e}")))?;
    // A server that ignores ranges sends the whole control file; the header
    // is still at the front of it.
    let bytes = match &body {
        RangeBody::Partial(b) | RangeBody::Full(b) => b,
    };
    if bytes.len() >= HEADER_REQUEST && !has_terminator(bytes) {
        return Err(Error::Zsync(format!("header of {zsync_url} did not fit in one request")));
    }
    parse_header(bytes).map_err(|r| Error::Zsync(format!("{zsync_url}: {r}")))
}

/// Fetch and parse a whole control file.
pub fn fetch_control(zsync_url: &str) -> Result<ControlFile> {
    let bytes = super::fetch::get_all(zsync_url, CONTROL_MAX)
        .map_err(|e| Error::Zsync(format!("fetching {zsync_url}: {e}")))?;
    parse_control(&bytes).map_err(|r| Error::Zsync(format!("{zsync_url}: {r}")))
}

/// Parse only the header; the caller may not have the whole file.
pub fn parse_header(bytes: &[u8]) -> std::result::Result<Header, String> {
    let (end, _) = terminator(bytes)
        .ok_or_else(|| "header not terminated by a blank line".to_string())?;
    let text = String::from_utf8_lossy(&bytes[..end]);

    let mut header = Header {
        filename: None,
        length: 0,
        sha1: None,
        url: None,
        blocksize: None,
        hash_lengths: HashLengths::DEFAULT,
    };
    let mut length = None;
    let mut first = true;

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("{line:?} is not a header line"));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if first {
            if key != "zsync" {
                return Err("file does not start with a zsync version".to_string());
            }
            first = false;
        }
        match key.as_str() {
            "filename" => header.filename = Some(value.to_string()),
            "sha-1" => header.sha1 = Some(value.to_ascii_lowercase()),
            // split_once already split on the FIRST colon, so URLs survive.
            "url" => header.url = Some(value.to_string()),
            "length" => {
                length = Some(value.parse::<u64>().map_err(|_| format!("{value:?} is not a length"))?);
            }
            "blocksize" => {
                let size =
                    value.parse::<u64>().map_err(|_| format!("{value:?} is not a block size"))?;
                if size == 0 || !size.is_power_of_two() {
                    return Err(format!("{size} is not a usable block size"));
                }
                header.blocksize = Some(size);
            }
            "hash-lengths" => header.hash_lengths = HashLengths::parse(value)?,
            _ => {}
        }
    }
    header.length = length.ok_or_else(|| "header carries no Length".to_string())?;
    Ok(header)
}

/// Parse a whole control file: header plus block table.
pub fn parse_control(bytes: &[u8]) -> std::result::Result<ControlFile, String> {
    let header = parse_header(bytes)?;
    let (end, skip) =
        terminator(bytes).ok_or_else(|| "header not terminated by a blank line".to_string())?;

    let blocksize = header.blocksize.ok_or_else(|| "header carries no Blocksize".to_string())?;
    if header.length == 0 {
        return Err("header says the complete file is empty".to_string());
    }

    let hash_lengths = header.hash_lengths;
    let entry_size = hash_lengths.entry_size();
    let count = header.length.div_ceil(blocksize);
    // Settle the allocation from the table size before allocating.
    let needed = usize::try_from(count)
        .ok()
        .and_then(|c| c.checked_mul(entry_size))
        .ok_or_else(|| {
            format!(
                "{} bytes in blocks of {blocksize} would need more checksums than fit in memory",
                header.length
            )
        })?;

    let table = &bytes[end + skip..];
    if table.len() < needed {
        return Err(format!(
            "block table is short: {count} blocks need {needed} bytes, file has {}",
            table.len()
        ));
    }

    let mut blocks = Vec::with_capacity(count as usize);
    for entry in table[..needed].chunks_exact(entry_size) {
        let (weak, strong) = entry.split_at(hash_lengths.rsum_bytes as usize);
        // The table holds the LAST bytes of the big-endian value.
        let mut rsum = [0u8; 4];
        rsum[4 - weak.len()..].copy_from_slice(weak);
        let mut checksum = [0u8; 16];
        checksum[..strong.len()].copy_from_slice(strong);
        blocks.push(BlockSum { rsum: u32::from_be_bytes(rsum), checksum });
    }

    Ok(ControlFile { header, blocksize, hash_lengths, blocks })
}

fn has_terminator(bytes: &[u8]) -> bool {
    terminator(bytes).is_some()
}

/// Start and length of the blank line ending the header. Binary table bytes
/// may contain `\n\n`, so whichever of `\n\n` / `\r\n\r\n` comes first wins.
fn terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|w| w == b"\n\n").map(|at| (at, 2));
    let crlf = bytes.windows(4).position(|w| w == b"\r\n\r\n").map(|at| (at, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (found, _) => found.or(crlf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes() -> Vec<u8> {
        b"zsync: 0.6.2\nFilename: app-x86_64.AppImage\nBlocksize: 2048\nLength: 5000\n\
          Hash-Lengths: 2,2,4\nURL: app-x86_64.AppImage\n\
          SHA-1: 3c7d3061b7f2372d314fee553f3e37d2c2e5c03b\n\n"
            .to_vec()
    }

    #[test]
    fn parses_header_fields() {
        let h = parse_header(&header_bytes()).unwrap();
        assert_eq!(h.filename.as_deref(), Some("app-x86_64.AppImage"));
        assert_eq!(h.length, 5000);
        assert_eq!(h.blocksize, Some(2048));
        assert_eq!(
            h.hash_lengths,
            HashLengths { seq_matches: 2, rsum_bytes: 2, checksum_bytes: 4 }
        );
        assert_eq!(h.url.as_deref(), Some("app-x86_64.AppImage"));
        assert_eq!(
            h.sha1.as_deref(),
            Some("3c7d3061b7f2372d314fee553f3e37d2c2e5c03b")
        );
    }

    #[test]
    fn default_hash_lengths_when_absent() {
        let h = parse_header(b"zsync: 0.6.2\nLength: 10\n\n").unwrap();
        assert_eq!(h.hash_lengths, HashLengths::DEFAULT);
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(parse_header(b"nope\n\n").is_err());
        assert!(parse_header(b"zsync: 0.6.2\nLength: 10").is_err()); // no blank line
        assert!(parse_header(b"zsync: 0.6.2\n\n").is_err()); // no Length
        assert!(parse_header(b"zsync: 0.6.2\nLength: 10\nBlocksize: 1000\n\n").is_err());
        assert!(parse_header(b"zsync: 0.6.2\nLength: 10\nHash-Lengths: 3,2,4\n\n").is_err());
    }

    #[test]
    fn parses_block_table() {
        let mut bytes = header_bytes();
        // 5000 bytes in 2048-blocks = 3 blocks; entries are 2+4 bytes.
        // Block 0: weak bytes [0x12,0x34] -> rsum 0x1234; strong [1,2,3,4].
        bytes.extend_from_slice(&[0x12, 0x34, 1, 2, 3, 4]);
        bytes.extend_from_slice(&[0x00, 0x01, 5, 6, 7, 8]);
        bytes.extend_from_slice(&[0xff, 0xff, 9, 9, 9, 9]);
        let c = parse_control(&bytes).unwrap();
        assert_eq!(c.block_count(), 3);
        assert_eq!(c.blocks[0].rsum, 0x1234);
        assert_eq!(&c.blocks[0].checksum[..4], &[1, 2, 3, 4]);
        assert_eq!(c.checksum(2), Some([9, 9, 9, 9].as_slice()));
        assert_eq!(c.blockshift(), 11);
    }

    #[test]
    fn rejects_short_table() {
        let mut bytes = header_bytes();
        bytes.extend_from_slice(&[0u8; 6]); // only 1 of 3 blocks
        assert!(parse_control(&bytes).is_err());
    }

    #[test]
    fn truncate_masks_per_rsum_bytes() {
        let r = Rsum { a: 0xabcd, b: 0x1234 };
        let l = |wb| HashLengths { seq_matches: 1, rsum_bytes: wb, checksum_bytes: 4 };
        assert_eq!(l(4).truncate(r), 0xabcd_1234);
        assert_eq!(l(3).truncate(r), 0x00cd_1234);
        assert_eq!(l(2).truncate(r), 0x0000_1234);
        // The one-byte quirk: whole b against eight stored bits.
        assert_eq!(l(1).truncate(r), 0x0000_1234);
        assert_eq!(l(1).truncate(Rsum { a: 0, b: 0x34 }), 0x34);
    }
}
