use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use dagcmp_core::compare::{compare, Counts, DiffNode, Status};
use dagcmp_core::scan::{
    scan_auto_with_progress, scan_pair_with_progress, ScanMethod, ScanOptions, ScanProgress,
    ScanResult,
};
use dagcmp_core::sync::{self, Direction};

#[derive(Parser)]
#[command(
    name = "dagcmp",
    version,
    about = "MFT-accelerated folder tree compare & sync"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory tree and print statistics
    Scan {
        /// Directory to scan
        path: PathBuf,
        /// Disable the MFT fast path; always use directory traversal
        /// (MFT needs a local NTFS volume and an elevated process)
        #[arg(long)]
        no_mft: bool,
    },
    /// Compare two directory trees
    Compare {
        /// Left-hand directory tree
        left: PathBuf,
        /// Right-hand directory tree
        right: PathBuf,
        /// Disable the MFT fast path; always use directory traversal
        /// (MFT needs a local NTFS volume and an elevated process)
        #[arg(long)]
        no_mft: bool,
        /// Also list identical files (default: differences only)
        #[arg(long)]
        all: bool,
        /// Limit printed tree depth; 0 prints the summary only
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
    },
    /// Copy missing/newer files one way (no deletes). Dry-run unless --apply.
    Sync {
        /// Left-hand directory tree
        left: PathBuf,
        /// Right-hand directory tree
        right: PathBuf,
        /// Copy direction: "l2r" (left to right) or "r2l" (right to left).
        /// The destination side is only added to, never deleted from
        #[arg(long, default_value = "l2r", value_name = "l2r|r2l")]
        direction: String,
        /// Disable the MFT fast path; always use directory traversal
        /// (MFT needs a local NTFS volume and an elevated process)
        #[arg(long)]
        no_mft: bool,
        /// Perform the copies. Without this flag, only print the plan
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { path, no_mft } => {
            let res = do_scan(&path, no_mft)?;
            let (files, dirs, bytes) = res.root.totals();
            println!(
                "{} files, {} dirs, {} — scanned in {:.2?} via {}",
                files,
                dirs,
                human_bytes(bytes),
                res.duration,
                method_name(&res)
            );
        }
        Command::Compare {
            left,
            right,
            no_mft,
            all,
            depth,
        } => {
            let (l, r) = do_scan_pair(&left, &right, no_mft)?;
            report_scan(&l);
            report_scan(&r);
            let diff = compare(&l.root, &r.root);
            let max_depth = depth.unwrap_or(usize::MAX);
            if max_depth > 0 {
                for child in &diff.children {
                    print_tree(child, 0, max_depth, all);
                }
            }
            print_summary(&diff.counts);
        }
        Command::Sync {
            left,
            right,
            direction,
            no_mft,
            apply,
        } => {
            let dir = match direction.as_str() {
                "l2r" => Direction::LeftToRight,
                "r2l" => Direction::RightToLeft,
                other => anyhow::bail!("invalid --direction '{other}' (use l2r or r2l)"),
            };
            let (l, r) = do_scan_pair(&left, &right, no_mft)?;
            let diff = compare(&l.root, &r.root);
            let plan = sync::plan(&diff, &left, &right, dir);
            if plan.ops.is_empty() {
                println!("Nothing to do — trees are in sync for this direction.");
                return Ok(());
            }
            for op in &plan.ops {
                match op {
                    sync::SyncOp::CreateDir { dst } => println!("mkdir  {}", dst.display()),
                    sync::SyncOp::Copy { src, dst, size } => {
                        println!("copy   {} -> {} ({})", src.display(), dst.display(), human_bytes(*size))
                    }
                }
            }
            println!(
                "\n{} files, {} to copy",
                plan.total_files,
                human_bytes(plan.total_bytes)
            );
            if apply {
                let outcome = sync::execute(&plan, |p| {
                    eprint!(
                        "\r{}/{} files, {}/{}    ",
                        p.done_files,
                        p.total_files,
                        human_bytes(p.done_bytes),
                        human_bytes(p.total_bytes)
                    );
                });
                eprintln!();
                println!(
                    "Copied {} files ({}). {} errors.",
                    outcome.copied_files,
                    human_bytes(outcome.copied_bytes),
                    outcome.errors.len()
                );
                for (path, e) in outcome.errors.iter().take(20) {
                    eprintln!("  error: {}: {}", path.display(), e);
                }
            } else {
                println!("Dry run — pass --apply to execute.");
            }
        }
    }
    Ok(())
}

fn progress_message(label: &str, p: ScanProgress) -> String {
    match p {
        ScanProgress::MftLoading => format!("{label}: reading MFT…"),
        ScanProgress::MftReading { bytes } => {
            format!("{label}: reading MFT… {}", human_bytes(bytes))
        }
        ScanProgress::MftLoaded { records, bytes } => {
            format!("{label}: MFT loaded ({records} records, {})", human_bytes(bytes))
        }
        ScanProgress::MftRecords {
            done,
            total,
            files,
            dirs,
        } => {
            format!("{label}: {done}/{total} MFT records — {files} files, {dirs} dirs matched")
        }
        ScanProgress::WalkEntries { entries } => {
            format!("{label}: {entries} entries walked")
        }
    }
}

/// Stderr single-line progress printer; erases itself when done.
struct ProgressLine {
    label: String,
    erase_len: usize,
}

impl ProgressLine {
    fn new(label: String) -> Self {
        ProgressLine {
            label,
            erase_len: 0,
        }
    }
    fn update(&mut self, p: ScanProgress) {
        let msg = progress_message(&self.label, p);
        let pad = self.erase_len.saturating_sub(msg.len());
        self.erase_len = self.erase_len.max(msg.len());
        eprint!("\r{msg}{}", " ".repeat(pad));
    }
    fn finish(&mut self) {
        if self.erase_len > 0 {
            eprint!("\r{}\r", " ".repeat(self.erase_len));
            self.erase_len = 0;
        }
    }
}

fn do_scan(path: &PathBuf, no_mft: bool) -> Result<ScanResult> {
    let mut line = ProgressLine::new(path.display().to_string());
    let res = scan_auto_with_progress(path, ScanOptions { try_mft: !no_mft }, &mut |p| {
        line.update(p)
    });
    line.finish();
    Ok(res?)
}

/// Scan both sides of a compare/sync. Same-volume pairs share one MFT read.
fn do_scan_pair(
    left: &PathBuf,
    right: &PathBuf,
    no_mft: bool,
) -> Result<(ScanResult, ScanResult)> {
    let mut line = ProgressLine::new("scan".to_string());
    let res = scan_pair_with_progress(left, right, ScanOptions { try_mft: !no_mft }, &mut |p| {
        line.update(p)
    });
    line.finish();
    Ok(res?)
}

fn method_name(res: &ScanResult) -> String {
    match res.method {
        ScanMethod::Mft => "MFT".to_string(),
        ScanMethod::Walk => match &res.mft_fallback_reason {
            Some(reason) => format!("directory walk (MFT unavailable: {reason})"),
            None => "directory walk".to_string(),
        },
    }
}

fn report_scan(res: &ScanResult) {
    let (files, dirs, _) = res.root.totals();
    eprintln!(
        "scanned {} ({} files, {} dirs) in {:.2?} via {}",
        res.root_path.display(),
        files,
        dirs,
        res.duration,
        method_name(res)
    );
}

fn status_marker(s: Status) -> &'static str {
    match s {
        Status::Identical => "  =  ",
        Status::LeftNewer => " <<  ",
        Status::RightNewer => "  >> ",
        Status::Different => "  !  ",
        Status::LeftOnly => " <-  ",
        Status::RightOnly => "  -> ",
    }
}

fn print_tree(node: &DiffNode, indent: usize, max_depth: usize, all: bool) {
    if !all && node.counts.in_sync() {
        return;
    }
    if node.is_dir {
        println!(
            "{}[{}] {}{}",
            " ".repeat(indent * 2 + 5),
            if node.counts.in_sync() { "=" } else { "≠" },
            node.name,
            summary_suffix(&node.counts)
        );
        if indent + 1 < max_depth {
            for c in &node.children {
                print_tree(c, indent + 1, max_depth, all);
            }
        }
    } else {
        println!(
            "{}{}{}",
            status_marker(node.status),
            " ".repeat(indent * 2),
            node.name
        );
    }
}

fn summary_suffix(c: &Counts) -> String {
    if c.in_sync() {
        String::new()
    } else {
        format!("  ({} differing)", c.differing())
    }
}

fn print_summary(c: &Counts) {
    println!();
    println!(
        "identical: {}   left-newer: {}   right-newer: {}   different: {}   left-only: {}   right-only: {}",
        c.identical, c.left_newer, c.right_newer, c.different, c.left_only, c.right_only
    );
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
