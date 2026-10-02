//! Writing a FAT32 image.
//!
//! Enough of one for a kernel to read: a boot sector with its BPB, the
//! backup copy FAT32 keeps at sector 6, an FSInfo sector, two file
//! allocation tables, and a root directory with some files in it
//! (docs/adr/0025-fase4-fat32-read-only.md).
//!
//! What makes this worth trusting is not the tests below — a formatter and
//! a reader written by the same person pass each other's tests even when
//! both are wrong. It is `fsck.vfat`, which CI runs over the result: an
//! implementation written by other people that knows what a valid FAT32
//! image is.

use std::collections::BTreeMap;

pub const SECTOR_BYTES: usize = 512;
/// What FAT32 reserves before the first table. Thirty-two is what every
/// formatter uses: it leaves room for the boot sector, the FSInfo sector
/// and the backup copies of both.
const RESERVED_SECTORS: u32 = 32;
/// Two, so that a damaged one can be recovered from the other. Nothing
/// here recovers anything; the second table exists because the format says
/// so and because `fsck` checks that the two agree.
const FAT_COUNT: u32 = 2;
/// Where FAT32 keeps the copy of the boot sector.
const BACKUP_BOOT_SECTOR: u32 = 6;
/// The first cluster that can hold anything. Entries 0 and 1 of the table
/// are reserved, so data starts at 2 — which is also where the root
/// directory goes.
pub const FIRST_DATA_CLUSTER: u32 = 2;
/// What a FAT32 entry holds when a chain ends. Only the low 28 bits of an
/// entry are the cluster number; the top four are reserved.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;
/// And what the first entry holds: the media descriptor, extended.
const MEDIA_DESCRIPTOR: u8 = 0xF8;

/// A directory entry is thirty-two bytes.
const DIRECTORY_ENTRY_BYTES: usize = 32;
/// The attribute that marks an entry as the volume's label rather than a
/// file.
const ATTR_VOLUME_LABEL: u8 = 0x08;
const ATTR_ARCHIVE: u8 = 0x20;

/// A file to put on the image: an 8.3 name and its contents.
pub struct File {
    /// Exactly as it will appear in the directory: eight characters of
    /// name and three of extension, space-padded, upper case. Checked, not
    /// trusted, because a name of the wrong length would shift every field
    /// after it in the entry.
    pub name: &'static str,
    pub contents: Vec<u8>,
}

/// What the image turned out to be, for whoever wants to check it against
/// what the kernel says it read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub total_sectors: u32,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u32,
    pub fat_count: u32,
    pub sectors_per_fat: u32,
    pub root_cluster: u32,
    pub clusters: u32,
}

impl Geometry {
    /// The first sector of the data region — where cluster 2 begins.
    pub const fn first_data_sector(&self) -> u32 {
        self.reserved_sectors + self.fat_count * self.sectors_per_fat
    }

    /// Which sector a cluster starts at.
    pub const fn sector_of_cluster(&self, cluster: u32) -> u32 {
        self.first_data_sector() + (cluster - FIRST_DATA_CLUSTER) * self.sectors_per_cluster as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// Fewer than 65 525 clusters is FAT16 by definition, whatever the BPB
    /// says. An image like that is what a careless reader accepts and
    /// `fsck` rejects.
    TooSmallForFat32 { clusters: u32 },
    /// A name that is not eight characters and three would shift every
    /// field after it in its directory entry.
    BadName { name: &'static str },
    /// More files than the one cluster of root directory holds.
    RootDirectoryFull { entries: usize },
    /// The files do not fit in the data region.
    OutOfClusters { needed: u32, available: u32 },
}

/// Lays out a FAT32 volume of `total_sectors` sectors, with `files` in its
/// root directory, and answers the bytes of the whole image together with
/// what its geometry turned out to be.
///
/// Pure: the same arguments give the same bytes, so what it writes can be
/// checked field by field without a disk anywhere.
pub fn format(
    total_sectors: u32,
    sectors_per_cluster: u8,
    label: &str,
    files: &[File],
) -> Result<(Vec<u8>, Geometry), FormatError> {
    for file in files {
        if !is_eight_three(file.name) {
            return Err(FormatError::BadName { name: file.name });
        }
    }

    let sectors_per_fat = sectors_per_fat_for(total_sectors, sectors_per_cluster);
    let geometry = Geometry {
        total_sectors,
        sectors_per_cluster,
        reserved_sectors: RESERVED_SECTORS,
        fat_count: FAT_COUNT,
        sectors_per_fat,
        root_cluster: FIRST_DATA_CLUSTER,
        clusters: (total_sectors - RESERVED_SECTORS - FAT_COUNT * sectors_per_fat)
            / sectors_per_cluster as u32,
    };
    // The line between FAT16 and FAT32 is the cluster count, not the BPB.
    if geometry.clusters < 65_525 {
        return Err(FormatError::TooSmallForFat32 {
            clusters: geometry.clusters,
        });
    }

    let cluster_bytes = SECTOR_BYTES * sectors_per_cluster as usize;
    // One entry for the label and one per file; the root directory is one
    // cluster, which is all this kernel needs.
    let entries = files.len() + 1;
    if entries * DIRECTORY_ENTRY_BYTES > cluster_bytes {
        return Err(FormatError::RootDirectoryFull { entries });
    }

    // Where each file goes: the root directory holds cluster 2, and the
    // files follow it, each taking as many clusters as it needs.
    let mut chains: BTreeMap<u32, u32> = BTreeMap::new();
    chains.insert(FIRST_DATA_CLUSTER, END_OF_CHAIN);
    let mut next_free = FIRST_DATA_CLUSTER + 1;
    let mut placed = Vec::new();
    for file in files {
        let needed = file.contents.len().div_ceil(cluster_bytes) as u32;
        if needed == 0 {
            // A file of no bytes owns no cluster, and its entry says so
            // with a first cluster of zero. Giving it one anyway makes a
            // cluster that is allocated and belongs to nothing, which is
            // what a checker calls a lost chain — and it is what the first
            // version of this did, until 7-Zip refused to read the file.
            placed.push((file, 0));
            continue;
        }
        if next_free + needed > FIRST_DATA_CLUSTER + geometry.clusters {
            return Err(FormatError::OutOfClusters {
                needed,
                available: FIRST_DATA_CLUSTER + geometry.clusters - next_free,
            });
        }
        let first = next_free;
        for index in 0..needed {
            let cluster = first + index;
            let next = if index + 1 == needed {
                END_OF_CHAIN
            } else {
                cluster + 1
            };
            chains.insert(cluster, next);
        }
        placed.push((file, first));
        next_free += needed;
    }

    let mut image = vec![0u8; total_sectors as usize * SECTOR_BYTES];

    // The boot sector, and the copy FAT32 keeps at sector 6.
    let boot = boot_sector(&geometry, label);
    image[..SECTOR_BYTES].copy_from_slice(&boot);
    let backup = BACKUP_BOOT_SECTOR as usize * SECTOR_BYTES;
    image[backup..backup + SECTOR_BYTES].copy_from_slice(&boot);

    // The FSInfo sector, and its copy next to the backup boot sector.
    let info = fs_info_sector(geometry.clusters, next_free);
    image[SECTOR_BYTES..2 * SECTOR_BYTES].copy_from_slice(&info);
    let backup_info = backup + SECTOR_BYTES;
    image[backup_info..backup_info + SECTOR_BYTES].copy_from_slice(&info);

    // Both tables, with the same contents: `fsck` checks that they agree.
    for table in 0..FAT_COUNT {
        let at = (RESERVED_SECTORS + table * sectors_per_fat) as usize * SECTOR_BYTES;
        // Entry 0 is the media descriptor and entry 1 is a marker; neither
        // names a cluster.
        write_u32(&mut image, at, 0x0FFF_FF00 | u32::from(MEDIA_DESCRIPTOR));
        write_u32(&mut image, at + 4, END_OF_CHAIN);
        for (cluster, next) in &chains {
            write_u32(&mut image, at + *cluster as usize * 4, *next);
        }
    }

    // The root directory: the volume label first, as a formatter does,
    // then one entry per file.
    let root_at = geometry.sector_of_cluster(geometry.root_cluster) as usize * SECTOR_BYTES;
    let mut entry_at = root_at;
    image[entry_at..entry_at + 11].copy_from_slice(&padded_label(label));
    image[entry_at + 11] = ATTR_VOLUME_LABEL;
    entry_at += DIRECTORY_ENTRY_BYTES;
    for (file, first) in &placed {
        let entry = directory_entry(file, *first);
        image[entry_at..entry_at + DIRECTORY_ENTRY_BYTES].copy_from_slice(&entry);
        entry_at += DIRECTORY_ENTRY_BYTES;
    }

    // And the files themselves. The empty ones have nowhere to be, which
    // is the point of their first cluster being zero.
    for (file, first) in &placed {
        if *first == 0 {
            continue;
        }
        let at = geometry.sector_of_cluster(*first) as usize * SECTOR_BYTES;
        image[at..at + file.contents.len()].copy_from_slice(&file.contents);
    }

    Ok((image, geometry))
}

/// How many sectors each table needs: one 32-bit entry per cluster, plus
/// the two reserved ones.
///
/// The cluster count depends on the table size and the table size depends
/// on the cluster count. Chasing that as a fixed point — compute the size
/// from the clusters, recompute the clusters from the size — does not
/// settle: it **oscillates** between two sizes, each one entry short of
/// what the other implies, and stops on whichever the loop ran out on.
///
/// So it is asked as a question with a yes or no answer instead. A table
/// of `size` sectors is big enough when it holds an entry for every
/// cluster left once both tables are taken out; growing the table shrinks
/// the data, so once an answer is yes every larger one is too. That makes
/// the smallest size that works findable by halving the range.
fn sectors_per_fat_for(total_sectors: u32, sectors_per_cluster: u8) -> u32 {
    let entries_per_sector = (SECTOR_BYTES / 4) as u32;
    let fits = |size: u32| {
        let data = total_sectors
            .saturating_sub(RESERVED_SECTORS)
            .saturating_sub(FAT_COUNT * size);
        let clusters = data / sectors_per_cluster as u32;
        clusters + FIRST_DATA_CLUSTER <= size * entries_per_sector
    };
    let (mut smallest, mut largest) = (1, total_sectors.max(1));
    while smallest < largest {
        let middle = smallest + (largest - smallest) / 2;
        if fits(middle) {
            largest = middle;
        } else {
            smallest = middle + 1;
        }
    }
    smallest
}

/// Whether a name is exactly the eight and three a directory entry holds.
fn is_eight_three(name: &str) -> bool {
    let Some((base, extension)) = name.split_once('.') else {
        return false;
    };
    !base.is_empty()
        && base.len() <= 8
        && !extension.is_empty()
        && extension.len() <= 3
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'.')
}

/// A name as a directory entry holds it: eight characters then three, each
/// padded with spaces, and no dot — the dot is implied by the split.
fn padded_name(name: &str) -> [u8; 11] {
    let mut padded = [b' '; 11];
    let (base, extension) = name.split_once('.').unwrap_or((name, ""));
    padded[..base.len()].copy_from_slice(base.as_bytes());
    padded[8..8 + extension.len()].copy_from_slice(extension.as_bytes());
    padded
}

fn padded_label(label: &str) -> [u8; 11] {
    let mut padded = [b' '; 11];
    let taken = label.len().min(11);
    padded[..taken].copy_from_slice(&label.as_bytes()[..taken]);
    padded
}

fn directory_entry(file: &File, first_cluster: u32) -> [u8; DIRECTORY_ENTRY_BYTES] {
    let mut entry = [0u8; DIRECTORY_ENTRY_BYTES];
    entry[..11].copy_from_slice(&padded_name(file.name));
    entry[11] = ATTR_ARCHIVE;
    // The cluster number arrives in two halves, sixteen bits apart, with
    // the high half earlier in the entry than the low one.
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&(file.contents.len() as u32).to_le_bytes());
    entry
}

fn boot_sector(geometry: &Geometry, label: &str) -> [u8; SECTOR_BYTES] {
    let mut sector = [0u8; SECTOR_BYTES];
    // A jump instruction, which is what every reader looks at first and
    // what `fsck` checks before anything else.
    sector[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    sector[3..11].copy_from_slice(b"HARLAN  ");
    sector[11..13].copy_from_slice(&(SECTOR_BYTES as u16).to_le_bytes());
    sector[13] = geometry.sectors_per_cluster;
    sector[14..16].copy_from_slice(&(geometry.reserved_sectors as u16).to_le_bytes());
    sector[16] = geometry.fat_count as u8;
    // Root entries and the 16-bit sector count are zero on FAT32: the root
    // directory is a cluster chain and the count is in the 32-bit field.
    sector[17..19].copy_from_slice(&0u16.to_le_bytes());
    sector[19..21].copy_from_slice(&0u16.to_le_bytes());
    sector[21] = MEDIA_DESCRIPTOR;
    // The 16-bit FAT size is zero for the same reason.
    sector[22..24].copy_from_slice(&0u16.to_le_bytes());
    sector[24..26].copy_from_slice(&32u16.to_le_bytes());
    sector[26..28].copy_from_slice(&2u16.to_le_bytes());
    sector[28..32].copy_from_slice(&0u32.to_le_bytes());
    sector[32..36].copy_from_slice(&geometry.total_sectors.to_le_bytes());
    sector[36..40].copy_from_slice(&geometry.sectors_per_fat.to_le_bytes());
    sector[40..42].copy_from_slice(&0u16.to_le_bytes());
    sector[42..44].copy_from_slice(&0u16.to_le_bytes());
    sector[44..48].copy_from_slice(&geometry.root_cluster.to_le_bytes());
    sector[48..50].copy_from_slice(&1u16.to_le_bytes());
    sector[50..52].copy_from_slice(&(BACKUP_BOOT_SECTOR as u16).to_le_bytes());
    sector[64] = 0x80;
    sector[66] = 0x29;
    sector[67..71].copy_from_slice(&0x4841_524Cu32.to_le_bytes());
    sector[71..82].copy_from_slice(&padded_label(label));
    sector[82..90].copy_from_slice(b"FAT32   ");
    sector[510] = 0x55;
    sector[511] = 0xAA;
    sector
}

fn fs_info_sector(clusters: u32, next_free: u32) -> [u8; SECTOR_BYTES] {
    let mut sector = [0u8; SECTOR_BYTES];
    sector[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    sector[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    // How many clusters are free, and where to start looking. Both are
    // hints; a reader may ignore them and `fsck` checks them against the
    // tables.
    let free = clusters + FIRST_DATA_CLUSTER - next_free;
    sector[488..492].copy_from_slice(&free.to_le_bytes());
    sector[492..496].copy_from_slice(&next_free.to_le_bytes());
    sector[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    sector
}

fn write_u32(image: &mut [u8], at: usize, value: u32) {
    image[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Big enough to be FAT32 at all: the format needs more than 65 525
    /// clusters, and below that the specification says the volume is
    /// FAT16 whatever its BPB claims.
    const SECTORS: u32 = 128 * 1024;

    fn read_u16(image: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([image[at], image[at + 1]])
    }

    fn read_u32(image: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]])
    }

    fn sample() -> (Vec<u8>, Geometry) {
        format(
            SECTORS,
            1,
            "HARLAN",
            &[File {
                name: "HELLO.TXT",
                contents: b"hello from the disk\n".to_vec(),
            }],
        )
        .expect("a volume that fits")
    }

    #[test]
    fn the_boot_sector_says_what_the_volume_is() {
        let (image, geometry) = sample();
        assert_eq!(&image[0..3], &[0xEB, 0x58, 0x90], "the jump");
        assert_eq!(read_u16(&image, 11), 512, "bytes per sector");
        assert_eq!(image[13], 1, "sectors per cluster");
        assert_eq!(read_u16(&image, 14), 32, "reserved sectors");
        assert_eq!(image[16], 2, "two tables");
        assert_eq!(read_u16(&image, 17), 0, "no root entries: FAT32 has none");
        assert_eq!(read_u16(&image, 19), 0, "the 16-bit count is unused");
        assert_eq!(read_u16(&image, 22), 0, "and so is the 16-bit FAT size");
        assert_eq!(read_u32(&image, 32), SECTORS, "the 32-bit count is not");
        assert_eq!(read_u32(&image, 36), geometry.sectors_per_fat);
        assert_eq!(read_u32(&image, 44), 2, "the root directory's cluster");
        assert_eq!(read_u16(&image, 48), 1, "where FSInfo is");
        assert_eq!(read_u16(&image, 50), 6, "and the backup boot sector");
        assert_eq!(&image[82..90], b"FAT32   ");
        assert_eq!((image[510], image[511]), (0x55, 0xAA));
    }

    /// FAT32 keeps a copy of the boot sector at sector 6, and it is what
    /// the kernel checks its own reading against.
    #[test]
    fn the_backup_boot_sector_is_a_copy_of_the_first() {
        let (image, _) = sample();
        let backup = BACKUP_BOOT_SECTOR as usize * SECTOR_BYTES;
        assert_eq!(
            &image[..SECTOR_BYTES],
            &image[backup..backup + SECTOR_BYTES],
            "byte for byte"
        );
        // And the FSInfo sector has its copy next to it.
        assert_eq!(
            &image[SECTOR_BYTES..2 * SECTOR_BYTES],
            &image[backup + SECTOR_BYTES..backup + 2 * SECTOR_BYTES]
        );
    }

    #[test]
    fn both_tables_say_the_same_thing() {
        let (image, geometry) = sample();
        let first = RESERVED_SECTORS as usize * SECTOR_BYTES;
        let second = (RESERVED_SECTORS + geometry.sectors_per_fat) as usize * SECTOR_BYTES;
        let bytes = geometry.sectors_per_fat as usize * SECTOR_BYTES;
        assert_eq!(&image[first..first + bytes], &image[second..second + bytes]);

        // Entry 0 carries the media descriptor, entry 1 is a marker, and
        // the root directory's cluster ends its own chain.
        assert_eq!(read_u32(&image, first) & 0xFF, u32::from(MEDIA_DESCRIPTOR));
        assert_eq!(read_u32(&image, first + 4), END_OF_CHAIN);
        assert_eq!(read_u32(&image, first + 8), END_OF_CHAIN, "the root");
    }

    #[test]
    fn a_file_has_an_entry_that_points_at_its_bytes() {
        let (image, geometry) = sample();
        let root = geometry.sector_of_cluster(2) as usize * SECTOR_BYTES;
        // The volume label comes first, as a formatter writes it.
        assert_eq!(&image[root..root + 6], b"HARLAN");
        assert_eq!(image[root + 11], ATTR_VOLUME_LABEL);

        let entry = root + DIRECTORY_ENTRY_BYTES;
        assert_eq!(&image[entry..entry + 11], b"HELLO   TXT", "8.3, padded");
        assert_eq!(image[entry + 11], ATTR_ARCHIVE);
        assert_eq!(read_u32(&image, entry + 28), 20, "its length");

        // The cluster number arrives in two halves, the high one earlier
        // in the entry than the low one.
        let cluster =
            u32::from(read_u16(&image, entry + 20)) << 16 | u32::from(read_u16(&image, entry + 26));
        assert_eq!(cluster, 3, "the first cluster after the root directory");

        let at = geometry.sector_of_cluster(cluster) as usize * SECTOR_BYTES;
        assert_eq!(&image[at..at + 20], b"hello from the disk\n");
    }

    /// A file longer than one cluster is a chain, which is the thing a
    /// reader has to walk.
    #[test]
    fn a_long_file_is_a_chain_of_clusters() {
        let (image, geometry) = format(
            SECTORS,
            1,
            "HARLAN",
            &[File {
                name: "LONG.BIN",
                contents: vec![7; 3 * SECTOR_BYTES + 1],
            }],
        )
        .unwrap();
        let first = RESERVED_SECTORS as usize * SECTOR_BYTES;
        // Four clusters: three full and one with the odd byte.
        assert_eq!(read_u32(&image, first + 3 * 4), 4);
        assert_eq!(read_u32(&image, first + 4 * 4), 5);
        assert_eq!(read_u32(&image, first + 5 * 4), 6);
        assert_eq!(read_u32(&image, first + 6 * 4), END_OF_CHAIN);
        assert_eq!(read_u32(&image, first + 7 * 4), 0, "and nothing beyond");

        let entry = geometry.sector_of_cluster(2) as usize * SECTOR_BYTES + DIRECTORY_ENTRY_BYTES;
        assert_eq!(read_u32(&image, entry + 28), 3 * SECTOR_BYTES as u32 + 1);
    }

    /// Where a cluster is, which is the arithmetic a reader has to get
    /// right and the one with no symptom when it is wrong.
    #[test]
    fn a_cluster_is_where_the_geometry_says() {
        let (_, geometry) = sample();
        assert_eq!(
            geometry.first_data_sector(),
            RESERVED_SECTORS + 2 * geometry.sectors_per_fat
        );
        assert_eq!(
            geometry.sector_of_cluster(2),
            geometry.first_data_sector(),
            "cluster 2 is the first"
        );
        assert_eq!(
            geometry.sector_of_cluster(3),
            geometry.first_data_sector() + u32::from(geometry.sectors_per_cluster)
        );
    }

    /// The table has to be big enough for an entry per cluster, and the
    /// two depend on each other. Written before the code that settles it,
    /// and it caught the first attempt: a fixed-point loop that oscillated
    /// between two sizes and stopped on the short one.
    #[test]
    fn the_tables_are_big_enough_for_the_clusters_they_describe() {
        let (_, geometry) = sample();
        let entries = geometry.clusters + FIRST_DATA_CLUSTER;
        let held = geometry.sectors_per_fat * (SECTOR_BYTES / 4) as u32;
        assert!(held >= entries, "{held} entries for {entries} clusters");
        // And not wastefully big: within one sector's worth of entries of
        // what is needed, which is what "the smallest that fits" means
        // when sizes come in whole sectors.
        assert!(
            held - entries <= (SECTOR_BYTES / 4) as u32,
            "{held} for {entries}"
        );
    }

    #[test]
    fn what_will_not_be_formatted() {
        // Too small to be FAT32 at all, whatever its BPB would claim.
        assert!(matches!(
            format(2048, 1, "HARLAN", &[]),
            Err(FormatError::TooSmallForFat32 { .. })
        ));

        // A name that is not eight and three would shift every field
        // after it in its entry.
        for name in ["TOOLONGNAME.TXT", "NOEXTENSION", "lower.txt", "A.TOOLONG"] {
            assert!(
                matches!(
                    format(
                        SECTORS,
                        1,
                        "HARLAN",
                        &[File {
                            name,
                            contents: vec![]
                        }]
                    ),
                    Err(FormatError::BadName { .. })
                ),
                "{name} should be refused"
            );
        }

        // More files than one cluster of root directory holds. With one
        // sector per cluster that is fifteen, after the label's entry.
        let many: Vec<File> = (0..16)
            .map(|n| File {
                name: Box::leak(format!("FILE{n:04}.BIN").into_boxed_str()),
                contents: vec![],
            })
            .collect();
        assert!(matches!(
            format(SECTORS, 1, "HARLAN", &many),
            Err(FormatError::RootDirectoryFull { .. })
        ));
    }

    /// A file of no bytes owns **no cluster**, and its entry says so with
    /// a first cluster of zero.
    ///
    /// This test used to assert the opposite — that an empty file got a
    /// cluster anyway — and passed, because the formatter and the test had
    /// the same misunderstanding written into them. 7-Zip, which did not,
    /// refused to read the file. A cluster allocated and owned by nothing
    /// is what a checker calls a lost chain.
    #[test]
    fn an_empty_file_owns_no_cluster() {
        let (image, geometry) = format(
            SECTORS,
            1,
            "HARLAN",
            &[
                File {
                    name: "EMPTY.BIN",
                    contents: vec![],
                },
                File {
                    name: "AFTER.TXT",
                    contents: b"not empty".to_vec(),
                },
            ],
        )
        .unwrap();
        let root = geometry.sector_of_cluster(2) as usize * SECTOR_BYTES;
        let empty = root + DIRECTORY_ENTRY_BYTES;
        assert_eq!(read_u32(&image, empty + 28), 0, "no bytes");
        assert_eq!(read_u16(&image, empty + 26), 0, "and no cluster");
        assert_eq!(read_u16(&image, empty + 20), 0);

        // And it took none, so the file after it gets the first one going.
        let after = empty + DIRECTORY_ENTRY_BYTES;
        assert_eq!(read_u16(&image, after + 26), 3);
        let table = RESERVED_SECTORS as usize * SECTOR_BYTES;
        assert_eq!(read_u32(&image, table + 3 * 4), END_OF_CHAIN);
        assert_eq!(read_u32(&image, table + 4 * 4), 0, "nothing else is taken");
    }

    // -----------------------------------------------------------------
    // The round trip: what this writes, the kernel's own reader reads
    // -----------------------------------------------------------------

    /// The image in memory, handed out a sector at a time, which is all
    /// the reader asks of a disk.
    struct Image(Vec<u8>);

    impl Image {
        /// Where a sector is, or an error naming it: off the end of the
        /// disk is something to report, not to panic over.
        fn at(&self, sector: u32) -> Result<usize, u32> {
            let at = sector as usize * SECTOR_BYTES;
            if at + SECTOR_BYTES > self.0.len() {
                return Err(sector);
            }
            Ok(at)
        }
    }

    impl harlan_hal::fat::Sectors for Image {
        type Error = u32;

        fn read_sector(&mut self, sector: u32, into: &mut [u8; 512]) -> Result<(), u32> {
            let at = self.at(sector)?;
            into.copy_from_slice(&self.0[at..at + SECTOR_BYTES]);
            Ok(())
        }

        fn write_sector(&mut self, sector: u32, from: &[u8; 512]) -> Result<(), u32> {
            let at = self.at(sector)?;
            self.0[at..at + SECTOR_BYTES].copy_from_slice(from);
            Ok(())
        }
    }

    fn volume() -> harlan_hal::fat::Volume<Image> {
        let (image, _) = format(
            SECTORS,
            1,
            "HARLAN",
            &[
                File {
                    name: "HELLO.TXT",
                    contents: b"HARLAN reads its own disk.
"
                    .to_vec(),
                },
                File {
                    name: "LONG.BIN",
                    contents: (0..4 * 512 + 1).map(|n| (n % 251) as u8).collect(),
                },
                File {
                    name: "EMPTY.BIN",
                    contents: Vec::new(),
                },
            ],
        )
        .expect("a volume that fits");
        harlan_hal::fat::Volume::mount(Image(image)).expect("a volume the kernel can read")
    }

    /// The whole path, in a host test: boot sector, root directory, the
    /// table, and a file's bytes. Everything the kernel does with a disk
    /// except asking the disk for the sectors.
    #[test]
    fn the_kernels_own_reader_reads_what_this_writes() {
        let mut volume = volume();
        let boot = *volume.boot_sector();
        assert_eq!(boot.total_sectors, SECTORS);
        assert_eq!(boot.sectors_per_cluster, 1);
        assert_eq!(boot.root_cluster, 2);

        let mut names = Vec::new();
        volume
            .read_root(|entry| {
                names.push((entry.name().to_string(), entry.size, entry.first_cluster));
                true
            })
            .expect("a root directory");
        assert_eq!(
            names,
            vec![
                ("HELLO.TXT".to_string(), 27, 3),
                ("LONG.BIN".to_string(), 2049, 4),
                ("EMPTY.BIN".to_string(), 0, 0),
            ],
            "the label is not a file, and the three that are come back"
        );
    }

    /// A file that fits in one cluster, read through the directory.
    #[test]
    fn a_short_file_comes_back_byte_for_byte() {
        let mut volume = volume();
        let entry = volume
            .find("hello.txt")
            .expect("a readable volume")
            .expect("the file is there");
        let mut bytes = [0u8; 64];
        let read = volume.read_file(&entry, &mut bytes).expect("its contents");
        assert_eq!(read, 27);
        assert_eq!(
            &bytes[..read],
            b"HARLAN reads its own disk.
"
        );
        // And nothing past its length was written, even though the rest of
        // the cluster was read.
        assert!(bytes[read..].iter().all(|byte| *byte == 0));
    }

    /// A file of four clusters and a byte: the chain has to be walked, and
    /// the read has to stop where the length says rather than where the
    /// last cluster does.
    #[test]
    fn a_long_file_is_followed_through_its_chain() {
        let mut volume = volume();
        let entry = volume.find("LONG.BIN").unwrap().expect("the file is there");
        assert_eq!(entry.size, 4 * 512 + 1);

        let mut bytes = vec![0u8; 8192];
        let read = volume.read_file(&entry, &mut bytes).expect("its contents");
        assert_eq!(read, 2049);
        let expected: Vec<u8> = (0..2049).map(|n| (n % 251) as u8).collect();
        assert_eq!(&bytes[..read], &expected[..], "every byte, in order");
        assert!(bytes[read..].iter().all(|byte| *byte == 0), "and no more");

        // The chain really is five clusters long, ending where it should.
        let mut cluster = entry.first_cluster;
        let mut walked = 1;
        while let harlan_hal::fat::Entry::Next(next) = volume.next_cluster(cluster).unwrap() {
            cluster = next;
            walked += 1;
        }
        assert_eq!(walked, 5, "four full clusters and one for the odd byte");
        assert_eq!(
            volume.next_cluster(cluster).unwrap(),
            harlan_hal::fat::Entry::End
        );
    }

    /// A file of no bytes owns no cluster, so reading it reads nothing —
    /// and must not follow its first cluster of zero into the reserved
    /// entries of the table.
    #[test]
    fn an_empty_file_reads_as_nothing() {
        let mut volume = volume();
        let entry = volume
            .find("EMPTY.BIN")
            .unwrap()
            .expect("the file is there");
        assert_eq!(entry.first_cluster, 0);
        let mut bytes = [0xAAu8; 16];
        assert_eq!(volume.read_file(&entry, &mut bytes), Ok(0));
        assert_eq!(bytes, [0xAA; 16], "nothing was written");
    }

    /// A chain that ends before its file does. A reader that followed a
    /// chain without looking at what each entry says would stop quietly
    /// and report a file it had not read.
    #[test]
    fn a_chain_that_ends_early_is_not_a_file_that_is_all_there() {
        let (mut image, geometry) = format(
            SECTORS,
            1,
            "HARLAN",
            &[File {
                name: "LONG.BIN",
                contents: vec![7; 4 * SECTOR_BYTES],
            }],
        )
        .unwrap();
        // Free the second cluster of its chain, in both tables, so that
        // the chain ends where the file does not.
        for table in 0..FAT_COUNT {
            let at = (RESERVED_SECTORS + table * geometry.sectors_per_fat) as usize * SECTOR_BYTES;
            write_u32(&mut image, at + 4 * 4, 0);
        }

        let mut volume =
            harlan_hal::fat::Volume::mount(Image(image)).expect("the volume still mounts");
        let entry = volume.find("LONG.BIN").unwrap().expect("the file is there");
        let mut bytes = vec![0u8; 8192];
        assert_eq!(
            volume.read_file(&entry, &mut bytes),
            Err(harlan_hal::fat::VolumeError::BrokenChain {
                cluster: 4,
                entry: harlan_hal::fat::Entry::Free
            }),
            "it says where the chain broke and what it found"
        );
    }

    #[test]
    fn what_the_reader_refuses() {
        let mut volume = volume();
        assert_eq!(volume.find("NOSUCH.TXT").unwrap(), None);

        // A buffer smaller than the file is refused rather than filled
        // with as much as fits.
        let entry = volume.find("LONG.BIN").unwrap().unwrap();
        let mut small = [0u8; 100];
        assert_eq!(
            volume.read_file(&entry, &mut small),
            Err(harlan_hal::fat::VolumeError::TooBig { size: 2049 })
        );
        assert_eq!(small, [0u8; 100], "and nothing was written");

        // Clusters 0 and 1 are the table's own entries and name no data.
        for cluster in [0, 1] {
            assert_eq!(
                volume.next_cluster(cluster),
                Err(harlan_hal::fat::VolumeError::BadCluster { cluster })
            );
        }
    }

    // -----------------------------------------------------------------
    // Writing: what this reads back, and what another tool makes of it
    // -----------------------------------------------------------------

    #[test]
    fn a_file_written_is_a_file_read_back() {
        let mut volume = volume();
        let written = volume
            .write_file("NEW.TXT", b"written by the kernel\n")
            .expect("a file this volume has room for");
        assert_eq!(written.name(), "NEW.TXT");
        assert_eq!(written.size, 22);
        assert_ne!(written.first_cluster, 0);

        // And it is there when asked for by name, like any other.
        let found = volume.find("new.txt").unwrap().expect("it is in the root");
        assert_eq!(found, written);
        let mut bytes = [0u8; 64];
        let read = volume.read_file(&found, &mut bytes).expect("its contents");
        assert_eq!(&bytes[..read], b"written by the kernel\n");

        // The files that were already there are still there.
        let mut names = Vec::new();
        volume
            .read_root(|entry| {
                names.push(entry.name().to_string());
                true
            })
            .unwrap();
        assert_eq!(
            names,
            vec![
                "HELLO.TXT".to_string(),
                "LONG.BIN".to_string(),
                "EMPTY.BIN".to_string(),
                "NEW.TXT".to_string()
            ]
        );
    }

    /// A file of several clusters: the chain has to be written as well as
    /// the bytes, and read back the same way.
    #[test]
    fn a_long_file_written_keeps_its_chain() {
        let mut volume = volume();
        let contents: Vec<u8> = (0..3 * 512 + 7).map(|n| (n % 253) as u8).collect();
        let written = volume.write_file("BIG.BIN", &contents).unwrap();
        assert_eq!(written.size, contents.len() as u32);

        let mut bytes = vec![0u8; 8192];
        let read = volume.read_file(&written, &mut bytes).unwrap();
        assert_eq!(&bytes[..read], &contents[..]);

        // Four clusters, the last one ending the chain.
        let mut cluster = written.first_cluster;
        let mut walked = 1;
        while let harlan_hal::fat::Entry::Next(next) = volume.next_cluster(cluster).unwrap() {
            cluster = next;
            walked += 1;
        }
        assert_eq!(walked, 4);
        assert_eq!(
            volume.next_cluster(cluster).unwrap(),
            harlan_hal::fat::Entry::End
        );
    }

    /// Writing over a file keeps one entry, not two, and gives back what
    /// the old contents were using.
    #[test]
    fn writing_over_a_file_replaces_it() {
        let mut volume = volume();
        let first = volume.write_file("SAME.TXT", &[1u8; 2000]).unwrap();
        let old_cluster = first.first_cluster;
        assert_eq!(first.size, 2000);

        let second = volume.write_file("SAME.TXT", b"shorter").unwrap();
        assert_eq!(second.size, 7);

        let mut seen = 0;
        volume
            .read_root(|entry| {
                if entry.is_named("SAME.TXT") {
                    seen += 1;
                }
                true
            })
            .unwrap();
        assert_eq!(seen, 1, "one file called that, not two");

        let mut bytes = [0u8; 32];
        let read = volume.read_file(&second, &mut bytes).unwrap();
        assert_eq!(&bytes[..read], b"shorter");

        // The clusters the long version used are free again. The first of
        // them may well have been taken back by the short one, so this
        // looks at the fourth, which nothing needs now.
        assert_eq!(
            volume.next_cluster(old_cluster + 3).unwrap(),
            harlan_hal::fat::Entry::Free
        );
    }

    #[test]
    fn what_will_not_be_written() {
        let mut volume = volume();
        for name in ["TOOLONGNAME.TXT", "A.TOOLONG", ""] {
            assert_eq!(
                volume.write_file(name, b"x"),
                Err(harlan_hal::fat::WriteError::BadName),
                "{name}"
            );
        }

        let huge = vec![0u8; (harlan_hal::fat::MAX_FILE_CLUSTERS + 1) * 512];
        assert!(matches!(
            volume.write_file("HUGE.BIN", &huge),
            Err(harlan_hal::fat::WriteError::TooManyClusters { .. })
        ));
    }

    /// A file of no bytes owns no cluster, the same when written as when
    /// formatted — which is the thing 7-Zip caught the formatter getting
    /// wrong.
    #[test]
    fn a_file_of_no_bytes_written_takes_no_cluster() {
        let mut volume = volume();
        let written = volume.write_file("NOTHING.BIN", &[]).unwrap();
        assert_eq!(written.size, 0);
        assert_eq!(written.first_cluster, 0);
        let mut bytes = [0xAAu8; 8];
        assert_eq!(volume.read_file(&written, &mut bytes), Ok(0));
        assert_eq!(bytes, [0xAA; 8]);
    }

    /// Both tables are written, because a volume whose tables disagree is
    /// one every other system calls damaged.
    #[test]
    fn both_tables_are_kept_in_step_when_writing() {
        let (image, geometry) = format(
            SECTORS,
            1,
            "HARLAN",
            &[File {
                name: "ONE.TXT",
                contents: b"one".to_vec(),
            }],
        )
        .unwrap();
        let mut volume = harlan_hal::fat::Volume::mount(Image(image)).unwrap();
        volume.write_file("TWO.TXT", &[9u8; 1500]).unwrap();

        let mut first = [0u8; 512];
        let mut second = [0u8; 512];
        for sector in 0..geometry.sectors_per_fat {
            harlan_hal::fat::Sectors::read_sector(
                volume.sectors_mut(),
                RESERVED_SECTORS + sector,
                &mut first,
            )
            .unwrap();
            harlan_hal::fat::Sectors::read_sector(
                volume.sectors_mut(),
                RESERVED_SECTORS + geometry.sectors_per_fat + sector,
                &mut second,
            )
            .unwrap();
            assert_eq!(first, second, "the two tables disagree at sector {sector}");
        }
    }

    /// A name typed in lower case is stored upper case, because that is
    /// how a short name is stored and how every other reader will look
    /// for it.
    #[test]
    fn a_name_is_stored_the_way_the_format_stores_it() {
        let mut volume = volume();
        let written = volume.write_file("lower.txt", b"x").unwrap();
        assert_eq!(written.name(), "LOWER.TXT");

        // And it is found whichever way it is asked for.
        assert!(volume.find("LOWER.TXT").unwrap().is_some());
        assert!(volume.find("lower.txt").unwrap().is_some());

        // The bytes in the entry are upper case too, not only what the
        // reader hands back.
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        let (slots, _) = bytes.as_chunks::<32>();
        assert!(
            slots.iter().any(|slot| &slot[..11] == b"LOWER   TXT"),
            "the entry holds the name in upper case"
        );
    }

    /// The top four bits of a table entry are not part of the cluster
    /// number and belong to whatever put them there. Writing an entry
    /// keeps them.
    #[test]
    fn writing_an_entry_keeps_the_bits_that_are_not_its_own() {
        let mut volume = volume();
        // Reach into the table and set the reserved bits of an entry that
        // is free, then make a file that will take that cluster.
        let table = volume.boot_sector().first_fat_sector();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), table, &mut bytes).unwrap();
        // Cluster 20 is past everything the volume was formatted with.
        let at = 20 * 4;
        bytes[at..at + 4].copy_from_slice(&0xF000_0000u32.to_le_bytes());
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), table, &bytes).unwrap();
        // It still reads as free: the reserved bits are not the number.
        assert_eq!(
            volume.next_cluster(20).unwrap(),
            harlan_hal::fat::Entry::Free
        );

        volume.set_next_cluster(20, 21).unwrap();
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), table, &mut bytes).unwrap();
        let entry = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        assert_eq!(
            entry, 0xF000_0015,
            "the number changed and the rest did not"
        );
    }

    /// A cluster the volume has not got is refused rather than written
    /// somewhere else in the table.
    #[test]
    fn an_entry_outside_the_volume_is_not_written() {
        let mut volume = volume();
        let past = harlan_hal::fat::FIRST_DATA_CLUSTER + volume.boot_sector().clusters;
        for cluster in [0, 1, past, u32::MAX] {
            assert!(
                matches!(
                    volume.set_next_cluster(cluster, 3),
                    Err(harlan_hal::fat::WriteError::Reading(
                        harlan_hal::fat::VolumeError::BadCluster { .. }
                    ))
                ),
                "cluster {cluster}"
            );
        }
    }

    /// A volume with nothing free writes nothing at all: a file half
    /// written because the disk filled up is worse than a file that is
    /// not there.
    #[test]
    fn a_full_volume_is_left_as_it_was() {
        let (mut image, geometry) = format(
            SECTORS,
            1,
            "HARLAN",
            &[File {
                name: "ONE.TXT",
                contents: b"one".to_vec(),
            }],
        )
        .unwrap();
        // Mark every cluster used, in both tables, by hand.
        for table in 0..FAT_COUNT {
            let first =
                (RESERVED_SECTORS + table * geometry.sectors_per_fat) as usize * SECTOR_BYTES;
            let bytes = geometry.sectors_per_fat as usize * SECTOR_BYTES;
            let (entries, _) = image[first..first + bytes].as_chunks_mut::<4>();
            for entry in entries {
                *entry = 0x0FFF_FFFFu32.to_le_bytes();
            }
        }
        let mut volume = harlan_hal::fat::Volume::mount(Image(image)).unwrap();

        let before = root_bytes(&mut volume);
        assert!(matches!(
            volume.write_file("NEW.TXT", b"anything"),
            Err(harlan_hal::fat::WriteError::Full { .. })
        ));
        assert_eq!(
            root_bytes(&mut volume),
            before,
            "the directory was not touched"
        );
    }

    fn root_bytes(volume: &mut harlan_hal::fat::Volume<Image>) -> [u8; 512] {
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        bytes
    }

    /// What a file does not fill is zeroed, not left as it was. Handing
    /// back what used to be somebody else's file is how a disk leaks.
    #[test]
    fn the_rest_of_a_files_last_sector_is_not_somebody_elses() {
        let mut volume = volume();
        // Fill the cluster the next write will take with somebody else's
        // bytes. Writing reserves before it frees, so a new file lands on
        // the first free cluster rather than on the one it replaces —
        // this is how a cluster with something in it gets reused.
        let next_free = (harlan_hal::fat::FIRST_DATA_CLUSTER..)
            .find(|cluster| volume.next_cluster(*cluster) == Ok(harlan_hal::fat::Entry::Free))
            .expect("this volume has a free cluster");
        let sector = volume.boot_sector().sector_of_cluster(next_free).unwrap();
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), sector, &[0xAB; 512]).unwrap();

        let short = volume.write_file("LEAK.BIN", b"short").unwrap();
        assert_eq!(short.first_cluster, next_free, "it took that one");

        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), sector, &mut bytes).unwrap();
        assert_eq!(&bytes[..5], b"short");
        assert!(
            bytes[5..].iter().all(|byte| *byte == 0),
            "what was there before is gone, not handed on"
        );
    }

    /// An entry written over an old one is written whole: the fields this
    /// kernel does not use — the times, the reserved byte — are cleared
    /// rather than left saying something about a file that is gone.
    #[test]
    fn an_entry_written_over_another_keeps_nothing_of_it() {
        let mut volume = volume();
        // Put junk in the fields nothing here writes.
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        // The first entry is the volume's label, so HELLO.TXT is the
        // second. Bytes 12 to 20 of it are the reserved byte and the
        // creation time, which nothing here writes.
        const HELLO: usize = 32;
        bytes[HELLO + 12..HELLO + 20].fill(0xEE);
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), root, &bytes).unwrap();

        volume.write_file("HELLO.TXT", b"again").unwrap();
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        assert_eq!(&bytes[HELLO..HELLO + 11], b"HELLO   TXT", "the same entry");
        assert!(
            bytes[HELLO + 12..HELLO + 20].iter().all(|byte| *byte == 0),
            "what the old entry said is gone"
        );
    }

    // -----------------------------------------------------------------
    // What a review of this code turned up, each measured before it was
    // believed and each left here as the test it should have had
    // -----------------------------------------------------------------

    /// Some tools store a short name in lower case. Looking a name up
    /// and looking for its slot have to agree about that, or a file that
    /// is already there gets a second entry.
    #[test]
    fn a_lower_case_stored_name_is_the_same_file() {
        let mut volume = volume();
        // Store a name the way some tools do, in lower case.
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        bytes[32..43].copy_from_slice(b"hello   txt");
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), root, &bytes).unwrap();

        volume.write_file("HELLO.TXT", b"replaced").unwrap();
        let mut seen = 0;
        volume
            .read_root(|entry| {
                if entry.is_named("HELLO.TXT") {
                    seen += 1;
                }
                true
            })
            .unwrap();
        assert_eq!(seen, 1, "one file called that, not two");
    }

    /// A directory is not a file to be written over: doing it frees its
    /// chain and orphans everything inside.
    #[test]
    fn a_directory_is_not_written_over_as_a_file() {
        let mut volume = volume();
        // Turn EMPTY.BIN's entry into a directory with a cluster.
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), root, &mut bytes).unwrap();
        const THIRD: usize = 3 * 32;
        bytes[THIRD..THIRD + 11].copy_from_slice(b"SUBDIR     ");
        bytes[THIRD + 11] = 0x10;
        bytes[THIRD + 26..THIRD + 28].copy_from_slice(&9u16.to_le_bytes());
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), root, &bytes).unwrap();

        let written = volume.write_file("SUBDIR", b"not a directory");
        assert!(
            written.is_err(),
            "a directory is not a file to be written over: {written:?}"
        );
    }

    /// A root directory with no room left in its first cluster and a
    /// chain that points back at itself. The walk has to follow the
    /// chain, and following it for ever is the kernel never coming back.
    ///
    /// The first version of this test left a free slot in the first
    /// cluster, so the walk stopped there and never followed anything —
    /// it was asserting against a path it did not take.
    #[test]
    fn a_looping_root_chain_is_refused_rather_than_walked_for_ever() {
        let mut volume = volume();
        // Fill every slot of the root's one cluster, so that a write has
        // to look further.
        let root = volume.boot_sector().sector_of_cluster(2).unwrap();
        let mut bytes = [0u8; 512];
        for slot in 0..512 / 32 {
            let at = slot * 32;
            bytes[at..at + 11].copy_from_slice(b"FULL    BIN");
            bytes[at + 11] = 0x20;
            bytes[at + 26..at + 28].copy_from_slice(&3u16.to_le_bytes());
        }
        harlan_hal::fat::Sectors::write_sector(volume.sectors_mut(), root, &bytes).unwrap();
        volume.set_next_cluster(2, 2).unwrap();

        assert_eq!(
            volume.write_file("NEW.TXT", b"x"),
            Err(harlan_hal::fat::WriteError::Reading(
                harlan_hal::fat::VolumeError::ChainLoops
            )),
            "a root that points at itself is a volume to refuse"
        );
    }

    /// A write interrupted between its directory entry and freeing the
    /// old chain leaves clusters marked used that nothing names. The next
    /// write to that name takes them — and must not then free them as if
    /// they were still the old file's.
    #[test]
    fn freeing_an_old_chain_never_takes_the_new_file_with_it() {
        let mut volume = volume();
        let first = volume.write_file("SAME.TXT", &[1u8; 1500]).unwrap();
        // The state an interrupted overwrite leaves: the directory has
        // been written and the old chain not yet freed. Simulate the
        // other half — free the old chain's head by hand — and then write
        // over the file again.
        volume.set_next_cluster(first.first_cluster, 0).unwrap();

        let second = volume.write_file("SAME.TXT", b"short").unwrap();
        let mut bytes = [0u8; 32];
        let read = volume.read_file(&second, &mut bytes).unwrap();
        assert_eq!(&bytes[..read], b"short");
        assert_ne!(
            volume.next_cluster(second.first_cluster).unwrap(),
            harlan_hal::fat::Entry::Free,
            "the new file's own first cluster was freed under it"
        );
    }

    /// A loadable segment that occupies nothing never reaches the
    /// loader, which would take one from its end to find its last page.
    #[test]
    fn a_segment_of_no_memory_never_reaches_the_loader() {
        // Built here rather than in the loader, which needs page tables.
        let mut file = vec![0u8; 0x2000];
        file[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        file[4] = 2;
        file[5] = 1;
        file[6] = 1;
        file[16..18].copy_from_slice(&2u16.to_le_bytes());
        file[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        file[24..32].copy_from_slice(&0x40_0000u64.to_le_bytes());
        file[32..40].copy_from_slice(&64u64.to_le_bytes());
        file[54..56].copy_from_slice(&56u16.to_le_bytes());
        file[56..58].copy_from_slice(&2u16.to_le_bytes());
        // A real segment, so the entry point is inside something.
        let one = 64;
        file[one..one + 4].copy_from_slice(&1u32.to_le_bytes());
        file[one + 4..one + 8].copy_from_slice(&5u32.to_le_bytes());
        file[one + 8..one + 16].copy_from_slice(&0x1000u64.to_le_bytes());
        file[one + 16..one + 24].copy_from_slice(&0x40_0000u64.to_le_bytes());
        file[one + 32..one + 40].copy_from_slice(&0x100u64.to_le_bytes());
        file[one + 40..one + 48].copy_from_slice(&0x100u64.to_le_bytes());
        // And one of no size at all, at address zero.
        let two = 64 + 56;
        file[two..two + 4].copy_from_slice(&1u32.to_le_bytes());
        file[two + 4..two + 8].copy_from_slice(&4u32.to_le_bytes());

        assert_eq!(
            harlan_hal::elf::parse(&file, 0x0000_8000_0000_0000),
            Err(harlan_hal::elf::ElfError::SegmentOfNothing { at: 0 }),
            "refused here, so the loader is never asked to find its last page"
        );
    }

    /// What FSInfo ends up saying, against the same count made one entry
    /// at a time.
    ///
    /// `update_fs_info` reads the table a sector at a time and turns a
    /// position inside the sector back into a cluster number. That is the
    /// arithmetic that is off by one and still looks plausible, so it is
    /// pinned to `next_cluster`, which reads a single entry and has
    /// nowhere to be wrong.
    #[test]
    fn fs_info_says_what_counting_one_entry_at_a_time_says() {
        let mut volume = volume();
        // Write something, so FSInfo is rewritten rather than left as the
        // formatter wrote it.
        volume.write_file("COUNT.BIN", &[7u8; 3000]).unwrap();

        let clusters = volume.boot_sector().clusters;
        let mut free = 0u32;
        let mut first_free = 0u32;
        for cluster in
            harlan_hal::fat::FIRST_DATA_CLUSTER..harlan_hal::fat::FIRST_DATA_CLUSTER + clusters
        {
            if volume.next_cluster(cluster).unwrap() == harlan_hal::fat::Entry::Free {
                free += 1;
                if first_free == 0 {
                    first_free = cluster;
                }
            }
        }

        let at = u32::from(volume.boot_sector().fs_info_sector);
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), at, &mut bytes).unwrap();
        let said_free = u32::from_le_bytes([bytes[488], bytes[489], bytes[490], bytes[491]]);
        let said_next = u32::from_le_bytes([bytes[492], bytes[493], bytes[494], bytes[495]]);
        assert_eq!(said_free, free, "how many are free");
        assert_eq!(said_next, first_free, "which one is the first free");
        // And the hint has to be worth something: not every cluster, and
        // not none of them.
        assert!(free > 0 && free < clusters);
    }

    /// The sector walk must not count the two reserved entries, nor any
    /// entry past the last cluster, however many of those share the last
    /// sector of the table.
    #[test]
    fn the_sector_walk_counts_only_real_clusters() {
        let mut volume = volume();
        volume.write_file("EDGE.BIN", b"edge").unwrap();

        let clusters = volume.boot_sector().clusters;
        let at = u32::from(volume.boot_sector().fs_info_sector);
        let mut bytes = [0u8; 512];
        harlan_hal::fat::Sectors::read_sector(volume.sectors_mut(), at, &mut bytes).unwrap();
        let said_free = u32::from_le_bytes([bytes[488], bytes[489], bytes[490], bytes[491]]);

        // Entries 0 and 1 are the signature and the dirty flag, never
        // free, and the entries after the last cluster are padding. If
        // either were counted the total would be above the cluster count.
        assert!(
            said_free <= clusters,
            "{said_free} free of {clusters} clusters: something outside the volume was counted"
        );
        // And the first free one is never a reserved entry.
        let said_next = u32::from_le_bytes([bytes[492], bytes[493], bytes[494], bytes[495]]);
        assert!(said_next >= harlan_hal::fat::FIRST_DATA_CLUSTER);
    }

    /// Reading in pieces gives exactly what reading whole gives.
    ///
    /// This is the property that covers every piece of `read_at`'s
    /// arithmetic at once: which cluster a byte is in, which sector of
    /// that cluster, and which byte of that sector. An off-by-one in any
    /// of the three shows up here as bytes in the wrong place.
    ///
    /// The sizes are chosen to straddle the boundaries that exist. On this
    /// volume a cluster is one sector of 512 bytes, so a sector boundary
    /// and a cluster boundary are the same boundary and a file of 5000
    /// bytes crosses it nine times, ending part of the way into the tenth.
    /// Pieces of 1, 3, 511, 512, 513, 1024 and 4096 bytes each start and
    /// end somewhere different with respect to it.
    #[test]
    fn reading_in_pieces_is_reading_whole() {
        let mut volume = volume();
        // Not a repeating pattern: a byte in the wrong place has to be
        // visible as a wrong value, not hidden by its neighbours.
        let contents: Vec<u8> = (0..5000u32)
            .map(|at| (at.wrapping_mul(31).wrapping_add(7) % 251) as u8)
            .collect();
        let entry = volume.write_file("PIECES.BIN", &contents).unwrap();

        let mut whole = vec![0u8; contents.len()];
        let read = volume.read_file(&entry, &mut whole).unwrap();
        assert_eq!(read, contents.len());
        assert_eq!(whole, contents, "the whole file, for a baseline");

        for piece in [1usize, 3, 511, 512, 513, 1024, 4096] {
            let mut rebuilt = Vec::new();
            let mut buffer = vec![0u8; piece];
            loop {
                let at = rebuilt.len() as u32;
                let got = volume.read_at(&entry, at, &mut buffer).unwrap();
                if got == 0 {
                    break;
                }
                assert!(got <= piece, "more than was asked for");
                rebuilt.extend_from_slice(&buffer[..got]);
            }
            assert_eq!(
                rebuilt.len(),
                contents.len(),
                "pieces of {piece}: wrong length"
            );
            assert_eq!(rebuilt, contents, "pieces of {piece}: wrong bytes");
        }
    }

    /// Every single offset, one byte at a time, against the whole file.
    ///
    /// Slower than the loop above and worth it: it reads *from* every
    /// offset rather than only from the ones a piece size lands on, so an
    /// error that only happens at, say, the first byte of the second
    /// cluster cannot hide between two pieces.
    #[test]
    fn every_offset_reads_the_byte_that_is_there() {
        let mut volume = volume();
        let contents: Vec<u8> = (0..4600u32)
            .map(|at| (at.wrapping_mul(17).wrapping_add(3) % 253) as u8)
            .collect();
        let entry = volume.write_file("OFFSETS.BIN", &contents).unwrap();

        let mut one = [0u8; 1];
        for (at, want) in contents.iter().enumerate() {
            let got = volume.read_at(&entry, at as u32, &mut one).unwrap();
            assert_eq!(got, 1, "offset {at} read nothing");
            assert_eq!(one[0], *want, "offset {at} read the wrong byte");
        }
        // And one past the end is zero, not an error: that is how a
        // reading loop knows it is done (ADR 0028).
        assert_eq!(
            volume
                .read_at(&entry, contents.len() as u32, &mut one)
                .unwrap(),
            0
        );
        assert_eq!(volume.read_at(&entry, u32::MAX, &mut one).unwrap(), 0);
    }

    /// A read that asks for more than is left gets what is left, not an
    /// error and not padding.
    #[test]
    fn a_read_past_the_end_gets_what_is_left() {
        let mut volume = volume();
        let entry = volume.write_file("SHORT.BIN", &[9u8; 700]).unwrap();

        let mut buffer = [0xAAu8; 1024];
        let got = volume.read_at(&entry, 600, &mut buffer).unwrap();
        assert_eq!(got, 100, "100 bytes left of 700 from offset 600");
        assert!(buffer[..got].iter().all(|byte| *byte == 9));
        // What was not read was not touched.
        assert!(buffer[got..].iter().all(|byte| *byte == 0xAA));
    }

    /// A file of no bytes reads as zero from the start, and owns no
    /// cluster to walk to.
    #[test]
    fn an_empty_file_reads_nothing_without_following_anything() {
        let mut volume = volume();
        let entry = volume.write_file("NONE.BIN", b"").unwrap();
        assert_eq!(entry.first_cluster, 0, "an empty file owns no cluster");

        let mut buffer = [0u8; 16];
        assert_eq!(volume.read_at(&entry, 0, &mut buffer).unwrap(), 0);
        assert_eq!(volume.read_at(&entry, 100, &mut buffer).unwrap(), 0);
    }

    /// A buffer of no bytes reads no bytes, and says so rather than
    /// walking the chain to find that out.
    #[test]
    fn an_empty_buffer_reads_nothing() {
        let mut volume = volume();
        let entry = volume.write_file("SOME.BIN", &[1u8; 2000]).unwrap();
        assert_eq!(volume.read_at(&entry, 0, &mut []).unwrap(), 0);
        assert_eq!(volume.read_at(&entry, 1000, &mut []).unwrap(), 0);
    }

    /// A directory that claims a file reaches further than its chain does
    /// is refused at the offset where the two disagree, not read into
    /// whatever cluster happens to be next.
    #[test]
    fn an_offset_past_the_chain_is_refused() {
        let mut volume = volume();
        let mut entry = volume.write_file("LIAR.BIN", &[4u8; 2000]).unwrap();
        // The entry says the file is much larger than its chain.
        entry.size = 40_000;

        let mut buffer = [0u8; 16];
        // Inside the chain, this still reads.
        assert_eq!(volume.read_at(&entry, 0, &mut buffer).unwrap(), 16);
        // Past it, the two disagree and the read refuses.
        let far = volume.read_at(&entry, 30_000, &mut buffer);
        assert!(
            matches!(far, Err(harlan_hal::fat::VolumeError::BrokenChain { .. })),
            "{far:?}"
        );
    }

    /// `read_at` on a volume whose clusters hold more than one sector.
    ///
    /// The disk this kernel boots from has one sector per cluster, so a
    /// sector boundary and a cluster boundary are the same boundary and
    /// `read_at`'s inner loop over the sectors inside a cluster runs once
    /// every time. Everything that tells the two apart — the byte offset
    /// inside a cluster, which sector of it that lands in, and carrying on
    /// to the next sector without following the chain — is untested
    /// without a volume like this one.
    #[test]
    fn reading_in_pieces_across_sectors_inside_a_cluster() {
        // Two, not more: FAT32 needs at least 65 525 clusters to be
        // FAT32 at all, so every sector added to a cluster needs twice as
        // many sectors in the volume to stay legal. Two already separates
        // a sector boundary from a cluster boundary, which is the whole
        // point, and keeps the image the size of the others here.
        const PER_CLUSTER: u8 = 2;
        const BIG_CLUSTER_SECTORS: u32 = 140_000;
        let (image, geometry) = format(BIG_CLUSTER_SECTORS, PER_CLUSTER, "HARLAN", &[]).unwrap();
        assert_eq!(geometry.sectors_per_cluster, PER_CLUSTER);
        let mut volume = harlan_hal::fat::Volume::mount(Image(image)).unwrap();
        assert_eq!(volume.boot_sector().sectors_per_cluster, PER_CLUSTER);

        // Twelve clusters and a bit: 1024 bytes to a cluster here.
        let contents: Vec<u8> = (0..13_000u32)
            .map(|at| (at.wrapping_mul(37).wrapping_add(11) % 249) as u8)
            .collect();
        let entry = volume.write_file("BIG.BIN", &contents).unwrap();

        let mut whole = vec![0u8; contents.len()];
        assert_eq!(
            volume.read_file(&entry, &mut whole).unwrap(),
            contents.len()
        );
        assert_eq!(whole, contents, "whole, for a baseline");

        // Piece sizes that land on, just before and just after both kinds
        // of boundary: 512 is a sector and 1024 is a cluster.
        for piece in [1usize, 7, 511, 512, 513, 1023, 1024, 1025, 9000] {
            let mut rebuilt = Vec::new();
            let mut buffer = vec![0u8; piece];
            loop {
                let got = volume
                    .read_at(&entry, rebuilt.len() as u32, &mut buffer)
                    .unwrap();
                if got == 0 {
                    break;
                }
                rebuilt.extend_from_slice(&buffer[..got]);
            }
            assert_eq!(rebuilt, contents, "pieces of {piece}");
        }

        // And the offsets either side of each boundary, one byte at a
        // time, which is where an error of one shows up as a wrong byte.
        let mut one = [0u8; 1];
        for boundary in [512usize, 1024, 1536, 2048, 4096, 8192, 12_288] {
            let from = boundary.saturating_sub(2);
            let to = (boundary + 2).min(contents.len());
            for (step, want) in contents[from..to].iter().enumerate() {
                let at = from + step;
                let got = volume.read_at(&entry, at as u32, &mut one).unwrap();
                assert_eq!(got, 1, "offset {at}");
                assert_eq!(one[0], *want, "offset {at} near {boundary}");
            }
        }
    }

    /// `read_at` across a chain that is not contiguous.
    ///
    /// Found by a mutation that survived: removing the bound on how many
    /// sectors of a cluster to read passed every other test here. Not
    /// because the bound is unnecessary — because every chain in every
    /// other test is contiguous, so reading past a cluster without
    /// following the chain lands on the next physical cluster, which holds
    /// exactly the bytes that were wanted. Only a chain that jumps can
    /// tell the two apart.
    ///
    /// The fragmentation is made with the write path rather than by hand,
    /// because the write path is what will make it on a real disk: a file
    /// is written, overwritten shorter so its tail is freed, and then a
    /// longer file takes the freed clusters and has to carry on past them
    /// (first free from the start of the table, ADR 0027 point 7).
    #[test]
    fn reading_a_chain_that_is_not_contiguous() {
        const PER_CLUSTER: u8 = 2;
        let (image, _) = format(140_000, PER_CLUSTER, "HARLAN", &[]).unwrap();
        let mut volume = harlan_hal::fat::Volume::mount(Image(image)).unwrap();
        let per_cluster = 1024usize;

        // Four clusters, then one after them, then the four shrunk to one:
        // what was the tail of the first file is now a hole with a file on
        // the far side of it.
        volume.write_file("HOLE.BIN", &[1u8; 4 * 1024]).unwrap();
        volume.write_file("WALL.BIN", &[2u8; 1024]).unwrap();
        volume.write_file("HOLE.BIN", &[1u8; 16]).unwrap();

        // Six clusters: the hole cannot hold them all, so the chain has to
        // jump over WALL.BIN to finish.
        let contents: Vec<u8> = (0..6 * per_cluster as u32)
            .map(|at| (at.wrapping_mul(29).wrapping_add(5) % 247) as u8)
            .collect();
        let entry = volume.write_file("JUMPS.BIN", &contents).unwrap();

        // Prove the chain really does jump before trusting what follows.
        let mut cluster = entry.first_cluster;
        let mut jumped = false;
        while let harlan_hal::fat::Entry::Next(next) = volume.next_cluster(cluster).unwrap() {
            if next != cluster + 1 {
                jumped = true;
            }
            cluster = next;
        }
        assert!(
            jumped,
            "this test is worthless unless the chain is fragmented"
        );

        // Whole, in pieces, and one byte at a time across the jump.
        let mut whole = vec![0u8; contents.len()];
        assert_eq!(
            volume.read_file(&entry, &mut whole).unwrap(),
            contents.len()
        );
        assert_eq!(whole, contents, "whole");

        for piece in [1usize, 512, 1024, 1536, 9000] {
            let mut rebuilt = Vec::new();
            let mut buffer = vec![0u8; piece];
            loop {
                let got = volume
                    .read_at(&entry, rebuilt.len() as u32, &mut buffer)
                    .unwrap();
                if got == 0 {
                    break;
                }
                rebuilt.extend_from_slice(&buffer[..got]);
            }
            assert_eq!(rebuilt, contents, "pieces of {piece} across a jump");
        }

        let mut one = [0u8; 1];
        for (at, want) in contents.iter().enumerate() {
            assert_eq!(volume.read_at(&entry, at as u32, &mut one).unwrap(), 1);
            assert_eq!(one[0], *want, "offset {at} across a jump");
        }

        // And the file on the far side of the hole was not read over.
        let wall = volume.find("WALL.BIN").unwrap().unwrap();
        let mut bytes = [0u8; 1024];
        assert_eq!(volume.read_file(&wall, &mut bytes).unwrap(), 1024);
        assert!(bytes.iter().all(|byte| *byte == 2), "WALL.BIN is intact");
    }
}
