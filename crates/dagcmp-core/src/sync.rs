use crate::compare::{DiffNode, Status};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
}

#[derive(Debug, Clone)]
pub enum SyncOp {
    CreateDir { dst: PathBuf },
    Copy { src: PathBuf, dst: PathBuf, size: u64 },
}

#[derive(Debug, Default)]
pub struct SyncPlan {
    pub ops: Vec<SyncOp>,
    pub total_bytes: u64,
    pub total_files: u64,
}

/// Build the list of operations to make the destination side contain
/// everything that is missing or older there. Copies source-only and
/// source-newer files; never deletes, never overwrites a strictly newer
/// destination file.
pub fn plan(
    diff: &DiffNode,
    left_root: &Path,
    right_root: &Path,
    direction: Direction,
) -> SyncPlan {
    let mut p = SyncPlan::default();
    walk(diff, left_root, right_root, direction, &mut p, true);
    p
}

fn walk(
    node: &DiffNode,
    left: &Path,
    right: &Path,
    dir: Direction,
    p: &mut SyncPlan,
    is_root: bool,
) {
    let (l, r) = if is_root {
        (left.to_path_buf(), right.to_path_buf())
    } else {
        (left.join(&node.name), right.join(&node.name))
    };
    let (src, dst, only_here, newer_here) = match dir {
        Direction::LeftToRight => (&l, &r, Status::LeftOnly, Status::LeftNewer),
        Direction::RightToLeft => (&r, &l, Status::RightOnly, Status::RightNewer),
    };

    if node.is_dir {
        if node.status == only_here {
            p.ops.push(SyncOp::CreateDir { dst: dst.clone() });
        }
        // Skip subtrees with nothing to do in this direction.
        let relevant = match dir {
            Direction::LeftToRight => node.counts.left_only + node.counts.left_newer,
            Direction::RightToLeft => node.counts.right_only + node.counts.right_newer,
        };
        if relevant == 0 {
            return;
        }
        for c in &node.children {
            walk(c, &l, &r, dir, p, false);
        }
    } else if node.status == only_here || node.status == newer_here {
        let size = match dir {
            Direction::LeftToRight => node.left.map(|m| m.size).unwrap_or(0),
            Direction::RightToLeft => node.right.map(|m| m.size).unwrap_or(0),
        };
        p.total_bytes += size;
        p.total_files += 1;
        p.ops.push(SyncOp::Copy {
            src: src.clone(),
            dst: dst.clone(),
            size,
        });
    }
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub done_files: u64,
    pub done_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
    pub current: PathBuf,
}

#[derive(Debug)]
pub struct SyncOutcome {
    pub copied_files: u64,
    pub copied_bytes: u64,
    pub errors: Vec<(PathBuf, std::io::Error)>,
}

/// Execute a plan sequentially, reporting progress after each file.
///
/// v0 uses `std::fs::copy` (CopyFileEx under the hood on Windows, which
/// preserves modification time). The optimized engine (overlapped unbuffered
/// I/O, small-file batching, parallel queues) replaces this later.
pub fn execute(plan: &SyncPlan, mut progress: impl FnMut(&Progress)) -> SyncOutcome {
    let mut out = SyncOutcome {
        copied_files: 0,
        copied_bytes: 0,
        errors: Vec::new(),
    };
    for op in &plan.ops {
        match op {
            SyncOp::CreateDir { dst } => {
                if let Err(e) = std::fs::create_dir_all(dst) {
                    out.errors.push((dst.clone(), e));
                }
            }
            SyncOp::Copy { src, dst, size } => {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(src, dst) {
                    Ok(_) => {
                        out.copied_files += 1;
                        out.copied_bytes += size;
                    }
                    Err(e) => out.errors.push((src.clone(), e)),
                }
                progress(&Progress {
                    done_files: out.copied_files,
                    done_bytes: out.copied_bytes,
                    total_files: plan.total_files,
                    total_bytes: plan.total_bytes,
                    current: dst.clone(),
                });
            }
        }
    }
    out
}
