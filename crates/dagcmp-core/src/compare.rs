use crate::model::{Meta, Node};
use std::ops::AddAssign;

/// Timestamp comparison tolerance: 2 s in FILETIME (100 ns) units, to absorb
/// FAT's 2-second mtime resolution and DST-related copy skew.
pub const MTIME_TOLERANCE: u64 = 2 * 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Identical,
    LeftNewer,
    RightNewer,
    /// Same mtime but different size, or a file/directory type mismatch.
    Different,
    LeftOnly,
    RightOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub identical: u64,
    pub left_newer: u64,
    pub right_newer: u64,
    pub different: u64,
    pub left_only: u64,
    pub right_only: u64,
}

impl Counts {
    pub fn from_status(s: Status) -> Self {
        let mut c = Counts::default();
        match s {
            Status::Identical => c.identical = 1,
            Status::LeftNewer => c.left_newer = 1,
            Status::RightNewer => c.right_newer = 1,
            Status::Different => c.different = 1,
            Status::LeftOnly => c.left_only = 1,
            Status::RightOnly => c.right_only = 1,
        }
        c
    }

    /// True if nothing in this subtree differs.
    pub fn in_sync(&self) -> bool {
        self.left_newer == 0
            && self.right_newer == 0
            && self.different == 0
            && self.left_only == 0
            && self.right_only == 0
    }

    pub fn differing(&self) -> u64 {
        self.left_newer + self.right_newer + self.different + self.left_only + self.right_only
    }
}

impl AddAssign for Counts {
    fn add_assign(&mut self, o: Counts) {
        self.identical += o.identical;
        self.left_newer += o.left_newer;
        self.right_newer += o.right_newer;
        self.different += o.different;
        self.left_only += o.left_only;
        self.right_only += o.right_only;
    }
}

/// A node in the merged diff tree.
#[derive(Debug, Clone)]
pub struct DiffNode {
    pub name: String,
    pub is_dir: bool,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub status: Status,
    /// Aggregate file counts for this subtree (directories) or just this
    /// file's own status (files).
    pub counts: Counts,
    pub children: Vec<DiffNode>,
}

fn file_status(l: &Meta, r: &Meta) -> Status {
    let dt = l.mtime.abs_diff(r.mtime);
    if l.size == r.size && dt <= MTIME_TOLERANCE {
        Status::Identical
    } else if dt <= MTIME_TOLERANCE {
        Status::Different
    } else if l.mtime > r.mtime {
        Status::LeftNewer
    } else {
        Status::RightNewer
    }
}

fn one_sided(node: &Node, status: Status) -> DiffNode {
    let mut counts = Counts::default();
    let mut children: Vec<DiffNode> = node
        .children
        .values()
        .map(|c| one_sided(c, status))
        .collect();
    children.sort_by(sort_dirs_first);
    if node.is_dir {
        for c in &children {
            counts += c.counts;
        }
    } else {
        counts = Counts::from_status(status);
    }
    let meta = Some(node.meta);
    DiffNode {
        name: node.name.clone(),
        is_dir: node.is_dir,
        left: if status == Status::LeftOnly { meta } else { None },
        right: if status == Status::RightOnly { meta } else { None },
        status,
        counts,
        children,
    }
}

fn sort_dirs_first(a: &DiffNode, b: &DiffNode) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

/// Merge two scanned trees into a diff tree. `left` and `right` must be the
/// two root directories; their own names may differ and are not compared.
pub fn compare(left: &Node, right: &Node) -> DiffNode {
    let mut children = Vec::new();
    let mut counts = Counts::default();

    let mut right_keys: std::collections::BTreeSet<&String> = right.children.keys().collect();

    for (key, lchild) in &left.children {
        match right.children.get(key) {
            Some(rchild) => {
                right_keys.remove(key);
                if lchild.is_dir != rchild.is_dir {
                    // Type mismatch: show as two one-sided entries.
                    children.push(one_sided(lchild, Status::LeftOnly));
                    children.push(one_sided(rchild, Status::RightOnly));
                } else if lchild.is_dir {
                    children.push(compare(lchild, rchild));
                } else {
                    let status = file_status(&lchild.meta, &rchild.meta);
                    children.push(DiffNode {
                        name: lchild.name.clone(),
                        is_dir: false,
                        left: Some(lchild.meta),
                        right: Some(rchild.meta),
                        status,
                        counts: Counts::from_status(status),
                        children: Vec::new(),
                    });
                }
            }
            None => children.push(one_sided(lchild, Status::LeftOnly)),
        }
    }
    for key in right_keys {
        children.push(one_sided(&right.children[key], Status::RightOnly));
    }

    children.sort_by(sort_dirs_first);
    for c in &children {
        counts += c.counts;
    }

    let status = if counts.in_sync() {
        Status::Identical
    } else {
        Status::Different
    };
    DiffNode {
        name: left.name.clone(),
        is_dir: true,
        left: Some(left.meta),
        right: Some(right.meta),
        status,
        counts,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Meta, Node};

    fn file(name: &str, size: u64, mtime: u64) -> Node {
        Node::new_file(
            name,
            Meta {
                size,
                mtime,
                attrs: 0,
            },
        )
    }

    fn tree(children: Vec<Node>) -> Node {
        let mut root = Node::new_dir("root", Meta::default());
        for c in children {
            root.children.insert(c.name.to_lowercase(), c);
        }
        root
    }

    const HOUR: u64 = 3600 * 10_000_000;

    #[test]
    fn statuses() {
        let left = tree(vec![
            file("same.txt", 10, HOUR),
            file("newer_left.txt", 10, 3 * HOUR),
            file("newer_right.txt", 10, HOUR),
            file("size_diff.txt", 10, HOUR),
            file("left_only.txt", 10, HOUR),
        ]);
        let right = tree(vec![
            file("SAME.TXT", 10, HOUR),
            file("newer_left.txt", 10, HOUR),
            file("newer_right.txt", 10, 3 * HOUR),
            file("size_diff.txt", 20, HOUR),
            file("right_only.txt", 10, HOUR),
        ]);
        let diff = compare(&left, &right);
        let get = |n: &str| {
            diff.children
                .iter()
                .find(|c| c.name.to_lowercase() == n)
                .unwrap()
                .status
        };
        assert_eq!(get("same.txt"), Status::Identical);
        assert_eq!(get("newer_left.txt"), Status::LeftNewer);
        assert_eq!(get("newer_right.txt"), Status::RightNewer);
        assert_eq!(get("size_diff.txt"), Status::Different);
        assert_eq!(get("left_only.txt"), Status::LeftOnly);
        assert_eq!(get("right_only.txt"), Status::RightOnly);
        assert_eq!(diff.counts.identical, 1);
        assert_eq!(diff.counts.differing(), 5);
    }

    #[test]
    fn mtime_tolerance_absorbs_fat_resolution() {
        let l = tree(vec![file("a.txt", 10, HOUR)]);
        let r = tree(vec![file("a.txt", 10, HOUR + MTIME_TOLERANCE)]);
        assert!(compare(&l, &r).counts.in_sync());
        let r2 = tree(vec![file("a.txt", 10, HOUR + MTIME_TOLERANCE + 1)]);
        assert!(!compare(&l, &r2).counts.in_sync());
    }

    #[test]
    fn dir_aggregation() {
        let mut l_sub = Node::new_dir("sub", Meta::default());
        l_sub
            .children
            .insert("x.txt".into(), file("x.txt", 1, HOUR));
        let mut r_sub = Node::new_dir("sub", Meta::default());
        r_sub
            .children
            .insert("y.txt".into(), file("y.txt", 1, HOUR));
        let l = tree(vec![l_sub]);
        let r = tree(vec![r_sub]);
        let diff = compare(&l, &r);
        assert_eq!(diff.counts.left_only, 1);
        assert_eq!(diff.counts.right_only, 1);
        let sub = &diff.children[0];
        assert!(sub.is_dir);
        assert_eq!(sub.counts.differing(), 2);
    }
}
