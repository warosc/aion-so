//! Physical and virtual addresses, kept apart by the type system.
//!
//! Mixing the two is the classic kernel bug — writing a physical address
//! where the code meant a virtual one, and finding out only when the
//! machine faults or, worse, quietly writes somewhere else. They are both
//! 64-bit numbers, so nothing but a type can tell them apart.
//!
//! Turning either into a pointer is deliberately explicit (`VirtAddr::as_ptr`)
//! and only the code that knows a mapping exists should do it.

use core::fmt;
use core::ops::Add;

macro_rules! address_type {
    ($name:ident, $what:literal) => {
        #[doc = concat!("A ", $what, " address.")]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(addr: u64) -> Self {
                Self(addr)
            }

            pub const fn as_u64(self) -> u64 {
                self.0
            }

            pub const fn is_aligned_to(self, align: u64) -> bool {
                self.0.is_multiple_of(align)
            }

            /// Rounds down to a multiple of `align` (a power of two, as
            /// every alignment here is).
            pub const fn align_down(self, align: u64) -> Self {
                Self(self.0 - self.0 % align)
            }

            /// `None` on overflow: addresses come from firmware data and
            /// from arithmetic that must not wrap.
            pub const fn checked_add(self, offset: u64) -> Option<Self> {
                match self.0.checked_add(offset) {
                    Some(addr) => Some(Self(addr)),
                    None => None,
                }
            }

            /// How far `self` is past `earlier`, or 0 if it is not.
            pub const fn saturating_sub(self, earlier: Self) -> u64 {
                self.0.saturating_sub(earlier.0)
            }
        }

        impl Add<u64> for $name {
            type Output = Self;

            fn add(self, offset: u64) -> Self {
                Self(self.0 + offset)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:#x})"), self.0)
            }
        }

        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }
    };
}

address_type!(PhysAddr, "physical");
address_type!(VirtAddr, "virtual");

impl VirtAddr {
    /// The pointer this address names.
    ///
    /// Safe to build — nothing is read or written here — but only code that
    /// knows the address is mapped, and what lives there, may dereference
    /// it.
    pub const fn as_ptr<T>(self) -> *mut T {
        self.as_u64() as usize as *mut T
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_carry_their_number_and_print_it_in_hex() {
        let phys = PhysAddr::new(0x1234);
        assert_eq!(phys.as_u64(), 0x1234);
        assert_eq!(std::format!("{phys}"), "0x1234");
        assert_eq!(std::format!("{phys:?}"), "PhysAddr(0x1234)");
        assert_eq!(std::format!("{phys:#x}"), "0x1234");
        assert_eq!(std::format!("{phys:x}"), "1234");
    }

    #[test]
    fn alignment_is_checked_and_rounded_down() {
        assert!(PhysAddr::new(0x2000).is_aligned_to(0x1000));
        assert!(!PhysAddr::new(0x2001).is_aligned_to(0x1000));
        assert_eq!(
            VirtAddr::new(0x2FFF).align_down(0x1000),
            VirtAddr::new(0x2000)
        );
    }

    #[test]
    fn arithmetic_refuses_to_wrap_around() {
        assert_eq!(PhysAddr::new(0x1000) + 0x1000, PhysAddr::new(0x2000));
        assert_eq!(PhysAddr::new(u64::MAX).checked_add(1), None);
        assert_eq!(
            VirtAddr::new(0x3000).saturating_sub(VirtAddr::new(0x1000)),
            0x2000
        );
        assert_eq!(
            VirtAddr::new(0x1000).saturating_sub(VirtAddr::new(0x3000)),
            0
        );
    }

    #[test]
    fn a_virtual_address_names_a_pointer() {
        let mut value = 7u64;
        let addr = VirtAddr::new(&raw mut value as u64);
        // SAFETY: the address names this test's own local.
        assert_eq!(unsafe { *addr.as_ptr::<u64>() }, 7);
    }
}
