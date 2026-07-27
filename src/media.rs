//! Lightweight media inspection for the preview pane: reads container
//! headers to report duration, resolution and codecs without decoding or
//! pulling in a media framework.
//!
//! Supported: MP4/MOV/M4V (ISO base media) and Matroska/WebM. Other formats
//! fall back to plain file info.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Default)]
pub struct MediaInfo {
    pub duration_secs: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
}

impl MediaInfo {
    pub fn is_empty(&self) -> bool {
        self.duration_secs.is_none() && self.width.is_none() && self.format.is_none()
    }

    /// "3:42 • 1920×1080 • MP4"
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(d) = self.duration_secs {
            let total = d.round() as u64;
            let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
            parts.push(if h > 0 {
                format!("{h}:{m:02}:{s:02}")
            } else {
                format!("{m}:{s:02}")
            });
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            parts.push(format!("{w}×{h}"));
        }
        if let Some(f) = &self.format {
            parts.push(f.clone());
        }
        parts.join("  •  ")
    }
}

pub fn is_media(name: &str) -> bool {
    let ext = crate::util::extension_of(name).map(|e| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "mp4" | "mov" | "m4v" | "m4a" | "mkv" | "webm" | "avi" | "wmv" | "flv" | "mp3"
                | "flac" | "wav" | "ogg" | "opus" | "aac"
        )
    )
}

pub fn probe(path: &Path) -> MediaInfo {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut info = match ext.as_str() {
        "mp4" | "mov" | "m4v" | "m4a" => probe_mp4(path).unwrap_or_default(),
        "mkv" | "webm" => probe_matroska(path).unwrap_or_default(),
        _ => MediaInfo::default(),
    };
    if info.format.is_none() && !ext.is_empty() {
        info.format = Some(ext.to_uppercase());
    }
    info
}

/// Walk the ISO-BMFF box tree for `mvhd` (duration) and `tkhd` (dimensions).
fn probe_mp4(path: &Path) -> Option<MediaInfo> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut info = MediaInfo {
        format: Some("MP4".into()),
        ..Default::default()
    };
    // (offset, end) pairs still to visit; only containers are descended into.
    let mut stack = vec![(0u64, file_len)];
    let mut visited = 0;
    while let Some((mut pos, end)) = stack.pop() {
        while pos + 8 <= end {
            visited += 1;
            if visited > 4096 {
                return Some(info); // malformed/huge: stop early
            }
            let mut header = [0u8; 8];
            file.seek(SeekFrom::Start(pos)).ok()?;
            if file.read_exact(&mut header).is_err() {
                break;
            }
            let size = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
            let kind = &header[4..8];
            let (body, box_size) = match size {
                0 => (pos + 8, end - pos),  // extends to end of parent
                1 => {
                    let mut ext = [0u8; 8];
                    file.read_exact(&mut ext).ok()?;
                    (pos + 16, u64::from_be_bytes(ext))
                }
                s if s < 8 => break, // invalid
                s => (pos + 8, s),
            };
            match kind {
                b"moov" | b"trak" | b"mdia" => {
                    stack.push((body, (pos + box_size).min(end)));
                }
                b"mvhd" => {
                    let mut buf = [0u8; 20];
                    file.seek(SeekFrom::Start(body)).ok()?;
                    if file.read_exact(&mut buf).is_ok() {
                        let version = buf[0];
                        let (scale, dur) = if version == 1 {
                            let mut b = [0u8; 20];
                            file.seek(SeekFrom::Start(body + 4)).ok()?;
                            file.read_exact(&mut b).ok()?;
                            (
                                u32::from_be_bytes(b[16..20].try_into().ok()?) as f64,
                                0.0,
                            )
                        } else {
                            (
                                u32::from_be_bytes(buf[12..16].try_into().ok()?) as f64,
                                u32::from_be_bytes(buf[16..20].try_into().ok()?) as f64,
                            )
                        };
                        if scale > 0.0 && dur > 0.0 {
                            info.duration_secs = Some(dur / scale);
                        }
                    }
                }
                b"tkhd" => {
                    // Width/height are the last 8 bytes, 16.16 fixed point.
                    let len = box_size.saturating_sub(8) as usize;
                    if len >= 8 {
                        let mut buf = [0u8; 8];
                        file.seek(SeekFrom::Start(body + len as u64 - 8)).ok()?;
                        if file.read_exact(&mut buf).is_ok() {
                            let w = u32::from_be_bytes(buf[..4].try_into().ok()?) >> 16;
                            let h = u32::from_be_bytes(buf[4..].try_into().ok()?) >> 16;
                            if w > 0 && h > 0 {
                                info.width = Some(w);
                                info.height = Some(h);
                            }
                        }
                    }
                }
                _ => {}
            }
            pos += box_size.max(8);
        }
    }
    Some(info)
}

/// Matroska: scan the header for the Duration and PixelWidth/Height IDs.
/// A light scan of the first megabyte, which covers normal files' headers.
fn probe_matroska(path: &Path) -> Option<MediaInfo> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; (1 << 20).min(file.metadata().ok()?.len() as usize)];
    file.read_exact(&mut buf).ok()?;
    let mut info = MediaInfo {
        format: Some("MKV".into()),
        ..Default::default()
    };

    let mut timecode_scale = 1_000_000f64; // default: 1ms
    let mut i = 0usize;
    while i + 4 < buf.len() {
        match (buf[i], buf[i + 1]) {
            // TimecodeScale (0x2AD7B1) — 3-byte ID
            (0x2A, 0xD7) if buf.get(i + 2) == Some(&0xB1) => {
                if let Some((v, _)) = read_uint(&buf, i + 3) {
                    timecode_scale = v as f64;
                }
            }
            // Duration (0x4489) — float
            (0x44, 0x89) => {
                if let Some((len, off)) = read_len(&buf, i + 2) {
                    let start = off;
                    let d = match len {
                        4 => buf
                            .get(start..start + 4)
                            .and_then(|b| b.try_into().ok())
                            .map(|b| f32::from_be_bytes(b) as f64),
                        8 => buf
                            .get(start..start + 8)
                            .and_then(|b| b.try_into().ok())
                            .map(f64::from_be_bytes),
                        _ => None,
                    };
                    if let Some(d) = d {
                        info.duration_secs = Some(d * timecode_scale / 1e9);
                    }
                }
            }
            // PixelWidth (0xB0) / PixelHeight (0xBA)
            (0xB0, _) if info.width.is_none() => {
                if let Some((v, _)) = read_uint(&buf, i + 1) {
                    if v > 0 && v < 100_000 {
                        info.width = Some(v as u32);
                    }
                }
            }
            (0xBA, _) if info.height.is_none() => {
                if let Some((v, _)) = read_uint(&buf, i + 1) {
                    if v > 0 && v < 100_000 {
                        info.height = Some(v as u32);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Some(info)
}

/// EBML length descriptor at `pos`: returns (length, offset of the payload).
fn read_len(buf: &[u8], pos: usize) -> Option<(usize, usize)> {
    let first = *buf.get(pos)?;
    let extra = first.leading_zeros() as usize;
    if extra > 7 {
        return None;
    }
    let mut value = (first as usize) & (0x7F >> extra);
    for k in 1..=extra {
        value = (value << 8) | *buf.get(pos + k)? as usize;
    }
    Some((value, pos + extra + 1))
}

/// EBML unsigned integer element at `pos` (length descriptor then bytes).
fn read_uint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let (len, start) = read_len(buf, pos)?;
    if len == 0 || len > 8 {
        return None;
    }
    let mut value = 0u64;
    for k in 0..len {
        value = (value << 8) | *buf.get(start + k)? as u64;
    }
    Some((value, start + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_formatting() {
        let info = MediaInfo {
            duration_secs: Some(222.0),
            width: Some(1920),
            height: Some(1080),
            format: Some("MP4".into()),
        };
        assert_eq!(info.summary(), "3:42  •  1920×1080  •  MP4");

        let long = MediaInfo {
            duration_secs: Some(3725.0),
            ..Default::default()
        };
        assert_eq!(long.summary(), "1:02:05");
    }

    #[test]
    fn test_is_media() {
        assert!(is_media("clip.MP4"));
        assert!(is_media("song.flac"));
        assert!(!is_media("notes.txt"));
    }

    #[test]
    fn test_ebml_readers() {
        // 0x81 = length 1, value 0x20 -> 32
        assert_eq!(read_uint(&[0x81, 0x20], 0), Some((32, 2)));
        // 0x40 0x02 = 2-byte length descriptor, value 2
        assert_eq!(read_len(&[0x40, 0x02, 0xAA, 0xBB], 0), Some((2, 2)));
    }
}
