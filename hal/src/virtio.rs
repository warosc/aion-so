//! Virtio over PCI, the modern way.
//!
//! A virtio device keeps its registers in memory and says where through
//! PCI capabilities: each one names a BAR, an offset into it and a length,
//! and a `cfg_type` saying which structure it describes
//! (docs/adr/0023-fase4-device-registers.md).
//!
//! Two things live here. The decoding of those capabilities, which is
//! pure. And the register structures themselves, which are unsafe to touch
//! but whose **layout** is not architecture-specific at all: given a base
//! address, every offset and every width is fixed by the specification, so
//! a host test can point one at a buffer and check that the right bytes
//! move. Getting an offset wrong is a mistake with no symptom — the device
//! simply does something else.
//!
//! This is virtio 1.0 only. Legacy virtio keeps its registers behind I/O
//! ports and its queue addresses in 32 bits; it is deprecated, and a
//! driver written for it would have to be thrown away (ADR 0023, point 1).

use crate::addr::VirtAddr;
use crate::pci::{Address, ConfigSpace};

/// Every virtio device answers with Red Hat's vendor id.
pub const VENDOR: u16 = 0x1AF4;
/// A modern virtio device's id is `0x1040` plus the device type. A block
/// device is type 2.
pub const DEVICE_BLOCK: u16 = 0x1042;
/// A transitional device answers with the legacy id instead, and speaks
/// both protocols. Recognised so the kernel can say so rather than fail
/// to find anything (ADR 0023, point 2).
pub const DEVICE_BLOCK_TRANSITIONAL: u16 = 0x1001;
/// The PCI capability id virtio uses: vendor-specific.
pub const CAPABILITY_ID: u8 = 0x09;

/// Which structure a virtio capability describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    /// Status, features, and which queue is being configured.
    Common,
    /// Where to write to tell the device a queue has something new.
    Notify,
    /// Why the device raised an interrupt.
    Isr,
    /// Whatever this kind of device has of its own — for a disk, its size.
    Device,
    /// A window for reaching the others through configuration space.
    /// Nothing here uses it.
    PciConfig,
    Unknown(u8),
}

impl Structure {
    const fn from_cfg_type(cfg_type: u8) -> Self {
        match cfg_type {
            1 => Structure::Common,
            2 => Structure::Notify,
            3 => Structure::Isr,
            4 => Structure::Device,
            5 => Structure::PciConfig,
            other => Structure::Unknown(other),
        }
    }
}

/// Where one of a device's structures lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub structure: Structure,
    /// Which of the six BARs it is in.
    pub bar: usize,
    /// Where in that BAR it starts, and how far it goes. The device says
    /// both; mapping more than the length would reach memory the device
    /// never claimed (ADR 0023, point 9).
    pub offset: u32,
    pub length: u32,
}

/// Decodes a virtio PCI capability from the four dwords at its offset.
///
/// The layout is fixed by the specification: the vendor id, the next
/// pointer and the length in the first three bytes, then `cfg_type`, then
/// which BAR, then a byte of id and two of padding, then the offset and
/// the length as little-endian dwords.
///
/// `None` when it is not a virtio capability, or names a BAR that cannot
/// exist, or describes nothing at all.
pub const fn decode_capability(words: &[u32; 4]) -> Option<Location> {
    if words[0] as u8 != CAPABILITY_ID {
        return None;
    }
    let bar = ((words[1] & 0xFF) as u8) as usize;
    if bar >= 6 {
        return None;
    }
    let length = words[3];
    if length == 0 {
        return None;
    }
    Some(Location {
        structure: Structure::from_cfg_type((words[0] >> 24) as u8),
        bar,
        offset: words[2],
        length,
    })
}

/// Every virtio structure a function describes, in the order the
/// capability list gives them.
///
/// Walks the list (`pci::capabilities`) and decodes the vendor-specific
/// entries; anything else in the list belongs to somebody else and is
/// passed over.
///
/// # Safety
///
/// As `ConfigSpace::read_dword`.
pub unsafe fn locations(space: &impl ConfigSpace, at: Address, mut each: impl FnMut(Location)) {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        crate::pci::capabilities(space, at, |capability| {
            if capability.id != CAPABILITY_ID {
                return;
            }
            let mut words = [0u32; 4];
            for (index, word) in words.iter_mut().enumerate() {
                // The capability is sixteen bytes from its own offset. An
                // offset near the end of configuration space would read
                // past it, so the whole of it has to fit.
                let Some(offset) = capability.offset.checked_add((index * 4) as u8) else {
                    return;
                };
                *word = space.read_dword(at, offset);
            }
            if let Some(location) = decode_capability(&words) {
                each(location);
            }
        })
    }
}

/// The one structure a driver cannot do without: where the common
/// configuration is, and long enough to hold the registers.
///
/// # Safety
///
/// As `locations`.
pub unsafe fn common_config_location(space: &impl ConfigSpace, at: Address) -> Option<Location> {
    let mut found = None;
    // SAFETY: forwarded from this function's contract.
    unsafe {
        locations(space, at, |location| {
            if location.structure == Structure::Common && found.is_none() {
                found = Some(location);
            }
        })
    };
    found
}

// ---------------------------------------------------------------------
// The device status register, which is the whole of the handshake
// ---------------------------------------------------------------------

/// The driver has noticed the device.
pub const STATUS_ACKNOWLEDGE: u8 = 1;
/// And knows how to drive it.
pub const STATUS_DRIVER: u8 = 2;
/// The device is ready to be used. Not set until the queues are up.
pub const STATUS_DRIVER_OK: u8 = 4;
/// The driver has finished choosing features and will not change them.
pub const STATUS_FEATURES_OK: u8 = 8;
/// The device gave up. A driver that sees this has to reset and start
/// again, or say so and stop.
pub const STATUS_DEVICE_NEEDS_RESET: u8 = 64;
/// Something went wrong on this side.
pub const STATUS_FAILED: u8 = 128;

/// Feature bit 32: the device speaks virtio 1.0. A modern driver must
/// negotiate it; without it there is nothing to talk about.
pub const FEATURE_VERSION_1: u32 = 1 << 0;
/// Which half of the 64-bit feature space bit 32 lives in.
pub const FEATURE_VERSION_1_SELECT: u32 = 1;

/// The common configuration structure, wherever it has been mapped.
///
/// Every access is volatile and of the width the specification gives. A
/// 16-bit register read as two bytes, or a write the compiler moves past
/// another, is a device doing something other than what was asked
/// (ADR 0023, point 10).
#[derive(Debug, Clone, Copy)]
pub struct CommonConfig {
    base: VirtAddr,
}

/// Offsets inside `virtio_pci_common_cfg`, from the specification.
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0C;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_ENABLE: u64 = 0x1C;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1E;
}

/// How many bytes of it the specification defines. A capability that
/// describes less than this is describing something else.
pub const COMMON_CONFIG_BYTES: u32 = 0x38;

impl CommonConfig {
    /// # Safety
    ///
    /// `base` must be the mapped start of a device's common configuration
    /// structure, uncacheable and writable, and must stay mapped for as
    /// long as this value is used. Nothing else may be driving the same
    /// device.
    pub const unsafe fn new(base: VirtAddr) -> Self {
        Self { base }
    }

    fn at<T>(&self, offset: u64) -> *mut T {
        (self.base + offset).as_ptr::<T>()
    }

    /// # Safety
    ///
    /// As `new`.
    pub unsafe fn status(&self) -> u8 {
        // SAFETY: inside the structure this was built over, and volatile
        // because the device writes it too.
        unsafe { self.at::<u8>(common::DEVICE_STATUS).read_volatile() }
    }

    /// # Safety
    ///
    /// As `new`. Writing this register is how a device is reset and how it
    /// is told the driver is ready: the order of the writes is the
    /// handshake, and getting it wrong leaves a device that refuses.
    pub unsafe fn set_status(&self, status: u8) {
        // SAFETY: as above.
        unsafe { self.at::<u8>(common::DEVICE_STATUS).write_volatile(status) }
    }

    /// The device's features, thirty-two bits at a time: `select` says
    /// which half.
    ///
    /// # Safety
    ///
    /// As `new`. The two accesses belong together — the select decides
    /// what the read answers — so nothing may come between them.
    pub unsafe fn device_features(&self, select: u32) -> u32 {
        // SAFETY: as above.
        unsafe {
            self.at::<u32>(common::DEVICE_FEATURE_SELECT)
                .write_volatile(select);
            self.at::<u32>(common::DEVICE_FEATURE).read_volatile()
        }
    }

    /// What the driver accepts, thirty-two bits at a time.
    ///
    /// # Safety
    ///
    /// As `device_features`.
    pub unsafe fn set_driver_features(&self, select: u32, features: u32) {
        // SAFETY: as above.
        unsafe {
            self.at::<u32>(common::DRIVER_FEATURE_SELECT)
                .write_volatile(select);
            self.at::<u32>(common::DRIVER_FEATURE)
                .write_volatile(features);
        }
    }

    /// # Safety
    ///
    /// As `new`.
    pub unsafe fn queue_count(&self) -> u16 {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::NUM_QUEUES).read_volatile() }
    }

    /// # Safety
    ///
    /// As `new`. The device's own configuration may change under a driver;
    /// this counter is how it says so.
    pub unsafe fn config_generation(&self) -> u8 {
        // SAFETY: as above.
        unsafe { self.at::<u8>(common::CONFIG_GENERATION).read_volatile() }
    }

    /// Chooses which queue the queue registers refer to.
    ///
    /// # Safety
    ///
    /// As `new`.
    pub unsafe fn select_queue(&self, queue: u16) {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_SELECT).write_volatile(queue) }
    }

    /// How many descriptors the selected queue holds. Zero means the
    /// device does not have that queue.
    ///
    /// # Safety
    ///
    /// As `new`, and a queue must have been selected.
    pub unsafe fn queue_size(&self) -> u16 {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_SIZE).read_volatile() }
    }

    /// Where to write to tell the device this queue has something new,
    /// as a multiple of the notify structure's stride.
    ///
    /// # Safety
    ///
    /// As `queue_size`.
    pub unsafe fn queue_notify_offset(&self) -> u16 {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_NOTIFY_OFF).read_volatile() }
    }

    /// Whether the selected queue is in use.
    ///
    /// # Safety
    ///
    /// As `queue_size`.
    pub unsafe fn queue_enabled(&self) -> bool {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_ENABLE).read_volatile() != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A virtio capability as QEMU really writes one: the common
    /// configuration structure, in BAR 4, at offset 0.
    #[test]
    fn a_capability_says_which_bar_and_where_in_it() {
        // vndr 0x09, next 0x60, len 0x10, cfg_type 1 (common).
        let words = [0x0110_6009, 0x0000_0004, 0x0000_0000, 0x0000_0038];
        assert_eq!(
            decode_capability(&words),
            Some(Location {
                structure: Structure::Common,
                bar: 4,
                offset: 0,
                length: 0x38,
            })
        );

        // The notify structure, further into the same BAR.
        let words = [0x0210_7009, 0x0000_0004, 0x0000_3000, 0x0000_1000];
        let notify = decode_capability(&words).expect("a notify structure");
        assert_eq!(notify.structure, Structure::Notify);
        assert_eq!((notify.offset, notify.length), (0x3000, 0x1000));
    }

    #[test]
    fn every_structure_the_specification_names() {
        for (cfg_type, expected) in [
            (1u8, Structure::Common),
            (2, Structure::Notify),
            (3, Structure::Isr),
            (4, Structure::Device),
            (5, Structure::PciConfig),
            (9, Structure::Unknown(9)),
        ] {
            let words = [0x0000_0009 | u32::from(cfg_type) << 24, 0, 0, 0x0000_0010];
            assert_eq!(
                decode_capability(&words).map(|l| l.structure),
                Some(expected),
                "cfg_type {cfg_type}"
            );
        }
    }

    #[test]
    fn what_is_not_a_virtio_capability() {
        // Somebody else's capability, chained in the same list.
        let words = [0x0000_0011, 0, 0, 0x10];
        assert_eq!(decode_capability(&words), None, "not vendor-specific");

        // A BAR that cannot exist.
        let words = [0x0110_0009, 0x0000_0006, 0, 0x10];
        assert_eq!(decode_capability(&words), None, "there is no BAR 6");

        // A structure of no length describes nothing.
        let words = [0x0110_0009, 0x0000_0004, 0, 0];
        assert_eq!(decode_capability(&words), None);
    }

    /// A device made of ordinary memory, so that a host test can check
    /// that every register is read and written where the specification
    /// puts it and at the width it says.
    struct FakeDevice {
        bytes: [u8; 0x40],
    }

    impl FakeDevice {
        fn new() -> Self {
            Self { bytes: [0; 0x40] }
        }

        fn config(&mut self) -> CommonConfig {
            let base = VirtAddr::new(self.bytes.as_mut_ptr() as u64);
            // SAFETY: the base is this test's own memory, writable and
            // alive for as long as the borrow lasts.
            unsafe { CommonConfig::new(base) }
        }

        fn byte(&self, offset: usize) -> u8 {
            self.bytes[offset]
        }

        fn word(&self, offset: usize) -> u16 {
            u16::from_le_bytes([self.bytes[offset], self.bytes[offset + 1]])
        }

        fn dword(&self, offset: usize) -> u32 {
            u32::from_le_bytes([
                self.bytes[offset],
                self.bytes[offset + 1],
                self.bytes[offset + 2],
                self.bytes[offset + 3],
            ])
        }
    }

    /// The handshake is written to one byte at `0x14`, and nothing else
    /// moves. A status written to the wrong offset is a device that never
    /// starts, with nothing in the log to say why.
    #[test]
    fn the_status_register_is_one_byte_at_its_own_offset() {
        let mut device = FakeDevice::new();
        let config = device.config();
        // SAFETY: the config is over this test's memory.
        unsafe {
            config.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        }
        assert_eq!(device.byte(0x14), 3);
        assert_eq!(device.byte(0x13), 0, "nothing below it");
        assert_eq!(device.byte(0x15), 0, "nothing above it");

        let config = device.config();
        // SAFETY: as above.
        assert_eq!(unsafe { config.status() }, 3, "and it reads back");

        // A reset is a zero written to the same place.
        // SAFETY: as above.
        unsafe { config.set_status(0) };
        assert_eq!(device.byte(0x14), 0);
    }

    /// Features are sixty-four bits reached thirty-two at a time, and the
    /// select register decides which half. Writing the select to the wrong
    /// offset would read the wrong half — and the half that matters,
    /// holding `VERSION_1`, is the second one.
    #[test]
    fn features_go_through_a_select_register() {
        let mut device = FakeDevice::new();
        let config = device.config();
        // SAFETY: as above.
        unsafe { config.set_driver_features(FEATURE_VERSION_1_SELECT, FEATURE_VERSION_1) };
        assert_eq!(device.dword(0x08), 1, "the driver's select");
        assert_eq!(device.dword(0x0C), 1, "and what it accepted");

        // Reading the device's features writes the other select.
        let config = device.config();
        // SAFETY: as above.
        let read = unsafe { config.device_features(1) };
        assert_eq!(device.dword(0x00), 1, "the device's select");
        assert_eq!(read, 0, "nothing offered by a device of zeroes");

        // The two selects are different registers: writing one must not
        // disturb the other.
        assert_eq!(device.dword(0x08), 1);
    }

    #[test]
    fn the_queue_registers_are_where_the_specification_puts_them() {
        let mut device = FakeDevice::new();
        let config = device.config();
        // SAFETY: as above.
        unsafe { config.select_queue(7) };
        assert_eq!(device.word(0x16), 7);

        // The ones the driver reads: sizes and offsets the device fills
        // in. Written here by hand, since this device answers nothing.
        device.bytes[0x12..0x14].copy_from_slice(&2u16.to_le_bytes());
        device.bytes[0x18..0x1A].copy_from_slice(&128u16.to_le_bytes());
        device.bytes[0x1C..0x1E].copy_from_slice(&1u16.to_le_bytes());
        device.bytes[0x1E..0x20].copy_from_slice(&3u16.to_le_bytes());
        device.bytes[0x15] = 9;
        let config = device.config();
        // SAFETY: as above.
        unsafe {
            assert_eq!(config.queue_count(), 2);
            assert_eq!(config.queue_size(), 128);
            assert!(config.queue_enabled());
            assert_eq!(config.queue_notify_offset(), 3);
            assert_eq!(config.config_generation(), 9);
        }
    }

    /// The structure the kernel maps has to be at least as long as the
    /// registers it reaches into, or the last of them is outside it.
    #[test]
    fn the_common_structure_is_long_enough_for_its_registers() {
        assert!(COMMON_CONFIG_BYTES as u64 > common::QUEUE_NOTIFY_OFF + 2);
        assert_eq!(COMMON_CONFIG_BYTES, 0x38, "what the specification defines");
    }

    /// A machine with one virtio function on it, so that the walk from
    /// the capability pointer to a decoded structure can be checked
    /// end to end.
    struct FakeFunction {
        space: [u32; 64],
    }

    impl ConfigSpace for FakeFunction {
        unsafe fn read_dword(&self, _at: Address, offset: u8) -> u32 {
            self.space[(offset / 4) as usize]
        }
    }

    fn virtio_function() -> FakeFunction {
        let mut space = [0u32; 64];
        space[0] = u32::from(VENDOR) | u32::from(DEVICE_BLOCK) << 16;
        // The status register's capability bit: bit 4 of the high half.
        space[1] = 1 << 20;
        space[0x34 / 4] = 0x40;
        // Common at 0x40, then somebody else's capability, then notify.
        space[0x40 / 4] = 0x09 | 0x50 << 8 | 0x10 << 16 | 0x01 << 24;
        space[0x44 / 4] = 0x04;
        space[0x48 / 4] = 0x0000_0000;
        space[0x4C / 4] = 0x0000_0038;
        // A power-management capability in the middle of the list.
        space[0x50 / 4] = 0x01 | 0x60 << 8;
        // And the notify structure, ending the list: its next pointer —
        // the second byte — stays zero, which is what "no more" is.
        space[0x60 / 4] = 0x09 | 0x14 << 16 | 0x02 << 24;
        space[0x64 / 4] = 0x04;
        space[0x68 / 4] = 0x0000_3000;
        space[0x6C / 4] = 0x0000_1000;
        FakeFunction { space }
    }

    #[test]
    fn the_structures_of_a_device_are_found_through_its_capabilities() {
        let function = virtio_function();
        let at = Address::new(0, 3, 0).unwrap();
        let mut found = Vec::new();
        // SAFETY: the fake function is ordinary memory.
        unsafe { locations(&function, at, |l| found.push(l)) };
        assert_eq!(
            found,
            vec![
                Location {
                    structure: Structure::Common,
                    bar: 4,
                    offset: 0,
                    length: 0x38
                },
                Location {
                    structure: Structure::Notify,
                    bar: 4,
                    offset: 0x3000,
                    length: 0x1000
                },
            ],
            "the two virtio ones, and not the power-management entry"
        );

        // SAFETY: as above.
        let common = unsafe { common_config_location(&function, at) };
        assert_eq!(common.map(|l| (l.bar, l.offset)), Some((4, 0)));
        assert!(
            common.map(|l| l.length).unwrap_or(0) >= COMMON_CONFIG_BYTES,
            "long enough for the registers the driver reaches"
        );
    }

    /// A device with no common configuration structure is one this driver
    /// cannot start, and saying so beats reading registers at a guess.
    #[test]
    fn a_device_without_a_common_structure_says_so() {
        let mut function = virtio_function();
        // Turn the common capability into a notify one.
        function.space[0x40 / 4] = 0x09 | 0x50 << 8 | 0x10 << 16 | 0x02 << 24;
        let at = Address::new(0, 3, 0).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { common_config_location(&function, at) }, None);
    }
}
