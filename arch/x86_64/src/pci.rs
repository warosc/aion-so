//! Reaching PCI configuration space on x86_64.
//!
//! Two ports: write which function and which dword to `0xCF8`, read the
//! four bytes from `0xCFC`. It is the old mechanism, and it reaches
//! everything this kernel needs — the first 256 bytes of every function of
//! every bus. The modern one, ECAM, is memory-mapped and needs ACPI to
//! find it, which is a subsystem this kernel does not have yet
//! (docs/adr/0022-fase4-pci-enumeration.md).
//!
//! Only the two instructions live here. What a header means and how the
//! bus is walked are in `hal::pci`, where they can be tested against a
//! machine that does not exist.
//!
//! Enumerating is reading. Nothing here writes to a device: not a BAR, not
//! the command register, not an interrupt line. The firmware assigned all
//! of that before handing the machine over, and redoing it would be a
//! fight with somebody who already finished.

use harlan_hal::pci::{Address, ConfigSpace, Devices, MAX_DEVICES};

/// Where the address of what to read goes.
const CONFIG_ADDRESS: u16 = 0xCF8;
/// And where the four bytes come back.
const CONFIG_DATA: u16 = 0xCFC;

/// Configuration space as this machine offers it.
pub struct Ports;

impl ConfigSpace for Ports {
    /// # Safety
    ///
    /// Writing `0xCF8` and reading `0xCFC` is how every PCI-capable x86
    /// machine answers this question, and the read has no effect on the
    /// device. The caller must be on a machine with a PCI host bridge —
    /// every machine this kernel boots on — and nothing else may be using
    /// these two ports in between, which in this kernel is nothing: the
    /// pair is written and read together.
    unsafe fn read_dword(&self, at: Address, offset: u8) -> u32 {
        // SAFETY: as this function's contract. The two accesses belong
        // together: the address selects what the data port reads.
        unsafe {
            crate::port::outl(CONFIG_ADDRESS, at.config_address(offset));
            crate::port::inl(CONFIG_DATA)
        }
    }
}

/// Every function on every bus.
///
/// # Safety
///
/// As `Ports::read_dword`. Called from the boot, before anything else
/// talks to a device.
pub unsafe fn scan() -> Devices {
    // SAFETY: forwarded from this function's contract.
    unsafe { harlan_hal::pci::scan(&Ports) }
}

/// How many functions a scan can keep, for whoever reports the result.
pub const KEPT_AT_MOST: usize = MAX_DEVICES;
