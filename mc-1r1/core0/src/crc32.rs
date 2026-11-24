//! Hardware IEEE 802.3 CRC-32 Peripheral Driver (`0x4010_5000`).
//!
//! Verified from `App1 0x6004_F5E0..0x6004_F616`:
//! - Feed 32-bit little-endian words to `CRC32_BASE + 0x00`.
//! - Read the resulting IEEE 802.3 CRC-32 from `CRC32_BASE + 0x80`.

#![allow(dead_code)]

use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::CRC32_BASE;

const CRC32_DATA_IN: usize = CRC32_BASE + 0x00;
const CRC32_RESULT: usize = CRC32_BASE + 0x80;

/// Owned handle to the hardware IEEE 802.3 CRC-32 engine (`0x4010_5000`).
pub struct HwCrc32 {
    _private: (),
}

impl HwCrc32 {
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }

    /// Compute hardware IEEE 802.3 CRC-32 over a 32-bit word slice.
    pub fn compute_words(&mut self, words: &[u32]) -> u32 {
        // SAFETY: Exclusive ownership of CRC32 registers guaranteed by Peripherals::take().
        unsafe {
            for &w in words {
                core::ptr::write_volatile(CRC32_DATA_IN as *mut u32, w);
            }
            dsb();
            core::ptr::read_volatile(CRC32_RESULT as *const u32)
        }
    }
}
