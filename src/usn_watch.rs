//! Live updates from the NTFS change journal (Windows only), as Everything
//! does (REA: `_db_monitor_ntfs_process_usn_records`, "Journal ID changed").
//!
//! Every create, delete and rename on an NTFS volume is appended to its USN
//! journal. Polling the journal from the position saved with the index gives
//! an exact change list — including everything that happened while FIND was
//! closed — without walking a single directory. Unlike a directory watcher,
//! nothing is missed under bursts, and no per-folder handles are held.
//!
//! When the journal no longer covers the saved position (it wrapped, or was
//! recreated), `needs_rescan` is raised and the app rebuilds that volume.
#![cfg(target_os = "windows")]

use crate::index::Index;
use crate::mft::frn_key;
use crate::service;
use crate::util::ExclusionMatcher;
use crate::volume::{
    UsnChange, VolumeError, USN_REASON_CLOSE, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub struct UsnWatchHandle {
    stop: Arc<AtomicBool>,
}

impl Drop for UsnWatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Poll every journal recorded in `index.volumes` once a second.
pub fn watch(
    index: Arc<RwLock<Index>>,
    exclusions: Vec<String>,
    dirty: Arc<AtomicBool>,
    index_locked: Arc<AtomicBool>,
    needs_rescan: Arc<AtomicBool>,
) -> UsnWatchHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let matcher = ExclusionMatcher::new(&exclusions);
    let _ = std::thread::Builder::new()
        .name("find-usn".into())
        .spawn(move || {
            // Directory renames arrive as OLD_NAME then NEW_NAME records; the
            // slot is parked here in between so the subtree moves intact.
            let mut renaming: HashMap<u64, u32> = HashMap::new();
            while !stop2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                if index_locked.load(Ordering::Relaxed) {
                    continue; // a scan owns the index; it will record a fresh position
                }
                let volumes = match index.read() {
                    Ok(g) => g.volumes.clone(),
                    Err(_) => return,
                };
                for vol in volumes {
                    let result = service::read_journal(vol.letter, vol.journal_id, vol.next_usn);
                    match result {
                        Ok((changes, next_usn)) => {
                            if next_usn == vol.next_usn && changes.is_empty() {
                                continue;
                            }
                            if index_locked.load(Ordering::Relaxed) {
                                break;
                            }
                            let Ok(mut g) = index.write() else { return };
                            for c in &changes {
                                apply(&mut g, vol.letter, c, &matcher, &mut renaming);
                            }
                            if let Some(v) = g.volumes.iter_mut().find(|v| v.root == vol.root) {
                                v.next_usn = next_usn;
                            }
                            drop(g);
                            if !changes.is_empty() {
                                dirty.store(true, Ordering::Relaxed);
                            }
                        }
                        Err(VolumeError::JournalReset) => {
                            needs_rescan.store(true, Ordering::Relaxed);
                        }
                        // Service stopped or restarting: try again next tick.
                        Err(_) => {}
                    }
                }
            }
        });
    UsnWatchHandle { stop }
}

/// Apply one journal record to the index.
pub fn apply(
    index: &mut Index,
    letter: char,
    c: &UsnChange,
    matcher: &ExclusionMatcher,
    renaming: &mut HashMap<u64, u32>,
) {
    let Some(parent_slot) = index.dir_by_frn(frn_key(letter, c.parent)) else {
        return; // parent excluded, or outside what we indexed
    };
    let path = index.full_path(parent_slot).join(&c.name);
    let key = frn_key(letter, c.frn);

    if c.reason & USN_REASON_RENAME_OLD_NAME != 0 {
        if c.is_dir {
            if let Some(slot) = index.dir_by_frn(key) {
                renaming.insert(key, slot);
                return;
            }
        }
        index.remove_path(&path);
        return;
    }

    if matcher.matches(&path) {
        // Renamed/moved into an excluded place: drop the old copy if any.
        if let Some(slot) = renaming.remove(&key) {
            let old = index.full_path(slot);
            index.remove_path(&old);
        }
        return;
    }

    if c.reason & USN_REASON_RENAME_NEW_NAME != 0 {
        if let Some(slot) = renaming.remove(&key) {
            index.move_entry(slot, parent_slot, &c.name);
            return;
        }
        if let Some(slot) = index.upsert_path_hint(&path, Some(c.is_dir)) {
            if c.is_dir {
                index.dir_frns.insert(key, slot);
            }
        }
        return;
    }

    if c.reason & USN_REASON_FILE_DELETE != 0 {
        if c.reason & USN_REASON_CLOSE != 0 {
            index.remove_path(&path);
            if c.is_dir {
                index.dir_frns.remove(&key);
            }
        }
        return;
    }

    // Created or modified. Act on the close record (reasons accumulate until
    // the last handle closes), or on the create itself so new folders exist
    // before their children's records arrive.
    if c.reason & (USN_REASON_CLOSE | USN_REASON_FILE_CREATE) != 0 {
        if let Some(slot) = index.upsert_path_hint(&path, Some(c.is_dir)) {
            if c.is_dir {
                index.dir_frns.insert(key, slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NO_PARENT;
    use crate::volume::ROOT_RECORD;

    /// A temp folder standing in for a drive root, registered by record number.
    fn setup() -> (std::path::PathBuf, Index) {
        let tmp = std::env::temp_dir().join(format!("find_usn_{}_{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut index = Index::default();
        let root = index.push_entry_pub(&tmp.to_string_lossy(), NO_PARENT, 0, 0, true);
        index.dir_map.insert(tmp.clone(), root);
        index.dir_frns.insert(frn_key('C', ROOT_RECORD), root);
        (tmp, index)
    }

    fn rand_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn change(frn: u64, parent: u64, name: &str, reason: u32, is_dir: bool) -> UsnChange {
        UsnChange {
            frn,
            parent,
            name: name.into(),
            reason,
            is_dir,
        }
    }

    fn live(index: &Index, name: &str) -> Option<u32> {
        index
            .entries
            .iter()
            .position(|e| e.name.as_ref() == name && !e.is_deleted())
            .map(|i| i as u32)
    }

    #[test]
    fn create_rename_and_delete() {
        let (tmp, mut index) = setup();
        let m = ExclusionMatcher::new(&[]);
        let mut renaming = HashMap::new();

        // Folder + file created.
        std::fs::create_dir(tmp.join("docs")).unwrap();
        apply(&mut index, 'C', &change(100, ROOT_RECORD, "docs", USN_REASON_FILE_CREATE, true), &m, &mut renaming);
        std::fs::write(tmp.join("docs").join("a.txt"), b"hello").unwrap();
        apply(&mut index, 'C', &change(101, 100, "a.txt", USN_REASON_FILE_CREATE | USN_REASON_CLOSE, false), &m, &mut renaming);
        let a = live(&index, "a.txt").expect("file added under new folder");
        assert_eq!(index.entries[a as usize].size, 5);

        // Folder renamed: the file must follow without being re-added.
        std::fs::rename(tmp.join("docs"), tmp.join("papers")).unwrap();
        apply(&mut index, 'C', &change(100, ROOT_RECORD, "docs", USN_REASON_RENAME_OLD_NAME, true), &m, &mut renaming);
        apply(&mut index, 'C', &change(100, ROOT_RECORD, "papers", USN_REASON_RENAME_NEW_NAME, true), &m, &mut renaming);
        assert_eq!(live(&index, "a.txt"), Some(a), "same slot, subtree moved");
        assert_eq!(index.full_path(a), tmp.join("papers").join("a.txt"));
        assert!(index.dir_map.contains_key(&tmp.join("papers")));
        assert!(!index.dir_map.contains_key(&tmp.join("docs")));

        // File deleted.
        std::fs::remove_file(tmp.join("papers").join("a.txt")).unwrap();
        apply(&mut index, 'C', &change(101, 100, "a.txt", USN_REASON_FILE_DELETE | USN_REASON_CLOSE, false), &m, &mut renaming);
        assert!(live(&index, "a.txt").is_none());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unreadable_paths_still_listed_by_name() {
        // The service can see files we can't stat; the name is still indexed.
        let (_tmp, mut index) = setup();
        let m = ExclusionMatcher::new(&[]);
        let mut renaming = HashMap::new();
        apply(&mut index, 'C', &change(200, ROOT_RECORD, "ghost.txt", USN_REASON_FILE_CREATE | USN_REASON_CLOSE, false), &m, &mut renaming);
        assert!(live(&index, "ghost.txt").is_some());
    }

    #[test]
    fn excluded_paths_ignored() {
        let (tmp, mut index) = setup();
        let m = ExclusionMatcher::new(&["node_modules".to_string()]);
        let mut renaming = HashMap::new();
        std::fs::create_dir(tmp.join("node_modules")).unwrap();
        apply(&mut index, 'C', &change(300, ROOT_RECORD, "node_modules", USN_REASON_FILE_CREATE, true), &m, &mut renaming);
        assert!(live(&index, "node_modules").is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
