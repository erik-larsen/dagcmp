pub mod walk;

#[cfg(windows)]
pub mod mft;

use crate::model::Node;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMethod {
    /// Whole-volume MFT read, filtered to the subtree (fast path).
    Mft,
    /// Recursive directory traversal (fallback).
    Walk,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Attempt the MFT fast path first (local NTFS + elevation required).
    pub try_mft: bool,
}

/// Coarse progress events emitted during a scan. Emitted at most every few
/// thousand records, so the callback may do I/O (e.g. print to stderr).
#[derive(Debug, Clone, Copy)]
pub enum ScanProgress {
    /// Opening the volume; the raw $MFT read follows immediately.
    MftLoading,
    /// Cumulative bytes read from the volume so far while slurping the $MFT
    /// (reported every 8 MiB chunk).
    MftReading { bytes: u64 },
    MftLoaded { records: u64, bytes: u64 },
    /// Progress through the in-memory record table. `files`/`dirs` count what
    /// has matched the requested subtree so far.
    MftRecords { done: u64, total: u64, files: u64, dirs: u64 },
    WalkEntries { entries: u64 },
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions { try_mft: true }
    }
}

#[derive(Debug)]
pub struct ScanResult {
    pub root_path: PathBuf,
    pub root: Node,
    pub method: ScanMethod,
    pub duration: Duration,
    /// Entries skipped due to access errors (walk scanner only).
    pub skipped: u64,
    /// Why the MFT fast path was not used, if it was requested but fell back.
    pub mft_fallback_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("path does not exist or is not accessible: {0}")]
    RootNotFound(PathBuf),
    #[error("path is not a directory: {0}")]
    NotADirectory(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Scan `root`, using the MFT fast path when possible and falling back to
/// directory traversal otherwise.
pub fn scan_auto(root: &Path, options: ScanOptions) -> Result<ScanResult, ScanError> {
    scan_auto_with_progress(root, options, &mut |_| {})
}

/// Scan two roots for a compare. When both live on the same local volume and
/// the MFT fast path applies, the volume's MFT is read **once** and both
/// trees are built in a single pass over the record table; otherwise each
/// side is scanned independently (each may still use its own volume's MFT).
pub fn scan_pair_with_progress(
    left: &Path,
    right: &Path,
    options: ScanOptions,
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<(ScanResult, ScanResult), ScanError> {
    #[cfg(windows)]
    if options.try_mft {
        if let (Some(a), Some(b)) = (mft::drive_of(left), mft::drive_of(right)) {
            if a == b {
                for p in [left, right] {
                    let md = std::fs::metadata(p)
                        .map_err(|_| ScanError::RootNotFound(p.to_path_buf()))?;
                    if !md.is_dir() {
                        return Err(ScanError::NotADirectory(p.to_path_buf()));
                    }
                }
                let start = std::time::Instant::now();
                match mft::scan_many(&[left, right], on_progress) {
                    Ok(mut nodes) => {
                        let duration = start.elapsed();
                        let right_node = nodes.pop().expect("two roots in");
                        let left_node = nodes.pop().expect("two roots in");
                        let mk = |path: &Path, root: Node| ScanResult {
                            root_path: path.to_path_buf(),
                            root,
                            method: ScanMethod::Mft,
                            duration,
                            skipped: 0,
                            mft_fallback_reason: None,
                        };
                        return Ok((mk(left, left_node), mk(right, right_node)));
                    }
                    Err(e) => {
                        // Shared fast path failed (e.g. not elevated). Walk
                        // both sides without retrying MFT per side, keeping
                        // the reason for reporting.
                        let reason = e.to_string();
                        let opts = ScanOptions { try_mft: false };
                        let mut l = scan_auto_with_progress(left, opts, on_progress)?;
                        let mut r = scan_auto_with_progress(right, opts, on_progress)?;
                        l.mft_fallback_reason = Some(reason.clone());
                        r.mft_fallback_reason = Some(reason);
                        return Ok((l, r));
                    }
                }
            }
        }
    }
    let l = scan_auto_with_progress(left, options, on_progress)?;
    let r = scan_auto_with_progress(right, options, on_progress)?;
    Ok((l, r))
}

/// Like [`scan_auto`], reporting coarse progress through `on_progress`.
pub fn scan_auto_with_progress(
    root: &Path,
    options: ScanOptions,
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<ScanResult, ScanError> {
    let start = std::time::Instant::now();
    let md = std::fs::metadata(root).map_err(|_| ScanError::RootNotFound(root.to_path_buf()))?;
    if !md.is_dir() {
        return Err(ScanError::NotADirectory(root.to_path_buf()));
    }

    let mut fallback_reason = None;

    #[cfg(windows)]
    if options.try_mft {
        match mft::scan(root, on_progress) {
            Ok(node) => {
                return Ok(ScanResult {
                    root_path: root.to_path_buf(),
                    root: node,
                    method: ScanMethod::Mft,
                    duration: start.elapsed(),
                    skipped: 0,
                    mft_fallback_reason: None,
                });
            }
            Err(e) => fallback_reason = Some(e.to_string()),
        }
    }

    #[cfg(not(windows))]
    let _ = options;

    let (node, skipped) = walk::scan(root, on_progress)?;
    Ok(ScanResult {
        root_path: root.to_path_buf(),
        root: node,
        method: ScanMethod::Walk,
        duration: start.elapsed(),
        skipped,
        mft_fallback_reason: fallback_reason,
    })
}
