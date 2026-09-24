//! Starting the disk.
//!
//! Finding the device on the bus is done (ADR 0022). This is the rest of
//! the way to a device that is negotiated and waiting: read its
//! capabilities to learn where its registers are, map them **uncacheable**
//! into the kernel's device region, and go through the handshake the
//! specification lays out — reset, acknowledge, agree on features, and
//! check that the device did not take the agreement back
//! (docs/adr/0023-fase4-device-registers.md).
//!
//! It stops at `FEATURES_OK`: negotiated, with no queues. `DRIVER_OK` is
//! what says a driver is ready to send requests, and there is nothing to
//! send them through yet.
//!
//! This is where the kernel writes to a device for the first time. Reading
//! the bus was reading; a handshake is not, and from here a mistake in the
//! kernel can make hardware do something.

use harlan_arch_x86_64::paging::{KERNEL_DEVICES_START, KernelPageTable};
use harlan_hal::addr::{PhysAddr, VirtAddr};
use harlan_hal::frame::PhysFrame;
use harlan_hal::memory_map::CachePolicy;
use harlan_hal::paging::{MapError, PAGE_SIZE, Page, PageFlags, PageMapper};
use harlan_hal::pci::{self, Devices};
use harlan_hal::virtio::{self, CommonConfig};

use crate::memory::frame_allocator::FramePurpose;
use crate::memory::zeroed_frames::KernelFrames;

/// Why the disk could not be started. Every one of these is a fact about
/// the machine, said out loud rather than worked around: a driver that
/// guesses at a device it does not understand is worse than no driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// Nothing on the bus with virtio's vendor and a block device's id.
    NotFound,
    /// There is a virtio block device, and it answers with the legacy id:
    /// it is transitional, and speaks a protocol this driver refuses. The
    /// machine is meant to present a modern-only device (ADR 0023,
    /// point 2), so this means the machine is configured wrong.
    Transitional,
    /// No capability described a common configuration structure.
    NoCommonConfig,
    /// The structure is shorter than the registers the driver reads, so
    /// the last of them would be outside it.
    TooShort { length: u32 },
    /// The BAR the device named is not there.
    NoSuchBar { bar: usize },
    /// It is an I/O BAR, which is where a legacy device keeps its
    /// registers. Reading a port number as an address would map whatever
    /// is at that physical address instead.
    NotMemory { bar: usize },
    /// Its registers could not be mapped.
    Mapping(MapError),
    /// The device does not offer virtio 1.0, so there is nothing to agree
    /// on.
    NoVersion1 { offered: u64 },
    /// The device cleared `FEATURES_OK` again: it did not accept what the
    /// driver chose, and the specification says not to go on.
    FeaturesRefused { status: u8 },
}

/// A disk that has been found and negotiated.
pub struct Disk {
    pub at: pci::Address,
    /// Which BAR its common configuration turned out to be in, and where
    /// that ended up mapped: the two ends of the chain from a capability
    /// to a register the kernel can read.
    pub bar: usize,
    pub registers: VirtAddr,
    /// Its common configuration registers, mapped.
    pub common: CommonConfig,
    /// What the device offers, both halves of the feature space.
    pub offered: u64,
    /// What the driver agreed to.
    pub accepted: u64,
    /// Its BARs, kept because more than one structure lives in them and
    /// the notify one is found later.
    pub header_bars: [u32; 6],
    pub queues: u16,
    /// How many descriptors queue 0 holds, which is what the next
    /// increment has to fit a request into.
    pub queue_size: u16,
    /// Where to write to tell queue 0 it has something new.
    pub notify_offset: u16,
}

/// Maps the pages holding `length` bytes from physical `start` into the
/// device region, and answers where `start` itself can be reached.
///
/// Uncacheable and not executable, because they are registers and not
/// memory (ADR 0023, point 8). Only the pages the device's own capability
/// described: what it did not claim stays unreachable.
///
/// A page that is already mapped is not an error here — two structures of
/// one device commonly share a page, and the second one finding the first
/// one's mapping is the expected case.
///
/// # Safety
///
/// `start` must be where a device really keeps registers, as one of its
/// capabilities said, and the kernel must own its tables.
unsafe fn map_registers(
    mapper: &mut KernelPageTable,
    frames: &mut KernelFrames<'_>,
    start: u64,
    length: u32,
) -> Result<VirtAddr, MapError> {
    let flags = PageFlags::kernel(true, false).cached_as(CachePolicy::Uncacheable);
    let first = start & !(PAGE_SIZE - 1);
    let last = (start + u64::from(length) - 1) & !(PAGE_SIZE - 1);
    let mut at = first;
    while at <= last {
        let frame = PhysFrame::containing_address(PhysAddr::new(at));
        let page = Page::containing_address(KERNEL_DEVICES_START + at);
        // SAFETY: the frame is a device's registers, not memory the
        // allocator hands out, so nothing else can be given it; the page
        // is in the kernel's own region and mapped for nobody else.
        match unsafe { mapper.map(page, frame, flags, frames) } {
            Ok(()) | Err(MapError::AlreadyMapped) => {}
            Err(err) => return Err(err),
        }
        at += PAGE_SIZE;
    }
    Ok(KERNEL_DEVICES_START + start)
}

/// Tells the device the driver has given up, so that it is not left
/// half-negotiated, and hands back the reason.
///
/// # Safety
///
/// `common` must be mapped registers of a device this kernel was driving.
unsafe fn give_up(common: &CommonConfig, why: StartError) -> StartError {
    // SAFETY: as this function's contract.
    unsafe { common.set_status(virtio::STATUS_FAILED) };
    why
}

/// Finds the disk, maps its registers and negotiates virtio 1.0 with it.
///
/// # Safety
///
/// The kernel must own its page tables and reach frames through its own
/// window, `devices` must be a scan of the bus this machine really has,
/// and nothing else may be driving the same device.
pub unsafe fn start(
    mapper: &mut KernelPageTable,
    frames: &mut KernelFrames<'_>,
    devices: &Devices,
) -> Result<Disk, StartError> {
    let found = match devices.find(virtio::VENDOR, virtio::DEVICE_BLOCK) {
        Some(found) => found,
        None => {
            return Err(
                match devices.find(virtio::VENDOR, virtio::DEVICE_BLOCK_TRANSITIONAL) {
                    Some(_) => StartError::Transitional,
                    None => StartError::NotFound,
                },
            );
        }
    };

    // SAFETY: the address came from a scan of this machine's bus, and
    // reading configuration space has no effect on a device.
    let location =
        unsafe { virtio::common_config_location(&harlan_arch_x86_64::pci::Ports, found.at) }
            .ok_or(StartError::NoCommonConfig)?;
    if location.length < virtio::COMMON_CONFIG_BYTES {
        return Err(StartError::TooShort {
            length: location.length,
        });
    }

    let bar = pci::decode_bar(&found.header.bars, location.bar)
        .ok_or(StartError::NoSuchBar { bar: location.bar })?;
    let base = bar
        .memory_address()
        .ok_or(StartError::NotMemory { bar: location.bar })?;

    // SAFETY: the device said these are its registers, and the kernel owns
    // its tables (this function's contract).
    let registers = unsafe {
        map_registers(
            mapper,
            frames,
            base + u64::from(location.offset),
            location.length,
        )
    }
    .map_err(StartError::Mapping)?;
    // SAFETY: just mapped, uncacheable and writable, and it stays mapped
    // for the life of the kernel.
    let common = unsafe { CommonConfig::new(registers) };

    // The handshake, in the order the specification gives it. Out of
    // order, a device simply refuses, and the refusal is the only symptom.
    // SAFETY: `common` is this device's mapped registers and nothing else
    // is driving it (this function's contract).
    let (offered, accepted, queues, queue_size, notify_offset) = unsafe {
        // A reset first, whatever state the firmware left it in.
        common.set_status(0);
        common.set_status(virtio::STATUS_ACKNOWLEDGE);
        common.set_status(virtio::STATUS_ACKNOWLEDGE | virtio::STATUS_DRIVER);

        let low = common.device_features(0);
        let high = common.device_features(virtio::FEATURE_VERSION_1_SELECT);
        let offered = u64::from(low) | u64::from(high) << 32;
        if high & virtio::FEATURE_VERSION_1 == 0 {
            // SAFETY: as above.
            return Err(give_up(&common, StartError::NoVersion1 { offered }));
        }
        // Nothing else is accepted yet: every feature beyond version 1
        // changes what a request looks like, and there are no requests.
        common.set_driver_features(0, 0);
        common.set_driver_features(virtio::FEATURE_VERSION_1_SELECT, virtio::FEATURE_VERSION_1);
        let accepted = u64::from(virtio::FEATURE_VERSION_1) << 32;

        common.set_status(
            virtio::STATUS_ACKNOWLEDGE | virtio::STATUS_DRIVER | virtio::STATUS_FEATURES_OK,
        );
        // Read back: the device clears this bit when it does not accept
        // what was chosen, and going on regardless is how a driver ends up
        // talking to something that stopped listening.
        let status = common.status();
        if status & virtio::STATUS_FEATURES_OK == 0 {
            // SAFETY: as above.
            return Err(give_up(&common, StartError::FeaturesRefused { status }));
        }

        let queues = common.queue_count();
        common.select_queue(0);
        (
            offered,
            accepted,
            queues,
            common.queue_size(),
            common.queue_notify_offset(),
        )
    };

    Ok(Disk {
        at: found.at,
        header_bars: found.header.bars,
        bar: location.bar,
        registers,
        common,
        offered,
        accepted,
        queues,
        queue_size,
        notify_offset,
    })
}

// ---------------------------------------------------------------------
// The queue, and reading a sector (docs/adr/0024-fase4-dma-and-the-queue.md)
// ---------------------------------------------------------------------

/// How many descriptors the queue gets. A block request uses three —
/// header, data, status — and four is the smallest power of two that
/// holds them.
const QUEUE_SIZE: u16 = 4;
/// The one queue a block device has.
const QUEUE: u16 = 0;
/// How many times to look at the used ring before giving up on a device
/// that is not answering. A limit turns a hang into a line in the log
/// (ADR 0024, point 9).
const POLL_LIMIT: u32 = 20_000_000;

/// Which descriptors a request uses. With one request in flight there is
/// nothing to allocate: the chain is always these three.
const HEAD: u16 = 0;
const DATA: u16 = 1;
const STATUS: u16 = 2;

/// Where in the request frame each piece goes. The three are in one frame,
/// far enough apart that a device writing the data cannot reach the header
/// or the status byte by accident.
const HEADER_AT: u64 = 0;
const DATA_AT: u64 = 0x200;
const STATUS_AT: u64 = 0xE00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// No frame for the queue or for the request.
    OutOfMemory,
    /// The queue could not be laid out in one frame.
    QueueTooBig { size: u16 },
    /// The device offers fewer descriptors than a request needs, or none:
    /// a queue size of zero means the device does not have this queue.
    QueueTooSmall { offered: u16 },
    /// The device did not accept the size the driver asked for, so the
    /// driver's rings and the device's idea of them would not match.
    SizeRefused { asked: u16, taken: u16 },
    /// The device did not answer in time.
    NoAnswer,
    /// It answered about a chain that was not the one sent.
    WrongChain { head: u16 },
    /// It answered, and said the request failed.
    Failed { status: u8 },
    /// It said it wrote a different number of bytes than were asked for.
    ShortRead { written: u32 },
}

/// A disk with a queue, ready to be asked for sectors.
pub struct Reader {
    queue: virtio::Queue,
    notify: virtio::Notify,
    /// The request frame, as the kernel reads it and as the device does.
    request: *mut u8,
    request_phys: u64,
}

/// Gives the disk a queue and tells it the driver is ready.
///
/// Two frames: one for the queue's three rings, one for the request. Both
/// are labelled device memory, so that nothing can hand them out again as
/// a page table or a stack while the device may be writing them.
///
/// # Safety
///
/// `disk` must be a device this kernel negotiated and has not started, the
/// kernel must own its tables, and `frames` must reach frames through the
/// kernel's window.
pub unsafe fn start_queue(
    disk: &Disk,
    mapper: &mut KernelPageTable,
    frames: &mut KernelFrames<'_>,
) -> Result<Reader, ReadError> {
    if disk.queue_size < 3 {
        return Err(ReadError::QueueTooSmall {
            offered: disk.queue_size,
        });
    }
    let layout = virtio::QueueLayout::in_one_frame(QUEUE_SIZE, PAGE_SIZE)
        .ok_or(ReadError::QueueTooBig { size: QUEUE_SIZE })?;

    let ring_frame = frames
        .allocate_for(FramePurpose::Dma)
        .ok_or(ReadError::OutOfMemory)?;
    let request_frame = frames
        .allocate_for(FramePurpose::Dma)
        .ok_or(ReadError::OutOfMemory)?;
    // The device is given physical addresses; the kernel reads the same
    // memory through its window. Two views of one frame, on purpose.
    let window = frames.window();
    let ring_phys = ring_frame.start_address().as_u64();
    let request_phys = request_frame.start_address().as_u64();
    let ring_base = VirtAddr::new(window.frame_ptr(ring_frame) as u64);
    let request = window.frame_ptr(request_frame);

    // SAFETY: the frame is the kernel's, fresh from the allocator and
    // zeroed, reachable through the window, and laid out by `layout`.
    let queue = unsafe { virtio::Queue::new(ring_base, layout) };

    // Where the doorbell is. Without a notify structure the device would
    // never be told anything, so its absence is not something to shrug at.
    // SAFETY: reading configuration space has no effect on a device.
    let notify_location = unsafe {
        let mut found = None;
        virtio::locations(&harlan_arch_x86_64::pci::Ports, disk.at, |location| {
            if location.structure == virtio::Structure::Notify && found.is_none() {
                found = Some(location);
            }
        });
        found
    };
    let notify_location = notify_location.ok_or(ReadError::NoAnswer)?;
    let notify_bar = pci::decode_bar(&disk.header_bars, notify_location.bar)
        .and_then(|bar| bar.memory_address())
        .ok_or(ReadError::NoAnswer)?;
    // SAFETY: the device said these are its registers, and the kernel owns
    // its tables (this function's contract).
    let notify_base = unsafe {
        map_registers(
            mapper,
            frames,
            notify_bar + u64::from(notify_location.offset),
            notify_location.length,
        )
    }
    .map_err(|_| ReadError::NoAnswer)?;
    // SAFETY: just mapped, and the multiplier is the one this device's own
    // capability gave.
    let notify =
        unsafe { virtio::Notify::new(notify_base, notify_location.notify_multiplier.unwrap_or(0)) };

    // SAFETY: `disk.common` is this device's mapped registers, nothing else
    // drives it, and the queue is written before it is enabled.
    let taken = unsafe {
        disk.common.select_queue(QUEUE);
        disk.common.set_queue_size(QUEUE_SIZE);
        // Read back: a device that ignored the size would be looking at
        // rings of a different shape than the ones written here.
        let taken = disk.common.queue_size();
        if taken == QUEUE_SIZE {
            disk.common.set_queue_addresses(
                ring_phys + layout.descriptors,
                ring_phys + layout.available,
                ring_phys + layout.used,
            );
            disk.common.enable_queue();
            disk.common.set_status(
                virtio::STATUS_ACKNOWLEDGE
                    | virtio::STATUS_DRIVER
                    | virtio::STATUS_FEATURES_OK
                    | virtio::STATUS_DRIVER_OK,
            );
        }
        taken
    };
    if taken != QUEUE_SIZE {
        return Err(ReadError::SizeRefused {
            asked: QUEUE_SIZE,
            taken,
        });
    }

    Ok(Reader {
        queue,
        notify,
        request,
        request_phys,
    })
}

impl Reader {
    /// Reads one 512-byte sector into `into`.
    ///
    /// Three descriptors: the header the device reads, the data it writes,
    /// and one byte of status. They are separate descriptors because their
    /// permissions are — a device that could write the header could change
    /// what it was asked to do.
    ///
    /// # Safety
    ///
    /// The queue and the request frame must be this reader's own and no
    /// request may be in flight.
    pub unsafe fn read_sector(
        &mut self,
        disk: &Disk,
        sector: u64,
        into: &mut [u8; 512],
    ) -> Result<(), ReadError> {
        let header = virtio::block::header(virtio::block::TYPE_IN, sector);
        // SAFETY: the request frame is this reader's, reachable through the
        // kernel's window, and nothing else writes it.
        unsafe {
            self.request
                .add(HEADER_AT as usize)
                .copy_from_nonoverlapping(header.as_ptr(), header.len());
            // The status byte starts as something the device never writes,
            // so that "it worked" cannot be read off memory that was
            // already zero.
            self.request.add(STATUS_AT as usize).write_volatile(0xFF);
        }

        // SAFETY: the queue is this reader's and no request is in flight.
        let published = unsafe {
            self.queue.set_descriptor(
                HEAD,
                virtio::Descriptor {
                    address: self.request_phys + HEADER_AT,
                    length: virtio::block::HEADER_BYTES as u32,
                    flags: virtio::DESC_NEXT,
                    next: DATA,
                },
            );
            self.queue.set_descriptor(
                DATA,
                virtio::Descriptor {
                    address: self.request_phys + DATA_AT,
                    length: virtio::block::SECTOR_BYTES,
                    flags: virtio::DESC_NEXT | virtio::DESC_WRITE,
                    next: STATUS,
                },
            );
            self.queue.set_descriptor(
                STATUS,
                virtio::Descriptor {
                    address: self.request_phys + STATUS_AT,
                    length: 1,
                    flags: virtio::DESC_WRITE,
                    next: 0,
                },
            );
            self.queue.publish(HEAD)
        };

        // SAFETY: the chain is written and published; from here the device
        // may act on it.
        unsafe { self.notify.ring(disk.notify_offset, QUEUE) };

        // Wait for it, but not for ever: a device that never answers is a
        // line in the log, not a boot that stops here.
        let mut looks = 0;
        // SAFETY: the queue is this reader's.
        while unsafe { self.queue.used_index() } != published {
            looks += 1;
            if looks > POLL_LIMIT {
                return Err(ReadError::NoAnswer);
            }
            core::hint::spin_loop();
        }

        // SAFETY: the device has published this entry.
        let done = unsafe { self.queue.completion(published.wrapping_sub(1)) };
        if done.head != HEAD {
            return Err(ReadError::WrongChain { head: done.head });
        }
        // SAFETY: the request frame is this reader's and the device has
        // finished with it.
        let status = unsafe { self.request.add(STATUS_AT as usize).read_volatile() };
        if status != virtio::block::STATUS_OK {
            return Err(ReadError::Failed { status });
        }
        // The device counts the status byte it wrote as well as the data.
        if done.written != virtio::block::SECTOR_BYTES + 1 {
            return Err(ReadError::ShortRead {
                written: done.written,
            });
        }
        // SAFETY: as above; the sector is 512 bytes inside the frame.
        unsafe {
            into.as_mut_ptr()
                .copy_from_nonoverlapping(self.request.add(DATA_AT as usize), into.len())
        };
        Ok(())
    }
}
