//! FIND core: indexing, querying, searching, content grep, settings, watching.
//! The GUI lives in the `find` binary; this library is GUI-free and unit-tested.

pub mod content;
pub mod doctext;
pub mod fileops;
pub mod index;
#[cfg(target_os = "windows")]
pub mod mft;
pub mod media;
pub mod query;
pub mod search;
#[cfg(target_os = "windows")]
pub mod service;
pub mod settings;
#[cfg(target_os = "windows")]
pub mod thumbnail;
#[cfg(target_os = "windows")]
pub mod usn_watch;
pub mod util;
#[cfg(target_os = "windows")]
pub mod volume;
pub mod watcher;
