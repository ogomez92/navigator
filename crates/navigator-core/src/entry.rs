use serde::{Deserialize, Serialize};

/// Windows FILETIME — 100-ns ticks since 1601-01-01 UTC. Kept raw so no
/// precision is lost; convert only at display time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileTime(pub u64);

impl FileTime {
    pub const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

    pub fn to_unix_secs(self) -> i64 {
        ((self.0 as i128 - Self::UNIX_EPOCH_TICKS as i128) / 10_000_000) as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One filesystem entry. Name is stored separately from the parent path so
/// the virtual ListView can display a million entries without duplicating
/// parent strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: FileTime,
    pub created: FileTime,
    pub attrs: u32,
    pub hidden: bool,
    pub system: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool { matches!(self.kind, EntryKind::Directory) }
}
