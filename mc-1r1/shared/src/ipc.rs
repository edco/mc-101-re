//! Minimal Inter-Core "Hello World" IPC Channel.
//!
//! Uses a dedicated shared-memory slot in the Inter-Core IPC Mailbox SRAM
//! (`0x0114_0000`), which is accessible over the AXI bus matrix by both Core 0
//! and Core 1.
//!
//! NOTE: This initial bring-up channel does NOT yet replicate the stock
//! firmware's full `0x0114_0000` / `0x60F0_0000` / `0x4110_0000` ring protocol.
//! See `mc-1r1/PLAN.md` for the migration path to full stock compatibility.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

/// Offset within `IPC_MBOX_BASE` (`0x0114_0000`) used for the bring-up mailbox.
/// Placed at +0x0800 to avoid colliding with early stock header words at +0x0000.
pub const HELLO_MBOX_ADDR: usize = crate::memmap::IPC_MBOX_BASE + 0x0800;

pub const HELLO_MAGIC: u32 = 0x4D43_3152; // "MC1R"
pub const MAX_MSG_LEN: usize = 64;

#[repr(C)]
pub struct HelloMailbox {
    /// Magic value written by Core 0 on init (`HELLO_MAGIC`).
    pub magic: AtomicU32,
    /// Sequence counter incremented by Core 1 each time a new message is posted.
    pub c1_to_c0_seq: AtomicU32,
    /// Sequence counter acknowledged by Core 0 after reading.
    pub c0_ack_seq: AtomicU32,
    /// Length of the UTF-8/ASCII message in `msg`.
    pub msg_len: AtomicU32,
    /// Fixed-size message buffer.
    pub msg: [u8; MAX_MSG_LEN],
}

impl HelloMailbox {
    /// Returns a reference to the shared mailbox in IPC SRAM.
    ///
    /// # Safety
    /// Caller must ensure both cores agree on `HELLO_MBOX_ADDR` and no other
    /// subsystem is concurrently using `0x0114_0800..0x0114_0850`.
    #[inline]
    pub unsafe fn get() -> &'static mut Self {
        &mut *(HELLO_MBOX_ADDR as *mut Self)
    }

    /// Core 0: Initialize the bring-up mailbox before releasing Core 1.
    pub fn init_core0(&mut self) {
        self.c1_to_c0_seq.store(0, Ordering::Relaxed);
        self.c0_ack_seq.store(0, Ordering::Relaxed);
        self.msg_len.store(0, Ordering::Relaxed);
        for b in self.msg.iter_mut() {
            *b = 0;
        }
        dmb();
        self.magic.store(HELLO_MAGIC, Ordering::Release);
        dsb();
    }

    /// Core 1: Post a message to Core 0 and signal with `SEV`.
    pub fn post_from_core1(&mut self, text: &[u8]) {
        let len = text.len().min(MAX_MSG_LEN);
        for (i, b) in text[..len].iter().enumerate() {
            // Volatile write to shared AXI SRAM.
            unsafe { core::ptr::write_volatile(&mut self.msg[i], *b) };
        }
        self.msg_len.store(len as u32, Ordering::Release);
        dmb();
        let next = self.c1_to_c0_seq.load(Ordering::Relaxed).wrapping_add(1);
        self.c1_to_c0_seq.store(next, Ordering::Release);
        dsb();
        sev();
    }

    /// Core 0: Poll for a new message from Core 1.
    /// Returns `Some(len)` if a new message was copied into `out`.
    pub fn poll_core0(&mut self, out: &mut [u8; MAX_MSG_LEN]) -> Option<usize> {
        if self.magic.load(Ordering::Acquire) != HELLO_MAGIC {
            return None;
        }
        let seq = self.c1_to_c0_seq.load(Ordering::Acquire);
        let ack = self.c0_ack_seq.load(Ordering::Relaxed);
        if seq == ack {
            return None;
        }
        dmb();
        let len = (self.msg_len.load(Ordering::Acquire) as usize).min(MAX_MSG_LEN);
        for i in 0..len {
            out[i] = unsafe { core::ptr::read_volatile(&self.msg[i]) };
        }
        self.c0_ack_seq.store(seq, Ordering::Release);
        Some(len)
    }
}

#[inline(always)]
pub fn dmb() {
    unsafe { asm!("dmb sy", options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn dsb() {
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn sev() {
    unsafe { asm!("sev", options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn wfi() {
    unsafe { asm!("wfi", options(nostack, preserves_flags)) };
}

#[inline(always)]
pub fn wfe() {
    unsafe { asm!("wfe", options(nostack, preserves_flags)) };
}
