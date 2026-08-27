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
    ///
    /// Not serialized: `PathBuf` fails to serialize when a path is not valid
    /// UTF-8 (legal on Windows), which made saving the whole index fail and
    /// forced a full rescan on every launch. Both maps are derived from
    /// `entries`, so they are rebuilt after loading instead.
    #[serde(skip)]
    pub dir_map: HashMap<PathBuf, u32>,
    /// Children of each directory entry.
    #[serde(skip)]
    pub children: HashMap<u32, Vec<u32>>,
    /// Stored as strings for the same reason as above: a `PathBuf` that is
    /// not valid UTF-8 aborts serialization of the entire index.
    #[serde(with = "roots_as_strings")]
    pub roots: Vec<PathBuf>,
    pub scanned_at: i64,
    /// True only when every root has been fully scanned. A cache saved
    /// mid-scan (checkpoint, quit) is loadable but NOT complete, and the
    /// scan resumes instead of the app pretending the index is whole.
    #[serde(default)]
    pub complete: bool,
    /// Roots (as strings) whose scan finished — lets an interrupted
    /// multi-root build resume with only the missing roots.
    #[serde(default)]
    pub completed_roots: Vec<String>,
}

mod roots_as_strings {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(roots: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        let strings: Vec<String> = roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        serde::Serialize::serialize(&strings, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        let strings: Vec<String> = Vec::deserialize(d)?;
        Ok(strings.into_iter().map(PathBuf::from).collect())
    }
}

impl Index {
    /// Number of live (non-deleted) entries.
    pub fn live_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_deleted()).count()
    }

    pub fn full_path(&self, idx: u32) -> PathBuf {
        let mut parts: Vec<&str> = Vec::with_capacity(16);
        let mut cur = idx;
        let mut hops = 0;
        loop {
            let Some(e) = self.entries.get(cur as usize) else {
                break;
            };
            parts.push(&e.name);
            if e.parent == NO_PARENT {
                break;
            }
            // Defensive: a parent must be a directory, and paths are not
            // thousands of levels deep. Either signals a corrupt pointer —
            // stop rather than emitting a nonsense file-inside-file chain.
            match self.entries.get(e.parent as usize) {
                Some(p) if p.is_dir() => {}
                _ => break,
            }
            hops += 1;
            if hops > 256 {
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

    /// Reconstruct `children` and `dir_map` from `entries` (they are not
    /// serialized). O(n) plus a path build per directory.
    pub fn rebuild_derived(&mut self) {
        self.children.clear();
        self.dir_map.clear();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.parent != NO_PARENT {
                self.children
                    .entry(entry.parent)
                    .or_default()
                    .push(i as u32);
            }
        }
        let dirs: Vec<u32> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_dir() && !e.is_deleted())
            .map(|(i, _)| i as u32)
            .collect();
        for idx in dirs {
            let path = self.full_path(idx);
            self.dir_map.insert(path, idx);
        }
    }

    pub fn full_path_string(&self, idx: u32) -> String {
        self.full_path(idx).to_string_lossy().into_owned()
    }

    /// Public wrapper used by the NTFS fast path.
    pub fn push_entry_pub(
        &mut self,
        name: &str,
        parent: u32,
        size: u64,
        modified: i64,
        is_dir: bool,
    ) -> u32 {
        self.push_entry(name, parent, size, modified, is_dir)
    }

    /// Fill in size/modified for entries left blank by the fast path. Runs in
    /// parallel batches so the app stays responsive; `cancel` stops it.
    pub fn fill_metadata(
        index: &std::sync::RwLock<Self>,
        progress: &AtomicUsize,
        cancel: &AtomicBool,
    ) {
        use rayon::prelude::*;
        const BATCH: usize = 20_000;
        let total = match index.read() {
            Ok(g) => g.entries.len(),
            Err(_) => return,
        };
        let mut start = 0usize;
        while start < total {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let end = (start + BATCH).min(total);
            // Collect paths for this batch under a short read lock.
            let paths: Vec<(u32, PathBuf)> = match index.read() {
                Ok(g) => (start..end)
                    .filter(|&i| g.entries[i].modified == 0 && !g.entries[i].is_deleted())
                    .map(|i| (i as u32, g.full_path(i as u32)))
                    .collect(),
                Err(_) => return,
            };
            let stats: Vec<(u32, u64, i64)> = paths
                .into_par_iter()
                .filter_map(|(i, path)| {
                    let meta = std::fs::symlink_metadata(&path).ok()?;
                    Some((
                        i,
                        if meta.is_dir() { 0 } else { meta.len() },
                        system_time_secs(meta.modified().ok()),
                    ))
                })
                .collect();
            if let Ok(mut g) = index.write() {
                for (i, size, modified) in stats {
                    let e = &mut g.entries[i as usize];
                    e.size = size;
                    e.modified = modified;
                }
            }
            progress.store(end, Ordering::Relaxed);
            start = end;
        }
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

    /// Remove one root's subtree (used when resuming an interrupted build):
    /// its entries are tombstoned and its dir_map keys dropped, so the root
    /// can be re-streamed without duplicates.
    pub fn purge_root(&mut self, root: &Path) {
        if let Some(&idx) = self.dir_map.get(root) {
            self.mark_deleted_recursive(idx);
        }
        self.dir_map.retain(|path, _| !path.starts_with(root));
        let root_str = root.to_string_lossy().into_owned();
        self.completed_roots.retain(|r| *r != root_str);
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

pub fn system_time_secs(t: Option<SystemTime>) -> i64 {
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
    completed: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
) -> Index {
    if let Ok(mut c) = completed.lock() {
        c.clear();
    }
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
        // Try the NTFS bulk read first (seconds instead of minutes); it
        // reports false when unavailable and we walk directories instead.
        #[cfg(target_os = "windows")]
        let fast_ok = crate::mft::enumerate_volume(&mut index, root, exclusions, progress, cancel);
        #[cfg(not(target_os = "windows"))]
        let fast_ok = false;
        if !fast_ok {
            scan_root(&mut index, root, exclusions, progress, cancel);
        }
        if !cancel.load(Ordering::Relaxed) {
            if let Ok(mut c) = completed.lock() {
                c.insert(root.clone());
            }
            let root_str = root.to_string_lossy().into_owned();
            if !index.completed_roots.contains(&root_str) {
                index.completed_roots.push(root_str);
            }
        }
    }
    if !cancel.load(Ordering::Relaxed)
        && roots
            .iter()
            .all(|r| index.completed_roots.contains(&r.to_string_lossy().into_owned()))
    {
        index.complete = true;
        index.scanned_at = system_time_secs(Some(SystemTime::now()));
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

/// Stream one root's tree into the live index in batches, with periodic
/// cache checkpoints. The checkpointed cache is (correctly) incomplete —
/// `complete` only becomes true in `finalize_live`.
fn stream_root_live(
    live: &std::sync::RwLock<Index>,
    root: &Path,
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
    dirty: &AtomicBool,
    last_save: &mut std::time::Instant,
) {
    const BATCH: usize = 65_536;
    // Local mirrors let the walk assign final indices without holding the lock.
    let mut dir_map_local: HashMap<PathBuf, u32> = HashMap::new();
    let mut pending: Vec<Entry> = Vec::with_capacity(BATCH);
    let mut pending_dirs: Vec<(PathBuf, u32)> = Vec::new();
    let mut base: u32 = live.read().map(|g| g.entries.len() as u32).unwrap_or(0);

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
                *last_save = std::time::Instant::now();
            }
        }
    }
    flush_batch(live, &mut pending, &mut pending_dirs, &mut base, dirty);
}

/// After streaming, record a finished root both in the shared UI set and in
/// the index itself (so an interrupted build knows where to resume).
fn record_root_done(
    live: &std::sync::RwLock<Index>,
    completed: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    root: &Path,
) {
    if let Ok(mut c) = completed.lock() {
        c.insert(root.to_path_buf());
    }
    if let Ok(mut guard) = live.write() {
        let root_str = root.to_string_lossy().into_owned();
        if !guard.completed_roots.contains(&root_str) {
            guard.completed_roots.push(root_str);
        }
    }
}

/// Mark the index complete once every requested root finished.
fn finalize_live(live: &std::sync::RwLock<Index>, roots: &[PathBuf], cancel: &AtomicBool) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut guard) = live.write() {
        let all_done = roots
            .iter()
            .all(|r| guard.completed_roots.contains(&r.to_string_lossy().into_owned()));
        if all_done {
            guard.complete = true;
            guard.scanned_at = system_time_secs(Some(SystemTime::now()));
        }
    }
}

/// First-run scan: streams entries into the shared `live` index in batches so
/// the app is searchable immediately, with results growing as the scan runs.
/// Replaces whatever is in `live`; use `scan_resume` to continue an
/// interrupted build instead.
pub fn scan_into(
    live: &std::sync::RwLock<Index>,
    roots: &[PathBuf],
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
    dirty: &AtomicBool,
    completed: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
) {
    if let Ok(mut c) = completed.lock() {
        c.clear();
    }
    {
        let mut guard = live.write().unwrap();
        *guard = Index {
            roots: roots.to_vec(),
            scanned_at: system_time_secs(Some(SystemTime::now())),
            ..Default::default()
        };
    }
    progress.store(0, Ordering::Relaxed);

    let mut last_save = std::time::Instant::now();
    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        stream_root_live(live, root, exclusions, progress, cancel, dirty, &mut last_save);
        if !cancel.load(Ordering::Relaxed) {
            record_root_done(live, completed, root);
        }
    }
    finalize_live(live, roots, cancel);
}

/// Continue an interrupted index build: roots already marked complete are
/// kept as-is; every other root is purged from the index and re-streamed.
/// Nothing is thrown away, so each session makes real progress instead of
/// restarting from zero.
pub fn scan_resume(
    live: &std::sync::RwLock<Index>,
    roots: &[PathBuf],
    exclusions: &[String],
    progress: &AtomicUsize,
    cancel: &AtomicBool,
    dirty: &AtomicBool,
    completed: &std::sync::Mutex<std::collections::HashSet<PathBuf>>,
) {
    // Seed the UI's per-root status from what the cache already finished.
    let done: Vec<String> = live
        .read()
        .map(|g| g.completed_roots.clone())
        .unwrap_or_default();
    if let Ok(mut c) = completed.lock() {
        c.clear();
        c.extend(done.iter().map(PathBuf::from));
    }
    if let Ok(mut guard) = live.write() {
        guard.roots = roots.to_vec();
    }
    progress.store(0, Ordering::Relaxed);

    let mut last_save = std::time::Instant::now();
    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let root_str = root.to_string_lossy().into_owned();
        if done.contains(&root_str) {
            continue; // finished in an earlier session
        }
        // Drop whatever partial data a previous attempt left for this root,
        // then stream it fresh.
        if let Ok(mut guard) = live.write() {
            guard.purge_root(root);
        }
        stream_root_live(live, root, exclusions, progress, cancel, dirty, &mut last_save);
        if !cancel.load(Ordering::Relaxed) {
            record_root_done(live, completed, root);
        }
    }
    finalize_live(live, roots, cancel);
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

/// What the app should do at launch with a loaded cache. This tiny function
/// IS the anti-reindex guarantee, so it lives here where tests can pin it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LaunchPlan {
    /// Complete and young enough: no scanning of any kind.
    UseAsIs,
    /// Complete but older than the user's opt-in refresh threshold.
    Refresh,
    /// Saved mid-build: keep it and finish only the missing roots.
    Resume,
}

pub fn launch_plan(complete: bool, scanned_at: i64, now: i64, auto_refresh_hours: u32) -> LaunchPlan {
    if !complete {
        return LaunchPlan::Resume;
    }
    let threshold = auto_refresh_hours as i64 * 3600;
    if threshold > 0 && now - scanned_at > threshold {
        LaunchPlan::Refresh
    } else {
        LaunchPlan::UseAsIs
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
    save_to_path(index, &path)
}

pub fn save_to_path(index: &Index, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = bincode::serialize(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let mut bytes = Vec::with_capacity(body.len() + 4);
    bytes.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&body);
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Bumped whenever the on-disk layout changes, so an old cache is discarded
/// (and rebuilt) instead of failing to deserialize in confusing ways.
const CACHE_FORMAT_VERSION: u32 = 3;

pub fn load_from_disk() -> Option<Index> {
    load_from_path(&cache_path()?)
}

pub fn load_from_path(path: &Path) -> Option<Index> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 4 || u32::from_le_bytes(bytes[..4].try_into().ok()?) != CACHE_FORMAT_VERSION {
        return None;
    }
    let mut index: Index = bincode::deserialize(&bytes[4..]).ok()?;
    index.rebuild_derived();
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_index(dir: &Path) -> Index {
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        let index = scan(&[dir.to_path_buf()], &[], &progress, &cancel, &completed);
        assert!(completed.lock().unwrap().contains(dir));
        index
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

    /// The complete launch lifecycle, cold start to cold start, as it plays
    /// out on disk. If this passes, the app cannot legally rescan at launch
    /// once a build has completed.
    #[test]
    fn test_lifecycle_no_rescan_after_completed_build() {
        let tmp = std::env::temp_dir().join(format!("find_life_{}", std::process::id()));
        let root = tmp.join("data");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), b"bb").unwrap();
        let cache = tmp.join("index.bin");

        // Session 1: fresh streaming build completes and is saved.
        let live = std::sync::RwLock::new(Index::default());
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let dirty = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        scan_into(&live, &[root.clone()], &[], &progress, &cancel, &dirty, &completed);
        {
            let g = live.read().unwrap();
            assert!(g.complete, "finished build must be marked complete");
            assert_eq!(g.completed_roots, vec![root.to_string_lossy().into_owned()]);
            save_to_path(&g, &cache).unwrap();
        }

        // Session 2 (cold start): the cache loads complete, and the launch
        // plan — with refresh disabled (default) — is UseAsIs: NO scan.
        let loaded = load_from_path(&cache).expect("cache must load");
        assert!(loaded.complete);
        assert_eq!(loaded.live_count(), 4); // root, a.txt, sub, b.txt
        let now = system_time_secs(Some(SystemTime::now()));
        assert_eq!(
            launch_plan(loaded.complete, loaded.scanned_at, now, 0),
            LaunchPlan::UseAsIs
        );
        // Even a year later, refresh disabled means no scan at launch.
        assert_eq!(
            launch_plan(loaded.complete, loaded.scanned_at, now + 365 * 86_400, 0),
            LaunchPlan::UseAsIs
        );
        // With an opt-in threshold, young caches still skip the scan...
        assert_eq!(
            launch_plan(loaded.complete, loaded.scanned_at, now, 24),
            LaunchPlan::UseAsIs
        );
        // ...and only genuinely old ones refresh.
        assert_eq!(
            launch_plan(loaded.complete, loaded.scanned_at, now + 2 * 86_400, 24),
            LaunchPlan::Refresh
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A build interrupted mid-way must resume (only the unfinished roots),
    /// not restart, and must not duplicate the finished root's entries.
    #[test]
    fn test_lifecycle_interrupted_build_resumes_missing_roots() {
        let tmp = std::env::temp_dir().join(format!("find_resume_{}", std::process::id()));
        let root_a = tmp.join("done_root");
        let root_b = tmp.join("pending_root");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("finished.txt"), b"x").unwrap();
        std::fs::write(root_b.join("missing.txt"), b"y").unwrap();
        let cache = tmp.join("index.bin");

        // Session 1: only root A completes (as if the user quit during B).
        let live = std::sync::RwLock::new(Index::default());
        let progress = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        let dirty = AtomicBool::new(false);
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        scan_into(&live, &[root_a.clone()], &[], &progress, &cancel, &dirty, &completed);
        {
            let mut g = live.write().unwrap();
            // The interruption: B was requested but never finished. A quit
            // mid-B saves exactly this shape via the checkpoint/quit paths.
            g.roots = vec![root_a.clone(), root_b.clone()];
            g.complete = false;
            // Stale half-scanned junk from the aborted B pass:
            let fake_b_root = g.push_entry_pub(&root_b.to_string_lossy(), NO_PARENT, 0, 0, true);
            g.push_entry_pub("halfway.txt", fake_b_root, 1, 1, false);
            g.dir_map.insert(root_b.clone(), fake_b_root);
            save_to_path(&g, &cache).unwrap();
        }

        // Session 2: loads incomplete -> plan is Resume regardless of age.
        let loaded = load_from_path(&cache).expect("cache must load");
        assert!(!loaded.complete);
        assert_eq!(
            launch_plan(loaded.complete, loaded.scanned_at, i64::MAX / 2, 0),
            LaunchPlan::Resume
        );

        let live = std::sync::RwLock::new(loaded);
        let a_count_before = {
            let g = live.read().unwrap();
            g.entries
                .iter()
                .filter(|e| !e.is_deleted() && e.name.as_ref() == "finished.txt")
                .count()
        };
        assert_eq!(a_count_before, 1);

        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        scan_resume(
            &live,
            &[root_a.clone(), root_b.clone()],
            &[],
            &progress,
            &cancel,
            &dirty,
            &completed,
        );
        let g = live.read().unwrap();
        assert!(g.complete, "resume finishing all roots must mark complete");
        // A was NOT rescanned or duplicated.
        assert_eq!(
            g.entries
                .iter()
                .filter(|e| !e.is_deleted() && e.name.as_ref() == "finished.txt")
                .count(),
            1
        );
        // B's real file arrived; the stale junk from the aborted pass is gone.
        assert_eq!(
            g.entries
                .iter()
                .filter(|e| !e.is_deleted() && e.name.as_ref() == "missing.txt")
                .count(),
            1
        );
        assert_eq!(
            g.entries
                .iter()
                .filter(|e| !e.is_deleted() && e.name.as_ref() == "halfway.txt")
                .count(),
            0,
            "stale partial entries must be purged on resume"
        );
        drop(g);
        // And the completed state persists for session 3.
        let g = live.read().unwrap();
        save_to_path(&g, &cache).unwrap();
        drop(g);
        let reloaded = load_from_path(&cache).unwrap();
        assert!(reloaded.complete);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_full_path_rejects_file_as_parent() {
        // Regression: a corrupt parent pointer (file used as a parent) once
        // produced absurd "a.mp4\b.zip\c.dll\..." chains.
        let mut index = Index::default();
        let root = index.push_entry("C:\\", NO_PARENT, 0, 0, true);
        let file = index.push_entry("video.mp4", root, 10, 0, false);
        // Corrupt: point an entry at a *file* as its parent.
        let bogus = index.push_entry("thing.png", file, 5, 0, false);
        let path = index.full_path(bogus);
        assert_eq!(
            path,
            PathBuf::from("thing.png"),
            "must not chain through a file parent"
        );
        // A well-formed entry still resolves fully.
        assert_eq!(index.full_path(file), PathBuf::from("C:\\").join("video.mp4"));
    }

    #[test]
    fn test_cache_survives_non_utf8_names_and_rebuilds_maps() {
        // Regression: dir_map was serialized as PathBuf keys, and serde
        // refuses non-UTF-8 paths — one oddly named file made saving the
        // whole index fail, so every launch rescanned from scratch.
        let mut index = Index {
            roots: vec![PathBuf::from("C:\\")],
            scanned_at: 99,
            ..Default::default()
        };
        let root = index.push_entry("C:\\", NO_PARENT, 0, 0, true);
        let dir = index.push_entry("sub", root, 0, 0, true);
        // A name with a lone surrogate, as produced by lossy conversion of
        // an unpaired UTF-16 surrogate in a real Windows filename.
        index.push_entry("odd\u{FFFD}name.txt", dir, 7, 0, false);
        index.dir_map.insert(PathBuf::from("C:\\sub"), dir);

        let bytes = bincode::serialize(&index).expect("index must serialize");
        let mut restored: Index = bincode::deserialize(&bytes).unwrap();
        assert!(restored.dir_map.is_empty(), "maps are not serialized");
        restored.rebuild_derived();

        assert_eq!(restored.entries.len(), 3);
        assert_eq!(restored.roots, vec![PathBuf::from("C:\\")]);
        // Derived maps came back.
        assert_eq!(restored.dir_map.len(), 2);
        assert!(restored.dir_map.contains_key(&PathBuf::from("C:\\").join("sub")));
        assert_eq!(restored.children.get(&dir).map(Vec::len), Some(1));
    }

    #[test]
    fn test_cache_roundtrip_and_version_guard() {
        // A versioned header must survive a save/load cycle, and a file with
        // the wrong version must be rejected rather than misparsed.
        let index = Index {
            roots: vec![PathBuf::from("/tmp/x")],
            scanned_at: 42,
            ..Default::default()
        };
        let body = bincode::serialize(&index).unwrap();
        let mut good = CACHE_FORMAT_VERSION.to_le_bytes().to_vec();
        good.extend_from_slice(&body);
        let parsed: Index = bincode::deserialize(&good[4..]).unwrap();
        assert_eq!(parsed.scanned_at, 42);

        let mut bad = (CACHE_FORMAT_VERSION + 1).to_le_bytes().to_vec();
        bad.extend_from_slice(&body);
        assert_ne!(
            u32::from_le_bytes(bad[..4].try_into().unwrap()),
            CACHE_FORMAT_VERSION,
            "version guard must reject a newer cache format"
        );
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
        let completed = std::sync::Mutex::new(std::collections::HashSet::new());
        scan_into(&live, &[tmp.clone()], &[], &progress, &cancel, &dirty, &completed);
        assert!(completed.lock().unwrap().contains(&tmp));

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
