/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Minimal EBML scanner for extracting WebM cluster timecodes from appended
//! MSE segments. Not a full parser — just enough to index buffered ranges.

/// Scan a webm/mkv bytestream and return the cluster timecodes found, in
/// stream units (typically milliseconds after applying the usual scale of
/// 1ms that both YouTube and ffmpeg use).
pub(crate) fn scan_cluster_timecodes(data: &[u8]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        // Cluster element id: 0x1F43B675
        if data[pos] == 0x1F && data[pos + 1] == 0x43 && data[pos + 2] == 0xB6 && data[pos + 3] == 0x75 {
            // scan a limited window inside the cluster head for the Timecode
            // element (0xE7) — it appears before any Block data.
            let window_end = (pos + 4 + 512).min(data.len());
            let mut i = pos + 4;
            while i + 2 <= window_end {
                if data[i] == 0xE7 {
                    // vint size of the timecode element content
                    let (size, vlen) = read_vint(&data[i + 1..]);
                    if size > 0 && size <= 8 && i + 1 + vlen + size <= window_end {
                        let mut val: u64 = 0;
                        for b in &data[i + 1 + vlen..i + 1 + vlen + size] {
                            val = (val << 8) | (*b as u64);
                        }
                        out.push(val as f64 / 1000.0);
                        break;
                    }
                }
                i += 1;
            }
            // jump past the cluster payload using its header size if possible
            let (size, vlen) = read_vint_size(&data[pos + 4..]);
            if size > 0 {
                let next = pos + 4 + vlen + size as usize;
                pos = if next > pos { next.min(data.len()) } else { pos + 4 };
            } else {
                pos += 4;
            }
        } else {
            pos += 1;
        }
    }
    out
}

/// Scan a webm/mkv bytestream for cluster byte offsets + timecodes. Same
/// scanning logic as scan_cluster_timecodes but records where each cluster
/// starts — the MSE re-feed needs byte-accurate cluster boundaries because
/// player appends are not cluster-aligned (sec1zt126).
pub(crate) fn scan_cluster_offsets(data: &[u8]) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        if data[pos] == 0x1F && data[pos + 1] == 0x43 && data[pos + 2] == 0xB6 && data[pos + 3] == 0x75 {
            let window_end = (pos + 4 + 512).min(data.len());
            let mut i = pos + 4;
            let mut tc = None;
            while i + 2 <= window_end {
                if data[i] == 0xE7 {
                    let (size, vlen) = read_vint(&data[i + 1..]);
                    if size > 0 && size <= 8 && i + 1 + vlen + size <= window_end {
                        let mut val: u64 = 0;
                        for b in &data[i + 1 + vlen..i + 1 + vlen + size] {
                            val = (val << 8) | (*b as u64);
                        }
                        tc = Some(val as f64 / 1000.0);
                        break;
                    }
                }
                i += 1;
            }
            out.push((pos, tc.unwrap_or(0.0)));
            let (size, vlen) = read_vint_size(&data[pos + 4..]);
            if size > 0 {
                let next = pos + 4 + vlen + size as usize;
                pos = if next > pos { next.min(data.len()) } else { pos + 4 };
            } else {
                pos += 4;
            }
        } else {
            pos += 1;
        }
    }
    out
}

/// Cluster scan with timecode-element byte positions: (cluster_offset,
/// timecode_secs, tc_content_offset, tc_content_size). The content position
/// and size let the MSE re-feed rewrite timestamps in place — re-basing the
/// refed stream onto the post-seek domain without changing stream length
/// (sec1zt128/129).
pub(crate) fn scan_cluster_full(data: &[u8]) -> Vec<(usize, f64, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        if data[pos] == 0x1F && data[pos + 1] == 0x43 && data[pos + 2] == 0xB6 && data[pos + 3] == 0x75 {
            let window_end = (pos + 4 + 512).min(data.len());
            let mut i = pos + 4;
            let mut found = None;
            while i + 2 <= window_end {
                if data[i] == 0xE7 {
                    let (size, vlen) = read_vint(&data[i + 1..]);
                    if size > 0 && size <= 8 && i + 1 + vlen + size <= window_end {
                        let content_off = i + 1 + vlen;
                        let mut val: u64 = 0;
                        for b in &data[content_off..content_off + size] {
                            val = (val << 8) | (*b as u64);
                        }
                        found = Some((val as f64 / 1000.0, content_off, size));
                        break;
                    }
                }
                i += 1;
            }
            if let Some((tc, coff, csize)) = found {
                out.push((pos, tc, coff, csize));
            } else {
                out.push((pos, 0.0, 0, 0));
            }
            let (size, vlen) = read_vint_size(&data[pos + 4..]);
            if size > 0 {
                let next = pos + 4 + vlen + size as usize;
                pos = if next > pos { next.min(data.len()) } else { pos + 4 };
            } else {
                pos += 4;
            }
        } else {
            pos += 1;
        }
    }
    out
}

/// Scan an fmp4 bytestream for moof box byte offsets. Each moof+mdat pair is
/// a self-contained media fragment — the natural re-feed boundary (sec1zt126).
pub(crate) fn scan_moof_offsets(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        if &data[pos + 4..pos + 8] == b"moof" {
            out.push(pos);
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos = if size >= 8 { pos + size } else { pos + 8 };
        } else {
            pos += 1;
        }
    }
    out
}

/// read a vint content size (strips the marker bit)
fn read_vint(data: &[u8]) -> (usize, usize) {
    if data.is_empty() {
        return (0, 0);
    }
    let first = data[0];
    for len in 1..=8usize {
        if first & (0x80 >> (len - 1)) != 0 {
            let mut val: u64 = (first & !(0xFF << (8 - len))) as u64;
            for b in &data[1..len.min(data.len())] {
                val = (val << 8) | (*b as u64);
            }
            return (val as usize, len);
        }
    }
    (0, 0)
}

/// read the element payload size vint WITHOUT stripping unknown-size marker
fn read_vint_size(data: &[u8]) -> (u64, usize) {
    let (v, l) = read_vint(data);
    (v as u64, l)
}


/// Scan an fmp4 bytestream for per-moof decode-time spans: returns (start,end)
/// pairs in seconds using the track timescale parsed from the init segment's
/// mdhd box. `init` is the cached init segment (may be empty — then 90000 is
/// assumed, the most common timescale). Approximation: each moof contributes
/// its tfdt start; the end of the last is estimated by mirroring the average
/// inter-moof spacing, matching the webm scanner's tail-span heuristic.
pub(crate) fn scan_fmp4_timecodes(data: &[u8], init: &[u8]) -> Vec<(f64, f64)> {
    let timescale = mdhd_timescale(init).unwrap_or(90000) as f64;
    let mut starts = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        if &data[pos + 4..pos + 8] == b"tfdt" {
            let ver = data[pos + 8];
            let val = if ver == 1 {
                if pos + 20 > data.len() { break; }
                u64::from_be_bytes(data[pos + 12..pos + 20].try_into().unwrap())
            } else {
                if pos + 16 > data.len() { break; }
                u32::from_be_bytes(data[pos + 12..pos + 16].try_into().unwrap()) as u64
            };
            starts.push(val as f64 / timescale);
        }
        pos += 1;
    }
    if starts.is_empty() {
        return Vec::new();
    }
    let first = *starts.first().unwrap();
    let last = *starts.last().unwrap();
    let span = if starts.len() > 1 {
        (last - first) / (starts.len() - 1) as f64
    } else {
        1.0
    };
    vec![(first, last + span)]
}

/// Timescale from the mdhd box of an fmp4 init segment (version 0: u32 at
/// offset +20 after box type; version 1: u32 at +28).
fn mdhd_timescale(init: &[u8]) -> Option<u32> {
    let mut pos = 0usize;
    while pos + 24 <= init.len() {
        if &init[pos + 4..pos + 8] == b"mdhd" {
            let ver = init[pos + 8];
            let off = if ver == 1 { pos + 28 } else { pos + 20 };
            if off + 4 <= init.len() {
                let ts = u32::from_be_bytes(init[off..off + 4].try_into().unwrap());
                if ts > 0 {
                    return Some(ts);
                }
            }
        }
        pos += 1;
    }
    None
}


/// Apply a timestampOffset to an fmp4 byte stream in place: shift every tfdt
/// baseMediaDecodeTime by `delta_units` (in track timescale units). This is the
/// mechanism hls.js-style transmuxing depends on — segments carry their source
/// PTS and the page repositions them on the media timeline through the offset.
/// Values saturate at zero (tfdt is unsigned); negative timestamps are not
/// representable in the container and players never intend them.
pub(crate) fn rewrite_tfdt_offsets(data: &mut [u8], delta_units: i64) {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        if &data[pos + 4..pos + 8] == b"tfdt" {
            let ver = data[pos + 8];
            if ver == 1 {
                if pos + 20 > data.len() { break; }
                let cur = u64::from_be_bytes(data[pos + 12..pos + 20].try_into().unwrap()) as i64;
                let newv = (cur + delta_units).max(0) as u64;
                data[pos + 12..pos + 20].copy_from_slice(&newv.to_be_bytes());
            } else {
                if pos + 16 > data.len() { break; }
                let cur = u32::from_be_bytes(data[pos + 12..pos + 16].try_into().unwrap()) as i64;
                let newv = (cur + delta_units).max(0) as u32;
                data[pos + 12..pos + 16].copy_from_slice(&newv.to_be_bytes());
            }
        }
        pos += 1;
    }
}

/// Timescale lookup shared with scanning — exposed for offset conversion.
pub(crate) fn fmp4_timescale(init: &[u8]) -> u32 {
    mdhd_timescale(init).unwrap_or(90000)
}

/// Streaming EBML indexer for WebM MSE byte streams. Unlike the flat scanner
/// (scan_cluster_timecodes), this parses the concatenated byte stream
/// sequentially with cross-append state: real players (YouTube verified) split
/// single clusters across multiple appendBuffer calls, so per-append scanning
/// both misses split clusters and false-positives on the 1F43B675 byte pattern
/// inside binary payload — producing swiss-cheese buffered ranges that poison
/// player append scheduling. This parser skips element payloads by their exact
/// declared sizes, so payload bytes are never misinterpreted as element headers.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct WebmIndexer {
    /// Unparsed stream tail — only holds the current partial element, bounded
    /// by the largest single cluster size (a few MB at most).
    pending: Vec<u8>,
    last_timecode: Option<f64>,
}

impl WebmIndexer {
    pub(crate) fn new() -> Self {
        WebmIndexer {
            pending: Vec::new(),
            last_timecode: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.pending.clear();
        self.last_timecode = None;
    }

    /// Feed appended bytes; returns timecodes (seconds) of clusters whose
    /// heads are now fully parsed, in stream order. Cluster payload need not
    /// be complete for the timecode to count — the timecode element sits in
    /// the cluster head — but consumption only advances past complete
    /// elements so a partial tail is re-parsed with the next append.
    pub(crate) fn feed(&mut self, data: &[u8]) -> Vec<f64> {
        let mut stream = std::mem::take(&mut self.pending);
        stream.extend_from_slice(data);
        let mut out = Vec::new();
        let mut pos = 0usize;
        loop {
            let avail = stream.len() - pos;
            if avail < 2 {
                break;
            }
            let (id, idlen) = read_id(&stream[pos..]);
            if idlen == 0 || avail < idlen + 1 {
                break;
            }
            let (raw_size, szlen) = read_vint(&stream[pos + idlen..]);
            if szlen == 0 {
                break;
            }
            let header = idlen + szlen;
            const ID_EBML_HEADER: u64 = 0x1A45DFA3;
            const ID_SEGMENT: u64 = 0x18538067;
            const ID_CLUSTER: u64 = 0x1F43B675;
            const UNKNOWN: usize = usize::MAX;
            let size = if raw_size == vint_unknown_value(szlen) {
                UNKNOWN
            } else {
                raw_size
            };
            if id == ID_SEGMENT {
                // master element: descend — content continues inline. Unknown
                // size is normal for segments.
                pos += header;
                continue;
            }
            if id == ID_CLUSTER {
                if size == UNKNOWN {
                    // cannot delimit — index the head if present, then resync
                    // to the next cluster marker; if none yet, wait for more.
                    if let Some(tc) = scan_cluster_head_timecode(&stream[pos + header..]) {
                        out.push(tc);
                    }
                    match find_next_cluster(&stream[pos + header..]) {
                        Some(off) => {
                            pos += header + off;
                            continue;
                        },
                        None => break,
                    }
                }
                let total = header + size;
                if avail < total {
                    // partial cluster: keep everything for the next append.
                    break;
                }
                if let Some(tc) = scan_cluster_head_timecode(&stream[pos + header..pos + total]) {
                    out.push(tc);
                }
                pos += total;
                continue;
            }
            if size == UNKNOWN && id != ID_EBML_HEADER {
                // unknown-size non-cluster element: cannot skip safely —
                // resync to next cluster or wait.
                match find_next_cluster(&stream[pos + header..]) {
                    Some(off) => {
                        pos += header + off;
                        continue;
                    },
                    None => break,
                }
            }
            let total = header + size;
            if avail < total {
                break;
            }
            pos += total;
        }
        self.pending = stream[pos..].to_vec();
        if let Some(last) = out.last() {
            self.last_timecode = Some(*last);
        }
        out
    }

    pub(crate) fn last_timecode(&self) -> Option<f64> {
        self.last_timecode
    }
}

/// value a vint size field takes when it means "unknown size" — all payload
/// bits set for its length.
fn vint_unknown_value(len: usize) -> usize {
    if len >= 8 {
        return usize::MAX;
    }
    (1usize << (7 * len)) - 1
}

/// read an ebml element id (vint WITH marker bits retained). returns (id, len).
fn read_id(data: &[u8]) -> (u64, usize) {
    if data.is_empty() {
        return (0, 0);
    }
    let first = data[0];
    for len in 1..=4usize {
        if first & (0x80 >> (len - 1)) != 0 {
            if data.len() < len {
                return (0, 0);
            }
            let mut val: u64 = first as u64;
            for b in &data[1..len] {
                val = (val << 8) | (*b as u64);
            }
            return (val, len);
        }
    }
    (0, 0)
}

/// scan a cluster head for the Timecode element (0xE7) by properly walking
/// child elements — never byte-guessing inside payload. bounded to the first
/// 512 bytes or the cluster size, whichever is smaller.
fn scan_cluster_head_timecode(payload: &[u8]) -> Option<f64> {
    let end = payload.len().min(512);
    let mut i = 0usize;
    while i + 2 <= end {
        let (id, idlen) = read_id(&payload[i..]);
        if idlen == 0 || i + idlen >= end {
            return None;
        }
        let (size, szlen) = read_vint(&payload[i + idlen..]);
        if szlen == 0 {
            return None;
        }
        if id == 0xE7 {
            let start = i + idlen + szlen;
            if size == 0 || size > 8 || start + size > payload.len() {
                return None;
            }
            let mut val: u64 = 0;
            for b in &payload[start..start + size] {
                val = (val << 8) | (*b as u64);
            }
            // webm timecodes are in the segment timecode scale, 1ms in
            // practice for youtube and ffmpeg-produced streams.
            return Some(val as f64 / 1000.0);
        }
        // skip this child element
        let next = i + idlen + szlen + size;
        if next <= i {
            return None;
        }
        i = next;
    }
    None
}

/// find the next cluster element id byte pattern — used only as a resync
/// fallback for unknown-size elements, never as the primary parse mechanism.
fn find_next_cluster(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
}
