use std::collections::BTreeSet;

/// Mirror of the ListView selection state. Keeps indices so we can re-drive
/// the OS control after sorts or virtual backing changes.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    items: BTreeSet<usize>,
    anchor: Option<usize>,
    focus: Option<usize>,
}

impl Selection {
    pub fn clear(&mut self) {
        self.items.clear();
        self.anchor = None;
        self.focus = None;
    }

    pub fn contains(&self, i: usize) -> bool {
        self.items.contains(&i)
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.items.iter().copied()
    }

    pub fn focus(&self) -> Option<usize> {
        self.focus
    }
    pub fn set_focus(&mut self, i: Option<usize>) {
        self.focus = i;
    }
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }
    pub fn set_anchor(&mut self, i: Option<usize>) {
        self.anchor = i;
    }

    pub fn set_single(&mut self, i: usize) {
        self.items.clear();
        self.items.insert(i);
        self.anchor = Some(i);
        self.focus = Some(i);
    }

    pub fn toggle(&mut self, i: usize) {
        if !self.items.insert(i) {
            self.items.remove(&i);
        }
        self.focus = Some(i);
        if self.anchor.is_none() {
            self.anchor = Some(i);
        }
    }

    /// Idempotent add. Used when mirroring LVN_ITEMCHANGED — unlike
    /// `toggle`, this doesn't flip off an already-present item.
    pub fn insert(&mut self, i: usize) {
        self.items.insert(i);
    }

    /// Idempotent remove.
    pub fn remove(&mut self, i: usize) {
        self.items.remove(&i);
    }

    /// Extend selection from the anchor to `i` (shift+arrow / shift+click).
    pub fn extend_to(&mut self, i: usize) {
        let anchor = self.anchor.unwrap_or(i);
        self.items.clear();
        let (lo, hi) = if anchor <= i {
            (anchor, i)
        } else {
            (i, anchor)
        };
        for k in lo..=hi {
            self.items.insert(k);
        }
        self.focus = Some(i);
    }
}
