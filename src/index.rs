//! The file index: an Everything-style in-memory catalog of every file and
//! folder on the configured roots. Entries store only their own name plus a
//! parent pointer, so full paths are reconstructed on demand and memory stays
//! small even with millions of files.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

pub const NO_PARENT: u32 = u32::MAX;

const FLAG_DIR: u8 = 1;
const FLAG_DELETED: u8 = 2;

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub name: Box<str>,
    pub parent: u32,
    pub size: u64,
    /// Unix seconds of last modification.
    pub modified: i64,
    flags: u8,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub entries: Vec<Entry>,
    /// Full path -> entry index, for directories only. Used to resolve parents
    /// during scanning and to apply file-watcher events.
    pub dir_map: HashMap<PathBuf, u32>,
    /// Children of each directory entry.
    pub children: HashMap<u32, Vec<u32>>,
    pub roots: Vec<PathBuf>,
    pub scanned_at: i64,
}

impl Index {
    /// Number of live (non-deleted) entries.
    pub fn live_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_deleted()).count()
    }

    pub fn full_path(&self, idx: u32) -> PathBuf {
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = idx;
        loop {
            let e = &self.entries[cur as usize];
            parts.push(&e.name);
            if e.parent == NO_PARENT {
                break;
            }
            cur = e.parent;
        }
        let mut path = PathBuf::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for part in parts.iter().rev() {
            if path.as_os_str().is_empty() {
                path.push(part);
            } else {
                path.push(part);
            }
        }
        path
    }

    pub fn full_path_string(&self, idx: u32) -> String {
        self.full_path(idx).to_string_lossy().into_owned()
    }

    fn push_entry(&mut self, name: &str, parent: u32, size: u64, modified: i64, is_dir: bool) -> u32 {
        let idx = self.entries.len() as u32;
        self.entries.push(Entry {
            name: name.into(),
            parent,
            size,
            modified,
            flags: if is_dir { FLAG_DIR } else { 0 },
        });
        if parent != NO_PARENT {
            self.children.entry(parent).or_default().push(idx);
        }
        idx
    }

    /// Insert or refresh a single path (used by the file watcher).
    pub fn upsert_path(&mut self, path: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        let modified = system_time_secs(meta.modified().ok());
        let size = if meta.is_dir() { 0 } else { meta.len() };
        let is_dir = meta.is_dir();

        // Existing directory: refresh in place.
        if let Some(&idx) = self.dir_map.get(path) {
            let e = &mut self.entries[idx as usize];
            e.modified = modified;
            e.flags &= !FLAG_DELETED;
            return;
        }

        let Some(parent_path) = path.parent() else {
            return;
        };
        let Some(&parent_idx) = self.dir_map.get(parent_path) else {
            // Parent isn't indexed (excluded or root not covered) — ignore.
            return;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };

        // Existing file under this parent: refresh.
        if let Some(kids) = self.children.get(&parent_idx) {
            for &k in kids {
                let e = &self.entries[k as usize];
                if !e.is_dir() == !is_dir && e.name.as_ref() == name {
                    let e = &mut self.entries[k as usize];
                    e.size = size;
                    e.modified = modified;
                    e.flags &= !FLAG_DELETED;
                    return;
                }
            }
        }

        let idx = self.push_entry(&name, parent_idx, size, modified, is_dir);
        if is_dir {
            self.dir_map.insert(path.to_path_buf(), idx);
        }
    }

    /// Mark a path (and any subtree) deleted (used by the file watcher).
    pub fn remove_path(&mut self, path: &Path) {
        if let Some(&idx) = self.dir_map.get(path) {
            self.mark_deleted_recursive(idx);
            self.dir_map.remove(path);
            return;
        }
        // A file: locate through its parent's children.
        let Some(parent_idx) = path.parent().and_then(|p| self.dir_map.get(p)).copied() else {
            return;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
            return;
        };
        if let Some(kids) = self.children.get(&parent_idx).cloned() {
            for k in kids {
                if self.entries[k as usize].name.as_ref() == name.as_ref() {
                    self.mark_deleted_recursive(k);
                }
            }
        }
    }

    fn mark_deleted_recursive(&mut self, idx: u32) {
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            self.entries[i as usize].flags |= FLAG_DELETED;
            if let Some(kids) = self.children.get(&i) {
                stack.extend(kids.iter().copied());
            }
        }
    }
}

fn system_time_secs(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Scan all roots into a fresh index. `progress` counts entries discovered so
/// far; `cancel` aborts the scan early (the partial index is still returned).
pub fn scan(
    roots: &[PathBuf],
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) -> Index {
    let mut index = Index {
        roots: roots.to_vec(),
        scanned_at: system_time_secs(Some(SystemTime::now())),
        ..Default::default()
    };
    progress.store(0, Ordering::Relaxed);

    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        scan_root(&mut index, root, exclusions, progress, cancel);
    }
    index
}

type Walker = jwalk::WalkDirGeneric<((), Option<std::fs::Metadata>)>;

fn make_walker(root: &Path, exclusions: &[String]) -> Walker {
    let matcher = std::sync::Arc::new(crate::util::ExclusionMatcher::new(exclusions));
    Walker::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|res| match res {
                Ok(entry) => !matcher.matches(&entry.path()),
                Err(_) => false,
            });
            for child in children.iter_mut().flatten() {
                child.client_state = child.metadata().ok();
            }
        })
}

/// First-run scan: streams entries into the shared `live` index in batches so
/// the app is searchable immediately, with results growing as the scan runs.
/// Only call when starting from scratch — it replaces whatever is in `live`.
pub fn scan_into(
    live: &std::sync::RwLock<Index>,
    roots: &[PathBuf],
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
    dirty: &AtomicBool,
) {
    const BATCH: usize = 65_536;
    {
        let mut guard = live.write().unwrap();
        *guard = Index {
            roots: roots.to_vec(),
            scanned_at: system_time_secs(Some(SystemTime::now())),
            ..Default::default()
        };
    }
    progress.store(0, Ordering::Relaxed);

    // Local mirrors let the walk assign final indices without holding the lock.
    let mut dir_map_local: HashMap<PathBuf, u32> = HashMap::new();
    let mut pending: Vec<Entry> = Vec::with_capacity(BATCH);
    let mut pending_dirs: Vec<(PathBuf, u32)> = Vec::new();
    let mut base: u32 = 0;
    let mut last_save = std::time::Instant::now();

    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        for entry in make_walker(root, exclusions) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let is_dir = entry.file_type().is_dir();
            let (size, modified) = match &entry.client_state {
                Some(meta) => (
                    if is_dir { 0 } else { meta.len() },
                    system_time_secs(meta.modified().ok()),
                ),
                None => (0, 0),
            };
            let parent_idx = if entry.depth() == 0 {
                NO_PARENT
            } else {
                match dir_map_local.get(entry.parent_path()) {
                    Some(&i) => i,
                    None => continue,
                }
            };
            let name = if entry.depth() == 0 {
                path.to_string_lossy().into_owned()
            } else {
                entry.file_name().to_string_lossy().into_owned()
            };

            let idx = base + pending.len() as u32;
            pending.push(Entry {
                name: name.into(),
                parent: parent_idx,
                size,
                modified,
                flags: if is_dir { FLAG_DIR } else { 0 },
            });
            if is_dir {
                dir_map_local.insert(path.clone(), idx);
                pending_dirs.push((path, idx));
            }
            progress.fetch_add(1, Ordering::Relaxed);
            if pending.len() >= BATCH {
                flush_batch(live, &mut pending, &mut pending_dirs, &mut base, dirty);
                // Checkpoint at most once a minute: serializing a huge index
                // holds the read lock for seconds, which queues a writer and
                // stalls everything behind it — keep that rare.
                if last_save.elapsed().as_secs() >= 60 {
                    if let Ok(guard) = live.read() {
                        let _ = save_to_disk(&guard);
                    }
                    last_save = std::time::Instant::now();
                }
            }
        }
    }
    flush_batch(live, &mut pending, &mut pending_dirs, &mut base, dirty);
}

fn flush_batch(
    live: &std::sync::RwLock<Index>,
    pending: &mut Vec<Entry>,
    pending_dirs: &mut Vec<(PathBuf, u32)>,
    base: &mut u32,
    dirty: &AtomicBool,
) {
    if pending.is_empty() && pending_dirs.is_empty() {
        return;
    }
    let mut guard = live.write().unwrap();
    for entry in pending.drain(..) {
        let idx = guard.entries.len() as u32;
        if entry.parent != NO_PARENT {
            guard.children.entry(entry.parent).or_default().push(idx);
        }
        guard.entries.push(entry);
    }
    guard.dir_map.extend(pending_dirs.drain(..));
    *base = guard.entries.len() as u32;
    dirty.store(true, Ordering::Relaxed);
}

fn scan_root(
    index: &mut Index,
    root: &Path,
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
) {
    for entry in make_walker(root, exclusions) {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let is_dir = entry.file_type().is_dir();
        let (size, modified) = match &entry.client_state {
            Some(meta) => (
                if is_dir { 0 } else { meta.len() },
                system_time_secs(meta.modified().ok()),
            ),
            None => (0, 0),
        };

        let parent_idx = if entry.depth() == 0 {
            NO_PARENT
        } else {
            match index.dir_map.get(entry.parent_path()) {
                Some(&i) => i,
                None => continue, // parent excluded/failed
            }
        };

        let name = if entry.depth() == 0 {
            // Keep the root's full path as its "name" so paths reconstruct.
            path.to_string_lossy().into_owned()
        } else {
            entry.file_name().to_string_lossy().into_owned()
        };

        let idx = index.push_entry(&name, parent_idx, size, modified, is_dir);
        if is_dir {
            index.dir_map.insert(path, idx);
        }
        let n = progress.fetch_add(1, Ordering::Relaxed);
        if n % 100_000 == 0 {
            // Cheap heartbeat point; nothing to do, but keeps the counter hot.
        }
    }
}

/// Where the serialized index lives on disk.
pub fn cache_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "find", "FIND")
        .map(|d| d.cache_dir().join("index.bin"))
}

pub fn save_to_disk(index: &Index) -> std::io::Result<()> {
    let Some(path) = cache_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = bincode::serialize(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_from_disk() -> Option<Index> {
    let path = cache_path()?;
    let bytes = std::fs::read(path).ok()?;
    bincode::deserialize(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_index(dir: &Path) -> Index {
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        scan(&[dir.to_path_buf()], &[], &progress, &cancel)
    }

    #[test]
    fn test_scan_and_paths() {
        let tmp = std::env::temp_dir().join(format!("find_test_{}", std::process::id()));
        let sub = tmp.join("sub dir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("hello.txt"), b"hello world").unwrap();
        std::fs::write(sub.join("nested.rs"), b"fn main() {}").unwrap();

        let index = build_test_index(&tmp);
        assert!(index.entries.len() >= 4); // root, sub dir, 2 files

        let nested = index
            .entries
            .iter()
            .position(|e| e.name.as_ref() == "nested.rs")
            .unwrap() as u32;
        let path = index.full_path(nested);
        assert_eq!(path, sub.join("nested.rs"));

        let hello = index
            .entries
            .iter()
            .find(|e| e.name.as_ref() == "hello.txt")
            .unwrap();
        assert_eq!(hello.size, 11);
        assert!(!hello.is_dir());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_scan_into_live_index() {
        let tmp = std::env::temp_dir().join(format!("find_test_live_{}", std::process::id()));
        let sub = tmp.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("a.txt"), b"a").unwrap();
        std::fs::write(sub.join("b.txt"), b"bb").unwrap();

        let live = std::sync::RwLock::new(Index::default());
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let dirty = AtomicBool::new(false);
        scan_into(&live, &[tmp.clone()], &[], &progress, &cancel, &dirty);

        assert!(dirty.load(Ordering::Relaxed));
        let guard = live.read().unwrap();
        assert_eq!(guard.live_count(), 4); // root, a.txt, nested, b.txt
        let b = guard
            .entries
            .iter()
            .position(|e| e.name.as_ref() == "b.txt")
            .unwrap() as u32;
        assert_eq!(guard.full_path(b), sub.join("b.txt"));
        // dir_map was published, so the watcher could resolve parents.
        assert!(guard.dir_map.contains_key(&sub));
        drop(guard);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_upsert_and_remove() {
        let tmp = std::env::temp_dir().join(format!("find_test_ur_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), b"a").unwrap();

        let mut index = build_test_index(&tmp);
        let before = index.live_count();

        std::fs::write(tmp.join("b.txt"), b"bb").unwrap();
        index.upsert_path(&tmp.join("b.txt"));
        assert_eq!(index.live_count(), before + 1);

        index.remove_path(&tmp.join("b.txt"));
        assert_eq!(index.live_count(), before);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
