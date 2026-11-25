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
