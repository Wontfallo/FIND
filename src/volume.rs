//! Raw NTFS volume access — the part of Everything's design that makes a full
//! index take seconds and keeps it current without rescanning.
//!
//! Two operations, both needing an open handle to `\\.\C:` (Administrator, or
//! the FIND service, see `service.rs`):
//!
//! - `read_mft`: read the Master File Table straight off the volume and parse
//!   every file record. One sequential read yields every name, parent, size,
//!   timestamp and directory flag — no per-file `stat` afterwards. This is the
//!   same approach as Everything's `ntfs_enum_mft` (found with REA: it asks
//!   `FSCTL_GET_NTFS_VOLUME_DATA` where the MFT lives, decodes the `$MFT` data
//!   runs from record 0, then reads the clusters in large chunks).
//! - `query_journal` / `read_journal`: the USN change journal, which records
//!   every create/delete/rename on the volume. Reading it from a saved
//!   position replays exactly what changed, even while FIND was closed.
//!
//! File reference numbers (FRNs) carry a 16-bit sequence number in the top
//! bits. Everything below works on the 48-bit record number (`record_number`)
//! so that parent references always match the records they point to.
#![cfg(target_os = "windows")]

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::io::AsRawHandle;

/// Record number of an NTFS volume's root directory.
pub const ROOT_RECORD: u64 = 5;
/// `$Extend` — holds NTFS internals ($UsnJrnl, $Quota, ...), never indexed.
const EXTEND_RECORD: u64 = 11;
/// Records below this are NTFS metadata files ($MFT, $LogFile, ...).
const FIRST_USER_RECORD: u64 = 16;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00b3;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00bb;
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;

const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_JOURNAL_DELETE_IN_PROGRESS: i32 = 1178;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;

pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Strip the sequence number from a file reference.
pub fn record_number(frn: u64) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}

/// One name of one file. Hard-linked files produce one record per name, as
/// in Everything; `frn` is shared between them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FileRecord {
    pub frn: u64,
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds of last modification; 0 when the source didn't carry it.
    pub modified: i64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
}

/// One journal record: something happened to `name` inside `parent`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsnChange {
    pub frn: u64,
    pub parent: u64,
    pub name: String,
    pub reason: u32,
    pub is_dir: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum VolumeError {
    /// The volume couldn't be opened: not elevated, not NTFS, not a fixed drive.
    Unavailable(String),
    /// The journal no longer covers the requested position (it wrapped, or was
    /// deleted and recreated). The index for this volume must be rebuilt.
    JournalReset,
    /// No USN journal on this volume.
    NoJournal,
    Io(String),
}

impl std::fmt::Display for VolumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VolumeError::Unavailable(m) => write!(f, "volume unavailable: {m}"),
            VolumeError::JournalReset => write!(f, "USN journal was reset"),
            VolumeError::NoJournal => write!(f, "no USN journal on this volume"),
            VolumeError::Io(m) => write!(f, "{m}"),
        }
    }
}

pub type VResult<T> = Result<T, VolumeError>;

/// Only local fixed drives are served. Keeps the service from being pointed at
/// removable media or network shares.
pub fn is_fixed_drive(letter: char) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    if !letter.is_ascii_alphabetic() {
        return false;
    }
    let root: Vec<u16> = format!("{}:\\\0", letter.to_ascii_uppercase())
        .encode_utf16()
        .collect();
    // DRIVE_FIXED = 3
    unsafe { GetDriveTypeW(root.as_ptr()) == 3 }
}

pub fn open_volume(letter: char) -> VResult<File> {
    if !is_fixed_drive(letter) {
        return Err(VolumeError::Unavailable(format!("{letter}: is not a local fixed drive")));
    }
    let path = format!("\\\\.\\{}:", letter.to_ascii_uppercase());
    std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|e| VolumeError::Unavailable(e.to_string()))
}

/// True when this process can open raw volume handles (i.e. is elevated).
pub fn can_open_directly(letter: char) -> bool {
    open_volume(letter).is_ok()
}

fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, i32> {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            code,
            if input.is_empty() { std::ptr::null() } else { input.as_ptr() as *const _ },
            input.len() as u32,
            output.as_mut_ptr() as *mut _,
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(returned as usize)
    }
}

// ---------------------------------------------------------------- byte helpers

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}
fn i64_at(b: &[u8], o: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}
fn utf16_at(b: &[u8], o: usize, chars: usize) -> Option<String> {
    let raw = b.get(o..o + chars * 2)?;
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Windows FILETIME (100 ns since 1601) to Unix seconds.
pub fn filetime_to_unix(ft: u64) -> i64 {
    if ft == 0 {
        return 0;
    }
    (ft / 10_000_000) as i64 - 11_644_473_600
}

// ------------------------------------------------------------ MFT enumeration

struct VolumeData {
    bytes_per_cluster: u64,
    record_size: usize,
    mft_valid_bytes: u64,
    mft_start_lcn: u64,
}

fn volume_data(file: &File) -> VResult<VolumeData> {
    // NTFS_VOLUME_DATA_BUFFER; extended data may follow, so leave room.
    let mut out = [0u8; 256];
    let n = ioctl(file, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut out)
        .map_err(|e| VolumeError::Unavailable(format!("not NTFS (error {e})")))?;
    if n < 96 {
        return Err(VolumeError::Io("short NTFS volume data".into()));
    }
    let bytes_per_cluster = u32_at(&out, 44).unwrap_or(0) as u64;
    let record_size = u32_at(&out, 48).unwrap_or(0) as usize;
    let mft_valid_bytes = i64_at(&out, 56).unwrap_or(0) as u64;
    let mft_start_lcn = i64_at(&out, 64).unwrap_or(0) as u64;
    if bytes_per_cluster == 0 || record_size < 512 || record_size > 65_536 {
        return Err(VolumeError::Io("implausible NTFS geometry".into()));
    }
    Ok(VolumeData {
        bytes_per_cluster,
        record_size,
        mft_valid_bytes,
        mft_start_lcn,
    })
}

/// Undo the NTFS update-sequence fixup in place. Every 512-byte stride ends
/// with a copy of the sequence number; the real bytes live in the array.
fn apply_fixup(rec: &mut [u8]) -> bool {
    let (Some(usa_off), Some(usa_count)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let usa_off = usa_off as usize;
    let usa_count = usa_count as usize;
    if usa_count < 2 || usa_off + usa_count * 2 > rec.len() {
        return false;
    }
    let stride = rec.len() / (usa_count - 1);
    if stride < 2 {
        return false;
    }
    let check = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * stride;
        if end > rec.len() {
            return false;
        }
        if rec[end - 2..end] != check {
            return false; // torn write: treat the record as unreadable
        }
        rec[end - 2] = rec[usa_off + i * 2];
        rec[end - 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// What one MFT record (plus its extension records) contributes.
#[derive(Default, Clone)]
struct Parsed {
    in_use: bool,
    is_dir: bool,
    size: u64,
    modified: i64,
    names: Vec<(u64, String)>,
}

/// Walk a record's attributes. Returns the base record number when this is an
/// extension record (its attributes belong to that base).
fn parse_record(rec: &[u8], out: &mut Parsed) -> Option<u64> {
    if rec.get(0..4)? != b"FILE" {
        return None;
    }
    let flags = u16_at(rec, 0x16)?;
    out.in_use = flags & 0x01 != 0;
    out.is_dir = flags & 0x02 != 0;
    let base = record_number(u64_at(rec, 0x20)?);
    let mut off = u16_at(rec, 0x14)? as usize;
    while off + 16 <= rec.len() {
        let kind = u32_at(rec, off)?;
        if kind == 0xFFFF_FFFF {
            break;
        }
        let len = u32_at(rec, off + 4)? as usize;
        if len < 16 || off + len > rec.len() {
            break;
        }
        let attr = &rec[off..off + len];
        let non_resident = attr[8] != 0;
        let name_len = attr[9];
        let value = if non_resident {
            None
        } else {
            let vlen = u32_at(attr, 0x10)? as usize;
            let voff = u16_at(attr, 0x14)? as usize;
            attr.get(voff..voff + vlen)
        };
        match kind {
            // $STANDARD_INFORMATION: modification time at +8.
            0x10 => {
                if let Some(v) = value {
                    if let Some(ft) = u64_at(v, 8) {
                        out.modified = filetime_to_unix(ft);
                    }
                }
            }
            // $FILE_NAME: parent reference, then the name.
            0x30 => {
                if let Some(v) = value {
                    let parent = record_number(u64_at(v, 0)?);
                    let chars = *v.get(0x40)? as usize;
                    let namespace = *v.get(0x41)?;
                    // Namespace 2 is the 8.3 DOS alias of a name we already
                    // have; skipping it avoids "PROGRA~1"-style duplicates.
                    if namespace != 2 {
                        if let Some(name) = utf16_at(v, 0x42, chars) {
                            out.names.push((parent, name));
                        }
                    }
                }
            }
            // Unnamed $DATA: the file's size.
            0x80 if name_len == 0 => {
                if non_resident {
                    // Only the first extent (lowest VCN 0) carries real sizes.
                    if u64_at(attr, 0x10)? == 0 {
                        out.size = u64_at(attr, 0x30)?;
                    }
                } else if let Some(v) = value {
                    out.size = v.len() as u64;
                }
            }
            _ => {}
        }
        off += len;
    }
    (base != 0).then_some(base)
}

/// Decode the mapping pairs of a non-resident attribute into (LCN, clusters).
fn decode_runs(attr: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut off = u16_at(attr, 0x20)? as usize;
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    while off < attr.len() {
        let header = attr[off];
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        let mut length: u64 = 0;
        for i in 0..len_size {
            length |= (*attr.get(off + 1 + i)? as u64) << (8 * i);
        }
        if off_size == 0 {
            // Sparse run: the MFT is never sparse, so this is corruption.
            return None;
        }
        let mut delta: i64 = 0;
        for i in 0..off_size {
            delta |= (*attr.get(off + 1 + len_size + i)? as i64) << (8 * i);
        }
        // Sign-extend.
        let shift = 64 - 8 * off_size as u32;
        delta = (delta << shift) >> shift;
        lcn += delta;
        if lcn < 0 {
            return None;
        }
        runs.push((lcn as u64, length));
        off += 1 + len_size + off_size;
    }
    Some(runs)
}

/// Find the `$MFT`'s own data runs in record 0. Fails (and the caller falls
/// back to `FSCTL_ENUM_USN_DATA`) when the MFT is so fragmented that its runs
/// spill into an attribute list.
fn mft_runs(rec0: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut off = u16_at(rec0, 0x14)? as usize;
    while off + 16 <= rec0.len() {
        let kind = u32_at(rec0, off)?;
        if kind == 0xFFFF_FFFF {
            break;
        }
        let len = u32_at(rec0, off + 4)? as usize;
        if len < 16 || off + len > rec0.len() {
            break;
        }
        let attr = &rec0[off..off + len];
        if kind == 0x20 {
            return None; // $ATTRIBUTE_LIST: runs continue elsewhere
        }
        if kind == 0x80 && attr[9] == 0 && attr[8] != 0 {
            return decode_runs(attr);
        }
        off += len;
    }
    None
}

/// Read every file record on the volume. `progress` counts records parsed.
pub fn read_mft(
    letter: char,
    progress: &std::sync::atomic::AtomicUsize,
    cancel: &std::sync::atomic::AtomicBool,
) -> VResult<Vec<FileRecord>> {
    use std::sync::atomic::Ordering;
    let mut file = open_volume(letter)?;
    let vd = volume_data(&file)?;
    let rs = vd.record_size;
    if (vd.bytes_per_cluster as usize) < rs && rs % vd.bytes_per_cluster as usize == 0 {
        // Records larger than a cluster can straddle two runs; the chunked
        // reader below assumes they don't. Rare (512-byte-cluster volumes),
        // and the caller falls back to FSCTL_ENUM_USN_DATA.
        return Err(VolumeError::Io("file records larger than clusters".into()));
    }
    let total = (vd.mft_valid_bytes / rs as u64) as usize;

    // Record 0 describes the MFT itself.
    let mut rec0 = vec![0u8; rs.max(vd.bytes_per_cluster as usize)];
    file.seek(SeekFrom::Start(vd.mft_start_lcn * vd.bytes_per_cluster))
        .map_err(|e| VolumeError::Io(e.to_string()))?;
    file.read_exact(&mut rec0)
        .map_err(|e| VolumeError::Io(e.to_string()))?;
    rec0.truncate(rs);
    if !apply_fixup(&mut rec0) {
        return Err(VolumeError::Io("MFT record 0 failed fixup".into()));
    }
    let runs = mft_runs(&rec0).ok_or_else(|| VolumeError::Io("MFT runs not in record 0".into()))?;

    let mut parsed: Vec<Parsed> = vec![Parsed::default(); total];
    // Extension records hold overflow attributes for a base record; merge them
    // in after the pass so the base sees all its names.
    let mut extensions: Vec<(u64, Parsed)> = Vec::new();

    // Read in ~4 MiB chunks, rounded to whole clusters and whole records.
    let cluster = vd.bytes_per_cluster as usize;
    let unit = cluster.max(rs);
    let chunk_bytes = ((4 << 20) / unit).max(1) * unit;
    let mut buf = vec![0u8; chunk_bytes];
    let mut record_no: usize = 0;

    'runs: for (lcn, clusters) in runs {
        let mut remaining = clusters * vd.bytes_per_cluster;
        file.seek(SeekFrom::Start(lcn * vd.bytes_per_cluster))
            .map_err(|e| VolumeError::Io(e.to_string()))?;
        while remaining > 0 && record_no < total {
            if cancel.load(Ordering::Relaxed) {
                return Err(VolumeError::Io("cancelled".into()));
            }
            let want = (remaining as usize).min(chunk_bytes);
            let got = &mut buf[..want];
            file.read_exact(got)
                .map_err(|e| VolumeError::Io(format!("MFT read failed: {e}")))?;
            remaining -= want as u64;
            for rec in got.chunks_exact_mut(rs) {
                if record_no >= total {
                    break 'runs;
                }
                if apply_fixup(rec) {
                    let mut p = Parsed::default();
                    if let Some(base) = parse_record(rec, &mut p) {
                        if p.in_use {
                            extensions.push((base, p));
                        }
                    } else {
                        parsed[record_no] = p;
                    }
                }
                record_no += 1;
            }
            progress.store(record_no, Ordering::Relaxed);
        }
    }

    for (base, ext) in extensions {
        if let Some(b) = parsed.get_mut(base as usize) {
            b.names.extend(ext.names);
            if ext.size > b.size {
                b.size = ext.size;
            }
        }
    }

    let mut out = Vec::with_capacity(total);
    for (no, p) in parsed.into_iter().enumerate() {
        let no = no as u64;
        if !p.in_use || no == ROOT_RECORD || (no < FIRST_USER_RECORD) {
            continue;
        }
        for (parent, name) in p.names {
            if parent == EXTEND_RECORD {
                continue;
            }
            out.push(FileRecord {
                frn: no,
                parent,
                name,
                is_dir: p.is_dir,
                size: if p.is_dir { 0 } else { p.size },
                modified: p.modified,
            });
        }
    }
    Ok(out)
}

/// Slower fallback: names and tree links only (no size or date), via
/// `FSCTL_ENUM_USN_DATA`. Used when the raw MFT can't be parsed.
pub fn enum_usn_data(
    letter: char,
    progress: &std::sync::atomic::AtomicUsize,
    cancel: &std::sync::atomic::AtomicBool,
) -> VResult<Vec<FileRecord>> {
    use std::sync::atomic::Ordering;
    let file = open_volume(letter)?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut start: u64 = 0;
    let mut out = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(VolumeError::Io("cancelled".into()));
        }
        // MFT_ENUM_DATA_V0 { StartFileReferenceNumber, LowUsn, HighUsn }
        let mut input = [0u8; 24];
        input[0..8].copy_from_slice(&start.to_le_bytes());
        input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        let n = match ioctl(&file, FSCTL_ENUM_USN_DATA, &input, &mut buffer) {
            Ok(n) => n,
            Err(ERROR_HANDLE_EOF) => break,
            Err(e) if out.is_empty() => {
                return Err(VolumeError::Unavailable(format!("enum failed (error {e})")))
            }
            Err(_) => break,
        };
        if n <= 8 {
            break;
        }
        start = u64_at(&buffer, 0).unwrap_or(0);
        for rec in usn_records(&buffer[8..n]) {
            if record_number(rec.frn) == ROOT_RECORD {
                continue;
            }
            out.push(FileRecord {
                frn: record_number(rec.frn),
                parent: record_number(rec.parent),
                name: rec.name,
                is_dir: rec.is_dir,
                size: 0,
                modified: 0,
            });
        }
        progress.store(out.len(), Ordering::Relaxed);
    }
    Ok(out)
}

/// Parse packed USN_RECORD_V2 entries.
fn usn_records(mut b: &[u8]) -> Vec<UsnChange> {
    let mut out = Vec::new();
    while b.len() >= 60 {
        let len = u32_at(b, 0).unwrap_or(0) as usize;
        if len < 60 || len > b.len() {
            break;
        }
        let rec = &b[..len];
        if u16_at(rec, 4) == Some(2) {
            let name_len = u16_at(rec, 56).unwrap_or(0) as usize;
            let name_off = u16_at(rec, 58).unwrap_or(0) as usize;
            if let Some(name) = utf16_at(rec, name_off, name_len / 2) {
                out.push(UsnChange {
                    frn: u64_at(rec, 8).unwrap_or(0),
                    parent: u64_at(rec, 16).unwrap_or(0),
                    reason: u32_at(rec, 40).unwrap_or(0),
                    is_dir: u32_at(rec, 52).unwrap_or(0) & FILE_ATTRIBUTE_DIRECTORY != 0,
                    name,
                });
            }
        }
        b = &b[len..];
    }
    out
}

// --------------------------------------------------------------- USN journal

pub fn query_journal(letter: char) -> VResult<JournalInfo> {
    let file = open_volume(letter)?;
    let mut out = [0u8; 80];
    match ioctl(&file, FSCTL_QUERY_USN_JOURNAL, &[], &mut out) {
        Ok(n) if n >= 32 => Ok(JournalInfo {
            journal_id: u64_at(&out, 0).unwrap_or(0),
            first_usn: i64_at(&out, 8).unwrap_or(0),
            next_usn: i64_at(&out, 16).unwrap_or(0),
            lowest_valid_usn: i64_at(&out, 24).unwrap_or(0),
        }),
        Ok(_) => Err(VolumeError::Io("short journal data".into())),
        Err(ERROR_JOURNAL_NOT_ACTIVE) | Err(ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
            Err(VolumeError::NoJournal)
        }
        Err(e) => Err(VolumeError::Io(format!("query journal failed (error {e})"))),
    }
}

/// Read journal records from `start_usn`. Returns the changes and the USN to
/// continue from. Never blocks waiting for new records. At most `max_bytes`
/// of records are returned per call so a huge backlog arrives in pieces.
pub fn read_journal(
    letter: char,
    journal_id: u64,
    start_usn: i64,
    max_bytes: usize,
) -> VResult<(Vec<UsnChange>, i64)> {
    let file = open_volume(letter)?;
    let info = query_journal(letter)?;
    if info.journal_id != journal_id || start_usn < info.lowest_valid_usn {
        return Err(VolumeError::JournalReset);
    }
    let mut buffer = vec![0u8; 1 << 16];
    let mut usn = start_usn;
    let mut changes = Vec::new();
    let mut read = 0usize;
    while read < max_bytes {
        // READ_USN_JOURNAL_DATA_V0 { StartUsn, ReasonMask, ReturnOnlyOnClose,
        //   Timeout, BytesToWaitFor, UsnJournalID }
        let mut input = [0u8; 40];
        input[0..8].copy_from_slice(&usn.to_le_bytes());
        input[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        input[32..40].copy_from_slice(&journal_id.to_le_bytes());
        let n = match ioctl(&file, FSCTL_READ_USN_JOURNAL, &input, &mut buffer) {
            Ok(n) => n,
            Err(ERROR_JOURNAL_ENTRY_DELETED) => return Err(VolumeError::JournalReset),
            Err(ERROR_JOURNAL_NOT_ACTIVE) | Err(ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                return Err(VolumeError::JournalReset)
            }
            Err(e) => return Err(VolumeError::Io(format!("read journal failed (error {e})"))),
        };
        if n < 8 {
            break;
        }
        let next = i64_at(&buffer, 0).unwrap_or(usn);
        if n == 8 || next == usn {
            usn = next;
            break; // caught up
        }
        changes.extend(usn_records(&buffer[8..n]).into_iter().map(|mut c| {
            c.frn = record_number(c.frn);
            c.parent = record_number(c.parent);
            c
        }));
        read += n;
        usn = next;
    }
    Ok((changes, usn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_number_strips_sequence() {
        // The bug this module fixes: the root's parent reference carries
        // sequence 5, so comparing against a bare 5 never matched.
        assert_eq!(record_number(0x0005_0000_0000_0005), ROOT_RECORD);
        assert_eq!(record_number(0x0001_0000_0012_3456), 0x12_3456);
    }

    #[test]
    fn filetime_epoch() {
        // 2024-01-01T00:00:00Z
        assert_eq!(filetime_to_unix(133_485_408_000_000_000), 1_704_067_200);
        assert_eq!(filetime_to_unix(0), 0);
    }

    #[test]
    fn decode_runs_sign_extends() {
        // Fake non-resident attribute: runs start at 0x40.
        let mut attr = vec![0u8; 0x40];
        attr[0x20] = 0x40;
        // Run 1: 0x11 → 1-byte length 0x10, 1-byte offset 0x20 (LCN 32).
        // Run 2: 0x11 → length 0x08, offset 0xF0 (-16 → LCN 16).
        attr.extend_from_slice(&[0x11, 0x10, 0x20, 0x11, 0x08, 0xF0, 0x00]);
        assert_eq!(decode_runs(&attr), Some(vec![(32, 16), (16, 8)]));
    }

    #[test]
    fn fixup_restores_sector_tails() {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes()); // USA offset
        rec[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 check + 2 strides
        rec[48..50].copy_from_slice(&[0xAB, 0xCD]); // sequence number
        rec[50..52].copy_from_slice(&[0x11, 0x22]); // real tail of stride 1
        rec[52..54].copy_from_slice(&[0x33, 0x44]); // real tail of stride 2
        rec[510..512].copy_from_slice(&[0xAB, 0xCD]);
        rec[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        assert!(apply_fixup(&mut rec));
        assert_eq!(&rec[510..512], &[0x11, 0x22]);
        assert_eq!(&rec[1022..1024], &[0x33, 0x44]);
        // A torn record (tail doesn't match the sequence number) is rejected.
        let mut torn = rec.clone();
        torn[48..50].copy_from_slice(&[0x99, 0x99]);
        assert!(!apply_fixup(&mut torn));
    }
}
