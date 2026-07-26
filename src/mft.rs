//! Fast NTFS enumeration (Windows only) — the trick that makes Everything
//! index a whole drive in seconds.
//!
//! Instead of walking directories, this asks the volume for its entire file
//! record table in bulk (`FSCTL_ENUM_USN_DATA`). Each record carries the
//! file's name, its own reference number, and its parent's, which is enough
//! to rebuild the whole tree. One sequential read of the volume metadata
//! replaces millions of directory opens.
//!
//! Requirements and fallbacks:
//! - NTFS only, and opening a raw volume handle needs Administrator. When
//!   either is missing, `enumerate_volume` returns None and the caller falls
//!   back to the normal directory walk.
//! - USN records carry no size or timestamp, so entries start with zeroed
//!   metadata and are filled in afterwards by a background pass.
#![cfg(target_os = "windows")]

use crate::index::{Index, NO_PARENT};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V0};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// The USN record layout we consume (V2). Declared locally: only these
/// leading fields are needed, and the name follows at `file_name_offset`.
#[repr(C)]
struct UsnRecordV2 {
    record_length: u32,
    major_version: u16,
    minor_version: u16,
    file_reference_number: u64,
    parent_file_reference_number: u64,
    usn: i64,
    time_stamp: i64,
    reason: u32,
    source_info: u32,
    security_id: u32,
    file_attributes: u32,
    file_name_length: u16,
    file_name_offset: u16,
}

struct VolumeHandle(HANDLE);

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_volume(drive_letter: char) -> Option<VolumeHandle> {
    let path: Vec<u16> = format!("\\\\.\\{drive_letter}:\0").encode_utf16().collect();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            windows_sys::Win32::Foundation::GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        None
    } else {
        Some(VolumeHandle(handle))
    }
}

/// One record pulled from the volume: name plus tree links.
struct Record {
    name: String,
    parent_frn: u64,
    is_dir: bool,
}

/// Read every file record on the volume. Returns None when the volume can't
/// be opened (not NTFS, not elevated, removable media, etc.).
fn read_records(
    handle: &VolumeHandle,
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> Option<HashMap<u64, Record>> {
    // 1 MiB at a time: fewer syscalls, and the driver fills it densely.
    let mut buffer = vec![0u8; 1 << 20];
    let mut input = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };
    let mut records: HashMap<u64, Record> = HashMap::with_capacity(1 << 16);

    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let mut bytes_returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_ENUM_USN_DATA,
                &input as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                buffer.len() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        // Failure on the first call means "can't enumerate" (wrong FS or no
        // privilege); later it simply means we reached the end.
        if ok == 0 {
            return if records.is_empty() { None } else { Some(records) };
        }
        // The buffer starts with the next start reference number (8 bytes),
        // followed by tightly packed USN records.
        if bytes_returned <= 8 {
            return Some(records);
        }
        input.StartFileReferenceNumber =
            u64::from_le_bytes(buffer[..8].try_into().ok()?);

        let mut offset = 8usize;
        while offset + std::mem::size_of::<UsnRecordV2>() <= bytes_returned as usize {
            let record = unsafe { &*(buffer.as_ptr().add(offset) as *const UsnRecordV2) };
            let len = record.record_length as usize;
            if len == 0 || offset + len > bytes_returned as usize {
                break;
            }
            let name_start = offset + record.file_name_offset as usize;
            let name_len = record.file_name_length as usize;
            if name_start + name_len <= bytes_returned as usize {
                let utf16: Vec<u16> = buffer[name_start..name_start + name_len]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&utf16);
                records.insert(
                    record.file_reference_number,
                    Record {
                        name,
                        parent_frn: record.parent_file_reference_number,
                        is_dir: record.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    },
                );
                progress.fetch_add(1, Ordering::Relaxed);
            }
            offset += len;
        }
    }
}

/// Reference number of an NTFS volume's root directory.
const ROOT_FRN: u64 = 5;

/// Build index entries for one volume, e.g. `C:\`. Entries are appended to
/// `index`; sizes and timestamps are left at zero for the metadata pass.
/// Returns false if the fast path is unavailable for this volume.
pub fn enumerate_volume(
    index: &mut Index,
    root: &Path,
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> bool {
    let Some(letter) = root
        .to_string_lossy()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
    else {
        return false;
    };
    let Some(handle) = open_volume(letter) else {
        return false;
    };
    let Some(records) = read_records(&handle, progress, cancel) else {
        return false;
    };

    let matcher = crate::util::ExclusionMatcher::new(exclusions);
    let root_path = PathBuf::from(format!("{letter}:\\"));
    let root_idx = index.push_entry_pub(&root_path.to_string_lossy(), NO_PARENT, 0, 0, true);
    index.dir_map.insert(root_path.clone(), root_idx);

    // FRN -> index slot, filled as entries are created.
    let mut slots: HashMap<u64, u32> = HashMap::with_capacity(records.len() + 1);
    slots.insert(ROOT_FRN, root_idx);

    // Resolve a record to a slot, creating ancestors first. Iterative to
    // avoid deep recursion on long paths.
    let mut stack: Vec<u64> = Vec::with_capacity(64);
    for &frn in records.keys() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if slots.contains_key(&frn) {
            continue;
        }
        stack.clear();
        let mut cur = frn;
        // Walk up to a known ancestor.
        loop {
            if slots.contains_key(&cur) {
                break;
            }
            let Some(record) = records.get(&cur) else {
                break; // orphan: parent record missing
            };
            stack.push(cur);
            if stack.len() > 512 {
                break; // pathological chain; give up on this branch
            }
            cur = record.parent_frn;
        }
        // Create entries from the highest unresolved ancestor downwards.
        while let Some(frn) = stack.pop() {
            let Some(record) = records.get(&frn) else {
                continue;
            };
            let Some(&parent_slot) = slots.get(&record.parent_frn) else {
                continue; // ancestor unresolved (orphan branch)
            };
            let parent_path = index.full_path(parent_slot);
            let path = parent_path.join(&record.name);
            if matcher.matches(&path) {
                continue;
            }
            let slot =
                index.push_entry_pub(&record.name, parent_slot, 0, 0, record.is_dir);
            slots.insert(frn, slot);
            if record.is_dir {
                index.dir_map.insert(path, slot);
            }
        }
    }
    true
}

/// True when the process can open raw volume handles (i.e. is elevated).
pub fn can_use_fast_path() -> bool {
    open_volume('C').is_some()
}
