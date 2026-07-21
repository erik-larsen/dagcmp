use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Seconds between the Windows epoch (1601-01-01) and the Unix epoch (1970-01-01).
const WINDOWS_TO_UNIX_EPOCH_SECS: u64 = 11_644_473_600;

/// File metadata relevant for comparison. Modified time is stored in Windows
/// FILETIME units (100 ns ticks since 1601-01-01 UTC), the native NTFS format,
/// so MFT records need no conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meta {
    pub size: u64,
    pub mtime: u64,
    /// Windows FILE_ATTRIBUTE_* bits (0 where unavailable).
    pub attrs: u32,
}

impl Meta {
    pub fn from_fs(md: &std::fs::Metadata) -> Self {
        let mtime = md
            .modified()
            .ok()
            .map(filetime_from_systemtime)
            .unwrap_or(0);
        #[cfg(windows)]
        let attrs = {
            use std::os::windows::fs::MetadataExt;
            md.file_attributes()
        };
        #[cfg(not(windows))]
        let attrs = 0;
        Meta {
            size: if md.is_dir() { 0 } else { md.len() },
            mtime,
            attrs,
        }
    }

    pub fn mtime_systemtime(&self) -> Option<SystemTime> {
        let unix_100ns = self
            .mtime
            .checked_sub(WINDOWS_TO_UNIX_EPOCH_SECS * 10_000_000)?;
        Some(UNIX_EPOCH + Duration::from_nanos(unix_100ns * 100))
    }
}

pub fn filetime_from_systemtime(t: SystemTime) -> u64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => WINDOWS_TO_UNIX_EPOCH_SECS * 10_000_000 + (d.as_nanos() / 100) as u64,
        // Pre-1970 timestamps: clamp into the 1601..1970 range.
        Err(e) => {
            let back = (e.duration().as_nanos() / 100) as u64;
            (WINDOWS_TO_UNIX_EPOCH_SECS * 10_000_000).saturating_sub(back)
        }
    }
}

/// A node in a scanned tree. Children are keyed by lower-cased name for
/// case-insensitive matching (Windows semantics); the original name is kept
/// in `name`.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub is_dir: bool,
    pub meta: Meta,
    pub children: BTreeMap<String, Node>,
}

impl Node {
    pub fn new_dir(name: impl Into<String>, meta: Meta) -> Self {
        Node {
            name: name.into(),
            is_dir: true,
            meta,
            children: BTreeMap::new(),
        }
    }

    pub fn new_file(name: impl Into<String>, meta: Meta) -> Self {
        Node {
            name: name.into(),
            is_dir: false,
            meta,
            children: BTreeMap::new(),
        }
    }

    /// Insert a node at the given path (components relative to `self`),
    /// creating placeholder directories as needed. If the node already exists
    /// as a placeholder, its metadata is updated in place.
    pub fn insert_at(&mut self, components: &[&str], node: Node) {
        match components {
            [] => {
                // Path resolves to self: update metadata (placeholder fill-in).
                self.meta = node.meta;
                self.is_dir = node.is_dir;
            }
            [leaf] => {
                let key = leaf.to_lowercase();
                match self.children.get_mut(&key) {
                    Some(existing) => {
                        existing.meta = node.meta;
                        existing.is_dir = node.is_dir;
                        existing.name = node.name;
                    }
                    None => {
                        self.children.insert(key, node);
                    }
                }
            }
            [first, rest @ ..] => {
                let key = first.to_lowercase();
                let child = self
                    .children
                    .entry(key)
                    .or_insert_with(|| Node::new_dir(first.to_string(), Meta::default()));
                child.insert_at(rest, node);
            }
        }
    }

    /// Total number of files / directories / bytes in this subtree (excluding self).
    pub fn totals(&self) -> (u64, u64, u64) {
        let mut files = 0;
        let mut dirs = 0;
        let mut bytes = 0;
        for child in self.children.values() {
            if child.is_dir {
                dirs += 1;
                let (f, d, b) = child.totals();
                files += f;
                dirs += d;
                bytes += b;
            } else {
                files += 1;
                bytes += child.meta.size;
            }
        }
        (files, dirs, bytes)
    }
}
