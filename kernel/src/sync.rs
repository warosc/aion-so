//! Synchronization for data the kernel shares between its (single) flow of
//! execution and interrupt handlers.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use harlan_hal::InterruptControl;

/// Single-core lock: holding it keeps interrupts disabled, so no interrupt
/// handler can run in the middle of the critical section. On one core that
/// makes contention impossible except by re-entry (an interrupt handler or
/// the critical section itself taking the lock again). Re-entry panics
/// rather than spinning, because a spin with interrupts disabled would hang
/// the machine silently. Supporting several cores will mean spinning here.
pub struct IrqLock<I, T> {
    interrupts: I,
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: every access to `value` goes through a guard, and `locked`
// guarantees at most one guard exists at a time. `T: Send` because the
// value is reachable from whichever context takes the lock.
unsafe impl<I: Sync, T: Send> Sync for IrqLock<I, T> {}

impl<I: InterruptControl, T> IrqLock<I, T> {
    pub const fn new(interrupts: I, value: T) -> Self {
        Self {
            interrupts,
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> IrqLockGuard<'_, I, T> {
        let were_enabled = self.interrupts.disable();
        if self.locked.swap(true, Ordering::Acquire) {
            if were_enabled {
                self.interrupts.enable();
            }
            panic!("IrqLock re-entered: taken again by an interrupt handler or while already held");
        }
        IrqLockGuard {
            lock: self,
            were_enabled,
        }
    }
}

pub struct IrqLockGuard<'a, I: InterruptControl, T> {
    lock: &'a IrqLock<I, T>,
    /// Interrupts are re-enabled on release only if they were on before:
    /// nested critical sections must not turn them on early.
    were_enabled: bool,
}

impl<I: InterruptControl, T> Deref for IrqLockGuard<'_, I, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard is the only one in existence (see `lock`).
        unsafe { &*self.lock.value.get() }
    }
}

impl<I: InterruptControl, T> DerefMut for IrqLockGuard<'_, I, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: this guard is the only one in existence, and `&mut self`
        // makes this the only reference derived from it.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<I: InterruptControl, T> Drop for IrqLockGuard<'_, I, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.were_enabled {
            self.lock.interrupts.enable();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Interrupt flag of a pretend CPU, with a count of `enable` calls.
    #[derive(Default)]
    struct FakeInterrupts {
        enabled: Cell<bool>,
        enables: Cell<u32>,
    }

    impl InterruptControl for &FakeInterrupts {
        fn disable(&self) -> bool {
            self.enabled.replace(false)
        }

        fn enable(&self) {
            self.enabled.set(true);
            self.enables.set(self.enables.get() + 1);
        }

        fn are_enabled(&self) -> bool {
            self.enabled.get()
        }
    }

    #[test]
    fn holding_the_lock_keeps_interrupts_off_and_release_restores_them() {
        let cpu = FakeInterrupts::default();
        cpu.enabled.set(true);
        let lock = IrqLock::new(&cpu, 5);
        {
            let mut guard = lock.lock();
            assert!(!cpu.enabled.get());
            *guard += 1;
        }
        assert!(cpu.enabled.get());
        assert_eq!(*lock.lock(), 6);
    }

    #[test]
    fn release_leaves_interrupts_off_if_they_were_off() {
        let cpu = FakeInterrupts::default();
        let lock = IrqLock::new(&cpu, ());
        drop(lock.lock());
        assert!(!cpu.enabled.get());
        assert_eq!(cpu.enables.get(), 0);
    }

    #[test]
    fn nested_locks_turn_interrupts_back_on_only_at_the_outermost_release() {
        let cpu = FakeInterrupts::default();
        cpu.enabled.set(true);
        let (outer, inner) = (IrqLock::new(&cpu, ()), IrqLock::new(&cpu, ()));
        let outer_guard = outer.lock();
        drop(inner.lock());
        assert!(!cpu.enabled.get(), "inner release must not re-enable");
        drop(outer_guard);
        assert!(cpu.enabled.get());
        assert_eq!(cpu.enables.get(), 1);
    }

    #[test]
    #[should_panic(expected = "re-entered")]
    fn taking_a_held_lock_panics_instead_of_spinning() {
        let cpu = FakeInterrupts::default();
        let lock = IrqLock::new(&cpu, ());
        let _held = lock.lock();
        let _again = lock.lock();
    }
}
