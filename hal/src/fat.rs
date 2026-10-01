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
}
