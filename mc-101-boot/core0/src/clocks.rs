//! Core 0 System Clock Query & 44.1 kHz Audio PLL Configuration.
//!
//! Main CPU, bus, and SDRAM clocks are configured by the Boot ROM and `App0`
//! bootloader before entering Core 0 at `0x6000_0001`. The active system clock
//! frequency in Hz is published at `SCI_BASE + 0x30` (`0x4002_6030`).
//!
//! The 44.1 kHz fractional audio PLL (`0x4005_5000`) is initialized by
//! `App1 0x6001_C2D4` and replicated here.

#![allow(dead_code)]

use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::{PLL_CFG_BASE, SCI_BASE};

/// Fractional Audio PLL register block (`App1 0x6001_C2E6`).
const AUDIO_PLL_BASE: usize = 0x4005_5000;

/// Fallback CPU clock frequency (120 MHz) if `0x4002_6030` reads out of range.
const DEFAULT_SYSCLK_HZ: u32 = 120_000_000;

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

/// Owned handle to Core 0 clock & audio PLL configuration.
pub struct Clocks {
    _private: (),
}

impl Clocks {
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }

    /// Read the active system clock frequency in Hz from `SCI_BASE + 0x30` (`0x4002_6030`).
    pub fn sysclk_hz(&self) -> u32 {
        // SAFETY: Read-only query of the system clock Hz register.
        let hz = unsafe { rd32(SCI_BASE + 0x30) };
        if (1_000_000..=400_000_000).contains(&hz) {
            hz
        } else {
            DEFAULT_SYSCLK_HZ
        }
    }

    /// Configure and lock the 44.1 kHz fractional audio PLL (`App1 0x6001_C2D4..0x6001_C33C`).
    pub fn init_audio_pll_44k1(&mut self) {
        // SAFETY: Exclusive ownership of audio PLL registers guaranteed by Peripherals::take().
        unsafe {
            let gate = PLL_CFG_BASE + 0x28;
            wr32(gate, rd32(gate) | 0x4);

            let r54 = (rd32(AUDIO_PLL_BASE + 0x54) & !0x7F).wrapping_add(0x10);
            wr32(AUDIO_PLL_BASE + 0x54, r54);

            let r58 = (rd32(AUDIO_PLL_BASE + 0x58) & !0xFFFF) | 0xB680;
            wr32(AUDIO_PLL_BASE + 0x58, r58);

            let r5c = (rd32(AUDIO_PLL_BASE + 0x5C) & !0xFFFF) | 0xC350;
            wr32(AUDIO_PLL_BASE + 0x5C, r5c);

            wr32(AUDIO_PLL_BASE + 0x60, rd32(AUDIO_PLL_BASE + 0x60) & !1);
            wr32(AUDIO_PLL_BASE + 0x64, rd32(AUDIO_PLL_BASE + 0x64) & !1);

            wr32(AUDIO_PLL_BASE + 0x50, rd32(AUDIO_PLL_BASE + 0x50) | 1);
            dsb();

            wr32(SCI_BASE + 0x18, 0x2580_0001);
            while (rd32(SCI_BASE + 0x1C) & 1) == 0 {
                core::hint::spin_loop();
            }
            wr32(SCI_BASE + 0x18, 0);
            wr32(SCI_BASE + 0x10, rd32(SCI_BASE + 0x10) | 1);
            dsb();
        }
    }
}
