#![cfg_attr(not(test), no_std)]

pub mod addr;
mod console;
mod cpu;
pub mod frame;
pub mod framebuffer;
mod interrupts;
pub mod klog;
pub mod memory_map;
pub mod paging;
pub mod pe;
mod power;
mod timer;

pub use console::{Console, ConsoleKey};
pub use cpu::CpuControl;
pub use interrupts::InterruptControl;
pub use power::PowerControl;
pub use timer::TickCounter;
