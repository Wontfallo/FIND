//! FIND core: indexing, querying, searching, content grep, settings, watching.
//! The GUI lives in the `find` binary; this library is GUI-free and unit-tested.

pub mod content;
pub mod doctext;
pub mod index;
#[cfg(target_os = "windows")]
pub mod mft;
pub mod media;
pub mod query;
pub mod search;
pub mod settings;
pub mod util;
pub mod watcher;
