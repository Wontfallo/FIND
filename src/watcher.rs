//! Live filesystem watching: keeps the index up to date as files are created,
//! renamed, and deleted, so results stay accurate without rescanning.

use crate::index::Index;
use crate::util::is_excluded;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Handle that keeps the watcher threads alive; drop to stop watching.
pub struct WatchHandle {
    _watchers: Vec<RecommendedWatcher>,
    stop: Arc<AtomicBool>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Watch `roots` and apply changes to `index`. Events are batched and applied
/// every 500 ms to avoid write-lock churn during bursts (installs, builds...).
pub fn watch(
    roots: Vec<PathBuf>,
    exclusions: Vec<String>,
    index: Arc<RwLock<Index>>,
    dirty: Arc<AtomicBool>,
) -> Option<WatchHandle> {
    let (tx, rx) = crossbeam_channel::unbounded::<Event>();
    let stop = Arc::new(AtomicBool::new(false));

    let mut watchers = Vec::new();
    for root in &roots {
        let tx = tx.clone();
        let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        });
        match watcher {
            Ok(mut w) => {
                if w.watch(root, RecursiveMode::Recursive).is_ok() {
                    watchers.push(w);
                }
            }
            Err(_) => continue,
        }
    }
    if watchers.is_empty() {
        return None;
    }

    let stop2 = stop.clone();
    std::thread::Builder::new()
        .name("find-watcher".into())
        .spawn(move || {
            let mut pending: Vec<Event> = Vec::new();
            loop {
                if stop2.load(Ordering::Relaxed) {
                    return;
                }
                // Collect events for up to 500ms, then apply as one batch.
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => {
                        pending.push(ev);
                        while let Ok(ev) = rx.try_recv() {
                            pending.push(ev);
                            if pending.len() > 100_000 {
                                break;
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
                if pending.is_empty() {
                    continue;
                }
                let events = std::mem::take(&mut pending);
                if let Ok(mut idx) = index.write() {
                    for event in events {
                        apply_event(&mut idx, &event, &exclusions);
                    }
                }
                dirty.store(true, Ordering::Relaxed);
            }
        })
        .ok()?;

    Some(WatchHandle {
        _watchers: watchers,
        stop,
    })
}

fn apply_event(index: &mut Index, event: &Event, exclusions: &[String]) {
    for path in &event.paths {
        if is_excluded(path, exclusions) {
            continue;
        }
        match event.kind {
            EventKind::Remove(_) => index.remove_path(path),
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
                // Renames arrive as Modify(Name) with old and/or new paths;
                // resolve by checking what actually exists on disk.
                if path.exists() {
                    index.upsert_path(path);
                } else {
                    index.remove_path(path);
                }
            }
            EventKind::Access(_) => {}
        }
    }
}
