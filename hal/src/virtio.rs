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
    /// For a notify structure, the stride between queues' doorbells: a
    /// fifth dword the capability carries when it is long enough to. A
    /// multiplier of zero means every queue shares one doorbell and the
    /// value written is what tells them apart.
    ///
    /// `None` for every other structure, and for a notify capability too
    /// short to hold it — which is a device saying something the
    /// specification does not allow, so guessing would be worse than
    /// refusing to ring at all.
    pub notify_multiplier: Option<u32>,
}

/// How long a notify capability has to be to carry its multiplier: the
/// sixteen bytes every virtio capability has, plus one dword.
const NOTIFY_CAPABILITY_BYTES: u8 = 20;

/// Decodes a virtio PCI capability from the four dwords at its offset,
/// and the fifth if there was room to read one.
///
/// The layout is fixed by the specification: the vendor id, the next
/// pointer and the length in the first three bytes, then `cfg_type`, then
/// which BAR, then a byte of id and two of padding, then the offset and
/// the length as little-endian dwords. A notify capability has one dword
/// more, the multiplier, and says so in its length.
///
/// `None` when it is not a virtio capability, or names a BAR that cannot
/// exist, or describes nothing at all.
pub const fn decode_capability(words: &[u32; 4], fifth: Option<u32>) -> Option<Location> {
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
    let structure = Structure::from_cfg_type((words[0] >> 24) as u8);
    let capability_bytes = (words[0] >> 16) as u8;
    // The multiplier belongs to a notify capability and only when the
    // capability is long enough to hold one. Taking it from anywhere else
    // would be reading the next capability's first dword as a stride.
    let notify_multiplier = match (structure, fifth) {
        (Structure::Notify, Some(multiplier)) if capability_bytes >= NOTIFY_CAPABILITY_BYTES => {
            Some(multiplier)
        }
        _ => None,
    };
    Some(Location {
        structure,
        bar,
        offset: words[2],
        length,
        notify_multiplier,
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
            // And a notify capability has one dword more. Near the end of
            // configuration space there is no room for it, and then there
            // is no multiplier to be had.
            let fifth = capability
                .offset
                .checked_add(16)
                .map(|offset| space.read_dword(at, offset));
            if let Some(location) = decode_capability(&words, fifth) {
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
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
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
    /// Tells the device how many descriptors the driver will use. The
    /// device offers a maximum; a smaller power of two is allowed, and the
    /// caller must read `queue_size` back to see that it was taken (ADR
    /// 0024, point 7).
    ///
    /// # Safety
    ///
    /// As `new`, and a queue must have been selected and not enabled.
    pub unsafe fn set_queue_size(&self, size: u16) {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_SIZE).write_volatile(size) }
    }

    /// Where the three parts of the selected queue are, **physically**:
    /// the device does not walk page tables.
    ///
    /// # Safety
    ///
    /// As `set_queue_size`. The addresses must be memory the kernel owns
    /// and will not reuse while the device has the queue.
    pub unsafe fn set_queue_addresses(&self, descriptors: u64, available: u64, used: u64) {
        // SAFETY: as above.
        unsafe {
            self.at::<u64>(common::QUEUE_DESC)
                .write_volatile(descriptors);
            self.at::<u64>(common::QUEUE_DRIVER)
                .write_volatile(available);
            self.at::<u64>(common::QUEUE_DEVICE).write_volatile(used);
        }
    }

    /// Turns the selected queue on. Everything about it has to be written
    /// first: from here the device may read it.
    ///
    /// # Safety
    ///
    /// As `set_queue_addresses`.
    pub unsafe fn enable_queue(&self) {
        // SAFETY: as above.
        unsafe { self.at::<u16>(common::QUEUE_ENABLE).write_volatile(1) }
    }
}

/// Where to write to tell a device that one of its queues has something
/// new. One structure serves every queue, strided: a queue's doorbell is
/// at `its notify offset * the multiplier`.
#[derive(Debug, Clone, Copy)]
pub struct Notify {
    base: VirtAddr,
    multiplier: u32,
}

impl Notify {
    /// # Safety
    ///
    /// `base` must be a device's mapped notify structure, uncacheable and
    /// writable, and `multiplier` the one its own capability gave. A
    /// multiplier from somewhere else would ring a different queue's bell,
    /// or none.
    pub const unsafe fn new(base: VirtAddr, multiplier: u32) -> Self {
        Self { base, multiplier }
    }

    /// How far into the structure a queue's doorbell is.
    pub const fn doorbell(&self, notify_offset: u16) -> u64 {
        notify_offset as u64 * self.multiplier as u64
    }

    /// Rings it. What is written is which queue: the device looks at both
    /// the address and the value.
    ///
    /// # Safety
    ///
    /// As `new`, and everything the queue's entry describes must be
    /// written and published already — from here the device may act.
    pub unsafe fn ring(&self, notify_offset: u16, queue: u16) {
        let at = self.base + self.doorbell(notify_offset);
        // SAFETY: as this function's contract. Volatile, and after a fence
        // so that the ring's writes cannot be moved past the doorbell.
        unsafe {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            at.as_ptr::<u16>().write_volatile(queue);
        }
    }
}

/// What a block device's requests look like.
///
/// A request is three pieces in three descriptors: a header the device
/// reads, the data, and one byte of status the device writes. They are
/// separate because their permissions are: a device that could write the
/// header could change what it was asked to do.
pub mod block {
    /// Read from the disk into memory.
    pub const TYPE_IN: u32 = 0;
    /// Write from memory to the disk.
    pub const TYPE_OUT: u32 = 1;
    /// The header is sixteen bytes: type, a reserved word, and the sector.
    pub const HEADER_BYTES: usize = 16;
    /// Sectors are 512 bytes, whatever the disk's physical block size is:
    /// the `sector` field of a request is in these units and nothing else.
    pub const SECTOR_BYTES: u32 = 512;
    /// What the device writes into the status byte when it worked.
    pub const STATUS_OK: u8 = 0;
    pub const STATUS_IO_ERROR: u8 = 1;
    pub const STATUS_UNSUPPORTED: u8 = 2;

    /// The header for one request. Little-endian, because virtio is,
    /// whatever the machine is.
    pub const fn header(kind: u32, sector: u64) -> [u8; HEADER_BYTES] {
        let kind = kind.to_le_bytes();
        let sector = sector.to_le_bytes();
        [
            kind[0], kind[1], kind[2], kind[3],
            // The reserved word, which the specification says to zero.
            0, 0, 0, 0, sector[0], sector[1], sector[2], sector[3], sector[4], sector[5], sector[6],
            sector[7],
        ]
    }

    /// What the status byte means, for a log.
    pub const fn status_name(status: u8) -> &'static str {
        match status {
            STATUS_OK => "ok",
            STATUS_IO_ERROR => "an I/O error",
            STATUS_UNSUPPORTED => "unsupported",
            _ => "something the specification does not define",
        }
    }
}

// ---------------------------------------------------------------------
// The split virtqueue (docs/adr/0024-fase4-dma-and-the-queue.md)
// ---------------------------------------------------------------------

/// A descriptor is sixteen bytes: address, length, flags, next.
pub const DESCRIPTOR_BYTES: u64 = 16;
/// This descriptor is not the last of its chain.
pub const DESC_NEXT: u16 = 1;
/// The device writes this one; without it, the device reads it.
pub const DESC_WRITE: u16 = 2;

/// Where the three parts of a queue go inside one frame.
///
/// Virtio 1.0 lets the three live at independent addresses, each given to
/// the device by its own register, so they do not have to be packed the
/// way legacy virtio required. Putting them at round offsets of one frame
/// keeps every alignment the specification asks for — sixteen bytes for
/// the descriptors, two for the available ring, four for the used one —
/// obviously rather than arithmetically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLayout {
    pub size: u16,
    pub descriptors: u64,
    pub available: u64,
    pub used: u64,
}

/// Offsets inside the available ring.
pub mod available {
    /// Interrupt suppression. Left at zero.
    pub const FLAGS: u64 = 0;
    /// How many entries have ever been published. A window, not a count
    /// (ADR 0024, point 8).
    pub const INDEX: u64 = 2;
    /// The entries themselves, one `u16` each.
    pub const RING: u64 = 4;
}

/// Offsets inside the used ring.
pub mod used {
    pub const FLAGS: u64 = 0;
    /// How many entries the device has ever returned.
    pub const INDEX: u64 = 2;
    /// The entries, eight bytes each: the head descriptor's index, then
    /// how many bytes were written.
    pub const RING: u64 = 4;
    pub const ENTRY_BYTES: u64 = 8;
}

/// Where each part goes in the frame. Round numbers, so that every
/// alignment the specification asks for holds by inspection.
const DESCRIPTORS_AT: u64 = 0;
const AVAILABLE_AT: u64 = 0x400;
const USED_AT: u64 = 0x800;

/// The available ring can never reach the used one, so nothing checks it
/// at run time: the descriptor table bounds the queue size at
/// `AVAILABLE_AT / DESCRIPTOR_BYTES`, and a ring of that many entries is
/// far shorter than the gap between the two. A fact about these three
/// constants, so the compiler is the right thing to check it — and if one
/// of them ever moves, this stops the build instead of overlapping two
/// rings quietly.
const _AVAILABLE_RING_ENDS_BEFORE_THE_USED_ONE: () =
    assert!(AVAILABLE_AT + available::RING + 2 * (AVAILABLE_AT / DESCRIPTOR_BYTES) + 2 <= USED_AT);

impl QueueLayout {
    /// The layout of a queue of `size` descriptors in one frame.
    ///
    /// `None` unless `size` is a power of two — the specification requires
    /// it, because the ring index is taken modulo the size — and unless
    /// the descriptor table and the used ring fit where they have to.
    pub const fn in_one_frame(size: u16, frame_bytes: u64) -> Option<Self> {
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        let size64 = size as u64;
        // The descriptor table has to end before the available ring, and
        // the used ring before the frame does.
        if size64 * DESCRIPTOR_BYTES > AVAILABLE_AT
            || USED_AT + used::RING + used::ENTRY_BYTES * size64 + 2 > frame_bytes
        {
            return None;
        }
        Some(Self {
            size,
            descriptors: DESCRIPTORS_AT,
            available: AVAILABLE_AT,
            used: USED_AT,
        })
    }

    /// Where the descriptor at `index` starts. `None` past the end of the
    /// table: an index the queue does not have would otherwise be written
    /// over whatever follows.
    pub const fn descriptor(self, index: u16) -> Option<u64> {
        if index >= self.size {
            return None;
        }
        Some(self.descriptors + index as u64 * DESCRIPTOR_BYTES)
    }

    /// Where the available ring's slot for `index` is. The index is a
    /// window over the ring, so it wraps.
    pub const fn available_slot(self, index: u16) -> u64 {
        self.available + available::RING + (index % self.size) as u64 * 2
    }

    /// And the used ring's.
    pub const fn used_slot(self, index: u16) -> u64 {
        self.used + used::RING + (index % self.size) as u64 * used::ENTRY_BYTES
    }
}

/// One descriptor, as the driver fills it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    /// **Physical**, because the device does not walk page tables.
    pub address: u64,
    pub length: u32,
    pub flags: u16,
    /// The next descriptor of this chain, when `DESC_NEXT` is set.
    pub next: u16,
}

/// What the device returned: which chain, and how much it wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    pub head: u16,
    pub written: u32,
}

/// A queue the kernel can put requests in, wherever it has been laid out.
///
/// `base` is where the frame holding it can be **read and written by the
/// kernel**; the addresses inside descriptors are physical, because they
/// are for the device. The two are different views of the same memory and
/// that is the point (ADR 0024, point 2).
#[derive(Debug, Clone, Copy)]
pub struct Queue {
    base: VirtAddr,
    layout: QueueLayout,
}

impl Queue {
    /// # Safety
    ///
    /// `base` must be a frame the kernel owns, mapped readable and
    /// writable, laid out as `layout` says and given to no one but this
    /// device.
    pub const unsafe fn new(base: VirtAddr, layout: QueueLayout) -> Self {
        Self { base, layout }
    }

    pub const fn layout(&self) -> QueueLayout {
        self.layout
    }

    fn at<T>(&self, offset: u64) -> *mut T {
        (self.base + offset).as_ptr::<T>()
    }

    /// Writes one descriptor. Answers whether there is such a descriptor.
    ///
    /// # Safety
    ///
    /// As `new`, and the descriptor must not be one the device is using.
    pub unsafe fn set_descriptor(&self, index: u16, descriptor: Descriptor) -> bool {
        let Some(offset) = self.layout.descriptor(index) else {
            return false;
        };
        // SAFETY: inside the descriptor table of the frame this was built
        // over, and volatile because the device reads the same bytes.
        unsafe {
            self.at::<u64>(offset).write_volatile(descriptor.address);
            self.at::<u32>(offset + 8).write_volatile(descriptor.length);
            self.at::<u16>(offset + 12).write_volatile(descriptor.flags);
            self.at::<u16>(offset + 14).write_volatile(descriptor.next);
        }
        true
    }

    /// Puts the chain starting at `head` in the available ring and
    /// publishes it, answering the index that was published.
    ///
    /// The order is the protocol: the slot is written, then the index that
    /// makes it visible. The device may be looking in between, so a
    /// compiler fence separates them (ADR 0024, point 11).
    ///
    /// # Safety
    ///
    /// As `new`, and the chain must be written already.
    pub unsafe fn publish(&self, head: u16) -> u16 {
        // SAFETY: as this function's contract.
        unsafe {
            let index = self
                .at::<u16>(self.layout.available + available::INDEX)
                .read_volatile();
            self.at::<u16>(self.layout.available_slot(index))
                .write_volatile(head);
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.at::<u16>(self.layout.available + available::INDEX)
                .write_volatile(index.wrapping_add(1));
            index.wrapping_add(1)
        }
    }

    /// How many entries the device has returned, ever.
    ///
    /// # Safety
    ///
    /// As `new`.
    pub unsafe fn used_index(&self) -> u16 {
        // SAFETY: as above; the device writes this, so the read is
        // volatile and reordering it would be reading a stale answer.
        unsafe {
            let index = self
                .at::<u16>(self.layout.used + used::INDEX)
                .read_volatile();
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            index
        }
    }

    /// The entry the device wrote at `index`.
    ///
    /// # Safety
    ///
    /// As `new`, and `index` must be one `used_index` has reached: an
    /// entry the device has not written yet holds whatever was there.
    pub unsafe fn completion(&self, index: u16) -> Completion {
        let offset = self.layout.used_slot(index);
        // SAFETY: as this function's contract.
        unsafe {
            Completion {
                head: self.at::<u32>(offset).read_volatile() as u16,
                written: self.at::<u32>(offset + 4).read_volatile(),
            }
        }
    }

    /// The available ring's published index, for a test or a log.
    ///
    /// # Safety
    ///
    /// As `new`.
    pub unsafe fn available_index(&self) -> u16 {
        // SAFETY: as above.
        unsafe {
            self.at::<u16>(self.layout.available + available::INDEX)
                .read_volatile()
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;

    /// A virtio capability as QEMU really writes one: the common
    /// configuration structure, in BAR 4, at offset 0.
    #[test]
    fn a_capability_says_which_bar_and_where_in_it() {
        // vndr 0x09, next 0x60, len 0x10, cfg_type 1 (common).
        let words = [0x0110_6009, 0x0000_0004, 0x0000_0000, 0x0000_0038];
        assert_eq!(
            decode_capability(&words, None),
            Some(Location {
                structure: Structure::Common,
                bar: 4,
                offset: 0,
                length: 0x38,
                notify_multiplier: None,
            })
        );
        // A common capability is not sixteen bytes long *and* carrying a
        // multiplier: a fifth dword read after it belongs to whatever
        // comes next, and taking it as a stride would ring nothing.
        assert_eq!(
            decode_capability(&words, Some(4)).and_then(|l| l.notify_multiplier),
            None,
            "the multiplier is a notify capability's alone"
        );

        // The notify structure, further into the same BAR.
        // A notify capability of twenty bytes carries a multiplier; the
        // same capability declaring only sixteen does not, whatever is in
        // the memory after it.
        let words = [0x0214_7009, 0x0000_0004, 0x0000_3000, 0x0000_1000];
        let notify = decode_capability(&words, Some(4)).expect("a notify structure");
        assert_eq!(notify.structure, Structure::Notify);
        assert_eq!((notify.offset, notify.length), (0x3000, 0x1000));
        assert_eq!(notify.notify_multiplier, Some(4));

        let short = [0x0210_7009, 0x0000_0004, 0x0000_3000, 0x0000_1000];
        assert_eq!(
            decode_capability(&short, Some(4)).and_then(|l| l.notify_multiplier),
            None,
            "it said it was only sixteen bytes long"
        );
        assert_eq!(
            decode_capability(&words, None).and_then(|l| l.notify_multiplier),
            None,
            "and there was no room to read one"
        );
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
                decode_capability(&words, None).map(|l| l.structure),
                Some(expected),
                "cfg_type {cfg_type}"
            );
        }
    }

    #[test]
    fn what_is_not_a_virtio_capability() {
        // Somebody else's capability, chained in the same list.
        let words = [0x0000_0011, 0, 0, 0x10];
        assert_eq!(decode_capability(&words, None), None, "not vendor-specific");

        // A BAR that cannot exist.
        let words = [0x0110_0009, 0x0000_0006, 0, 0x10];
        assert_eq!(decode_capability(&words, None), None, "there is no BAR 6");

        // A structure of no length describes nothing.
        let words = [0x0110_0009, 0x0000_0004, 0, 0];
        assert_eq!(decode_capability(&words, None), None);
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

        fn qword(&self, offset: usize) -> u64 {
            u64::from(self.dword(offset)) | u64::from(self.dword(offset + 4)) << 32
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
        // the second byte — stays zero, which is what "no more" is. It
        // declares twenty bytes, so it carries a multiplier.
        space[0x60 / 4] = 0x09 | 0x14 << 16 | 0x02 << 24;
        space[0x64 / 4] = 0x04;
        space[0x68 / 4] = 0x0000_3000;
        space[0x6C / 4] = 0x0000_1000;
        space[0x70 / 4] = 0x0000_0004;
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
                    length: 0x38,
                    notify_multiplier: None
                },
                Location {
                    structure: Structure::Notify,
                    bar: 4,
                    offset: 0x3000,
                    length: 0x1000,
                    notify_multiplier: Some(4)
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

    /// Where the three parts of a queue go, and what will not fit.
    #[test]
    fn a_queue_of_four_fits_in_a_frame_with_room_to_spare() {
        let layout = QueueLayout::in_one_frame(4, 4096).expect("four fits");
        assert_eq!(layout.descriptors, 0);
        assert_eq!(layout.available, 0x400);
        assert_eq!(layout.used, 0x800);
        assert_eq!(layout.size, 4);

        // Every part ends before the next begins. The middle one holds
        // for every size this can answer with, which is why the compiler
        // checks it instead of `in_one_frame`.
        assert!(4 * DESCRIPTOR_BYTES <= layout.available);
        assert!(layout.available + available::RING + 2 * 4 + 2 <= layout.used);
        assert!(layout.used + used::RING + used::ENTRY_BYTES * 4 + 2 <= 4096);
        // The largest queue the descriptor table allows, to show the
        // available ring still ends before the used one.
        let biggest = (AVAILABLE_AT / DESCRIPTOR_BYTES) as u16;
        let layout = QueueLayout::in_one_frame(biggest, 8192).expect("the biggest that fits");
        assert!(layout.available + available::RING + 2 * u64::from(biggest) + 2 <= layout.used);

        // The specification requires a power of two, because the ring
        // index is taken modulo the size.
        assert_eq!(QueueLayout::in_one_frame(3, 4096), None);
        assert_eq!(QueueLayout::in_one_frame(0, 4096), None);
        assert!(QueueLayout::in_one_frame(64, 4096).is_some());
        // A queue whose descriptors would run into the available ring.
        assert_eq!(QueueLayout::in_one_frame(256, 4096), None);
        assert_eq!(QueueLayout::in_one_frame(128, 4096), None);
        // And a frame smaller than the layout assumes, which is the only
        // way the used ring runs past the end: for any size the
        // descriptor table allows, it fits in a frame of 4 KiB.
        assert_eq!(QueueLayout::in_one_frame(4, 2048), None);
        assert_eq!(QueueLayout::in_one_frame(4, 0), None);
    }

    #[test]
    fn a_descriptor_index_past_the_end_is_refused() {
        let layout = QueueLayout::in_one_frame(4, 4096).unwrap();
        assert_eq!(layout.descriptor(0), Some(0));
        assert_eq!(layout.descriptor(3), Some(48));
        assert_eq!(layout.descriptor(4), None, "there is no descriptor 4");
        assert_eq!(layout.descriptor(u16::MAX), None);
    }

    /// The index is a window over the ring, not a counter: it grows for
    /// ever and wraps at sixteen bits. Treating it as a counter works for
    /// the first sixty-five thousand requests.
    #[test]
    fn the_ring_indices_wrap_around_the_ring() {
        let layout = QueueLayout::in_one_frame(4, 4096).unwrap();
        assert_eq!(layout.available_slot(0), 0x404);
        assert_eq!(layout.available_slot(3), 0x40A);
        assert_eq!(
            layout.available_slot(4),
            layout.available_slot(0),
            "round again"
        );
        assert_eq!(layout.available_slot(u16::MAX), layout.available_slot(3));

        assert_eq!(layout.used_slot(0), 0x804);
        assert_eq!(layout.used_slot(1), 0x80C);
        assert_eq!(layout.used_slot(4), layout.used_slot(0));
    }

    /// A frame of ordinary memory, so that a host test can lay a queue out
    /// in it, play the device's part by hand, and read the bytes back.
    struct FakeQueue {
        frame: alloc::boxed::Box<[u8; 4096]>,
        layout: QueueLayout,
    }

    impl FakeQueue {
        fn new(size: u16) -> Self {
            Self {
                frame: alloc::boxed::Box::new([0; 4096]),
                layout: QueueLayout::in_one_frame(size, 4096).expect("a size that fits"),
            }
        }

        fn queue(&mut self) -> Queue {
            let base = VirtAddr::new(self.frame.as_mut_ptr() as u64);
            // SAFETY: the frame is this test's own, writable, and laid out
            // by `layout`.
            unsafe { Queue::new(base, self.layout) }
        }

        fn word(&self, offset: u64) -> u16 {
            u16::from_le_bytes([self.frame[offset as usize], self.frame[offset as usize + 1]])
        }

        fn dword(&self, offset: u64) -> u32 {
            let at = offset as usize;
            u32::from_le_bytes([
                self.frame[at],
                self.frame[at + 1],
                self.frame[at + 2],
                self.frame[at + 3],
            ])
        }

        fn qword(&self, offset: u64) -> u64 {
            u64::from(self.dword(offset)) | u64::from(self.dword(offset + 4)) << 32
        }

        /// What the device does: take the chain, write an entry into the
        /// used ring and bump its index.
        fn device_completes(&mut self, head: u16, written: u32) {
            let index = self.word(self.layout.used + used::INDEX);
            let slot = self.layout.used_slot(index) as usize;
            self.frame[slot..slot + 4].copy_from_slice(&u32::from(head).to_le_bytes());
            self.frame[slot + 4..slot + 8].copy_from_slice(&written.to_le_bytes());
            let at = (self.layout.used + used::INDEX) as usize;
            self.frame[at..at + 2].copy_from_slice(&index.wrapping_add(1).to_le_bytes());
        }
    }

    /// A descriptor is four fields at fixed offsets, and the address in it
    /// is sixty-four bits because it is physical.
    #[test]
    fn a_descriptor_is_written_field_by_field() {
        let mut queue = FakeQueue::new(4);
        let handle = queue.queue();
        // SAFETY: the queue is over this test's memory.
        assert!(unsafe {
            handle.set_descriptor(
                1,
                Descriptor {
                    address: 0x1_2345_6000,
                    length: 512,
                    flags: DESC_NEXT | DESC_WRITE,
                    next: 2,
                },
            )
        });
        assert_eq!(queue.qword(16), 0x1_2345_6000, "a 64-bit address");
        assert_eq!(queue.dword(16 + 8), 512);
        assert_eq!(queue.word(16 + 12), 3);
        assert_eq!(queue.word(16 + 14), 2);
        // And nothing outside that descriptor moved.
        assert_eq!(queue.qword(0), 0, "descriptor 0 is untouched");
        assert_eq!(queue.qword(32), 0, "and so is descriptor 2");

        // A descriptor the queue does not have is refused rather than
        // written over whatever follows the table.
        let handle = queue.queue();
        // SAFETY: as above.
        assert!(!unsafe {
            handle.set_descriptor(
                4,
                Descriptor {
                    address: 1,
                    length: 1,
                    flags: 0,
                    next: 0,
                },
            )
        });
        assert_eq!(queue.qword(64), 0, "nothing past the table was written");
    }

    /// Publishing is two writes in an order that matters: the slot, then
    /// the index that makes it visible.
    #[test]
    fn publishing_puts_the_chain_in_the_ring_and_then_says_so() {
        let mut queue = FakeQueue::new(4);
        let handle = queue.queue();
        // SAFETY: as above.
        let published = unsafe { handle.publish(0) };
        assert_eq!(published, 1, "one entry has ever been published");
        assert_eq!(queue.word(queue.layout.available_slot(0)), 0);
        assert_eq!(queue.word(queue.layout.available + available::INDEX), 1);

        // The next one goes in the next slot.
        let handle = queue.queue();
        // SAFETY: as above.
        assert_eq!(unsafe { handle.publish(3) }, 2);
        assert_eq!(queue.word(queue.layout.available_slot(1)), 3);
        assert_eq!(queue.word(queue.layout.available + available::INDEX), 2);
        assert_eq!(
            queue.word(queue.layout.available_slot(0)),
            0,
            "the first is still there"
        );
    }

    /// And collecting is reading what the device wrote, where it wrote it.
    #[test]
    fn a_completion_is_read_from_where_the_device_put_it() {
        let mut queue = FakeQueue::new(4);
        let handle = queue.queue();
        // SAFETY: as above.
        assert_eq!(unsafe { handle.used_index() }, 0, "nothing done yet");

        queue.device_completes(0, 513);
        let handle = queue.queue();
        // SAFETY: as above.
        unsafe {
            assert_eq!(handle.used_index(), 1);
            let done = handle.completion(0);
            assert_eq!(done.head, 0);
            assert_eq!(done.written, 513);
        }

        // A second one, in the next slot.
        queue.device_completes(2, 1);
        let handle = queue.queue();
        // SAFETY: as above.
        unsafe {
            assert_eq!(handle.used_index(), 2);
            assert_eq!(
                handle.completion(1),
                Completion {
                    head: 2,
                    written: 1
                }
            );
        }
    }

    #[test]
    fn the_queue_registers_are_written_where_the_device_reads_them() {
        let mut device = FakeDevice::new();
        let config = device.config();
        // SAFETY: the config is over this test's memory.
        unsafe {
            config.set_queue_size(4);
            config.set_queue_addresses(0x1_0000, 0x1_0400, 0x1_0800);
            config.enable_queue();
        }
        assert_eq!(device.word(0x18), 4, "the size");
        assert_eq!(device.qword(0x20), 0x1_0000, "the descriptors");
        assert_eq!(device.qword(0x28), 0x1_0400, "the available ring");
        assert_eq!(device.qword(0x30), 0x1_0800, "the used ring");
        assert_eq!(device.word(0x1C), 1, "and it is on");
    }

    /// One notify structure serves every queue, strided by the multiplier
    /// the device's own capability gave. A multiplier from somewhere else
    /// rings a different queue's bell, or none at all.
    #[test]
    fn a_doorbell_is_strided_by_the_multiplier() {
        let mut bells = [0u8; 64];
        let base = VirtAddr::new(bells.as_mut_ptr() as u64);
        // SAFETY: this test's own memory.
        let notify = unsafe { Notify::new(base, 4) };
        assert_eq!(notify.doorbell(0), 0);
        assert_eq!(notify.doorbell(3), 12);

        // SAFETY: as above.
        unsafe { notify.ring(3, 1) };
        assert_eq!(
            u16::from_le_bytes([bells[12], bells[13]]),
            1,
            "which queue, at that queue's bell"
        );
        assert_eq!(bells[0], 0, "and nobody else's bell rang");

        // A multiplier of zero is what QEMU uses when every queue shares
        // one bell, and the value written is what tells them apart.
        let base = VirtAddr::new(bells.as_mut_ptr() as u64);
        // SAFETY: as above.
        let shared = unsafe { Notify::new(base, 0) };
        assert_eq!(shared.doorbell(7), 0);
    }

    #[test]
    fn a_block_request_header_says_what_and_where() {
        let header = block::header(block::TYPE_IN, 0);
        assert_eq!(&header[0..4], &[0, 0, 0, 0], "a read");
        assert_eq!(&header[4..8], &[0, 0, 0, 0], "the reserved word");
        assert_eq!(&header[8..16], &[0; 8], "sector zero");

        // A sector number that needs more than one byte, to catch an
        // endianness the wrong way round.
        let header = block::header(block::TYPE_OUT, 0x1234);
        assert_eq!(u32::from_le_bytes(header[0..4].try_into().unwrap()), 1);
        assert_eq!(
            u64::from_le_bytes(header[8..16].try_into().unwrap()),
            0x1234
        );
        assert_eq!(header[8], 0x34, "little-endian, whatever the machine is");
        assert_eq!(header[9], 0x12);
        assert_eq!(header.len(), block::HEADER_BYTES);
    }

    #[test]
    fn a_status_byte_has_a_name() {
        assert_eq!(block::status_name(block::STATUS_OK), "ok");
        assert_eq!(block::status_name(block::STATUS_IO_ERROR), "an I/O error");
        assert_eq!(block::status_name(block::STATUS_UNSUPPORTED), "unsupported");
        assert!(block::status_name(99).contains("does not define"));
    }
}
