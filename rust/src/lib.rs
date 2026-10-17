//! Whole-disk file search for macOS: a fuzzy name index over every entry on
//! disk, kept live from FSEvents, plus a trigram content index for text
//! files. `Engine` runs it all in-process; the `tov-search` binary wraps one in
//! a daemon with a JSON-lines socket.
//!
//! Off macOS only the name index and search build (the `engine` binary).

// Content search is portable (only its iCloud/QoS hooks stay macOS-gated),
// which keeps its tests runnable everywhere.
pub mod content;
#[cfg(target_os = "macos")]
mod engine;
#[cfg(target_os = "macos")]
mod fsevents;
pub mod index;
pub mod live;
pub mod query;
pub mod walk;

pub use content::{FileMatches, Grep, GrepResult};
#[cfg(target_os = "macos")]
pub use engine::{Engine, Found, Options, Status, default_dir, gated, has_full_disk_access, no_materialize};
pub use query::{GrepMode, Query};

/// iCloud placeholders only exist on macOS.
#[cfg(not(target_os = "macos"))]
pub fn no_materialize() {}
