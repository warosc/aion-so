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
