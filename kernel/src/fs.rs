//! The disk, where a syscall can reach it
//! (docs/adr/0028-fase4-file-abi-v0.md).
//!
//! Until now the filesystem existed only inside `kmain`, as a `Volume`
//! built over borrowed locals: the kernel read what it needed while it
//! still had the disk in scope, and then let go. A syscall arrives from
//! somewhere else entirely, so the disk has to outlive the boot and be
//! reachable from a handler — which makes it the first device this kernel
//! shares between contexts, and the first that needs a lock.
//!
//! The volume itself is **not** kept. It is rebuilt for each operation
//! over the mounted disk, which costs nothing now that the boot sector is
//! read once (`Volume::with_boot_sector`), and means there is no
//! filesystem state anywhere to fall out of step with what is on the
//! disk. Every answer comes from the tables and the directory as they are.

use harlan_hal::fat::{self, BootSector, DirectoryEntry, Volume, VolumeError, WriteError};

use crate::devices::virtio_blk::{Disk, ReadError, Reader};
use crate::sync::IrqLock;

/// The disk, the one reader that drives it, and its boot sector.
struct Mounted {
    disk: Disk,
    reader: Reader,
    /// Read and validated once, at boot. Keeping it is what lets a
    /// syscall skip a sector read for a structure that cannot change
    /// while the volume is mounted.
    boot: BootSector,
}

// SAFETY: `Reader` holds raw pointers into two frames the kernel mapped
// for the device at boot and marked as device memory, so they are never
// handed out again and stay valid for as long as the kernel runs
// (docs/adr/0024-fase4-dma-and-the-queue.md). `Mounted` is reachable only
// through `DISK`, whose guard is the only way to a `&mut` of it, so those
// pointers are never followed from two places at once — which is also the
// condition `Reader::read_sector` asks of its caller.
unsafe impl Send for Mounted {}

/// The disk, for whoever holds the lock.
///
/// `None` until `adopt`: a kernel that found no disk, or could not
/// negotiate one, still boots and still runs programs — they just cannot
/// open anything (the kernel boots without a disk, as it boots without a
/// network).
static DISK: IrqLock<harlan_arch_x86_64::Cpu, Option<Mounted>> =
    IrqLock::new(harlan_arch_x86_64::Cpu, None);

/// The mounted disk as something a `Volume` can read sectors from.
struct Sectors<'a>(&'a mut Mounted);

impl fat::Sectors for Sectors<'_> {
    type Error = ReadError;

    fn read_sector(&mut self, sector: u32, into: &mut [u8; 512]) -> Result<(), Self::Error> {
        // SAFETY: the reader owns its queue and its request frame, and the
        // lock held by whoever built this makes it the only one using
        // them: a request is never in flight when another starts, because
        // this returns before the next call can be made.
        unsafe {
            self.0
                .reader
                .read_sector(&self.0.disk, u64::from(sector), into)
        }
    }

    fn write_sector(&mut self, sector: u32, from: &[u8; 512]) -> Result<(), Self::Error> {
        // SAFETY: as above.
        unsafe {
            self.0
                .reader
                .write_sector(&self.0.disk, u64::from(sector), from)
        }
    }
}

/// What can go wrong between a program asking for a file and the disk
/// answering.
///
/// Kept apart from the ABI's numbers (`user::ERR_*`): this says what
/// happened, and the syscall layer decides which number a program sees.
#[derive(Debug)]
pub enum FileError {
    /// There is no disk, or it was never negotiated. A kernel without one
    /// still boots and still runs programs.
    NoDisk,
    /// Not a name this filesystem can hold: too long, or holding a
    /// character the format reserves (ADR 0028, point 10).
    BadName,
    /// No file of that name in the root directory.
    NoSuchFile,
    /// That name belongs to a directory, which is not a file to read or to
    /// write over.
    IsADirectory,
    /// Somebody has that file open, so writing it would change what they
    /// are in the middle of reading (ADR 0028, point 12).
    Open,
    /// The volume, or the disk under it, said no.
    Reading(VolumeError<ReadError>),
    Writing(WriteError<ReadError>),
}

/// Takes the disk over, with the boot sector already read and parsed.
///
/// Called once, from the boot, after the volume has been shown to be a
/// FAT32 volume. A second call replaces what the first left, and the old
/// reader is dropped — which is why nothing else may be holding a
/// `Reader` for the same device.
pub fn adopt(disk: Disk, reader: Reader, boot: BootSector) {
    *DISK.lock() = Some(Mounted { disk, reader, boot });
}

/// Whether there is a disk to ask at all.
pub fn mounted() -> bool {
    DISK.lock().is_some()
}

/// Runs `operation` over the mounted volume, holding the disk's lock.
///
/// The volume is built here and dropped here. Nothing about the
/// filesystem survives the call, so there is no cached chain, no free
/// cluster count and no open-file state in this module that could
/// disagree with what is written on the disk.
fn with<R>(operation: impl FnOnce(&mut Volume<Sectors<'_>>) -> R) -> Result<R, FileError> {
    let mut disk = DISK.lock();
    let mounted = disk.as_mut().ok_or(FileError::NoDisk)?;
    let boot = mounted.boot;
    let mut volume = Volume::with_boot_sector(Sectors(mounted), boot);
    Ok(operation(&mut volume))
}

/// Finds a file in the root directory by name.
pub fn find(name: &str) -> Result<DirectoryEntry, FileError> {
    if fat::encode_name(name).is_none() {
        return Err(FileError::BadName);
    }
    match with(|volume| volume.find(name))? {
        Ok(Some(entry)) if entry.is_directory() => Err(FileError::IsADirectory),
        Ok(Some(entry)) => Ok(entry),
        Ok(None) => Err(FileError::NoSuchFile),
        Err(err) => Err(FileError::Reading(err)),
    }
}

/// Reads at most `into.len()` bytes from `offset` bytes into the file.
///
/// Zero means the offset is at or past the end, which is how a reading
/// loop knows it is done (ADR 0028, point 3).
pub fn read_at(entry: &DirectoryEntry, offset: u32, into: &mut [u8]) -> Result<usize, FileError> {
    with(|volume| volume.read_at(entry, offset, into))?.map_err(FileError::Reading)
}

/// Writes a whole file, creating or replacing it.
///
/// Refuses if anybody has that name open. A descriptor holds the chain it
/// was opened with, and replacing a file reuses that chain
/// (ADR 0027 point 9), so a reader part way through would start seeing the
/// new file's bytes among the old one's. Checking four descriptors per
/// process is cheap, and removes the whole class rather than documenting
/// it (ADR 0028, point 12).
pub fn write_file(name: &str, contents: &[u8]) -> Result<DirectoryEntry, FileError> {
    if fat::encode_name(name).is_none() {
        return Err(FileError::BadName);
    }
    // SAFETY: the scheduler's table is only read here, and this runs
    // inside a syscall or the boot — never from an interrupt that could
    // have preempted a change to it.
    if unsafe { crate::scheduler::anyone_has_open(name) } {
        return Err(FileError::Open);
    }
    match with(|volume| volume.write_file(name, contents))? {
        Ok(entry) => Ok(entry),
        Err(WriteError::IsADirectory) => Err(FileError::IsADirectory),
        Err(err) => Err(FileError::Writing(err)),
    }
}

/// Reads a whole file, for a caller that has room for all of it.
///
/// The kernel's own reads at boot are like this: it knows how big the
/// program it is loading may be, and a program that does not fit is one it
/// refuses rather than one it reads in pieces.
pub fn read_file(entry: &DirectoryEntry, into: &mut [u8]) -> Result<usize, FileError> {
    with(|volume| volume.read_file(entry, into))?.map_err(FileError::Reading)
}

/// One sector, as bytes.
///
/// For the two things that are about the volume rather than about a file:
/// the backup boot sector, and comparing the tables. Not reachable from
/// ring 3 — there is no syscall for it, and there will not be one, because
/// a program that can read any sector can read every file.
pub fn read_sector(sector: u32, into: &mut [u8; 512]) -> Result<(), FileError> {
    with(|volume| fat::Sectors::read_sector(volume.sectors_mut(), sector, into))?
        .map_err(|err| FileError::Reading(VolumeError::Device(err)))
}

/// What the volume says about itself, if one is mounted.
pub fn boot_sector() -> Option<BootSector> {
    DISK.lock().as_ref().map(|mounted| mounted.boot)
}

/// Calls `each` with every name in the root directory, stopping when it
/// answers `false`.
///
/// For listing, which a shell needs before it can do anything useful with
/// the rest of this module.
pub fn each_name(mut each: impl FnMut(&DirectoryEntry) -> bool) -> Result<(), FileError> {
    with(|volume| volume.read_root(|entry| each(&entry)))?.map_err(FileError::Reading)
}

/// The entry at position `index` in the root directory.
///
/// One entry per call, found by walking from the start (ADR 0029, point 2).
/// `NoSuchFile` for an index past the end, which is what a listing loop
/// stops on.
///
/// Walking the whole directory to reach entry *n* means listing *n* files
/// costs *n* walks. That is named rather than hidden (ADR 0029, point 3):
/// FAT has no index, so reaching the *n*th entry is passing the *n-1*
/// before it, and for a root directory of a handful of files it does not
/// measure. What it buys is that **each call is true by itself** — a
/// directory that changed between two calls gives a different answer to the
/// second, rather than half of one answer spread across a buffer.
pub fn entry_at(index: usize) -> Result<DirectoryEntry, FileError> {
    let mut at = 0;
    let mut found = None;
    with(|volume| {
        volume.read_root(|entry| {
            if at == index {
                found = Some(entry);
                false
            } else {
                at += 1;
                true
            }
        })
    })?
    .map_err(FileError::Reading)?;
    found.ok_or(FileError::NoSuchFile)
}
