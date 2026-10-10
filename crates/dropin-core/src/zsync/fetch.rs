//! Small HTTP fetching for zsync: ranged reads and whole-file reads.
//!
//! Everything here is bounded: callers name the byte range or the maximum
//! size, and oversized or misbehaving responses are refused rather than
//! buffered blindly.

use std::io::Read;

/// Result of a ranged GET: what the server actually sent.
pub enum RangeBody {
    /// 206: exactly the requested bytes.
    Partial(Vec<u8>),
    /// 200: the server ignored the Range header and sent everything.
    Full(Vec<u8>),
}

/// GET `url` with `Range: bytes=start-(start+len-1)`.
///
/// Servers that do not do ranges answer 200 with the whole file; that is
/// reported as [`RangeBody::Full`] so the caller can decide.
pub fn get_range(url: &str, start: u64, len: u64) -> Result<RangeBody, String> {
    let end = start.saturating_add(len).saturating_sub(1);
    let mut res = ureq::get(url)
        .header("Range", &format!("bytes={start}-{end}"))
        .header("User-Agent", super::USER_AGENT)
        .call()
        .map_err(|e| format!("request failed: {e}"))?;

    let status = res.status().as_u16();
    // Bound the read even on 200: a hostile or confused server sending
    // gigabytes here must not be buffered. Callers asking for a range never
    // need more than the range plus slack for a full-file fallback decision.
    let cap = len.saturating_add(1024 * 1024).max(2 * 1024 * 1024) as usize;
    let mut body = Vec::new();
    res.body_mut()
        .as_reader()
        .take(cap as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("reading body: {e}"))?;
    if body.len() > cap {
        return Err(format!("response exceeded {cap} bytes"));
    }
    match status {
        206 => Ok(RangeBody::Partial(body)),
        200 => Ok(RangeBody::Full(body)),
        s => Err(format!("unexpected status {s} for range request")),
    }
}

/// GET the whole file, refusing anything over `max_bytes`.
pub fn get_all(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut res = ureq::get(url)
        .header("User-Agent", super::USER_AGENT)
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    if res.status() != 200 {
        return Err(format!("unexpected status {}", res.status()));
    }
    let mut body = Vec::new();
    res.body_mut()
        .as_reader()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("reading body: {e}"))?;
    if body.len() > max_bytes {
        return Err(format!("file larger than {max_bytes} bytes"));
    }
    Ok(body)
}

/// Fetch bytes `start..start+len` of `url`, streaming them into `write`.
///
/// A 206 answers with exactly the range. A 200 means the server ignored the
/// Range header; the unwanted prefix is skipped in the stream so the caller
/// still receives exactly the requested bytes.
pub fn get_range_into(
    url: &str,
    start: u64,
    len: u64,
    mut write: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let end = start.saturating_add(len).saturating_sub(1);
    let mut res = ureq::get(url)
        .header("Range", &format!("bytes={start}-{end}"))
        .header("User-Agent", super::USER_AGENT)
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let ranged = match res.status().as_u16() {
        206 => true,
        200 => false,
        s => return Err(format!("unexpected status {s} for range request")),
    };
    let mut reader = res.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    // On 200 the stream starts at 0; skip to the range first.
    let mut skip = if ranged { 0 } else { start };
    let mut remaining = len;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("reading body: {e}"))?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];
        if skip > 0 {
            let drop = (chunk.len() as u64).min(skip) as usize;
            chunk = &chunk[drop..];
            skip -= drop as u64;
        }
        let take = (chunk.len() as u64).min(remaining) as usize;
        if take > 0 {
            write(&chunk[..take]).map_err(|e| format!("writing: {e}"))?;
            remaining -= take as u64;
        }
        if remaining == 0 {
            break;
        }
    }
    if remaining != 0 {
        return Err(format!("short read: {remaining} of {len} bytes missing"));
    }
    Ok(())
}
