//! A GPT-partitioned disk image, for a USB stick that boots on a machine
//! nobody has seen (docs/adr/0034-fase5-usb-image.md).
//!
//! What QEMU has been booting is a directory the emulator pretends is a FAT
//! volume, plus a second raw file for the data. Neither of those is a thing
//! anybody can write to a USB stick. This builds the one file that is: a
//! protective MBR, a GPT, an EFI System Partition with the bootloader in
//! it, and the HARLAN volume beside it.
//!
//! Partitioned rather than one bare FAT volume across the whole device.
//! Firmware often boots an unpartitioned removable disk, and "often" is the
//! problem: the machine this has to work on is one nobody here can try it
//! on first, and a GPT is what every mainstream installer writes because it
//! always works.

use anyhow::{Context, Result, bail};

use crate::fat32;

pub const SECTOR: usize = 512;

/// Where the first partition starts. One mebibyte in, which is what every
/// partitioner has done since disks stopped having real cylinders: it keeps
/// the data aligned to any erase block a flash device might have.
const FIRST_USABLE: u64 = 2048;

/// 128 entries of 128 bytes: the size everything expects, and the one a
/// header that said anything else would have to justify to every tool that
/// reads it.
const ENTRY_COUNT: u32 = 128;
const ENTRY_BYTES: u32 = 128;
/// 32 sectors of entries, plus the header's own.
const ENTRY_SECTORS: u64 = (ENTRY_COUNT * ENTRY_BYTES) as u64 / SECTOR as u64;

/// `EFI PART`, the eight bytes that say a GPT header is one.
const SIGNATURE: &[u8; 8] = b"EFI PART";
const REVISION: u32 = 0x0001_0000;
const HEADER_BYTES: u32 = 92;

/// The partition types, as the UEFI specification numbers them.
///
/// Written out as the bytes they are on disk rather than as a pretty
/// string: a GUID is three little-endian numbers followed by eight bytes in
/// order, and the mixed endianness is the single easiest thing to get wrong
/// about one.
const ESP_TYPE: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
/// "Microsoft basic data", which is what a FAT volume that is not an ESP is
/// called by everything that looks at one.
const DATA_TYPE: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

/// Fixed GUIDs for the disk and the two partitions.
///
/// Random ones would be more correct and would make every build produce a
/// different image, which turns "did the image change?" into a question
/// nobody can answer. Reproducible builds are a Fase 0 promise.
const DISK_GUID: [u8; 16] = *b"HARLAN-DISK-GUID";
const ESP_GUID: [u8; 16] = *b"HARLAN-ESP--PART";
const DATA_GUID: [u8; 16] = *b"HARLAN-DATA-PART";

/// What goes in one partition.
pub struct Partition<'a> {
    pub name: &'a str,
    pub type_guid: [u8; 16],
    pub guid: [u8; 16],
    /// The volume itself, already laid out. Its length decides the
    /// partition's.
    pub contents: Vec<u8>,
}

/// Builds the whole image: a protective MBR, a GPT, and the partitions.
///
/// The partitions are placed in the order given, each aligned to a mebibyte.
pub fn build(partitions: Vec<Partition<'_>>) -> Result<Vec<u8>> {
    if partitions.is_empty() {
        bail!("an image with no partitions is not an image");
    }
    if partitions.len() > ENTRY_COUNT as usize {
        bail!("{} partitions is more than a GPT holds", partitions.len());
    }

    // Where each one goes, and how big the whole thing has to be.
    let mut placed = Vec::new();
    let mut next = FIRST_USABLE;
    for partition in &partitions {
        if partition.contents.len() % SECTOR != 0 {
            bail!(
                "{}: {} bytes is not a whole number of sectors",
                partition.name,
                partition.contents.len()
            );
        }
        let sectors = (partition.contents.len() / SECTOR) as u64;
        if sectors == 0 {
            bail!("{}: a partition of no sectors", partition.name);
        }
        placed.push((next, next + sectors - 1));
        // The next one starts at the next mebibyte boundary.
        next = (next + sectors).div_ceil(FIRST_USABLE) * FIRST_USABLE;
    }

    // Room after the last partition for the backup entries and header.
    let last_lba = next + ENTRY_SECTORS; // the backup header's own sector
    let total_sectors = last_lba + 1;
    let mut image = vec![0u8; total_sectors as usize * SECTOR];

    // The partitions themselves.
    for (partition, (first, _)) in partitions.iter().zip(&placed) {
        let at = *first as usize * SECTOR;
        image[at..at + partition.contents.len()].copy_from_slice(&partition.contents);
    }

    // The entry array, which both headers describe and checksum.
    let mut entries = vec![0u8; (ENTRY_COUNT * ENTRY_BYTES) as usize];
    for (at, (partition, (first, last))) in partitions.iter().zip(&placed).enumerate() {
        let entry = &mut entries[at * ENTRY_BYTES as usize..][..ENTRY_BYTES as usize];
        entry[0..16].copy_from_slice(&partition.type_guid);
        entry[16..32].copy_from_slice(&partition.guid);
        entry[32..40].copy_from_slice(&first.to_le_bytes());
        entry[40..48].copy_from_slice(&last.to_le_bytes());
        // No attributes: not required, not hidden, not read-only.
        entry[48..56].copy_from_slice(&0u64.to_le_bytes());
        // The name, UTF-16LE, 36 characters' worth of room.
        for (step, unit) in partition.name.encode_utf16().take(36).enumerate() {
            entry[56 + step * 2..58 + step * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);

    // The primary: header at 1, entries from 2.
    let first_usable = FIRST_USABLE;
    let last_usable = next - 1;
    let primary = header(1, last_lba, 2, first_usable, last_usable, entries_crc);
    image[SECTOR..2 * SECTOR].copy_from_slice(&primary);
    let at = 2 * SECTOR;
    image[at..at + entries.len()].copy_from_slice(&entries);

    // The backup: entries just before the last sector, header in it. A disk
    // whose primary header is unreadable is still readable from the end,
    // which is the whole reason the backup exists.
    let backup_entries_lba = last_lba - ENTRY_SECTORS;
    let backup = header(
        last_lba,
        1,
        backup_entries_lba,
        first_usable,
        last_usable,
        entries_crc,
    );
    let at = backup_entries_lba as usize * SECTOR;
    image[at..at + entries.len()].copy_from_slice(&entries);
    let at = last_lba as usize * SECTOR;
    image[at..at + SECTOR].copy_from_slice(&backup);

    // And the protective MBR, so that anything which only understands the
    // old table sees one partition covering the disk and leaves it alone
    // rather than offering to "initialise" it.
    protective_mbr(&mut image, total_sectors);

    Ok(image)
}

/// One GPT header, checksummed.
fn header(
    this_lba: u64,
    other_lba: u64,
    entries_lba: u64,
    first_usable: u64,
    last_usable: u64,
    entries_crc: u32,
) -> [u8; SECTOR] {
    let mut sector = [0u8; SECTOR];
    sector[0..8].copy_from_slice(SIGNATURE);
    sector[8..12].copy_from_slice(&REVISION.to_le_bytes());
    sector[12..16].copy_from_slice(&HEADER_BYTES.to_le_bytes());
    // 16..20 is the header's own CRC, which is computed with these four
    // bytes zero — they are already zero here.
    sector[24..32].copy_from_slice(&this_lba.to_le_bytes());
    sector[32..40].copy_from_slice(&other_lba.to_le_bytes());
    sector[40..48].copy_from_slice(&first_usable.to_le_bytes());
    sector[48..56].copy_from_slice(&last_usable.to_le_bytes());
    sector[56..72].copy_from_slice(&DISK_GUID);
    sector[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    sector[80..84].copy_from_slice(&ENTRY_COUNT.to_le_bytes());
    sector[84..88].copy_from_slice(&ENTRY_BYTES.to_le_bytes());
    sector[88..92].copy_from_slice(&entries_crc.to_le_bytes());

    let crc = crc32(&sector[..HEADER_BYTES as usize]);
    sector[16..20].copy_from_slice(&crc.to_le_bytes());
    sector
}

/// The protective MBR of UEFI 5.2.3: one entry of type `0xEE` covering the
/// disk, so an old tool sees something it does not understand rather than
/// something it thinks is empty.
fn protective_mbr(image: &mut [u8], total_sectors: u64) {
    let entry = 446;
    image[entry] = 0x00; // not bootable: the firmware uses the GPT
    // CHS of the first sector, in the form everything writes and nothing
    // reads: head 0, sector 2, cylinder 0.
    image[entry + 1..entry + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    image[entry + 4] = 0xEE; // "there is a GPT here"
    // CHS of the last sector, saturated, for the same reason.
    image[entry + 5..entry + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    image[entry + 8..entry + 12].copy_from_slice(&1u32.to_le_bytes());
    // Everything after the MBR itself, saturated at what the field holds.
    let covered = u32::try_from(total_sectors - 1).unwrap_or(u32::MAX);
    image[entry + 12..entry + 16].copy_from_slice(&covered.to_le_bytes());
    image[510] = 0x55;
    image[511] = 0xAA;
}

/// CRC-32 as GPT uses it: the ordinary one, reflected, polynomial
/// `0xEDB88320`, starting and ending inverted.
///
/// Written here rather than taken from a crate because it is twelve lines
/// and because a wrong one would be found by any tool that reads the image
/// — which is the point of writing a GPT at all.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let carry = crc & 1;
            crc >>= 1;
            if carry != 0 {
                crc ^= 0xEDB8_8320;
            }
        }
    }
    !crc
}

/// Lays out the USB image: the ESP with the bootloader, and the HARLAN
/// volume with everything else.
pub fn usb_image(bootloader: Vec<u8>, data: Vec<u8>, esp_sectors: u32) -> Result<Vec<u8>> {
    let esp = fat32::format(
        esp_sectors,
        1,
        "HARLANESP",
        &[fat32::File {
            // The path UEFI looks for on a removable device, and the only
            // one: the specification fixes it, and firmware that does not
            // find a file there does not boot. The first image this built
            // put the loader at the root, where nothing looks — found by
            // 7-Zip listing the volume, before any firmware saw it.
            name: "EFI/BOOT/BOOTX64.EFI",
            contents: bootloader,
        }],
    )
    .map_err(|err| anyhow::anyhow!("the ESP could not be laid out: {err:?}"))?
    .0;

    build(vec![
        Partition {
            name: "HARLAN ESP",
            type_guid: ESP_TYPE,
            guid: ESP_GUID,
            contents: esp,
        },
        Partition {
            name: "HARLAN",
            type_guid: DATA_TYPE,
            guid: DATA_GUID,
            contents: data,
        },
    ])
    .context("the USB image could not be laid out")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CRC against values everybody else's CRC-32 agrees on.
    ///
    /// Checked against known answers and not against a second copy of this
    /// function: a checksum verified by its own author is a checksum that
    /// agrees with itself (ADR 0025, point 5).
    #[test]
    fn the_crc_is_the_one_everything_else_computes() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"abc"), 0x3524_41C2);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(&[0u8; 32]), 0x190A_55AD);
    }

    fn small(name: &str, sectors: usize) -> Partition<'_> {
        Partition {
            name,
            type_guid: DATA_TYPE,
            guid: DATA_GUID,
            contents: vec![0xAB; sectors * SECTOR],
        }
    }

    /// The header says it is one, in the place the specification puts it.
    #[test]
    fn the_header_is_where_and_what_it_should_be() {
        let image = build(vec![small("one", 64)]).expect("an image");
        assert_eq!(&image[SECTOR..SECTOR + 8], SIGNATURE);
        assert_eq!(
            u32::from_le_bytes(image[SECTOR + 8..SECTOR + 12].try_into().unwrap()),
            REVISION
        );
        assert_eq!(
            u64::from_le_bytes(image[SECTOR + 24..SECTOR + 32].try_into().unwrap()),
            1,
            "the primary header says it is at LBA 1"
        );
    }

    /// Both checksums are right, computed the way a reader computes them.
    ///
    /// This is the test that matters: a GPT with a bad CRC is a GPT every
    /// firmware ignores, and the image would simply not boot with no
    /// explanation.
    #[test]
    fn both_headers_and_the_entries_checksum() {
        let image = build(vec![small("one", 64), small("two", 64)]).expect("an image");

        for base in [SECTOR, backup_header_at(&image)] {
            let header = &image[base..base + SECTOR];
            let stated = u32::from_le_bytes(header[16..20].try_into().unwrap());
            let mut without = header[..HEADER_BYTES as usize].to_vec();
            without[16..20].fill(0);
            assert_eq!(crc32(&without), stated, "header at {base}");

            // And the entry array it points at.
            let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
            let count = u32::from_le_bytes(header[80..84].try_into().unwrap());
            let size = u32::from_le_bytes(header[84..88].try_into().unwrap());
            let at = entries_lba as usize * SECTOR;
            let entries = &image[at..at + (count * size) as usize];
            let stated = u32::from_le_bytes(header[88..92].try_into().unwrap());
            assert_eq!(crc32(entries), stated, "entries for the header at {base}");
        }
    }

    fn backup_header_at(image: &[u8]) -> usize {
        let primary = &image[SECTOR..2 * SECTOR];
        let backup_lba = u64::from_le_bytes(primary[32..40].try_into().unwrap());
        backup_lba as usize * SECTOR
    }

    /// The two headers describe the same disk from opposite ends.
    #[test]
    fn the_backup_points_back_at_the_primary() {
        let image = build(vec![small("one", 64)]).expect("an image");
        let primary = &image[SECTOR..2 * SECTOR];
        let backup_at = backup_header_at(&image);
        let backup = &image[backup_at..backup_at + SECTOR];

        let primary_other = u64::from_le_bytes(primary[32..40].try_into().unwrap());
        let backup_this = u64::from_le_bytes(backup[24..32].try_into().unwrap());
        let backup_other = u64::from_le_bytes(backup[32..40].try_into().unwrap());
        assert_eq!(primary_other, backup_this, "the primary names the backup");
        assert_eq!(backup_other, 1, "and the backup names the primary");

        // The usable range is the same in both, or a reader that trusted
        // one would place a partition where the other says it cannot go.
        assert_eq!(primary[40..56], backup[40..56]);
        // And the backup is the last sector of the image.
        assert_eq!(backup_at + SECTOR, image.len());
    }

    /// Each partition is where its entry says it is, and they do not
    /// overlap.
    #[test]
    fn the_partitions_are_where_the_entries_say() {
        let one = small("one", 64);
        let two = small("two", 96);
        let (one_len, two_len) = (one.contents.len(), two.contents.len());
        let image = build(vec![one, two]).expect("an image");
        let entries_at = 2 * SECTOR;

        let mut previous_end = FIRST_USABLE;
        for (at, len) in [(0usize, one_len), (1, two_len)] {
            let entry = &image[entries_at + at * ENTRY_BYTES as usize..][..ENTRY_BYTES as usize];
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            assert_eq!(
                (last - first + 1) as usize * SECTOR,
                len,
                "partition {at} is the size of its contents"
            );
            assert!(
                first >= previous_end,
                "partition {at} overlaps the one before"
            );
            assert_eq!(first % FIRST_USABLE, 0, "partition {at} is not aligned");
            // And its first byte really is its contents.
            assert_eq!(image[first as usize * SECTOR], 0xAB);
            previous_end = last + 1;
        }

        // The entry after the last one used is empty, so a reader stops.
        let empty = &image[entries_at + 2 * ENTRY_BYTES as usize..][..16];
        assert_eq!(empty, [0u8; 16], "a third partition was described");
    }

    /// The protective MBR covers the disk and says there is a GPT, so that
    /// a tool which only knows the old table leaves it alone instead of
    /// offering to initialise it.
    #[test]
    fn the_protective_mbr_protects() {
        let image = build(vec![small("one", 64)]).expect("an image");
        assert_eq!(image[450], 0xEE, "the type that means a GPT is here");
        assert_eq!(
            u32::from_le_bytes(image[454..458].try_into().unwrap()),
            1,
            "it starts at the sector after itself"
        );
        let covered = u32::from_le_bytes(image[458..462].try_into().unwrap());
        assert_eq!(
            u64::from(covered) + 1,
            (image.len() / SECTOR) as u64,
            "it covers the rest of the disk"
        );
        assert_eq!(&image[510..512], &[0x55, 0xAA]);
        // And nothing claims to be bootable there: the firmware is to use
        // the GPT, not this.
        assert_eq!(image[446], 0x00);
    }

    /// A partition whose contents are not a whole number of sectors is
    /// refused rather than rounded: rounding would put half a sector of
    /// somebody's data outside the partition that owns it.
    #[test]
    fn a_partition_that_is_not_whole_sectors_is_refused() {
        let ragged = Partition {
            name: "ragged",
            type_guid: DATA_TYPE,
            guid: DATA_GUID,
            contents: vec![0; SECTOR + 1],
        };
        assert!(build(vec![ragged]).is_err());
        assert!(build(vec![]).is_err(), "an image of nothing");
    }
}
