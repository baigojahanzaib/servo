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
