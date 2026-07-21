use crate::model::{Meta, Node};
use crate::scan::ScanProgress;
use std::path::Path;
use walkdir::WalkDir;

const PROGRESS_INTERVAL: u64 = 4096;

/// Fallback scanner: recursive directory traversal. Works on any filesystem
/// and without elevation. Unreadable entries are skipped and counted.
///
/// On Windows, `walkdir` fills metadata from the directory enumeration itself
/// (no extra stat per file), which keeps this reasonably fast.
pub fn scan(
    root: &Path,
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<(Node, u64), std::io::Error> {
    let root_md = std::fs::metadata(root)?;
    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let mut tree = Node::new_dir(root_name, Meta::from_fs(&root_md));
    let mut skipped: u64 = 0;
    let mut entries: u64 = 0;

    for entry in WalkDir::new(root).min_depth(1) {
        entries += 1;
        if entries % PROGRESS_INTERVAL == 0 {
            on_progress(ScanProgress::WalkEntries { entries });
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let components: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let comp_refs: Vec<&str> = components.iter().map(|s| s.as_str()).collect();
        let name = components.last().cloned().unwrap_or_default();
        let node = if md.is_dir() {
            Node::new_dir(name, Meta::from_fs(&md))
        } else {
            Node::new_file(name, Meta::from_fs(&md))
        };
        tree.insert_at(&comp_refs, node);
    }

    Ok((tree, skipped))
}
