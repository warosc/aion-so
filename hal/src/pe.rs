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
}

const SIGNATURE: [u8; 4] = *b"PE\0\0";
/// PE32+ (64-bit). A 32-bit image would not be running on this CPU.
const MAGIC_PE32_PLUS: u16 = 0x20B;
/// `IMAGE_SCN_MEM_EXECUTE`.
const EXECUTABLE: u32 = 0x2000_0000;

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
    let signature = read_u32(image, offsets::LFANEW)? as usize;
    let end = signature
        .checked_add(SIGNATURE.len())
        .ok_or(PeError::Truncated)?;
    if image.get(signature..end).ok_or(PeError::Truncated)? != SIGNATURE {
        return Err(PeError::NotPe32Plus);
    }
    let optional = signature + offsets::OPTIONAL_HEADER;
    if read_u16(image, optional)? != MAGIC_PE32_PLUS {
        return Err(PeError::NotPe32Plus);
    }
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
