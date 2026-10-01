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
}
