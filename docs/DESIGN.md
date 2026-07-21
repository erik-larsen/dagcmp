# dagcmp design notes

## Cross-platform snapshot & change-tracking model

Goal: initial analysis once, then stay current by tracking filesystem changes.
Background footprint must be near zero; work runs at high priority only during
dagcmp startup (catch-up) and while dagcmp is running (live view).

### Per-platform snapshot providers (planned trait)

| Platform | Initial scan | Catch-up after downtime | Live tracking while running |
|---|---|---|---|
| Windows (NTFS, elevated) | MFT read (`ntfs-reader`) | USN journal since last-seen USN — OS-maintained, persistent, zero cost while we're not running | USN journal polling / `ReadDirectoryChangesW` |
| Windows (non-NTFS / non-elevated / UNC) | parallel directory walk | full re-walk diffed against persisted snapshot | `ReadDirectoryChangesW` |
| macOS | parallel walk (`getattrlistbulk` under the hood) | FSEvents since last event ID — OS-maintained, persistent (`.fseventsd`), directory-granularity (re-stat touched dirs) | FSEvents |
| Linux | parallel walk (`getdents64`/`statx`) | full re-walk vs persisted snapshot (fast on Linux); optional later: idle-priority daemon holding fanotify/inotify subscriptions | inotify / fanotify (`notify` crate) |

Key point: on Windows and macOS the OS itself is the "background watcher" —
the journal accrues on disk with zero cycles from us. No service or daemon is
required for the catch-up model. Linux has no persistent journal; the fast
re-walk is the default answer there.

### Persisted snapshot cache

Each scanned root gets a serialized snapshot (tree + metadata + journal cursor:
last USN / FSEvents ID / walk timestamp). Startup = load snapshot, replay
journal, re-scan only where needed. Snapshots make second-and-later scans
near-instant on every platform.

### Priority policy

- Receiving journal/watch events is cheap (kernel queues, we drain) — nothing
  to throttle there.
- Deferred *processing* (big backlog replays, re-stat of touched dirs, future
  content hashing) runs in OS background mode, which throttles CPU **and I/O**:
  - Windows: `PROCESS_MODE_BACKGROUND_BEGIN` / `THREAD_MODE_BACKGROUND_BEGIN`
  - macOS: QoS class `background`
  - Linux: `SCHED_IDLE` + ionice idle class
- Interactive scans and the live diff view while dagcmp is open run at
  normal priority.

### Scanner selection: walk vs. MFT (planned)

The MFT path reads the whole volume's MFT regardless of subtree size, so for
small subtrees a direct walk wins and for large ones the MFT wins. Ideas, in
increasing order of ambition:

1. **Heuristics before scanning.** Cheap signals only — actually counting the
   subtree costs nearly as much as walking it. Usable signals: path depth
   (drive root / shallow paths → MFT), volume's total MFT size vs. a
   subtree-size estimate from a shallow sample walk (first 1–2 levels).
2. **Race and cancel.** Start the MFT read and the walk concurrently; first
   complete result wins, the loser is abandoned (both scanners already run on
   background threads; add a cancellation flag checked per chunk/entry).
   Caveat: on spinning disks the two compete for the head (sequential MFT
   read vs. random walk I/O), so racing may slow both; on SSD/NVMe the
   contention is minor.
3. **Scan-statistics cache.** Persist per-root results of previous scans
   (file/dir counts, which scanner won, durations). Staleness is harmless
   here — a wrong guess costs performance, never correctness. This makes the
   heuristic in (1) nearly free and self-correcting.
4. **Snapshot cache as data source** — the full design above (persisted
   snapshot + USN catch-up) subsumes all of this when implemented: a cached
   MFT-derived snapshot replayed through the USN journal is *exact*, not
   stale, and reduces every later scan to milliseconds. (1)–(3) are cheap
   stepping stones that don't require journal plumbing.

### Correctness invariant

Every journal/watch mechanism can gap (USN wrap, FSEvents drop with
must-rescan flag, inotify queue overflow, snapshot older than journal
retention). Journals are an optimization, never the source of truth: on any
detected gap, fall back to re-scanning the affected subtree. Scans are fast,
so degradation is graceful.

## Elevation model (Windows)

MFT + USN need elevation. v0: manifest-free, auto-fallback with reason
reporting. Later option (Everything-style): a one-time-install Windows
*service* (not a driver) runs elevated and serves volume data over IPC, so the
UI runs unelevated with no per-launch UAC prompt.
