//! The hardware the kernel drives.
//!
//! One device so far. The bus is enumerated elsewhere
//! (docs/adr/0022-fase4-pci-enumeration.md); what lives here is what
//! happens after a device has been found.

pub mod virtio_blk;
