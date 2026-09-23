//! Just enough PE to find the code inside a loaded UEFI image.
//!
//! The firmware loads the kernel as a PE32+ executable and tells us where
//! it put it, but not what is inside. Without that, the whole image has to
//! be mapped executable — including its data (ADR 0008). Reading the
//! section table says which parts are code, so everything else can be
//! marked no-execute and the code itself read-only.
//!
//! Plain data in, plain data out: no allocation, no architecture, no
//! firmware. It parses an image the firmware has already loaded and
//! relocated, so offsets are relative to the image base.

use crate::addr::PhysAddr;
use crate::frame::PhysRange;

/// Offsets that are the same in every PE file (Microsoft PE/COFF
/// specification, §"MS-DOS Stub" and §"Signature").
mod offsets {
    /// Where the DOS stub records the offset of the PE signature.
    pub const LFANEW: usize = 0x3C;
    /// Fields inside the COFF header, from the signature.
    pub const NUMBER_OF_SECTIONS: usize = 6;
    pub const SIZE_OF_OPTIONAL_HEADER: usize = 20;
    /// Where the optional header starts, from the signature.
    pub const OPTIONAL_HEADER: usize = 24;
    /// Section headers, each `SECTION_SIZE` bytes, follow the optional
    /// header.
    pub const SECTION_SIZE: usize = 40;
    pub const SECTION_VIRTUAL_SIZE: usize = 8;
    pub const SECTION_VIRTUAL_ADDRESS: usize = 12;
    pub const SECTION_CHARACTERISTICS: usize = 36;
    /// Data directories follow the fixed part of a PE32+ optional header.
    pub const DATA_DIRECTORIES: usize = OPTIONAL_HEADER + 112;
    /// The base relocation table is the sixth directory.
    pub const BASE_RELOCATION_DIRECTORY: usize = 5;
}

const SIGNATURE: [u8; 4] = *b"PE\0\0";
/// PE32+ (64-bit). A 32-bit image would not be running on this CPU.
const MAGIC_PE32_PLUS: u16 = 0x20B;
/// `IMAGE_SCN_MEM_EXECUTE`.
const EXECUTABLE: u32 = 0x2000_0000;

/// Padding inside a relocation block; carries no address.
const RELOCATION_ABSOLUTE: u16 = 0;
/// The whole 64-bit value at the address gets the delta added. The only
/// kind of relocation an x86_64 image needs.
const RELOCATION_DIR64: u16 = 10;
/// A relocation block starts with the page RVA and its own size.
const RELOCATION_BLOCK_HEADER: usize = 8;

/// How many code sections are kept. Linkers produce one (`.text`); a
/// handful covers anything unusual, and going over is reported rather
/// than silently dropping code.
pub const MAX_CODE_SECTIONS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeError {
    /// Too short, or a header pointing outside the image.
    Truncated,
    /// Not a PE32+ image: no signature, or the wrong magic.
    NotPe32Plus,
    /// No section is marked executable, which cannot be right for a
    /// program the firmware just ran.
    NoCode,
    /// More executable sections than `MAX_CODE_SECTIONS`.
    TooManySections,
    /// The image cannot be moved: it has no relocation table.
    NotRelocatable,
    /// A relocation this code does not know how to apply. Refusing beats
    /// leaving an address behind pointing at the old place.
    UnsupportedRelocation(u16),
}

/// Where the code lives inside an image, as `(offset from the image base,
/// length)` pairs in the order the section table lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeSections {
    ranges: [(u64, u64); MAX_CODE_SECTIONS],
    len: usize,
}

impl CodeSections {
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges[..self.len].iter().copied()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total number of bytes of code, for the boot log.
    pub fn total_bytes(&self) -> u64 {
        self.iter().map(|(_, len)| len).sum()
    }

    /// The same sections as physical ranges, given where the firmware put
    /// the image. `None` if any of them would run past the end of the
    /// address space, which would mean a header that cannot be trusted.
    pub fn at(&self, base: PhysAddr) -> Option<CodeRanges> {
        let mut placed = CodeRanges {
            ranges: [PhysRange::new(PhysAddr::new(0), 0); MAX_CODE_SECTIONS],
            len: self.len,
        };
        for (index, (offset, len)) in self.iter().enumerate() {
            let start = base.checked_add(offset)?;
            start.checked_add(len)?;
            placed.ranges[index] = PhysRange::new(start, len);
        }
        Some(placed)
    }
}

/// The code of an image already placed in memory: what the kernel needs
/// to keep executable, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRanges {
    ranges: [PhysRange; MAX_CODE_SECTIONS],
    len: usize,
}

impl CodeRanges {
    /// A set built from ranges already placed in memory, for a caller that
    /// learned where the code is some other way. `None` if there are more
    /// than `MAX_CODE_SECTIONS`.
    pub fn new(ranges: &[PhysRange]) -> Option<Self> {
        if ranges.len() > MAX_CODE_SECTIONS {
            return None;
        }
        let mut built = Self {
            ranges: [PhysRange::new(PhysAddr::new(0), 0); MAX_CODE_SECTIONS],
            len: ranges.len(),
        };
        built.ranges[..ranges.len()].copy_from_slice(ranges);
        Some(built)
    }

    pub fn iter(&self) -> impl Iterator<Item = PhysRange> + '_ {
        self.ranges[..self.len].iter().copied()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn total_bytes(&self) -> u64 {
        self.iter().map(|range| range.len).sum()
    }
}

/// Finds the executable sections of `image`, which must be the bytes of a
/// loaded PE32+ image starting at its base.
///
/// Nothing here trusts the image: every offset is checked against the
/// slice before it is read, so a malformed header is an error and never a
/// read out of bounds.
pub fn code_sections(image: &[u8]) -> Result<CodeSections, PeError> {
    let signature = signature_offset(image)?;
    let optional = signature + offsets::OPTIONAL_HEADER;
    let count = read_u16(image, signature + offsets::NUMBER_OF_SECTIONS)? as usize;
    let optional_size = read_u16(image, signature + offsets::SIZE_OF_OPTIONAL_HEADER)? as usize;
    let table = optional
        .checked_add(optional_size)
        .ok_or(PeError::Truncated)?;

    let mut found = CodeSections {
        ranges: [(0, 0); MAX_CODE_SECTIONS],
        len: 0,
    };
    for index in 0..count {
        let header = table
            .checked_add(index * offsets::SECTION_SIZE)
            .ok_or(PeError::Truncated)?;
        if read_u32(image, header + offsets::SECTION_CHARACTERISTICS)? & EXECUTABLE == 0 {
            continue;
        }
        let start = u64::from(read_u32(image, header + offsets::SECTION_VIRTUAL_ADDRESS)?);
        let len = u64::from(read_u32(image, header + offsets::SECTION_VIRTUAL_SIZE)?);
        if len == 0 {
            continue;
        }
        if found.len == MAX_CODE_SECTIONS {
            return Err(PeError::TooManySections);
        }
        found.ranges[found.len] = (start, len);
        found.len += 1;
    }
    if found.is_empty() {
        return Err(PeError::NoCode);
    }
    Ok(found)
}

fn read_u16(image: &[u8], at: usize) -> Result<u16, PeError> {
    let bytes = image.get(at..at + 2).ok_or(PeError::Truncated)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(image: &[u8], at: usize) -> Result<u32, PeError> {
    let bytes = image.get(at..at + 4).ok_or(PeError::Truncated)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNATURE_AT: usize = 0x80;
    const OPTIONAL_SIZE: usize = 240;

    /// A PE32+ image with the sections given as `(virtual address, size,
    /// characteristics)`, laid out like the ones the linker produces.
    fn image_with(sections: &[(u32, u32, u32)]) -> std::vec::Vec<u8> {
        let table = SIGNATURE_AT + offsets::OPTIONAL_HEADER + OPTIONAL_SIZE;
        let mut image = std::vec![0u8; table + sections.len() * offsets::SECTION_SIZE + 16];
        image[offsets::LFANEW..offsets::LFANEW + 4]
            .copy_from_slice(&(SIGNATURE_AT as u32).to_le_bytes());
        image[SIGNATURE_AT..SIGNATURE_AT + 4].copy_from_slice(&SIGNATURE);
        let coff = SIGNATURE_AT + offsets::NUMBER_OF_SECTIONS;
        image[coff..coff + 2].copy_from_slice(&(sections.len() as u16).to_le_bytes());
        let size_at = SIGNATURE_AT + offsets::SIZE_OF_OPTIONAL_HEADER;
        image[size_at..size_at + 2].copy_from_slice(&(OPTIONAL_SIZE as u16).to_le_bytes());
        let magic_at = SIGNATURE_AT + offsets::OPTIONAL_HEADER;
        image[magic_at..magic_at + 2].copy_from_slice(&MAGIC_PE32_PLUS.to_le_bytes());
        for (index, &(address, size, characteristics)) in sections.iter().enumerate() {
            let header = table + index * offsets::SECTION_SIZE;
            let put = |image: &mut [u8], at: usize, value: u32| {
                image[header + at..header + at + 4].copy_from_slice(&value.to_le_bytes());
            };
            put(&mut image, offsets::SECTION_VIRTUAL_ADDRESS, address);
            put(&mut image, offsets::SECTION_VIRTUAL_SIZE, size);
            put(
                &mut image,
                offsets::SECTION_CHARACTERISTICS,
                characteristics,
            );
        }
        image
    }

    const CODE: u32 = EXECUTABLE | 0x4000_0000 | 0x20;
    const DATA: u32 = 0x4000_0000 | 0x8000_0000;
    const READ_ONLY: u32 = 0x4000_0000;

    #[test]
    fn finds_the_code_and_leaves_the_data_out() {
        // The layout of this repo's own image.
        let image = image_with(&[
            (0x1000, 217_891, CODE),
            (0x37000, 52_157, READ_ONLY),
            (0x44000, 20_840, DATA),
            (0x4A000, 64, READ_ONLY),
            (0x4B000, 1408, READ_ONLY),
        ]);
        let code = code_sections(&image).unwrap();
        assert_eq!(code.len(), 1);
        assert_eq!(
            code.iter().collect::<std::vec::Vec<_>>(),
            [(0x1000, 217_891)]
        );
        assert_eq!(code.total_bytes(), 217_891);
    }

    #[test]
    fn keeps_every_executable_section_in_order() {
        let image = image_with(&[
            (0x1000, 0x2000, CODE),
            (0x3000, 0x1000, DATA),
            (0x4000, 0x1000, CODE),
        ]);
        let code = code_sections(&image).unwrap();
        assert_eq!(
            code.iter().collect::<std::vec::Vec<_>>(),
            [(0x1000, 0x2000), (0x4000, 0x1000)]
        );
    }

    #[test]
    fn an_executable_section_with_no_bytes_is_not_code() {
        let image = image_with(&[(0x1000, 0, CODE), (0x2000, 0x1000, CODE)]);
        let code = code_sections(&image).unwrap();
        assert_eq!(
            code.iter().collect::<std::vec::Vec<_>>(),
            [(0x2000, 0x1000)]
        );
    }

    #[test]
    fn sections_become_ranges_where_the_firmware_put_the_image() {
        let image = image_with(&[(0x1000, 0x2000, CODE), (0x4000, 0x1000, CODE)]);
        let base = PhysAddr::new(0x1DDD_3000);
        let placed = code_sections(&image).unwrap().at(base).unwrap();
        assert_eq!(placed.len(), 2);
        assert_eq!(
            placed.iter().collect::<std::vec::Vec<_>>(),
            [
                PhysRange::new(PhysAddr::new(0x1DDD_4000), 0x2000),
                PhysRange::new(PhysAddr::new(0x1DDD_7000), 0x1000),
            ]
        );
        assert_eq!(placed.total_bytes(), 0x3000);
        // A base that would push the code past the end of memory is not a
        // range anyone should map.
        assert_eq!(
            code_sections(&image).unwrap().at(PhysAddr::new(u64::MAX)),
            None
        );
    }

    #[test]
    fn a_set_of_ranges_can_be_built_by_hand_up_to_the_limit() {
        let one = PhysRange::new(PhysAddr::new(0x1000), 0x2000);
        let built = CodeRanges::new(&[one]).unwrap();
        assert_eq!(built.iter().collect::<std::vec::Vec<_>>(), [one]);
        assert_eq!(built.total_bytes(), 0x2000);
        let too_many = std::vec![one; MAX_CODE_SECTIONS + 1];
        assert_eq!(CodeRanges::new(&too_many), None);
    }

    #[test]
    fn an_image_without_code_is_refused() {
        let image = image_with(&[(0x1000, 0x1000, DATA)]);
        assert_eq!(code_sections(&image), Err(PeError::NoCode));
    }

    #[test]
    fn more_code_sections_than_there_is_room_for_is_refused() {
        let mut sections = std::vec::Vec::new();
        for index in 0..MAX_CODE_SECTIONS + 1 {
            sections.push(((index as u32 + 1) * 0x1000, 0x1000, CODE));
        }
        assert_eq!(
            code_sections(&image_with(&sections)),
            Err(PeError::TooManySections)
        );
    }

    #[test]
    fn a_header_pointing_outside_the_image_is_refused_instead_of_read() {
        let good = image_with(&[(0x1000, 0x1000, CODE)]);
        assert_eq!(code_sections(&[]), Err(PeError::Truncated));
        // The section table is announced but not there.
        assert_eq!(
            code_sections(&good[..good.len() - 20]),
            Err(PeError::Truncated)
        );
        // `e_lfanew` past the end.
        let mut wild = good.clone();
        wild[offsets::LFANEW..offsets::LFANEW + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(code_sections(&wild), Err(PeError::Truncated));
    }

    /// Writes a relocation table into `image` and points the directory at
    /// it. `blocks` is `(page rva, [(kind, offset)])`.
    fn with_relocations(
        mut image: std::vec::Vec<u8>,
        blocks: &[(u32, std::vec::Vec<(u16, u16)>)],
    ) -> std::vec::Vec<u8> {
        let table = image.len();
        let mut bytes = std::vec::Vec::new();
        for (page, entries) in blocks {
            let size = RELOCATION_BLOCK_HEADER + entries.len() * 2;
            bytes.extend_from_slice(&page.to_le_bytes());
            bytes.extend_from_slice(&(size as u32).to_le_bytes());
            for (kind, offset) in entries {
                bytes.extend_from_slice(&((kind << 12) | offset).to_le_bytes());
            }
        }
        let directory = SIGNATURE_AT
            + offsets::DATA_DIRECTORIES
            + offsets::BASE_RELOCATION_DIRECTORY * DATA_DIRECTORY_SIZE;
        image[directory..directory + 4].copy_from_slice(&(table as u32).to_le_bytes());
        image[directory + 4..directory + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        image.extend_from_slice(&bytes);
        image
    }

    fn one_code_section() -> std::vec::Vec<u8> {
        image_with(&[(0x1000, 0x2000, CODE)])
    }

    #[test]
    fn relocations_list_every_absolute_address_and_skip_the_padding() {
        let image = with_relocations(
            one_code_section(),
            &[
                (
                    0x1000,
                    std::vec![
                        (RELOCATION_DIR64, 0x18),
                        (RELOCATION_ABSOLUTE, 0),
                        (RELOCATION_DIR64, 0x20),
                    ],
                ),
                (0x2000, std::vec![(RELOCATION_DIR64, 0x8)]),
            ],
        );
        let found: std::vec::Vec<u64> = relocations(&image).unwrap().iter().collect();
        assert_eq!(found, [0x1018, 0x1020, 0x2008]);
    }

    #[test]
    fn an_image_that_cannot_be_moved_says_so() {
        assert_eq!(
            relocations(&one_code_section()),
            Err(PeError::NotRelocatable)
        );
    }

    #[test]
    fn a_relocation_kind_we_cannot_apply_is_refused() {
        // 3 is HIGHLOW, for 32-bit images.
        let image = with_relocations(one_code_section(), &[(0x1000, std::vec![(3, 0x10)])]);
        assert_eq!(relocations(&image), Err(PeError::UnsupportedRelocation(3)));
    }

    #[test]
    fn a_relocation_table_that_does_not_fit_is_refused_instead_of_walked() {
        let image = with_relocations(
            one_code_section(),
            &[(0x1000, std::vec![(RELOCATION_DIR64, 0x10)])],
        );
        // The table is announced longer than the image.
        let directory = SIGNATURE_AT
            + offsets::DATA_DIRECTORIES
            + offsets::BASE_RELOCATION_DIRECTORY * DATA_DIRECTORY_SIZE;
        let mut too_long = image.clone();
        too_long[directory + 4..directory + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(relocations(&too_long), Err(PeError::Truncated));
        let table =
            u32::from_le_bytes(image[directory..directory + 4].try_into().unwrap()) as usize;
        // A block that claims to run past the end of the table. Even, so
        // that it is its length that rejects it and not its parity.
        let mut bad_block = image.clone();
        bad_block[table + 4..table + 8].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(relocations(&bad_block), Err(PeError::Truncated));
        // And one whose size would cut an entry in half. Followed by a
        // block that does parse, so that only the odd size can reject it.
        let mut odd = one_code_section();
        let raw = {
            let mut bytes = std::vec::Vec::new();
            bytes.extend_from_slice(&0x1000u32.to_le_bytes());
            bytes.extend_from_slice(&9u32.to_le_bytes()); // one byte of entries
            bytes.push(0);
            bytes.extend_from_slice(&0x2000u32.to_le_bytes());
            bytes.extend_from_slice(&8u32.to_le_bytes()); // a block with none
            bytes
        };
        let at = odd.len();
        odd[directory..directory + 4].copy_from_slice(&(at as u32).to_le_bytes());
        odd[directory + 4..directory + 8].copy_from_slice(&(raw.len() as u32).to_le_bytes());
        odd.extend_from_slice(&raw);
        assert_eq!(relocations(&odd), Err(PeError::Truncated));
    }

    #[test]
    fn something_that_is_not_a_pe32_plus_image_is_refused() {
        let mut no_signature = image_with(&[(0x1000, 0x1000, CODE)]);
        no_signature[SIGNATURE_AT] = b'X';
        assert_eq!(code_sections(&no_signature), Err(PeError::NotPe32Plus));

        let mut pe32 = image_with(&[(0x1000, 0x1000, CODE)]);
        let magic_at = SIGNATURE_AT + offsets::OPTIONAL_HEADER;
        pe32[magic_at..magic_at + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
        assert_eq!(code_sections(&pe32), Err(PeError::NotPe32Plus));
    }
}

/// The base relocation table of a loaded image: where every absolute
/// 64-bit address sits, so the image can be moved.
///
/// The firmware already relocated the image once, to where it loaded it.
/// Applying the table again, with the difference to a new address, is what
/// lets the kernel run from somewhere else
/// (docs/adr/0012-fase3-higher-half-kernel.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relocations<'a> {
    blocks: &'a [u8],
}

impl<'a> Relocations<'a> {
    /// Offsets from the image base of the 64-bit slots to fix up.
    ///
    /// Infallible: `relocations` walked the whole table first, so every
    /// block and entry here is known to fit and to be a kind this code
    /// applies.
    pub fn iter(&self) -> impl Iterator<Item = u64> + 'a {
        let mut blocks = self.blocks;
        let mut entries: &[u8] = &[];
        let mut page = 0u32;
        core::iter::from_fn(move || {
            loop {
                if let Some((entry, rest)) = entries.split_at_checked(2) {
                    entries = rest;
                    let value = u16::from_le_bytes([entry[0], entry[1]]);
                    if value >> 12 == RELOCATION_ABSOLUTE {
                        continue;
                    }
                    return Some(u64::from(page) + u64::from(value & 0xFFF));
                }
                let header = blocks.get(..RELOCATION_BLOCK_HEADER)?;
                page = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                let size =
                    u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                entries = &blocks[RELOCATION_BLOCK_HEADER..size];
                blocks = &blocks[size..];
            }
        })
    }
}

/// Reads the base relocation table of `image`.
///
/// Walks it completely, so a table that runs off the end of the image or
/// carries a relocation kind this code cannot apply is an error here
/// rather than a bad write later.
pub fn relocations(image: &[u8]) -> Result<Relocations<'_>, PeError> {
    let signature = signature_offset(image)?;
    let directory = signature
        + offsets::DATA_DIRECTORIES
        + offsets::BASE_RELOCATION_DIRECTORY * DATA_DIRECTORY_SIZE;
    let start = read_u32(image, directory)? as usize;
    let size = read_u32(image, directory + 4)? as usize;
    if start == 0 || size == 0 {
        return Err(PeError::NotRelocatable);
    }
    let end = start.checked_add(size).ok_or(PeError::Truncated)?;
    let blocks = image.get(start..end).ok_or(PeError::Truncated)?;

    // Walk it once to reject anything the iterator could not handle.
    let mut rest = blocks;
    while !rest.is_empty() {
        let header = rest
            .get(..RELOCATION_BLOCK_HEADER)
            .ok_or(PeError::Truncated)?;
        let block = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if block < RELOCATION_BLOCK_HEADER || block > rest.len() || !block.is_multiple_of(2) {
            return Err(PeError::Truncated);
        }
        for entry in rest[RELOCATION_BLOCK_HEADER..block].as_chunks::<2>().0 {
            let kind = u16::from_le_bytes(*entry) >> 12;
            if kind != RELOCATION_ABSOLUTE && kind != RELOCATION_DIR64 {
                return Err(PeError::UnsupportedRelocation(kind));
            }
        }
        rest = &rest[block..];
    }
    Ok(Relocations { blocks })
}

/// Each data directory is an RVA and a size.
const DATA_DIRECTORY_SIZE: usize = 8;

fn signature_offset(image: &[u8]) -> Result<usize, PeError> {
    let signature = read_u32(image, offsets::LFANEW)? as usize;
    let end = signature
        .checked_add(SIGNATURE.len())
        .ok_or(PeError::Truncated)?;
    if image.get(signature..end).ok_or(PeError::Truncated)? != SIGNATURE {
        return Err(PeError::NotPe32Plus);
    }
    if read_u16(image, signature + offsets::OPTIONAL_HEADER)? != MAGIC_PE32_PLUS {
        return Err(PeError::NotPe32Plus);
    }
    Ok(signature)
}
