//! Master System DMA Controller 0 (`DMAC0`, `0x4000_1000`).
//!
//! Used by Core 0 for fast memory-to-memory / flash scatter-load transfers
//! (`docs/core0_boot_and_memory_architecture.md`).

#![allow(dead_code)]

use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::DMAC0_BASE;

/// Owned handle to `DMAC0` (`0x4000_1000`).
pub struct Dmac0 {
    _private: (),
}

impl Dmac0 {
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }

    /// Reset and enable the `DMAC0` global controller (`App1 0x6000_0198`).
    pub fn reset_and_enable(&mut self) {
        // SAFETY: Exclusive ownership of DMAC0 registers guaranteed by Peripherals::take().
        unsafe {
            let ctl = DMAC0_BASE as *mut u32;
            core::ptr::write_volatile(ctl, 0);
            dsb();
            core::ptr::write_volatile(ctl, 0x8000_0000);
            dsb();
        }
    }
}
