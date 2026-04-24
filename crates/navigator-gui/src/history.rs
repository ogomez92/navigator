//! Browser-style navigation history.
//!
//! Holds a cursor into a bounded ring of visited paths. `push` clears the
//! forward stack (as clicking a link in a browser would). `back` / `forward`
//! move the cursor without rewriting the ring — so a hop back then a second
//! nav can still be re-entered via `forward` without reloading state.

use navigator_core::NavPath;

const DEFAULT_CAP: usize = 128;

#[derive(Debug, Clone)]
pub struct History {
    entries: Vec<NavPath>,
    /// Index of the *current* entry, or `entries.len()` when empty.
    cursor: usize,
    cap: usize,
}

impl Default for History {
    fn default() -> Self { Self::with_capacity(DEFAULT_CAP) }
}

impl History {
    pub fn with_capacity(cap: usize) -> Self {
        Self { entries: Vec::with_capacity(cap.min(DEFAULT_CAP)), cursor: 0, cap: cap.max(1) }
    }

    /// Record a navigation. Drops anything the user had after the cursor
    /// (we just left that timeline), then appends. Trims the head when the
    /// ring is full so memory stays bounded.
    pub fn push(&mut self, path: NavPath) {
        // Collapse consecutive duplicates — navigating to the same place
        // shouldn't grow the history.
        if let Some(current) = self.entries.get(self.cursor) {
            if *current == path { return; }
        }
        if self.cursor < self.entries.len() {
            self.entries.truncate(self.cursor + 1);
        }
        self.entries.push(path);
        if self.entries.len() > self.cap {
            let drop = self.entries.len() - self.cap;
            self.entries.drain(0..drop);
        }
        self.cursor = self.entries.len() - 1;
    }

    pub fn current(&self) -> Option<&NavPath> {
        self.entries.get(self.cursor)
    }

    pub fn can_back(&self) -> bool {
        !self.entries.is_empty() && self.cursor > 0
    }

    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<&NavPath> {
        if !self.can_back() { return None; }
        self.cursor -= 1;
        self.entries.get(self.cursor)
    }

    pub fn forward(&mut self) -> Option<&NavPath> {
        if !self.can_forward() { return None; }
        self.cursor += 1;
        self.entries.get(self.cursor)
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}
