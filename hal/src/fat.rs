//! Reading what a FAT32 volume says about itself.
//!
//! The boot sector is the first thing a filesystem driver touches and the
//! last thing it should trust: it is data from outside the kernel, and
//! every number in it is used to compute an address. A `sectors_per_cluster`
//! of zero is a division by zero; a table size of four billion is a read
//! off the end of the disk. So it is validated, not believed
//! (docs/adr/0025-fase4-fat32-read-only.md).
//!
//! Pure, and tested against boot sectors built by hand — including ones
//! that are wrong in each of the ways that matter. The arithmetic from a
//! cluster number to a sector number is three multiplications and an
//! addition, and getting one wrong reads the wrong place with no symptom
//! at all.

/// A sector is 512 bytes. FAT allows others and no disk this kernel will
/// meet uses them; a volume that says otherwise is refused rather than
/// half-supported.
pub const SECTOR_BYTES: u16 = 512;
/// The first cluster that can hold anything: entries 0 and 1 of the table
/// are reserved.
pub const FIRST_DATA_CLUSTER: u32 = 2;
/// Below this many clusters the volume is FAT16 by definition, whatever
/// its boot sector claims.
pub const FAT32_MINIMUM_CLUSTERS: u32 = 65_525;

/// Why a boot sector was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootSectorError {
    /// The last two bytes are not `0x55 0xAA`. Either it is not a boot
    /// sector or the read went somewhere else entirely.
    NoSignature { found: u16 },
    /// A sector size this kernel does not read.
    SectorSize { found: u16 },
    /// Zero, or not a power of two. Both would make the cluster
    /// arithmetic nonsense, and zero divides by zero.
    SectorsPerCluster { found: u8 },
    /// No reserved sectors means no boot sector, which is a contradiction.
    ReservedSectors { found: u16 },
    /// A volume with no tables, or more than any formatter writes.
    FatCount { found: u8 },
    /// FAT32 keeps its table size in the 32-bit field and leaves the
    /// 16-bit one zero. A volume with the 16-bit one set is FAT12 or
    /// FAT16, which this does not read.
    NotFat32 { sixteen_bit_fat_size: u16 },
    /// A table of no sectors describes no clusters.
    FatSize { found: u32 },
    /// The root directory has to start at a real cluster.
    RootCluster { found: u32 },
    /// The tables and the reserved sectors do not fit in the volume, so
    /// there is no data region at all.
    NoDataRegion,
    /// Fewer clusters than FAT32 is allowed to have.
    TooFewClusters { found: u32 },
}

/// What a FAT32 volume's boot sector says, once it has been believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSector {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub total_sectors: u32,
    pub sectors_per_fat: u32,
    pub root_cluster: u32,
    pub fs_info_sector: u16,
    pub backup_boot_sector: u16,
    /// How many clusters the data region actually holds, which is what
    /// says whether this is FAT32 at all.
    pub clusters: u32,
}

/// Offsets in the boot sector, from the specification.
mod at {
    pub const BYTES_PER_SECTOR: usize = 11;
    pub const SECTORS_PER_CLUSTER: usize = 13;
    pub const RESERVED_SECTORS: usize = 14;
    pub const FAT_COUNT: usize = 16;
    pub const SIXTEEN_BIT_FAT_SIZE: usize = 22;
    pub const TOTAL_SECTORS: usize = 32;
    pub const SECTORS_PER_FAT: usize = 36;
    pub const ROOT_CLUSTER: usize = 44;
    pub const FS_INFO_SECTOR: usize = 48;
    pub const BACKUP_BOOT_SECTOR: usize = 50;
    pub const SIGNATURE: usize = 510;
}

fn u16_at(sector: &[u8; 512], at: usize) -> u16 {
    u16::from_le_bytes([sector[at], sector[at + 1]])
}

fn u32_at(sector: &[u8; 512], at: usize) -> u32 {
    u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]])
}

impl BootSector {
    /// Reads a boot sector, refusing every way it can be wrong that would
    /// turn into a bad address later.
    ///
    /// The order of the checks is the order the numbers get used in: a
    /// volume is refused on the first thing that does not make sense,
    /// rather than on whichever check happens to come last.
    pub fn parse(sector: &[u8; 512]) -> Result<Self, BootSectorError> {
        let signature = u16_at(sector, at::SIGNATURE);
        if signature != 0xAA55 {
            return Err(BootSectorError::NoSignature { found: signature });
        }

        let bytes_per_sector = u16_at(sector, at::BYTES_PER_SECTOR);
        if bytes_per_sector != SECTOR_BYTES {
            return Err(BootSectorError::SectorSize {
                found: bytes_per_sector,
            });
        }

        let sectors_per_cluster = sector[at::SECTORS_PER_CLUSTER];
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(BootSectorError::SectorsPerCluster {
                found: sectors_per_cluster,
            });
        }

        let reserved_sectors = u16_at(sector, at::RESERVED_SECTORS);
        if reserved_sectors == 0 {
            return Err(BootSectorError::ReservedSectors {
                found: reserved_sectors,
            });
        }

        let fat_count = sector[at::FAT_COUNT];
        if fat_count == 0 || fat_count > 4 {
            return Err(BootSectorError::FatCount { found: fat_count });
        }

        // The field that tells FAT32 from the others: on FAT32 the table
        // size lives in the 32-bit field and this one is zero.
        let sixteen_bit_fat_size = u16_at(sector, at::SIXTEEN_BIT_FAT_SIZE);
        if sixteen_bit_fat_size != 0 {
            return Err(BootSectorError::NotFat32 {
                sixteen_bit_fat_size,
            });
        }

        let sectors_per_fat = u32_at(sector, at::SECTORS_PER_FAT);
        if sectors_per_fat == 0 {
            return Err(BootSectorError::FatSize {
                found: sectors_per_fat,
            });
        }

        let root_cluster = u32_at(sector, at::ROOT_CLUSTER);
        if root_cluster < FIRST_DATA_CLUSTER {
            return Err(BootSectorError::RootCluster {
                found: root_cluster,
            });
        }

        let total_sectors = u32_at(sector, at::TOTAL_SECTORS);
        // Everything before the data region, computed so that it cannot
        // overflow or wrap: these are numbers from the disk.
        let before_data =
            u64::from(reserved_sectors) + u64::from(fat_count) * u64::from(sectors_per_fat);
        if before_data >= u64::from(total_sectors) {
            return Err(BootSectorError::NoDataRegion);
        }
        let clusters =
            ((u64::from(total_sectors) - before_data) / u64::from(sectors_per_cluster)) as u32;
        if clusters < FAT32_MINIMUM_CLUSTERS {
            return Err(BootSectorError::TooFewClusters { found: clusters });
        }

        Ok(Self {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            total_sectors,
            sectors_per_fat,
            root_cluster,
            fs_info_sector: u16_at(sector, at::FS_INFO_SECTOR),
            backup_boot_sector: u16_at(sector, at::BACKUP_BOOT_SECTOR),
            clusters,
        })
    }

    /// Where the first table starts.
    pub const fn first_fat_sector(&self) -> u32 {
        self.reserved_sectors as u32
    }

    /// Where the data region starts, which is also where cluster 2 is.
    pub const fn first_data_sector(&self) -> u32 {
        self.reserved_sectors as u32 + self.fat_count as u32 * self.sectors_per_fat
    }

    /// Which sector a cluster begins at, or `None` for a cluster this
    /// volume does not have — including 0 and 1, which name no data.
    pub const fn sector_of_cluster(&self, cluster: u32) -> Option<u32> {
        if cluster < FIRST_DATA_CLUSTER || cluster >= FIRST_DATA_CLUSTER + self.clusters {
            return None;
        }
        Some(
            self.first_data_sector()
                + (cluster - FIRST_DATA_CLUSTER) * self.sectors_per_cluster as u32,
        )
    }

    pub const fn cluster_bytes(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }
}

// ---------------------------------------------------------------------
// The allocation table
// ---------------------------------------------------------------------

/// Only the low 28 bits of a FAT32 entry are the cluster number; the top
/// four are reserved and a reader has to mask them off. A disk written by
/// something that left them set would otherwise look full of impossible
/// cluster numbers.
const ENTRY_MASK: u32 = 0x0FFF_FFFF;

/// What one entry of the table says about its cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Nothing is using it.
    Free,
    /// Reserved, or a value no chain may point at.
    Reserved,
    /// The chain goes on, there.
    Next(u32),
    /// The medium is bad here. A chain that reaches one is broken, not
    /// finished.
    Bad,
    /// The chain ends with this cluster.
    End,
}

impl Entry {
    /// Reads an entry, masking off the four bits that are not part of it.
    pub const fn decode(raw: u32) -> Self {
        match raw & ENTRY_MASK {
            0 => Entry::Free,
            1 => Entry::Reserved,
            0x0FFF_FFF7 => Entry::Bad,
            // Everything from 0x0FFFFFF8 up is an end-of-chain marker;
            // only 0x0FFFFFFF is written, and all of them mean the same.
            end if end >= 0x0FFF_FFF8 => Entry::End,
            next => Entry::Next(next),
        }
    }
}

// ---------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------

/// A directory entry is thirty-two bytes.
pub const DIRECTORY_ENTRY_BYTES: usize = 32;
/// A first byte of zero: this entry has never been used, and neither has
/// any after it. It is what ends a directory.
const ENTRY_NEVER_USED: u8 = 0x00;
/// A first byte of `0xE5`: the entry was deleted. The ones after it may
/// still be good.
const ENTRY_DELETED: u8 = 0xE5;
/// The attribute combination that marks a long-name fragment rather than
/// a file. They are skipped: this kernel reads 8.3 names (ADR 0025).
const ATTR_LONG_NAME: u8 = 0x0F;
const ATTR_VOLUME_LABEL: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;

/// What a directory says about one of the things in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// The 8.3 name, as `NAME.EXT`, upper case, without the padding the
    /// entry stores. Short enough that it needs no allocation.
    pub name: [u8; 12],
    pub name_len: usize,
    pub attributes: u8,
    pub first_cluster: u32,
    pub size: u32,
}

impl DirectoryEntry {
    pub fn is_directory(&self) -> bool {
        self.attributes & ATTR_DIRECTORY != 0
    }

    /// The name as text. Always ASCII: a short name holds no other.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("")
    }

    /// Whether this names the same file as `other`, ignoring case — short
    /// names are stored upper case, and nobody types them that way.
    pub fn is_named(&self, other: &str) -> bool {
        self.name().len() == other.len()
            && self
                .name()
                .bytes()
                .zip(other.bytes())
                .all(|(a, b)| a.eq_ignore_ascii_case(&b))
    }
}

/// What reading one slot of a directory turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Something that is in the directory.
    Entry(DirectoryEntry),
    /// A slot that holds nothing worth reporting: a deleted entry, a
    /// fragment of a long name, or the volume's label.
    Skip,
    /// The end of the directory. Nothing after this slot has ever been
    /// used, so there is no point reading on.
    End,
}

/// Reads one slot of a directory.
pub fn decode_slot(bytes: &[u8; DIRECTORY_ENTRY_BYTES]) -> Slot {
    match bytes[0] {
        ENTRY_NEVER_USED => return Slot::End,
        ENTRY_DELETED => return Slot::Skip,
        _ => {}
    }
    let attributes = bytes[11];
    // A long-name fragment is not a file, and neither is the label.
    if attributes & ATTR_LONG_NAME == ATTR_LONG_NAME || attributes & ATTR_VOLUME_LABEL != 0 {
        return Slot::Skip;
    }

    // The stored name is eight characters and three, each padded with
    // spaces and with no dot between them. The dot is put back here.
    let mut name = [0u8; 12];
    let mut len = 0;
    for byte in &bytes[..8] {
        if *byte == b' ' {
            break;
        }
        name[len] = *byte;
        len += 1;
    }
    if bytes[8] != b' ' {
        name[len] = b'.';
        len += 1;
        for byte in &bytes[8..11] {
            if *byte == b' ' {
                break;
            }
            name[len] = *byte;
            len += 1;
        }
    }

    Slot::Entry(DirectoryEntry {
        name,
        name_len: len,
        attributes,
        // The cluster number arrives in two halves, sixteen bits apart,
        // with the high one earlier in the entry than the low one.
        first_cluster: u32::from(u16::from_le_bytes([bytes[20], bytes[21]])) << 16
            | u32::from(u16::from_le_bytes([bytes[26], bytes[27]])),
        size: u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
    })
}

// ---------------------------------------------------------------------
// A volume, over whatever can hand it sectors
// ---------------------------------------------------------------------

/// Where sectors come from. A disk in the kernel, an image in a test: the
/// code that walks a volume does not know which, which is what lets the
/// whole path be tested without a machine.
pub trait Sectors {
    type Error;

    /// Reads one 512-byte sector.
    fn read_sector(&mut self, sector: u32, into: &mut [u8; 512]) -> Result<(), Self::Error>;

    /// Writes one. A source that cannot — an image a test only reads —
    /// says so with an error of its own rather than pretending
    /// (docs/adr/0027-fase4-fat32-write.md).
    fn write_sector(&mut self, sector: u32, from: &[u8; 512]) -> Result<(), Self::Error>;
}

/// What can go wrong reading a volume, beyond what the device itself says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeError<E> {
    /// The device could not read a sector.
    Device(E),
    /// Its boot sector is not one this kernel reads.
    BootSector(BootSectorError),
    /// A chain pointed at a cluster this volume does not have. A disk
    /// saying something impossible, not a bug to work around.
    BadCluster { cluster: u32 },
    /// A chain reached a cluster marked bad, or one that is free — either
    /// way the file is not all there.
    BrokenChain { cluster: u32, entry: Entry },
    /// A chain longer than the volume has clusters, which means it points
    /// back into itself. Stopping is the only safe answer.
    ChainLoops,
    /// The buffer offered is smaller than the file.
    TooBig { size: u32 },
}

/// A FAT32 volume that has been read far enough to be used.
pub struct Volume<S> {
    sectors: S,
    boot: BootSector,
}

impl<S: Sectors> Volume<S> {
    /// Reads the boot sector and believes none of it (`BootSector::parse`).
    pub fn mount(mut sectors: S) -> Result<Self, VolumeError<S::Error>> {
        let mut first = [0u8; 512];
        sectors
            .read_sector(0, &mut first)
            .map_err(VolumeError::Device)?;
        let boot = BootSector::parse(&first).map_err(VolumeError::BootSector)?;
        Ok(Self { sectors, boot })
    }

    pub const fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// The sectors underneath, for whoever needs to look at the volume as
    /// bytes rather than as files — a test comparing the two tables, say.
    pub const fn sectors_mut(&mut self) -> &mut S {
        &mut self.sectors
    }

    /// What the table says follows `cluster`.
    pub fn next_cluster(&mut self, cluster: u32) -> Result<Entry, VolumeError<S::Error>> {
        if cluster < FIRST_DATA_CLUSTER || cluster >= FIRST_DATA_CLUSTER + self.boot.clusters {
            return Err(VolumeError::BadCluster { cluster });
        }
        // Four bytes per entry, so a sector holds 128 of them.
        let per_sector = u32::from(self.boot.bytes_per_sector) / 4;
        let sector = self.boot.first_fat_sector() + cluster / per_sector;
        let offset = (cluster % per_sector) as usize * 4;
        let mut bytes = [0u8; 512];
        self.sectors
            .read_sector(sector, &mut bytes)
            .map_err(VolumeError::Device)?;
        Ok(Entry::decode(u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ])))
    }

    /// Hands every entry of the root directory to `each`, stopping where
    /// the directory ends.
    ///
    /// `each` answers whether to go on, so that looking for one name does
    /// not mean reading the whole directory.
    pub fn read_root(
        &mut self,
        mut each: impl FnMut(DirectoryEntry) -> bool,
    ) -> Result<(), VolumeError<S::Error>> {
        let mut cluster = self.boot.root_cluster;
        let mut seen = 0;
        loop {
            let first = self
                .boot
                .sector_of_cluster(cluster)
                .ok_or(VolumeError::BadCluster { cluster })?;
            for offset in 0..u32::from(self.boot.sectors_per_cluster) {
                let mut bytes = [0u8; 512];
                self.sectors
                    .read_sector(first + offset, &mut bytes)
                    .map_err(VolumeError::Device)?;
                // A sector holds a whole number of entries, so there is
                // nothing left over to think about.
                let (slots, _) = bytes.as_chunks::<DIRECTORY_ENTRY_BYTES>();
                for slot in slots {
                    match decode_slot(slot) {
                        Slot::End => return Ok(()),
                        Slot::Skip => {}
                        Slot::Entry(entry) => {
                            if !each(entry) {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            // A directory is a chain like any other file.
            seen += 1;
            if seen > self.boot.clusters {
                return Err(VolumeError::ChainLoops);
            }
            match self.next_cluster(cluster)? {
                Entry::Next(next) => cluster = next,
                Entry::End => return Ok(()),
                entry => return Err(VolumeError::BrokenChain { cluster, entry }),
            }
        }
    }

    /// The entry for `name` in the root directory, if it is there.
    pub fn find(&mut self, name: &str) -> Result<Option<DirectoryEntry>, VolumeError<S::Error>> {
        let mut found = None;
        self.read_root(|entry| {
            if entry.is_named(name) {
                found = Some(entry);
                false
            } else {
                true
            }
        })?;
        Ok(found)
    }

    /// Reads a file's bytes into `into`, following its chain, and answers
    /// how many there were.
    ///
    /// Stops where the file's length says, not where its last cluster
    /// does: the bytes after the end of a file inside its last cluster are
    /// whatever was there before.
    pub fn read_file(
        &mut self,
        entry: &DirectoryEntry,
        into: &mut [u8],
    ) -> Result<usize, VolumeError<S::Error>> {
        let size = entry.size as usize;
        if size > into.len() {
            return Err(VolumeError::TooBig { size: entry.size });
        }
        let mut cluster = entry.first_cluster;
        let mut written = 0;
        let mut seen = 0;
        // A file of no bytes owns no cluster, and its entry says so with a
        // first cluster of zero. Nothing special is needed for it: there
        // is nothing to read, so this loop does not run and that zero is
        // never followed into the reserved entries of the table. An early
        // return here would be a branch no test could ever take.
        while written < size {
            let first = self
                .boot
                .sector_of_cluster(cluster)
                .ok_or(VolumeError::BadCluster { cluster })?;
            for offset in 0..u32::from(self.boot.sectors_per_cluster) {
                if written == size {
                    break;
                }
                let mut bytes = [0u8; 512];
                self.sectors
                    .read_sector(first + offset, &mut bytes)
                    .map_err(VolumeError::Device)?;
                let taking = (size - written).min(bytes.len());
                into[written..written + taking].copy_from_slice(&bytes[..taking]);
                written += taking;
            }
            if written == size {
                break;
            }
            seen += 1;
            if seen > self.boot.clusters {
                return Err(VolumeError::ChainLoops);
            }
            match self.next_cluster(cluster)? {
                Entry::Next(next) => cluster = next,
                // The chain ended before the file did, which means the
                // directory and the table disagree about this file.
                entry => return Err(VolumeError::BrokenChain { cluster, entry }),
            }
        }
        Ok(written)
    }
}

// ---------------------------------------------------------------------
// Writing (docs/adr/0027-fase4-fat32-write.md)
// ---------------------------------------------------------------------

/// The most clusters one file may take. A limit, so that running out is an
/// error with a name rather than a loop that walks a whole volume.
pub const MAX_FILE_CLUSTERS: usize = 64;

/// What a name looks like in a directory entry: eight characters and
/// three, space-padded, upper case, with no dot between them.
///
/// `None` for a name that would not fit, because a name of the wrong
/// length does not get truncated into somebody else's file — it gets
/// refused.
pub fn encode_name(name: &str) -> Option<[u8; 11]> {
    let (base, extension) = match name.split_once('.') {
        Some((base, extension)) => (base, extension),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || extension.len() > 3 {
        return None;
    }
    let mut encoded = [b' '; 11];
    for (at, byte) in base.bytes().enumerate() {
        if !byte.is_ascii() || byte == b' ' {
            return None;
        }
        encoded[at] = byte.to_ascii_uppercase();
    }
    for (at, byte) in extension.bytes().enumerate() {
        if !byte.is_ascii() || byte == b' ' {
            return None;
        }
        encoded[8 + at] = byte.to_ascii_uppercase();
    }
    Some(encoded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError<E> {
    /// Everything reading can go wrong with.
    Reading(VolumeError<E>),
    /// The device could not write a sector.
    Device(E),
    /// A name that does not fit in eight and three.
    BadName,
    /// More clusters than one file may take here.
    TooManyClusters { needed: usize },
    /// The volume has not got that many free.
    Full { needed: usize, free: usize },
    /// The root directory has no slot left for another file.
    DirectoryFull,
}

impl<E> From<VolumeError<E>> for WriteError<E> {
    fn from(err: VolumeError<E>) -> Self {
        WriteError::Reading(err)
    }
}

impl<S: Sectors> Volume<S> {
    /// Writes one entry of the table, in **every** copy of it.
    ///
    /// A volume whose tables disagree is one a checker calls damaged, and
    /// keeping them the same costs one more sector written
    /// (ADR 0027, point 4).
    ///
    /// A cluster the volume has not got is refused. Every caller inside
    /// this module bounds its cluster before getting here, so the check
    /// would be unreachable if this were private — it is part of the
    /// surface instead, where it can be relied on and taken.
    pub fn set_next_cluster(
        &mut self,
        cluster: u32,
        entry: u32,
    ) -> Result<(), WriteError<S::Error>> {
        if cluster < FIRST_DATA_CLUSTER || cluster >= FIRST_DATA_CLUSTER + self.boot.clusters {
            return Err(VolumeError::BadCluster { cluster }.into());
        }
        let per_sector = u32::from(self.boot.bytes_per_sector) / 4;
        let offset = (cluster % per_sector) as usize * 4;
        for table in 0..u32::from(self.boot.fat_count) {
            let sector = self.boot.first_fat_sector()
                + table * self.boot.sectors_per_fat
                + cluster / per_sector;
            let mut bytes = [0u8; 512];
            self.sectors
                .read_sector(sector, &mut bytes)
                .map_err(VolumeError::Device)?;
            // The top four bits of an entry are reserved and belong to
            // whatever was there: only the low twenty-eight are ours.
            let kept = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) & !ENTRY_MASK;
            bytes[offset..offset + 4].copy_from_slice(&(kept | (entry & ENTRY_MASK)).to_le_bytes());
            self.sectors
                .write_sector(sector, &bytes)
                .map_err(WriteError::Device)?;
        }
        Ok(())
    }

    /// Finds `count` free clusters, from the start of the table.
    ///
    /// No bitmap and no memory of where it got to: a bitmap is state that
    /// has to be kept in step with the disk and rebuilt on every boot
    /// (ADR 0027, point 7). Nothing is written here — if there are not
    /// enough, nothing has been touched.
    fn free_clusters(
        &mut self,
        count: usize,
    ) -> Result<[u32; MAX_FILE_CLUSTERS], WriteError<S::Error>> {
        if count > MAX_FILE_CLUSTERS {
            return Err(WriteError::TooManyClusters { needed: count });
        }
        let mut found = [0u32; MAX_FILE_CLUSTERS];
        let mut at = 0;
        let mut cluster = FIRST_DATA_CLUSTER;
        while at < count && cluster < FIRST_DATA_CLUSTER + self.boot.clusters {
            if self.next_cluster(cluster)? == Entry::Free {
                found[at] = cluster;
                at += 1;
            }
            cluster += 1;
        }
        if at < count {
            return Err(WriteError::Full {
                needed: count,
                free: at,
            });
        }
        Ok(found)
    }

    /// Writes `contents` into the clusters of `chain`, which must be as
    /// many as the contents need.
    fn write_clusters(
        &mut self,
        chain: &[u32],
        contents: &[u8],
    ) -> Result<(), WriteError<S::Error>> {
        let per_cluster = self.boot.cluster_bytes() as usize;
        for (index, cluster) in chain.iter().enumerate() {
            let first = self
                .boot
                .sector_of_cluster(*cluster)
                .ok_or(VolumeError::BadCluster { cluster: *cluster })?;
            for offset in 0..u32::from(self.boot.sectors_per_cluster) {
                let at = index * per_cluster + offset as usize * 512;
                if at >= contents.len() {
                    break;
                }
                let taking = (contents.len() - at).min(512);
                // The rest of the last sector is zeroed rather than left
                // as it was: what a file does not fill is nobody's, and
                // handing back what used to be somebody else's is how a
                // disk leaks.
                let mut bytes = [0u8; 512];
                bytes[..taking].copy_from_slice(&contents[at..at + taking]);
                self.sectors
                    .write_sector(first + offset, &bytes)
                    .map_err(WriteError::Device)?;
            }
        }
        Ok(())
    }

    /// Where the entry for `name` is in the root directory, or where a new
    /// one would go.
    ///
    /// Answers the sector and the offset in it, and whether something was
    /// already there — overwriting reuses the entry so that a file does
    /// not appear twice.
    fn directory_slot(
        &mut self,
        name: &[u8; 11],
    ) -> Result<(u32, usize, Option<DirectoryEntry>), WriteError<S::Error>> {
        let mut cluster = self.boot.root_cluster;
        let mut free = None;
        loop {
            let first = self
                .boot
                .sector_of_cluster(cluster)
                .ok_or(VolumeError::BadCluster { cluster })?;
            for offset in 0..u32::from(self.boot.sectors_per_cluster) {
                let sector = first + offset;
                let mut bytes = [0u8; 512];
                self.sectors
                    .read_sector(sector, &mut bytes)
                    .map_err(VolumeError::Device)?;
                let (slots, _) = bytes.as_chunks::<DIRECTORY_ENTRY_BYTES>();
                for (index, slot) in slots.iter().enumerate() {
                    let at = index * DIRECTORY_ENTRY_BYTES;
                    match decode_slot(slot) {
                        Slot::Entry(entry) if slot[..11] == name[..] => {
                            return Ok((sector, at, Some(entry)));
                        }
                        Slot::Entry(_) => {}
                        Slot::Skip if free.is_none() && slot[0] == ENTRY_DELETED => {
                            free = Some((sector, at));
                        }
                        Slot::Skip => {}
                        // The end of the directory: nothing after it has
                        // ever been used, so this is where a new entry
                        // goes if no deleted one came first.
                        Slot::End => {
                            return Ok((
                                free.unwrap_or((sector, at)).0,
                                free.unwrap_or((sector, at)).1,
                                None,
                            ));
                        }
                    }
                }
            }
            match self.next_cluster(cluster)? {
                Entry::Next(next) => cluster = next,
                _ => break,
            }
        }
        match free {
            Some((sector, at)) => Ok((sector, at, None)),
            None => Err(WriteError::DirectoryFull),
        }
    }

    /// Creates `name` in the root directory with `contents`, or replaces
    /// what is there.
    ///
    /// The order is the whole of this (ADR 0027, point 3): the data goes
    /// into clusters nothing names, then the chain, then — in one write of
    /// one sector — the directory entry that makes the file appear. A
    /// machine that stopped between any two of those steps leaves a volume
    /// another reader still understands; at worst some clusters are
    /// marked used and belong to nobody, which is a lost chain and a thing
    /// `fsck` knows how to say.
    pub fn write_file(
        &mut self,
        name: &str,
        contents: &[u8],
    ) -> Result<DirectoryEntry, WriteError<S::Error>> {
        let encoded = encode_name(name).ok_or(WriteError::BadName)?;
        let per_cluster = self.boot.cluster_bytes() as usize;
        let needed = contents.len().div_ceil(per_cluster);

        // Where it will go, before anything is written. A file half
        // written because the disk filled up is worse than no file.
        let (sector, offset, existing) = self.directory_slot(&encoded)?;
        let chain = self.free_clusters(needed)?;
        let chain = &chain[..needed];

        self.write_clusters(chain, contents)?;
        for (index, cluster) in chain.iter().enumerate() {
            let next = match chain.get(index + 1) {
                Some(next) => *next,
                None => END_OF_CHAIN,
            };
            self.set_next_cluster(*cluster, next)?;
        }

        // And now the entry, which is what makes it a file.
        let mut bytes = [0u8; 512];
        self.sectors
            .read_sector(sector, &mut bytes)
            .map_err(VolumeError::Device)?;
        let entry = &mut bytes[offset..offset + DIRECTORY_ENTRY_BYTES];
        entry.fill(0);
        entry[..11].copy_from_slice(&encoded);
        entry[11] = ATTR_ARCHIVE;
        let first = chain.first().copied().unwrap_or(0);
        entry[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(first as u16).to_le_bytes());
        entry[28..32].copy_from_slice(&(contents.len() as u32).to_le_bytes());
        self.sectors
            .write_sector(sector, &bytes)
            .map_err(WriteError::Device)?;

        // Whatever the old one used is only free once nothing points at
        // it any more, which is now.
        if let Some(existing) = existing
            && existing.first_cluster != 0
        {
            self.free_chain(existing.first_cluster)?;
        }

        let written = decode_slot(
            bytes[offset..offset + DIRECTORY_ENTRY_BYTES]
                .try_into()
                .expect("an entry's worth of bytes"),
        );
        match written {
            Slot::Entry(entry) => Ok(entry),
            _ => Err(WriteError::BadName),
        }
    }

    /// Marks a chain's clusters free, from `first` to its end.
    ///
    /// Stops at anything that is not another cluster: a chain that was
    /// already broken is not made worse by walking off it.
    fn free_chain(&mut self, first: u32) -> Result<(), WriteError<S::Error>> {
        let mut cluster = first;
        let mut seen = 0;
        loop {
            let next = self.next_cluster(cluster)?;
            self.set_next_cluster(cluster, 0)?;
            seen += 1;
            if seen > self.boot.clusters {
                return Err(VolumeError::ChainLoops.into());
            }
            match next {
                Entry::Next(next) => cluster = next,
                _ => return Ok(()),
            }
        }
    }
}

/// What a file is marked as. Nothing here makes directories.
const ATTR_ARCHIVE: u8 = 0x20;
/// What ends a chain. Only the low 28 bits are the number, so this is the
/// value a formatter writes; anything from `0x0FFFFFF8` up means the same.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot sector like the one `xtask` writes, which is also the one
    /// `fsck.vfat` accepts.
    fn good() -> [u8; 512] {
        let mut sector = [0u8; 512];
        sector[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        sector[11..13].copy_from_slice(&512u16.to_le_bytes());
        sector[13] = 1;
        sector[14..16].copy_from_slice(&32u16.to_le_bytes());
        sector[16] = 2;
        sector[22..24].copy_from_slice(&0u16.to_le_bytes());
        sector[32..36].copy_from_slice(&131_072u32.to_le_bytes());
        sector[36..40].copy_from_slice(&1009u32.to_le_bytes());
        sector[44..48].copy_from_slice(&2u32.to_le_bytes());
        sector[48..50].copy_from_slice(&1u16.to_le_bytes());
        sector[50..52].copy_from_slice(&6u16.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        sector
    }

    #[test]
    fn a_good_boot_sector_says_where_everything_is() {
        let boot = BootSector::parse(&good()).expect("a volume this kernel can read");
        assert_eq!(boot.bytes_per_sector, 512);
        assert_eq!(boot.sectors_per_cluster, 1);
        assert_eq!(boot.reserved_sectors, 32);
        assert_eq!(boot.fat_count, 2);
        assert_eq!(boot.total_sectors, 131_072);
        assert_eq!(boot.sectors_per_fat, 1009);
        assert_eq!(boot.root_cluster, 2);
        assert_eq!(boot.fs_info_sector, 1);
        assert_eq!(boot.backup_boot_sector, 6);
        assert_eq!(boot.clusters, 131_072 - 32 - 2 * 1009);
        assert_eq!(boot.cluster_bytes(), 512);
    }

    /// The arithmetic from a cluster to a sector: three multiplications
    /// and an addition, and a wrong one reads the wrong place in silence.
    #[test]
    fn a_cluster_is_where_the_boot_sector_says() {
        let boot = BootSector::parse(&good()).unwrap();
        assert_eq!(boot.first_fat_sector(), 32);
        assert_eq!(boot.first_data_sector(), 32 + 2 * 1009);
        assert_eq!(
            boot.sector_of_cluster(2),
            Some(boot.first_data_sector()),
            "cluster 2 is the first"
        );
        assert_eq!(
            boot.sector_of_cluster(3),
            Some(boot.first_data_sector() + 1)
        );

        // Clusters 0 and 1 name no data; they are the two reserved entries
        // of the table.
        assert_eq!(boot.sector_of_cluster(0), None);
        assert_eq!(boot.sector_of_cluster(1), None);
        // And neither does one past the end.
        assert_eq!(
            boot.sector_of_cluster(FIRST_DATA_CLUSTER + boot.clusters),
            None
        );
        assert!(
            boot.sector_of_cluster(FIRST_DATA_CLUSTER + boot.clusters - 1)
                .is_some()
        );
        assert_eq!(boot.sector_of_cluster(u32::MAX), None);
    }

    /// With more than one sector per cluster the multiplication matters,
    /// which is the case a volume of one sector per cluster cannot catch.
    #[test]
    fn bigger_clusters_are_further_apart() {
        let mut sector = good();
        sector[13] = 8;
        // Eight sectors to a cluster means a eighth as many clusters, so
        // the volume has to grow to stay FAT32 at all — which the parser
        // insisted on, correctly, when this test first tried it at the
        // same size as the one above.
        sector[32..36].copy_from_slice(&1_048_576u32.to_le_bytes());
        let boot = BootSector::parse(&sector).unwrap();
        assert_eq!(boot.cluster_bytes(), 4096);
        assert_eq!(boot.sector_of_cluster(2), Some(boot.first_data_sector()));
        assert_eq!(
            boot.sector_of_cluster(3),
            Some(boot.first_data_sector() + 8),
            "a cluster further on is eight sectors further on"
        );
        assert_eq!(
            boot.sector_of_cluster(10),
            Some(boot.first_data_sector() + 64)
        );
    }

    /// Every way a boot sector can be wrong that would become a bad
    /// address later. This is the whole point of parsing it rather than
    /// believing it.
    #[test]
    fn a_boot_sector_is_refused_rather_than_believed() {
        let mut no_signature = good();
        no_signature[511] = 0;
        assert_eq!(
            BootSector::parse(&no_signature),
            Err(BootSectorError::NoSignature { found: 0x0055 })
        );

        let mut odd_sector = good();
        odd_sector[11..13].copy_from_slice(&4096u16.to_le_bytes());
        assert_eq!(
            BootSector::parse(&odd_sector),
            Err(BootSectorError::SectorSize { found: 4096 })
        );

        // Zero would divide by zero when the clusters are counted.
        let mut no_cluster = good();
        no_cluster[13] = 0;
        assert_eq!(
            BootSector::parse(&no_cluster),
            Err(BootSectorError::SectorsPerCluster { found: 0 })
        );
        // And a cluster size that is not a power of two is not a cluster
        // size at all.
        let mut odd_cluster = good();
        odd_cluster[13] = 3;
        assert_eq!(
            BootSector::parse(&odd_cluster),
            Err(BootSectorError::SectorsPerCluster { found: 3 })
        );

        let mut no_reserved = good();
        no_reserved[14..16].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            BootSector::parse(&no_reserved),
            Err(BootSectorError::ReservedSectors { found: 0 })
        );

        for count in [0u8, 5] {
            let mut tables = good();
            tables[16] = count;
            assert_eq!(
                BootSector::parse(&tables),
                Err(BootSectorError::FatCount { found: count })
            );
        }

        // A FAT16 volume: the 16-bit table size is set, and this kernel
        // does not read it.
        let mut fat16 = good();
        fat16[22..24].copy_from_slice(&256u16.to_le_bytes());
        assert_eq!(
            BootSector::parse(&fat16),
            Err(BootSectorError::NotFat32 {
                sixteen_bit_fat_size: 256
            })
        );

        let mut no_table = good();
        no_table[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            BootSector::parse(&no_table),
            Err(BootSectorError::FatSize { found: 0 })
        );

        for root in [0u32, 1] {
            let mut bad_root = good();
            bad_root[44..48].copy_from_slice(&root.to_le_bytes());
            assert_eq!(
                BootSector::parse(&bad_root),
                Err(BootSectorError::RootCluster { found: root })
            );
        }

        // Tables bigger than the volume: there is no data region, and
        // believing it would subtract past zero.
        let mut swallowed = good();
        swallowed[36..40].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert_eq!(
            BootSector::parse(&swallowed),
            Err(BootSectorError::NoDataRegion)
        );

        // A volume too small to be FAT32, whatever it says it is.
        let mut small = good();
        small[32..36].copy_from_slice(&10_000u32.to_le_bytes());
        assert!(matches!(
            BootSector::parse(&small),
            Err(BootSectorError::TooFewClusters { .. })
        ));
    }

    /// Numbers from a disk are not to be trusted to stay inside a `u32`
    /// when multiplied. All of these are refused, and none of them panics.
    #[test]
    fn numbers_that_would_overflow_are_refused_not_wrapped() {
        let mut enormous = good();
        enormous[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        enormous[16] = 4;
        assert_eq!(
            BootSector::parse(&enormous),
            Err(BootSectorError::NoDataRegion),
            "four tables of four billion sectors do not fit in any volume"
        );

        let mut everything_max = good();
        for at in [32, 36] {
            everything_max[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        // Whatever this is, it is refused rather than wrapped into
        // something that looks reasonable.
        assert!(BootSector::parse(&everything_max).is_err());
    }

    #[test]
    fn an_entry_says_what_follows_its_cluster() {
        assert_eq!(Entry::decode(0), Entry::Free);
        assert_eq!(Entry::decode(1), Entry::Reserved);
        assert_eq!(Entry::decode(3), Entry::Next(3));
        assert_eq!(Entry::decode(0x0FFF_FFF7), Entry::Bad);
        assert_eq!(Entry::decode(0x0FFF_FFFF), Entry::End);
        // Everything from 0x0FFFFFF8 up ends a chain; only the last is
        // written, and a reader that checked for equality would walk off
        // a disk written by something that used another.
        assert_eq!(Entry::decode(0x0FFF_FFF8), Entry::End);
        assert_eq!(Entry::decode(0x0FFF_FFFE), Entry::End);

        // The top four bits are not part of the number. A disk that left
        // them set would otherwise look full of impossible clusters.
        assert_eq!(Entry::decode(0xF000_0003), Entry::Next(3));
        assert_eq!(Entry::decode(0xFFFF_FFFF), Entry::End);
        assert_eq!(Entry::decode(0xF000_0000), Entry::Free);
    }

    fn entry_bytes(name: &[u8; 11], attributes: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..11].copy_from_slice(name);
        bytes[11] = attributes;
        bytes[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        bytes[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        bytes[28..32].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    #[test]
    fn a_directory_entry_gets_its_dot_back() {
        let slot = decode_slot(&entry_bytes(b"HELLO   TXT", 0x20, 3, 27));
        let Slot::Entry(entry) = slot else {
            panic!("a file, not {slot:?}");
        };
        assert_eq!(entry.name(), "HELLO.TXT");
        assert_eq!(entry.first_cluster, 3);
        assert_eq!(entry.size, 27);
        assert!(!entry.is_directory());

        // A name with no extension gets no dot.
        let Slot::Entry(entry) = decode_slot(&entry_bytes(b"README     ", 0x20, 4, 1)) else {
            panic!("a file");
        };
        assert_eq!(entry.name(), "README");

        // And one that fills both halves.
        let Slot::Entry(entry) = decode_slot(&entry_bytes(b"LONGNAMETXT", 0x20, 5, 1)) else {
            panic!("a file");
        };
        assert_eq!(entry.name(), "LONGNAME.TXT");

        // A directory says so in its attributes.
        let Slot::Entry(entry) = decode_slot(&entry_bytes(b"SUBDIR     ", 0x10, 6, 0)) else {
            panic!("a directory");
        };
        assert!(entry.is_directory());

        // The cluster number's two halves are sixteen bits apart, with the
        // high one earlier in the entry.
        let Slot::Entry(entry) = decode_slot(&entry_bytes(b"BIG     BIN", 0x20, 0x1234_5678, 9))
        else {
            panic!("a file");
        };
        assert_eq!(entry.first_cluster, 0x1234_5678);
    }

    #[test]
    fn what_a_directory_slot_is_not() {
        // The end of the directory: nothing after it has ever been used.
        assert_eq!(decode_slot(&[0u8; 32]), Slot::End);

        // A deleted entry, which the ones after it may still follow.
        let mut deleted = entry_bytes(b"GONE    TXT", 0x20, 3, 1);
        deleted[0] = 0xE5;
        assert_eq!(decode_slot(&deleted), Slot::Skip);

        // A fragment of a long name, which this kernel does not read.
        assert_eq!(
            decode_slot(&entry_bytes(b"XXXXXXXXXXX", 0x0F, 0, 0)),
            Slot::Skip
        );

        // And the volume's label, which is not a file.
        assert_eq!(
            decode_slot(&entry_bytes(b"HARLAN     ", 0x08, 0, 0)),
            Slot::Skip
        );
    }

    #[test]
    fn a_name_is_matched_whatever_case_it_is_typed_in() {
        let Slot::Entry(entry) = decode_slot(&entry_bytes(b"HELLO   TXT", 0x20, 3, 1)) else {
            panic!("a file");
        };
        assert!(entry.is_named("HELLO.TXT"));
        assert!(entry.is_named("hello.txt"));
        assert!(entry.is_named("Hello.Txt"));
        assert!(!entry.is_named("HELLO.TX"), "not a prefix");
        assert!(!entry.is_named("HELLO.TXTX"), "and not a longer one");
        assert!(!entry.is_named("OTHER.TXT"));
    }
}
