//! Fast NTFS indexing (Windows only) — Everything's approach.
//!
//! Instead of walking directories, read the volume's file table in one pass
//! (`volume::read_mft`), either directly when elevated or through the FIND
//! service when not (`service.rs`). Every record carries its name, its parent's
//! record number, size and timestamp, which is enough to build the whole tree
//! with real metadata — no per-file `stat` afterwards.
//!
//! Before reading, the USN journal position is recorded. Live updates resume
//! from exactly there (`usn_watch.rs`), so changes made during or after the
//! scan — even while FIND is closed — are replayed instead of rescanned.
#![cfg(target_os = "windows")]

use crate::index::{Index, VolumeState, NO_PARENT};
use crate::service;
use crate::volume::{self, FileRecord, ROOT_RECORD};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The drive letter when `root` is a whole drive (`C:\`), else None. Volume
/// enumeration always covers the entire drive, so a narrower root (say
/// `C:\Users`) must use the directory walk.
pub fn drive_root_letter(root: &Path) -> Option<char> {
    let s = root.to_string_lossy();
    let mut chars = s.chars();
    let letter = chars.next().filter(|c| c.is_ascii_alphabetic())?;
    match (chars.next(), chars.next(), chars.next()) {
        (Some(':'), Some('\\') | Some('/') | None, None) => Some(letter.to_ascii_uppercase()),
        _ => None,
    }
}

/// Build index entries for one volume, e.g. `C:\`, and append them to `index`.
/// Returns false when the fast path is unavailable for this root (not a whole
/// drive, not NTFS, not elevated and no service): the caller walks instead.
pub fn enumerate_volume(
    index: &mut Index,
    root: &Path,
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> bool {
    let Some(letter) = drive_root_letter(root) else {
        return false;
    };
    // Journal position first: anything that changes while we read the MFT is
    // then replayed by the watcher. Replays are idempotent.
    let journal = service::query_journal(letter).ok();
    let Ok((records, _full_metadata)) = service::enum_volume(letter, progress, cancel) else {
        return false;
    };
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    build_entries(index, letter, records, exclusions, cancel);
    let root_str = format!("{letter}:\\");
    index.volumes.retain(|v| v.root != root_str);
    if let Some(j) = journal {
        index.volumes.push(VolumeState {
            root: root_str,
            letter,
            journal_id: j.journal_id,
            next_usn: j.next_usn,
        });
    }
    true
}

/// Turn flat records into tree entries. Directories are created parent-first;
/// files then attach to their resolved directory.
fn build_entries(
    index: &mut Index,
    letter: char,
    records: Vec<FileRecord>,
    exclusions: &[String],
    cancel: &AtomicBool,
) {
    let matcher = crate::util::ExclusionMatcher::new(exclusions);
    let root_path = PathBuf::from(format!("{letter}:\\"));
    let root_idx = index.push_entry_pub(&root_path.to_string_lossy(), NO_PARENT, 0, 0, true);
    index.dir_map.insert(root_path.clone(), root_idx);
    index.dir_frns.insert(frn_key(letter, ROOT_RECORD), root_idx);

    let mut dirs: HashMap<u64, usize> = HashMap::new();
    let mut files: Vec<usize> = Vec::with_capacity(records.len());
    for (i, r) in records.iter().enumerate() {
        if r.is_dir {
            // A directory has exactly one name (NTFS forbids directory hard
            // links); keep the first seen.
            dirs.entry(r.frn).or_insert(i);
        } else {
            files.push(i);
        }
    }

    // Directory record number -> (slot, full path).
    let mut slots: HashMap<u64, (u32, PathBuf)> = HashMap::with_capacity(dirs.len() + 1);
    slots.insert(ROOT_RECORD, (root_idx, root_path));
    let mut dead: HashSet<u64> = HashSet::new(); // excluded or orphaned
    let mut stack: Vec<u64> = Vec::with_capacity(64);

    let dir_frns: Vec<u64> = dirs.keys().copied().collect();
    for frn in dir_frns {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if slots.contains_key(&frn) || dead.contains(&frn) {
            continue;
        }
        stack.clear();
        let mut cur = frn;
        // Walk up to a resolved (or dead) ancestor.
        let mut ok = true;
        loop {
            if slots.contains_key(&cur) {
                break;
            }
            if dead.contains(&cur) || stack.len() > 512 {
                ok = false;
                break;
            }
            let Some(&ri) = dirs.get(&cur) else {
                ok = false; // parent record missing: orphan
                break;
            };
            stack.push(cur);
            cur = records[ri].parent;
            if stack.contains(&cur) {
                ok = false; // cycle: corrupt metadata
                break;
            }
        }
        if !ok {
            dead.extend(stack.drain(..));
            continue;
        }
        while let Some(d) = stack.pop() {
            let r = &records[dirs[&d]];
            let (parent_slot, parent_path) = match slots.get(&r.parent) {
                Some((s, p)) => (*s, p.clone()),
                None => {
                    dead.insert(d);
                    dead.extend(stack.drain(..));
                    break;
                }
            };
            let path = parent_path.join(&r.name);
            if matcher.matches(&path) {
                dead.insert(d);
                dead.extend(stack.drain(..));
                break;
            }
            let slot = index.push_entry_pub(&r.name, parent_slot, 0, r.modified, true);
            index.dir_map.insert(path.clone(), slot);
            index.dir_frns.insert(frn_key(letter, d), slot);
            slots.insert(d, (slot, path));
        }
    }

    for (n, i) in files.into_iter().enumerate() {
        if n % 65_536 == 0 && cancel.load(Ordering::Relaxed) {
            return;
        }
        let r = &records[i];
        let Some((parent_slot, parent_path)) = slots.get(&r.parent) else {
            continue;
        };
        if matcher.matches(&parent_path.join(&r.name)) {
            continue;
        }
        index.push_entry_pub(&r.name, *parent_slot, r.size, r.modified, false);
    }
}

/// `dir_frns` key: drive letter in the top byte, record number below. Record
/// numbers are 48-bit, so the two never collide across volumes.
pub fn frn_key(letter: char, record: u64) -> u64 {
    ((letter.to_ascii_uppercase() as u64) << 56) | volume::record_number(record)
}

/// True when a whole-drive root can use the fast path right now.
pub fn can_use_fast_path() -> bool {
    service::access_for('C') != service::Access::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(frn: u64, parent: u64, name: &str, is_dir: bool, size: u64) -> FileRecord {
        FileRecord {
            frn,
            parent,
            name: name.into(),
            is_dir,
            size,
            modified: 1_700_000_000,
        }
    }

    #[test]
    fn drive_roots_only() {
        assert_eq!(drive_root_letter(Path::new("C:\\")), Some('C'));
        assert_eq!(drive_root_letter(Path::new("d:")), Some('D'));
        assert_eq!(drive_root_letter(Path::new("C:\\Users")), None);
        assert_eq!(drive_root_letter(Path::new("\\\\server\\share")), None);
    }

    #[test]
    fn builds_tree_with_metadata_and_exclusions() {
        // Records arrive in MFT order, children before parents included.
        let records = vec![
            rec(40, 30, "deep.txt", false, 12),
            rec(30, 20, "inner", true, 0),
            rec(20, ROOT_RECORD, "outer", true, 0),
            rec(21, ROOT_RECORD, "top.bin", false, 99),
            rec(50, ROOT_RECORD, "node_modules", true, 0),
            rec(51, 50, "hidden.js", false, 1),
            rec(60, 999, "orphan.txt", false, 1),
        ];
        let mut index = Index::default();
        let cancel = AtomicBool::new(false);
        build_entries(&mut index, 'C', records, &["node_modules".into()], &cancel);

        let find = |name: &str| {
            index
                .entries
                .iter()
                .position(|e| e.name.as_ref() == name)
                .map(|i| i as u32)
        };
        let deep = find("deep.txt").expect("nested file indexed");
        assert_eq!(index.full_path(deep), PathBuf::from("C:\\outer\\inner\\deep.txt"));
        assert_eq!(index.entries[deep as usize].size, 12);
        assert_eq!(index.entries[deep as usize].modified, 1_700_000_000);
        let top = find("top.bin").expect("top-level file indexed (root FRN fix)");
        assert_eq!(index.full_path(top), PathBuf::from("C:\\top.bin"));
        assert!(find("hidden.js").is_none(), "excluded folder's children skipped");
        assert!(find("orphan.txt").is_none(), "orphans skipped");
        assert!(index.dir_frns.contains_key(&frn_key('C', 30)));
    }
}
