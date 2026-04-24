use crate::{Entry, NavPath};

/// High-level events emitted by the shell and consumed by plugins + worker
/// threads. Kept plain (no lifetimes) so they cross the plugin ABI boundary
/// after serialization.
#[derive(Debug, Clone)]
pub enum Event {
    /// Directory fully listed.
    DirListed { path: NavPath, entries: Vec<Entry> },

    /// User navigated into a directory.
    Navigated { path: NavPath },

    /// Focused entry changed. `index` is into the current listing.
    Focused { path: NavPath, index: usize },

    /// Operation kicked off by the user.
    OpStarted { id: u64, kind: OpKind, items: Vec<NavPath>, dest: Option<NavPath> },

    /// Operation progress update.
    OpProgress { id: u64, bytes_done: u64, bytes_total: u64, current: Option<String> },

    /// Operation finished (success or failure).
    OpFinished { id: u64, result: OpOutcome },

    /// Something the user should hear via screen reader.
    Speak { text: String, interrupt: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Move,
    Delete,
    Rename,
}

#[derive(Debug, Clone)]
pub enum OpOutcome {
    Ok,
    Cancelled,
    Error(String),
}
