//! Hardware `Console` backend, valid after `ExitBootServices`: output is
//! drawn straight onto the GOP framebuffer (`harlan-fbcon`), input comes from
//! the PS/2 keyboard's IRQ-fed queue. Replaces `UefiConsole` (Fase 1) and
//! the invisible VGA-text placeholder (Incremento 2).
//!
//! x86_64-only for now, by way of the direct `harlan_arch_x86_64` import: an
//! honest compile error on another architecture rather than a quiet stub.

use harlan_arch_x86_64::keyboard::Keyboard;
use harlan_fbcon::{FramebufferSurface, TextConsole};
use harlan_hal::framebuffer::FramebufferInfo;
use harlan_hal::warn;
use harlan_hal::{Console, ConsoleKey};

pub struct HardwareConsole {
    /// `None` when the firmware gave us no usable framebuffer (or one whose
    /// description didn't add up): output is then dropped, but the kernel,
    /// the keyboard and the debugcon log all keep working.
    display: Option<TextConsole<FramebufferSurface>>,
    keyboard: Keyboard,
}

impl HardwareConsole {
    /// # Safety
    ///
    /// If `framebuffer` is `Some`, it must satisfy `FramebufferSurface::new`'s
    /// contract: describe memory that stays valid for volatile reads and
    /// writes for as long as this console lives, with nothing else writing
    /// to it. That holds for a GOP framebuffer captured before
    /// `ExitBootServices` and used only by this console afterwards.
    pub unsafe fn new(framebuffer: Option<FramebufferInfo>) -> Self {
        let display = framebuffer.and_then(|info| {
            // SAFETY: forwarded from this function's own contract.
            let surface = unsafe { FramebufferSurface::new(&info) };
            if surface.is_none() {
                warn!("HARLAN: framebuffer description is inconsistent; running without a display");
            }
            surface.map(TextConsole::new)
        });
        Self {
            display,
            keyboard: Keyboard::new(),
        }
    }
}

impl Console for HardwareConsole {
    fn read_key(&mut self) -> Option<ConsoleKey> {
        self.keyboard.read_key()
    }

    fn write_str(&mut self, s: &str) {
        if let Some(display) = &mut self.display {
            display.write_str(s);
        }
    }

    unsafe fn framebuffer_moved(&mut self, base: harlan_hal::addr::VirtAddr) {
        if let Some(display) = &mut self.display {
            // SAFETY: forwarded from this method's own contract.
            unsafe { display.surface_mut().rebase(base) };
        }
    }

    fn clear(&mut self) {
        if let Some(display) = &mut self.display {
            display.clear();
        }
    }
}
