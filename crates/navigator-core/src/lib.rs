//! Shared types used across the navigator crates.
//!
//! No GUI or OS-specific code lives here. Anything in this crate must be safe
//! to use from a plugin, the GUI, or a background worker.

pub mod entry;
pub mod error;
pub mod event;
pub mod path;
pub mod selection;

pub use entry::{Entry, EntryKind, FileTime};
pub use error::{Error, Result};
pub use event::Event;
pub use path::NavPath;
pub use selection::Selection;
