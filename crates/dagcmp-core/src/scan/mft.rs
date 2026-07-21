//! MFT fast-path scanner: reads the whole volume's Master File Table in
//! memory (user-mode raw volume read, no driver) and filters it down to the
//! requested subtree. Requires elevation and a local NTFS volume; callers
//! fall back to the walk scanner on any error.

use crate::model::{Meta, Node};
use crate::scan::ScanProgress;
use ntfs_reader::api::{
    NtfsAttributeType, NtfsFileRecordHeader, EPOCH_DIFFERENCE, FIRST_NORMAL_RECORD, SECTOR_SIZE,
};
use ntfs_reader::file_info::{FileInfo, VecCache};
use ntfs_reader::mft::Mft;
use ntfs_reader::volume::Volume;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, Prefix};

const PROGRESS_INTERVAL: u64 = 65_536;

/// Read granularity for raw volume access. Must be a multiple of the physical
/// sector size (covers both 512e and 4Kn drives); large so the multi-GB $MFT
/// read costs one syscall per chunk instead of ntfs-reader's one per 4 KB.
const READ_CHUNK: usize = 8 * 1024 * 1024;
const READ_ALIGN: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum MftError {
    #[error("not a local drive path")]
    NotLocalDrive,
    #[error("process is not elevated")]
    NotElevated,
    #[error("volume error: {0}")]
    Ntfs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ntfs_reader::errors::NtfsReaderError> for MftError {
    fn from(e: ntfs_reader::errors::NtfsReaderError) -> Self {
        let msg = e.to_string();
        if msg.to_lowercase().contains("elevat") {
            MftError::NotElevated
        } else {
            MftError::Ntfs(msg)
        }
    }
}

/// Sector-aligned buffered reader over a raw volume handle. Serves reads from
/// an 8 MiB internal buffer — one kernel round-trip per chunk — and reports
/// cumulative bytes read so callers can show progress during the $MFT slurp.
struct AlignedVolumeReader<'a> {
    file: File,
    /// Logical (unaligned) read position.
    position: u64,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    total_read: u64,
    on_bytes: &'a mut dyn FnMut(u64),
}

impl<'a> AlignedVolumeReader<'a> {
    fn open(path: &Path, on_bytes: &'a mut dyn FnMut(u64)) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            position: 0,
            buf: Vec::new(),
            buf_start: 0,
            buf_len: 0,
            total_read: 0,
            on_bytes,
        })
    }

    fn buffered(&self) -> Option<(usize, usize)> {
        if self.position >= self.buf_start && self.position < self.buf_start + self.buf_len as u64 {
            let off = (self.position - self.buf_start) as usize;
            Some((off, self.buf_len - off))
        } else {
            None
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let aligned = self.position / READ_ALIGN * READ_ALIGN;
        self.file.seek(SeekFrom::Start(aligned))?;
        self.buf.resize(READ_CHUNK, 0);
        let mut filled = 0usize;
        while filled < READ_CHUNK {
            match self.file.read(&mut self.buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.buf_start = aligned;
        self.buf_len = filled;
        self.total_read += filled as u64;
        (self.on_bytes)(self.total_read);
        Ok(())
    }
}

impl Read for AlignedVolumeReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.buffered().is_none() {
            self.fill()?;
        }
        match self.buffered() {
            None => Ok(0), // past end of volume
            Some((off, avail)) => {
                let n = out.len().min(avail);
                out[..n].copy_from_slice(&self.buf[off..off + n]);
                self.position += n as u64;
                Ok(n)
            }
        }
    }
}

impl Seek for AlignedVolumeReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => {
                if d >= 0 {
                    self.position.saturating_add(d as u64)
                } else {
                    self.position.checked_sub(d.unsigned_abs()).ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start")
                    })?
                }
            }
            SeekFrom::End(_) => return Err(std::io::Error::other("SeekFrom::End unsupported")),
        };
        self.position = new;
        Ok(new)
    }
}

/// Apply the NTFS update-sequence-array fixup to one FILE record in place.
/// Ported from ntfs-reader 0.4.5 (`Mft::fixup_record`, MIT OR Apache-2.0),
/// which is private there.
fn fixup_record(number: u64, data: &mut [u8]) -> Result<(), MftError> {
    let corrupt = || MftError::Ntfs(format!("corrupt MFT record {number}"));
    if data.len() < std::mem::size_of::<NtfsFileRecordHeader>() {
        return Err(corrupt());
    }
    let header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const NtfsFileRecordHeader) };

    let usn_start = header.update_sequence_offset as usize;
    if usn_start + 2 > data.len() {
        return Err(corrupt());
    }
    let usa_start = usn_start + 2;
    let usa_end =
        usn_start.saturating_add((header.update_sequence_length as usize).saturating_mul(2));
    if usa_end > data.len() {
        return Err(corrupt());
    }

    let usn0 = data[usn_start];
    let usn1 = data[usn_start + 1];

    let mut sector_off = SECTOR_SIZE - 2;
    for usa_off in (usa_start..usa_end).step_by(2) {
        if sector_off + 2 > data.len() {
            break;
        }
        let mut usa = [0u8; 2];
        usa.copy_from_slice(&data[usa_off..usa_off + 2]);
        if data[sector_off] != usn0 || data[sector_off + 1] != usn1 {
            return Err(corrupt());
        }
        data[sector_off..sector_off + 2].copy_from_slice(&usa);
        sector_off += SECTOR_SIZE;
    }
    Ok(())
}

/// Read the volume's $MFT into memory the fast way. Equivalent to
/// `Mft::new(volume)` but reads through [`AlignedVolumeReader`] (8 MiB per
/// syscall instead of 4 KB) and reports byte progress.
fn read_mft(volume: Volume, on_bytes: &mut dyn FnMut(u64)) -> Result<Mft, MftError> {
    let (mut data, bitmap) = {
        let mut reader = AlignedVolumeReader::open(&volume.path, on_bytes)?;
        let mft_record =
            Mft::get_record_fs(&mut reader, volume.file_record_size, volume.mft_position)?;
        let data = Mft::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Data)?
            .ok_or_else(|| MftError::Ntfs("missing $MFT Data attribute".into()))?;
        let bitmap =
            Mft::read_data_fs(&volume, &mut reader, &mft_record, NtfsAttributeType::Bitmap)?
                .ok_or_else(|| MftError::Ntfs("missing $MFT Bitmap attribute".into()))?;
        (data, bitmap)
    };

    let max_record = data.len() as u64 / volume.file_record_size;
    for number in 0..max_record {
        let start = (number * volume.file_record_size) as usize;
        let end = start + volume.file_record_size as usize;
        fixup_record(number, &mut data[start..end])?;
    }

    Ok(Mft {
        volume,
        data,
        bitmap,
        max_record,
    })
}

/// Drive letter of a path, if it lives on a plain local drive (UNC paths and
/// mapped network drives return `None` — they have no locally readable MFT).
pub fn drive_of(root: &Path) -> Option<char> {
    let canon = std::fs::canonicalize(root).ok()?;
    match canon.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(d) | Prefix::Disk(d) => Some(d as char),
            _ => None,
        },
        _ => None,
    }
}

pub fn scan(root: &Path, on_progress: &mut dyn FnMut(ScanProgress)) -> Result<Node, MftError> {
    Ok(scan_many(&[root], on_progress)?.pop().expect("one root in, one tree out"))
}

/// One subtree being collected out of the shared MFT pass.
struct Target {
    /// Path relative to the drive root, lower-cased for matching.
    rel_root: Vec<String>,
    tree: Node,
}

/// Scan several roots — all on the same local NTFS volume — with a single
/// MFT read and a single pass over the record table. This is what makes a
/// same-drive compare cost one MFT scan instead of two.
pub fn scan_many(
    roots: &[&Path],
    on_progress: &mut dyn FnMut(ScanProgress),
) -> Result<Vec<Node>, MftError> {
    assert!(!roots.is_empty());
    let drive = drive_of(roots[0]).ok_or(MftError::NotLocalDrive)?;

    let mut targets = Vec::with_capacity(roots.len());
    for root in roots {
        if drive_of(root) != Some(drive) {
            return Err(MftError::NotLocalDrive); // caller groups roots by volume
        }
        let canon = std::fs::canonicalize(root)?;
        let rel_root: Vec<String> = canon
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => Some(os.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        let root_name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{drive}:"));
        let root_md = std::fs::metadata(root)?;
        targets.push(Target {
            rel_root,
            tree: Node::new_dir(root_name, Meta::from_fs(&root_md)),
        });
    }

    on_progress(ScanProgress::MftLoading);
    let volume = Volume::new(format!("\\\\.\\{drive}:"))?;
    let mft = {
        let mut on_bytes = |bytes: u64| on_progress(ScanProgress::MftReading { bytes });
        read_mft(volume, &mut on_bytes)?
    };
    on_progress(ScanProgress::MftLoaded {
        records: mft.max_record,
        bytes: mft.data.len() as u64,
    });

    let mut cache = VecCache::default();
    let mut files: u64 = 0;
    let mut dirs: u64 = 0;
    for number in FIRST_NORMAL_RECORD..mft.max_record {
        if number % PROGRESS_INTERVAL == 0 {
            on_progress(ScanProgress::MftRecords {
                done: number,
                total: mft.max_record,
                files,
                dirs,
            });
        }
        if !mft.record_exists(number) {
            continue;
        }
        let file = match mft.get_record(number) {
            Some(f) if f.is_used() => f,
            _ => continue,
        };
        let info = FileInfo::with_cache(&mft, &file, &mut cache);
        if info.name.is_empty() || info.path.as_os_str().is_empty() {
            continue;
        }

        let mut attrs: u32 = 0;
        file.attributes(|att| {
            if att.header.type_id == NtfsAttributeType::StandardInformation as u32 {
                if let Some(si) = att.as_standard_info() {
                    attrs = si.file_attributes;
                }
            }
        });

        // Components of the file's full path, minus the volume prefix,
        // lower-cased once for matching against every target.
        let names: Vec<String> = info
            .path
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();

        let mtime = info
            .modified
            .map(|t| {
                let unix_100ns = t.unix_timestamp_nanos() / 100;
                (unix_100ns + EPOCH_DIFFERENCE as i128).max(0) as u64
            })
            .unwrap_or(0);
        let meta = Meta {
            size: if info.is_directory { 0 } else { info.size },
            mtime,
            attrs,
        };

        let mut counted = false;
        for target in &mut targets {
            // Inside this target's subtree, and not the root itself?
            if names.len() <= target.rel_root.len()
                || lower[..target.rel_root.len()] != target.rel_root[..]
            {
                continue;
            }
            let rel = &names[target.rel_root.len()..];
            if !counted {
                // Count each record once even if nested roots both receive it.
                if info.is_directory {
                    dirs += 1;
                } else {
                    files += 1;
                }
                counted = true;
            }
            let name = rel.last().unwrap().clone();
            let node = if info.is_directory {
                Node::new_dir(name, meta)
            } else {
                Node::new_file(name, meta)
            };
            let refs: Vec<&str> = rel.iter().map(|s| s.as_str()).collect();
            target.tree.insert_at(&refs, node);
        }
    }

    Ok(targets.into_iter().map(|t| t.tree).collect())
}
