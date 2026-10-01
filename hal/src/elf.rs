//! Reading an ELF64 program.
//!
//! Only as much as it takes to load a static executable: the header, and
//! the program headers that say which parts of the file go where in
//! memory and with what permissions (docs/adr/0026-fase4-elf-user-programs.md).
//! No relocations, no interpreter, no sections — a linked program does not
//! need them, and a loader that reads them would be reading things it does
//! not act on.
//!
//! Every number here comes from a file on a disk, which is to say from
//! outside the kernel, and every one of them becomes an address. So the
//! whole file is checked before anything is mapped: a bad offset is a read
//! past the end of the file, and a bad size is a mapping on top of
//! something else.
//!
//! Pure, and tested against headers built by hand — including each way
//! they can be wrong — and against what the toolchain really produces.

use crate::addr::VirtAddr;

/// `\x7fELF`.
const MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
/// 64-bit.
const CLASS_64: u8 = 2;
/// Little-endian.
const DATA_LITTLE_ENDIAN: u8 = 1;
/// The only ELF version there has ever been.
const VERSION_CURRENT: u8 = 1;
/// A program that has been linked and can be run as it is. The other kind
/// a loader might meet is `ET_DYN` (3), which needs relocating.
const TYPE_EXECUTABLE: u16 = 2;
/// AMD x86-64.
const MACHINE_X86_64: u16 = 0x3E;
/// The header is 64 bytes, and a program header 56.
const HEADER_BYTES: usize = 64;
const PROGRAM_HEADER_BYTES: usize = 56;
/// A segment to load into memory. Every other kind is something this
/// loader does not act on.
const PT_LOAD: u32 = 1;

/// What a segment may be done with. Straight from the file, so that W^X
/// inside a user program is a property of the program and not of the
/// loader.
pub const FLAG_EXECUTE: u32 = 1;
pub const FLAG_WRITE: u32 = 2;
pub const FLAG_READ: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Not an ELF file at all.
    NotElf,
    /// A 32-bit one, or big-endian, or a version this does not read.
    NotSixtyFourBitLittleEndian { class: u8, data: u8, version: u8 },
    /// Not a linked executable: a shared object, a relocatable object or
    /// a core dump. Loading one needs relocations (ADR 0026, point 2).
    NotAStaticExecutable { kind: u16 },
    /// Built for another machine.
    NotX86_64 { machine: u16 },
    /// The file is shorter than its own headers say.
    Truncated { needed: u64, length: u64 },
    /// A program header table of the wrong shape.
    BadProgramHeaders { entry_size: u16, count: u16 },
    /// A segment whose contents are not all in the file.
    SegmentOutsideFile { offset: u64, size: u64 },
    /// A segment that holds less memory than it holds file.
    SegmentShrinks { file_size: u64, memory_size: u64 },
    /// A segment that would land outside the half a program lives in.
    SegmentOutsideUserSpace { at: u64, size: u64 },
    /// A segment asking to be both writable and executable. The kernel
    /// does not do that to itself (ADR 0008) and will not do it for a
    /// program either.
    SegmentWritableAndExecutable { at: u64 },
    /// More segments than the loader will carry.
    TooManySegments { count: usize },
    /// Nothing to load.
    NoSegments,
    /// The entry point is not inside anything that gets loaded.
    EntryOutsideSegments { entry: u64 },
}

/// How many loadable segments a program may have. A statically linked
/// Rust program has three or four — code, read-only data, data, and
/// sometimes a header segment — and the limit is here so that running out
/// is an error rather than an allocation in the middle of a load.
pub const MAX_SEGMENTS: usize = 8;

/// One piece of a program, as the file describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Where in the file its contents start, and how many bytes of it
    /// there are.
    pub file_offset: u64,
    pub file_size: u64,
    /// Where it goes, and how much memory it takes once there. The
    /// difference between the two is zeroed: it is the `.bss`, and a
    /// loader that skips it leaves rubbish where the program expects
    /// zeroes (ADR 0026, point 5).
    pub at: VirtAddr,
    pub memory_size: u64,
    pub flags: u32,
}

impl Segment {
    pub const fn readable(&self) -> bool {
        self.flags & FLAG_READ != 0
    }

    pub const fn writable(&self) -> bool {
        self.flags & FLAG_WRITE != 0
    }

    pub const fn executable(&self) -> bool {
        self.flags & FLAG_EXECUTE != 0
    }

    /// How many bytes of this segment are zeroes the file does not carry.
    pub const fn zeroes(&self) -> u64 {
        self.memory_size - self.file_size
    }

    /// Where it ends in memory.
    pub const fn end(&self) -> u64 {
        self.at.as_u64() + self.memory_size
    }
}

/// A program, read far enough to be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Program {
    pub entry: VirtAddr,
    segments: [Option<Segment>; MAX_SEGMENTS],
}

impl Program {
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().flatten()
    }

    pub fn segment_count(&self) -> usize {
        self.segments().count()
    }

    /// The highest address any segment reaches, which is how much of the
    /// lower half the program takes.
    pub fn highest_address(&self) -> u64 {
        self.segments().map(Segment::end).max().unwrap_or(0)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

/// Where everything is in the two headers.
mod at {
    pub const CLASS: usize = 4;
    pub const DATA: usize = 5;
    pub const VERSION: usize = 6;
    pub const TYPE: usize = 16;
    pub const MACHINE: usize = 18;
    pub const ENTRY: usize = 24;
    pub const PROGRAM_HEADER_OFFSET: usize = 32;
    pub const PROGRAM_HEADER_ENTRY_SIZE: usize = 54;
    pub const PROGRAM_HEADER_COUNT: usize = 56;

    pub const SEGMENT_TYPE: usize = 0;
    pub const SEGMENT_FLAGS: usize = 4;
    pub const SEGMENT_FILE_OFFSET: usize = 8;
    pub const SEGMENT_VIRTUAL_ADDRESS: usize = 16;
    pub const SEGMENT_FILE_SIZE: usize = 32;
    pub const SEGMENT_MEMORY_SIZE: usize = 40;
}

/// Reads a program out of `file`, refusing everything that would become a
/// bad address later.
///
/// `user_space_end` is the first address a program may not reach: the
/// kernel's half starts there, and a segment that crossed into it would be
/// a program asking to be mapped over the kernel.
pub fn parse(file: &[u8], user_space_end: u64) -> Result<Program, ElfError> {
    if file.len() < HEADER_BYTES || file[..4] != MAGIC {
        return Err(ElfError::NotElf);
    }
    let (class, data, version) = (file[at::CLASS], file[at::DATA], file[at::VERSION]);
    if class != CLASS_64 || data != DATA_LITTLE_ENDIAN || version != VERSION_CURRENT {
        return Err(ElfError::NotSixtyFourBitLittleEndian {
            class,
            data,
            version,
        });
    }
    let kind = u16_at(file, at::TYPE);
    if kind != TYPE_EXECUTABLE {
        return Err(ElfError::NotAStaticExecutable { kind });
    }
    let machine = u16_at(file, at::MACHINE);
    if machine != MACHINE_X86_64 {
        return Err(ElfError::NotX86_64 { machine });
    }

    let entry_size = u16_at(file, at::PROGRAM_HEADER_ENTRY_SIZE);
    let count = u16_at(file, at::PROGRAM_HEADER_COUNT);
    if entry_size as usize != PROGRAM_HEADER_BYTES || count == 0 {
        return Err(ElfError::BadProgramHeaders { entry_size, count });
    }

    let table = u64_at(file, at::PROGRAM_HEADER_OFFSET);
    // In sixty-four bits throughout: these are numbers from a file, and a
    // multiplication that wrapped would point back inside it.
    let table_end = table + u64::from(count) * u64::from(entry_size);
    if table_end > file.len() as u64 {
        return Err(ElfError::Truncated {
            needed: table_end,
            length: file.len() as u64,
        });
    }

    let mut segments = [None; MAX_SEGMENTS];
    let mut found = 0;
    for index in 0..count {
        let at = (table + u64::from(index) * u64::from(entry_size)) as usize;
        let header = &file[at..at + PROGRAM_HEADER_BYTES];
        if u32_at(header, at::SEGMENT_TYPE) != PT_LOAD {
            continue;
        }
        let file_offset = u64_at(header, at::SEGMENT_FILE_OFFSET);
        let file_size = u64_at(header, at::SEGMENT_FILE_SIZE);
        let memory_size = u64_at(header, at::SEGMENT_MEMORY_SIZE);
        let address = u64_at(header, at::SEGMENT_VIRTUAL_ADDRESS);
        let flags = u32_at(header, at::SEGMENT_FLAGS);

        if file_offset
            .checked_add(file_size)
            .is_none_or(|end| end > file.len() as u64)
        {
            return Err(ElfError::SegmentOutsideFile {
                offset: file_offset,
                size: file_size,
            });
        }
        if memory_size < file_size {
            return Err(ElfError::SegmentShrinks {
                file_size,
                memory_size,
            });
        }
        match address.checked_add(memory_size) {
            Some(end) if end <= user_space_end => {}
            _ => {
                return Err(ElfError::SegmentOutsideUserSpace {
                    at: address,
                    size: memory_size,
                });
            }
        }
        if flags & FLAG_WRITE != 0 && flags & FLAG_EXECUTE != 0 {
            return Err(ElfError::SegmentWritableAndExecutable { at: address });
        }

        if found == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments { count: found + 1 });
        }
        segments[found] = Some(Segment {
            file_offset,
            file_size,
            at: VirtAddr::new(address),
            memory_size,
            flags,
        });
        found += 1;
    }

    if found == 0 {
        return Err(ElfError::NoSegments);
    }
    let entry = u64_at(file, at::ENTRY);
    // An entry point outside everything that gets loaded is a jump into
    // nothing, and the fault would come from ring 3 with no clue why.
    let inside = segments.iter().flatten().any(|segment| {
        entry >= segment.at.as_u64() && entry < segment.end() && segment.executable()
    });
    if !inside {
        return Err(ElfError::EntryOutsideSegments { entry });
    }

    Ok(Program {
        entry: VirtAddr::new(entry),
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

    /// Builds an ELF with the segments given, so that each way of being
    /// wrong can be made on purpose.
    struct Builder {
        entry: u64,
        segments: Vec<(u32, u32, u64, u64, u64, u64)>,
        kind: u16,
        machine: u16,
        class: u8,
        entry_size: u16,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                entry: 0x40_0000,
                segments: Vec::new(),
                kind: TYPE_EXECUTABLE,
                machine: MACHINE_X86_64,
                class: CLASS_64,
                entry_size: PROGRAM_HEADER_BYTES as u16,
            }
        }

        /// kind, flags, file offset, file size, address, memory size.
        fn segment(mut self, s: (u32, u32, u64, u64, u64, u64)) -> Self {
            self.segments.push(s);
            self
        }

        fn code(self) -> Self {
            self.segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                0x1000,
                0x100,
                0x40_0000,
                0x100,
            ))
        }

        fn build(&self) -> Vec<u8> {
            let table = HEADER_BYTES as u64;
            let mut file = vec![0u8; 0x2000];
            file[..4].copy_from_slice(&MAGIC);
            file[at::CLASS] = self.class;
            file[at::DATA] = DATA_LITTLE_ENDIAN;
            file[at::VERSION] = VERSION_CURRENT;
            file[at::TYPE..at::TYPE + 2].copy_from_slice(&self.kind.to_le_bytes());
            file[at::MACHINE..at::MACHINE + 2].copy_from_slice(&self.machine.to_le_bytes());
            file[at::ENTRY..at::ENTRY + 8].copy_from_slice(&self.entry.to_le_bytes());
            file[at::PROGRAM_HEADER_OFFSET..at::PROGRAM_HEADER_OFFSET + 8]
                .copy_from_slice(&table.to_le_bytes());
            file[at::PROGRAM_HEADER_ENTRY_SIZE..at::PROGRAM_HEADER_ENTRY_SIZE + 2]
                .copy_from_slice(&self.entry_size.to_le_bytes());
            file[at::PROGRAM_HEADER_COUNT..at::PROGRAM_HEADER_COUNT + 2]
                .copy_from_slice(&(self.segments.len() as u16).to_le_bytes());
            for (index, (kind, flags, offset, file_size, address, memory_size)) in
                self.segments.iter().enumerate()
            {
                let at_header = table as usize + index * PROGRAM_HEADER_BYTES;
                let header = &mut file[at_header..at_header + PROGRAM_HEADER_BYTES];
                header[at::SEGMENT_TYPE..at::SEGMENT_TYPE + 4].copy_from_slice(&kind.to_le_bytes());
                header[at::SEGMENT_FLAGS..at::SEGMENT_FLAGS + 4]
                    .copy_from_slice(&flags.to_le_bytes());
                header[at::SEGMENT_FILE_OFFSET..at::SEGMENT_FILE_OFFSET + 8]
                    .copy_from_slice(&offset.to_le_bytes());
                header[at::SEGMENT_VIRTUAL_ADDRESS..at::SEGMENT_VIRTUAL_ADDRESS + 8]
                    .copy_from_slice(&address.to_le_bytes());
                header[at::SEGMENT_FILE_SIZE..at::SEGMENT_FILE_SIZE + 8]
                    .copy_from_slice(&file_size.to_le_bytes());
                header[at::SEGMENT_MEMORY_SIZE..at::SEGMENT_MEMORY_SIZE + 8]
                    .copy_from_slice(&memory_size.to_le_bytes());
            }
            file
        }
    }

    #[test]
    fn a_program_says_where_its_pieces_go() {
        let file = Builder::new()
            .code()
            // Read-only data, after the code.
            .segment((PT_LOAD, FLAG_READ, 0x1100, 0x50, 0x40_1000, 0x50))
            // And data with a .bss after it: more memory than file.
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_WRITE,
                0x1200,
                0x20,
                0x40_2000,
                0x80,
            ))
            .build();
        let program = parse(&file, USER_SPACE_END).expect("a program this kernel can load");

        assert_eq!(program.entry.as_u64(), 0x40_0000);
        assert_eq!(program.segment_count(), 3);
        let segments: Vec<&Segment> = program.segments().collect();

        assert!(segments[0].executable() && !segments[0].writable());
        assert_eq!(segments[0].at.as_u64(), 0x40_0000);
        assert_eq!(segments[0].zeroes(), 0);

        assert!(segments[1].readable() && !segments[1].writable() && !segments[1].executable());

        assert!(segments[2].writable() && !segments[2].executable());
        assert_eq!(segments[2].zeroes(), 0x60, "the .bss, which must be zeroed");
        assert_eq!(segments[2].end(), 0x40_2080);
        assert_eq!(program.highest_address(), 0x40_2080);
    }

    /// Anything that is not a segment to load is not read: notes, the
    /// program header table's own entry, the stack marker.
    #[test]
    fn only_loadable_segments_are_loaded() {
        let file = Builder::new()
            // PT_PHDR, PT_NOTE and PT_GNU_STACK, which a real linker emits.
            .segment((6, FLAG_READ, 0x40, 0x100, 0x40_0040, 0x100))
            .code()
            .segment((4, FLAG_READ, 0x1500, 0x20, 0, 0))
            .segment((0x6474_E551, FLAG_READ | FLAG_WRITE, 0, 0, 0, 0))
            .build();
        let program = parse(&file, USER_SPACE_END).expect("a program");
        assert_eq!(program.segment_count(), 1, "only the one to load");
        assert_eq!(program.entry.as_u64(), 0x40_0000);
    }

    #[test]
    fn what_is_not_a_program_this_kernel_loads() {
        assert_eq!(parse(&[], USER_SPACE_END), Err(ElfError::NotElf));
        assert_eq!(parse(&[0; 64], USER_SPACE_END), Err(ElfError::NotElf));
        assert_eq!(
            parse(&Builder::new().code().build()[..32], USER_SPACE_END),
            Err(ElfError::NotElf),
            "shorter than a header"
        );

        let mut thirty_two = Builder::new().code();
        thirty_two.class = 1;
        assert!(matches!(
            parse(&thirty_two.build(), USER_SPACE_END),
            Err(ElfError::NotSixtyFourBitLittleEndian { class: 1, .. })
        ));

        // A shared object, which would need relocating.
        let mut dynamic = Builder::new().code();
        dynamic.kind = 3;
        assert_eq!(
            parse(&dynamic.build(), USER_SPACE_END),
            Err(ElfError::NotAStaticExecutable { kind: 3 })
        );

        let mut arm = Builder::new().code();
        arm.machine = 0xB7;
        assert_eq!(
            parse(&arm.build(), USER_SPACE_END),
            Err(ElfError::NotX86_64 { machine: 0xB7 })
        );

        let mut odd_headers = Builder::new().code();
        odd_headers.entry_size = 32;
        assert!(matches!(
            parse(&odd_headers.build(), USER_SPACE_END),
            Err(ElfError::BadProgramHeaders { entry_size: 32, .. })
        ));

        assert_eq!(
            parse(&Builder::new().build(), USER_SPACE_END),
            Err(ElfError::BadProgramHeaders {
                entry_size: PROGRAM_HEADER_BYTES as u16,
                count: 0
            })
        );
    }

    /// Every number in a segment header becomes an address. These are the
    /// ways one can be wrong, each refused by name.
    #[test]
    fn a_segment_that_would_become_a_bad_address_is_refused() {
        // Contents past the end of the file.
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                0x1F00,
                0x1000,
                0x40_0000,
                0x1000,
            ))
            .build();
        assert!(matches!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentOutsideFile { .. })
        ));

        // An offset so large that adding the size wraps.
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                u64::MAX,
                0x1000,
                0x40_0000,
                0x1000,
            ))
            .build();
        assert!(matches!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentOutsideFile { .. })
        ));

        // Less memory than file: the rest would have nowhere to go.
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                0x1000,
                0x100,
                0x40_0000,
                0x10,
            ))
            .build();
        assert_eq!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentShrinks {
                file_size: 0x100,
                memory_size: 0x10
            })
        );

        // A segment in the kernel's half, which is a program asking to be
        // mapped over the kernel.
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                0x1000,
                0x100,
                0xFFFF_8000_0000_0000,
                0x100,
            ))
            .build();
        assert!(matches!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentOutsideUserSpace { .. })
        ));

        // And one that ends just past the line, which the one above would
        // not catch if the check used the start address alone.
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_EXECUTE,
                0x1000,
                0x100,
                USER_SPACE_END - 0x80,
                0x100,
            ))
            .build();
        assert!(matches!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentOutsideUserSpace { .. })
        ));
    }

    /// W^X is a property of the file. A segment asking for both is a
    /// program the kernel will not load, for the same reason the kernel
    /// does not do it to itself (ADR 0008).
    #[test]
    fn a_segment_may_not_be_written_and_executed() {
        let file = Builder::new()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_WRITE | FLAG_EXECUTE,
                0x1000,
                0x100,
                0x40_0000,
                0x100,
            ))
            .build();
        assert_eq!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::SegmentWritableAndExecutable { at: 0x40_0000 })
        );
    }

    /// A jump into nothing would fault in ring 3 with no clue why.
    #[test]
    fn an_entry_point_has_to_be_inside_something_executable() {
        let mut past = Builder::new().code();
        past.entry = 0x50_0000;
        assert_eq!(
            parse(&past.build(), USER_SPACE_END),
            Err(ElfError::EntryOutsideSegments { entry: 0x50_0000 })
        );

        // Inside a segment, but one that cannot be executed.
        let mut data = Builder::new();
        data.entry = 0x40_1000;
        let file = data
            .code()
            .segment((
                PT_LOAD,
                FLAG_READ | FLAG_WRITE,
                0x1100,
                0x50,
                0x40_1000,
                0x50,
            ))
            .build();
        assert_eq!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::EntryOutsideSegments { entry: 0x40_1000 })
        );

        // And the last byte of the code segment is inside it.
        let mut last = Builder::new().code();
        last.entry = 0x40_00FF;
        assert!(parse(&last.build(), USER_SPACE_END).is_ok());
        last.entry = 0x40_0100;
        assert!(parse(&last.build(), USER_SPACE_END).is_err(), "one past");
    }

    /// A table of program headers that does not fit in the file. Without
    /// the check, reading the first header reads past the end of it.
    #[test]
    fn a_program_header_table_outside_the_file_is_refused() {
        let mut file = Builder::new().code().build();
        // Point the table just past the end.
        let past = file.len() as u64;
        file[at::PROGRAM_HEADER_OFFSET..at::PROGRAM_HEADER_OFFSET + 8]
            .copy_from_slice(&past.to_le_bytes());
        assert_eq!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::Truncated {
                needed: past + PROGRAM_HEADER_BYTES as u64,
                length: file.len() as u64
            })
        );

        // And one that starts inside and runs off the end.
        let mut file = Builder::new().code().build();
        let nearly = file.len() as u64 - 8;
        file[at::PROGRAM_HEADER_OFFSET..at::PROGRAM_HEADER_OFFSET + 8]
            .copy_from_slice(&nearly.to_le_bytes());
        assert!(matches!(
            parse(&file, USER_SPACE_END),
            Err(ElfError::Truncated { .. })
        ));
    }

    /// A file whose segments are all of kinds this does not load has
    /// nothing to load, which is a different thing from one whose entry
    /// point is in the wrong place.
    #[test]
    fn a_program_with_nothing_to_load_says_so() {
        let file = Builder::new()
            .segment((6, FLAG_READ, 0x40, 0x100, 0x40_0040, 0x100))
            .segment((4, FLAG_READ, 0x1500, 0x20, 0, 0))
            .build();
        assert_eq!(parse(&file, USER_SPACE_END), Err(ElfError::NoSegments));
    }

    #[test]
    fn more_segments_than_the_loader_carries() {
        let mut builder = Builder::new().code();
        for index in 0..MAX_SEGMENTS as u64 {
            builder = builder.segment((
                PT_LOAD,
                FLAG_READ,
                0x1000,
                0x10,
                0x41_0000 + index * 0x1000,
                0x10,
            ));
        }
        assert!(matches!(
            parse(&builder.build(), USER_SPACE_END),
            Err(ElfError::TooManySegments { .. })
        ));
    }
}
