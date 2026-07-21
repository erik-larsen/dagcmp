# dagcmp

Fast, open-source directory tree compare & sync — in the spirit of
[TreeComp](http://www.brobbel.net/treecomp/), at the speed of
[WizTree](https://wiztreefree.com/). On local NTFS volumes it reads the
volume's Master File Table directly (user-mode, no kernel driver) instead of
traversing directories, and falls back to a standard walk on network shares,
non-NTFS volumes, or unelevated runs. MIT licensed.

*(Why "dag"? A filesystem with hardlinks is a directed acyclic graph, and
`cmp` is honest Unix lineage. Also, every good tree word was taken.)*

## Build

### 1. Install Rust

**From an MSYS2 shell (CLANG64 or any other), or Git Bash:**

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

This installs the standard Windows rustup toolchain (MSVC). If you don't have
the Visual Studio C++ Build Tools yet, rustup-init detects that and offers to
install them for you — accept the defaults.

Equivalent from PowerShell/cmd: `winget install Rustlang.Rustup`.

**Then make cargo visible in MSYS2 shells.** MSYS2 does not inherit the
Windows user PATH by default, so add this to `~/.bashrc` (adjust if your MSYS2
username differs from your Windows username):

```sh
export PATH="$PATH:/c/Users/$USER/.cargo/bin"
```

Restart the shell and confirm with `cargo --version`. (Alternative: launch
MSYS2 with `MSYS2_PATH_TYPE=inherit` to pick up the whole Windows PATH.)

> **Untested alternative:** MSYS2 ships a native Rust
> (`pacman -S mingw-w64-clang-x86_64-rust`). It lags rustup by several
> releases (1.87 vs 1.97 at the time of writing) and targets `gnullvm`
> instead of MSVC; dagcmp is developed and tested against rustup/MSVC.

### 2. Compile

```sh
cd dagcmp
cargo build --release
```

Binaries land in `target/release/`: `dagcmp.exe` (CLI) and
`dagcmp-gui.exe` (GUI).

## Run

For the MFT fast path, run from an **elevated** shell (right-click →
"Run as administrator"). Without elevation everything still works — dagcmp
falls back to directory traversal and tells you why:
`via directory walk (MFT unavailable: process is not elevated)`.

### Scan — timing and statistics for one tree

```sh
cd target/release
dagcmp scan C:/some/folder            # add --no-mft to force the fallback walker
# 196649 files, 27659 dirs, 15.42 GB — scanned in 2.48s via directory walk
```

### Compare — two trees, differences only by default

```sh
dagcmp compare C:/projects/app D:/backup/app
dagcmp compare --all --depth 2 left right   # include identical files, limit depth
```

Output markers, TreeComp-style (directories aggregate their contents):

```
  =    identical            <<   left side newer
  !    different            >>   right side newer
 <-    left side only       ->   right side only
```

### Sync — one-way copy, dry-run by default

```sh
dagcmp sync C:/projects/app D:/backup/app                    # prints the plan only
dagcmp sync C:/projects/app D:/backup/app --apply            # actually copies
dagcmp sync left right --direction r2l --apply               # right-to-left
```

Sync copies files that are missing or newer on the source side. It never
deletes, and never overwrites a strictly newer destination file. Modification
times are preserved.

### GUI

```sh
target/release/dagcmp-gui.exe
```

Pick two folders, press **Compare**: merged tree with color-coded statuses,
live scan progress in the status bar, show/hide-identical toggle.

## Architecture

| Crate | Purpose |
|---|---|
| `dagcmp-core` | Scanning (MFT fast path + directory-walk fallback), tree model, compare engine, sync planner/executor |
| `dagcmp` | Command-line tool: `scan`, `compare`, `sync` |
| `dagcmp-gui` | egui desktop app: merged two-tree diff view |

### Scanning strategy

1. **MFT fast path** — on local NTFS volumes, running elevated, the whole
   volume's MFT is read and parsed in memory (via the `ntfs-reader` crate),
   then filtered to the requested subtree. No kernel driver required; plain
   user-mode volume reads.
2. **Fallback** — non-NTFS volumes, network shares, or non-elevated runs use a
   standard directory traversal. Selected automatically; the output reports
   which path was used.

### Compare model

Files are matched case-insensitively by name and compared by size and modified
time (2-second tolerance for FAT interop). Statuses: identical, left/right
newer, different, left/right only. Directories aggregate the counts of their
contents. Content (hash) comparison and moved/renamed detection are planned.

## Status

Early scaffold. Working: MFT + fallback scans with live progress, compare,
dry-run/apply one-way sync (copy only, no deletes), CLI, and a minimal GUI
diff viewer.

MFT scan speed is comparable to WizTree in side-by-side tests (same machine,
same volumes: ~2 s on a small NVMe volume, ~17 s on a large system drive —
matching WizTree on both). The scan reads the volume's `$MFT` in 8 MiB
aligned chunks; memory use during a scan is roughly the size of the MFT
(~1 KB per file on the volume).

Design notes for the cross-platform snapshot/change-tracking model live in
[docs/DESIGN.md](docs/DESIGN.md).

## Roadmap

- Parallel directory walker (network shares now; primary scanner on macOS/Linux later)
- Smarter walk-vs-MFT selection: cheap heuristics, race-and-cancel, cached scan statistics (see [docs/DESIGN.md](docs/DESIGN.md))
- Optimized copy engine (overlapped unbuffered I/O for large files, batching for small)
- USN-journal incremental rescans and live monitoring; persisted snapshots
- macOS (FSEvents) and Linux support
- Content compare (BLAKE3), moved/renamed file detection
- Similar file-group and subtree detection (Merkle-style subtree hashing)
- Launch external diff tools (WinMerge, difftastic) from the GUI

## License

MIT — see [LICENSE](LICENSE).
