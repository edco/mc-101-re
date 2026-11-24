//! Core 1 QSPI1 (`0x4003_2000` + `0x4003_0000` DMA) Loader & SMC1 Release.
//!
//! Implements the verified raw Chip 1 QSPI NOR read sequence from
//! `mc-1r1/DISPLAY.md` §7 & §8:
//! 1. `init_qspi1()` — `App1 0x6001_FEC0` channel timing setup + cold reset opcodes.
//! 2. `qspi1_read_raw()` — `0x6001_F530` single-line `0x03` Read Data + DMA ch0 (`0x4003_0010`).
//! 3. `load_core1_from_qspi1()` — parses the `"QSPI"` TOC (at flash `0x0000` or `0x0020`)
//!    and CPU-copies `idm1.bin` -> `0x0110_0000` and `sdram1.bin` -> `0x6100_0000`.
//! 4. `release_core1()` — SMC1 core-control command pair at `0x4110_0010` (`5` -> bit31 -> `0x16`).

use mc1r1_shared::ipc::{dmb, dsb};
use mc1r1_shared::memmap::{
    CORE1_ITCM_BASE, CORE1_ITCM_SIZE, CORE1_SDRAM_BASE, DMAC1_BASE, IPC_CORE1_ANCHOR_PTR,
    IPC_CORE1_MBOX_ANCHOR, IPC_CORE1_MBOX_HDR, IPC_CORE1_MBOX_TAIL, QSPI1_BASE, SCI_BASE,
};

const MAX_SDRAM1_COPY: usize = 0x0070_0000;
const QSPI1_TX_FIFO: usize = QSPI1_BASE + 0x50; // 0x4003_2050
const QSPI1_RX_FIFO: usize = QSPI1_BASE + 0x90; // 0x4003_2090
const QSPI1_FIFO_STAT: usize = QSPI1_BASE + 0x40; // bits [15:8]=RX cnt, [23:16]=TX cnt
const QSPI1_BB: usize = 0x4264_0000; // Bit-band alias base of 0x4003_2000
const CHUNK_SIZE: usize = 256;

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline(always)]
unsafe fn rd8(addr: usize) -> u8 {
    core::ptr::read_volatile(addr as *const u8)
}

#[inline(always)]
unsafe fn wr8(addr: usize, val: u8) {
    core::ptr::write_volatile(addr as *mut u8, val);
}

unsafe fn qspi1_send_cmd(op: u8) {
    while (rd32(QSPI1_BASE) & 2) != 0 {
        core::hint::spin_loop();
    }
    wr8(QSPI1_BASE + 0x3A, rd8(QSPI1_BASE + 0x3A) & !3);
    wr32(QSPI1_BASE + 0x4C, rd32(QSPI1_BASE + 0x4C) & !0x700);
    wr8(QSPI1_BASE + 0x3B, 8);
    wr8(QSPI1_BASE + 0x38, 1);
    wr8(QSPI1_BASE + 0x39, 0);
    wr8(QSPI1_TX_FIFO, op);
    wr32(QSPI1_BASE + 0x1C, rd32(QSPI1_BASE + 0x1C) | 2);
    // Bit 1 of +0x14 is DONE (1 = complete; verified at App1 0x6001_F62E).
    while (rd32(QSPI1_BASE + 0x14) & 2) == 0 {
        core::hint::spin_loop();
    }
    wr8(QSPI1_BASE + 0x38, 0);
}

/// Owned handle to QSPI1 (`0x4003_2000`) and the Core 1 boot loader/reset gate.
pub struct Qspi1 {
    _private: (),
}

impl Qspi1 {
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }

    /// Initialize QSPI1 (`0x4003_2000`) & DMAC1 (`0x4003_0000`).
    pub fn init(&mut self) {
        // SAFETY: Exclusive ownership of QSPI1 registers guaranteed by Peripherals::take().
        unsafe { init_qspi1() }
    }

    /// Publish stock IPC anchor words (`0x60F0_0000`, `0x60F0_0004`, `0x2000_03E0`).
    pub fn publish_ipc_anchors(&mut self) {
        // SAFETY: Exclusive ownership of Core 1 boot path before release_core1().
        unsafe {
            wr32(IPC_CORE1_MBOX_HDR, IPC_CORE1_MBOX_ANCHOR as u32);
            wr32(IPC_CORE1_MBOX_TAIL, CORE1_SDRAM_BASE as u32);
            wr32(IPC_CORE1_ANCHOR_PTR, IPC_CORE1_MBOX_ANCHOR as u32);
            dmb();
        }
    }

    /// Load `idm1.bin` -> `0x0110_0000` and `sdram1.bin` -> `0x6100_0000` from Chip 1 flash.
    pub fn load_core1(&mut self) -> bool {
        // SAFETY: Exclusive ownership of QSPI1; Core 1 is held in reset before release_core1().
        unsafe { load_core1_from_qspi1() }
    }

    /// Release Core 1 from reset (`0x4005_1054` / `0x4005_1058` / `0x4005_4028`).
    pub fn release_core1(&mut self) {
        // SAFETY: Called after load_core1() populated Core 1 ITCM1 & SDRAM1.
        unsafe { release_core1() }
    }
}

/// Minimal QSPI1 (`0x4003_2000`) & DMAC1 (`0x4003_0000`) init from `App1 0x6001_FEC0..FFCA`.
unsafe fn init_qspi1() {
    // Enable DMAC1 global controller.
    wr32(DMAC1_BASE, rd32(DMAC1_BASE) | 0x8000_0000);

    // 1. Disable QSPI1 (0x4264_0000 = 0) and wait until ready/active bit (0x4264_0010) clears.
    wr32(QSPI1_BB + 0x00, 0);
    while rd32(QSPI1_BB + 0x10) != 0 {
        core::hint::spin_loop();
    }
    wr32(QSPI1_BB + 0x1A10, 0);
    wr32(QSPI1_BB + 0x04, 0);

    // 2. Check SPC route gate bit 0 (App1) or bit 8 (bootloader) & program channel timing.
    let spc = rd32(SCI_BASE + 0x48);
    let gate = (spc & 0x101) != 0;
    let offsets: &[usize] = if gate {
        &[0x04, 0x08, 0x0C, 0x10]
    } else {
        &[0x08, 0x0C, 0x10]
    };
    let addend: u32 = if gate { 0x400 } else { 0x200 };
    for &off in offsets {
        let reg = QSPI1_BASE + off;
        let v = (rd32(reg) & !0xFE00).wrapping_add(addend);
        wr32(reg, v);
        wr32(reg, rd32(reg) | 0x100);
    }

    // 3. Enable QSPI1 controller (0x4264_0000 = 1) and wait until ready (0x4264_0010 != 0)!
    wr32(QSPI1_BB + 0x00, 1);
    while rd32(QSPI1_BB + 0x10) == 0 {
        core::hint::spin_loop();
    }
    dsb();

    // 4. Cold-state readiness opcodes (`DISPLAY.md` §8.2).
    let st = rd32(QSPI1_BASE);
    if st == 1 || st == 0x20 || st == 0xC2 {
        qspi1_send_cmd(0x66);
        qspi1_send_cmd(0x99);
        if st == 1 {
            qspi1_send_cmd(0xF0);
        }
    }
}

/// Pure PIO read of `len` bytes (1..=256) from Chip 1 NOR flash at 24-bit `addr`
/// into `dst` using single-line `0x0B` Fast Read (zero DMA; verified against
/// App1 `0x6001_FA00..0x6001_FB8A`).
unsafe fn qspi1_read_pio(addr: u32, dst: &mut [u8]) {
    let len = dst.len();
    if len == 0 {
        return;
    }

    // 1. Reset TX/RX FIFOs, single-line PIO mode, disable DMA.
    wr32(QSPI1_BB + 0x9B0, 1);
    wr32(QSPI1_BB + 0x9AC, 1);
    wr8(QSPI1_BASE + 0x3A, rd8(QSPI1_BASE + 0x3A) & !3);
    wr32(QSPI1_BASE + 0x4C, rd32(QSPI1_BASE + 0x4C) & !0x300);
    wr8(QSPI1_BASE + 0x3B, 0); // PIO mode
    wr32(QSPI1_BB + 0x684, 1); // RX enable

    // 2. Program total byte count (5 header bytes for 0x0B Fast Read + len data bytes) and assert CS.
    let total = (5 + len) as u16;
    core::ptr::write_volatile((QSPI1_BASE + 0x3C) as *mut u16, total);
    wr32(QSPI1_BB + 0x518, 1);
    wr8(QSPI1_BASE + 0x38, 1); // Assert CS

    // 3. Push 0x0B Fast Read + 24-bit MSB-first address + 1 dummy byte (5 bytes),
    // then set 0x4264_09A8 = 1 and push the 1st clock byte (0x6001_FABA) to start shifting.
    wr32(QSPI1_BB + 0x9A8, 0);
    wr8(QSPI1_TX_FIFO, 0x0B);
    wr8(QSPI1_TX_FIFO, ((addr >> 16) & 0xFF) as u8);
    wr8(QSPI1_TX_FIFO, ((addr >> 8) & 0xFF) as u8);
    wr8(QSPI1_TX_FIFO, (addr & 0xFF) as u8);
    wr8(QSPI1_TX_FIFO, 0x00); // dummy byte for 0x0B Fast Read
    wr32(QSPI1_BB + 0x9A8, 1);
    wr8(QSPI1_TX_FIFO, 0x00); // 0x6001_FABA: kicks shift engine for 1st data byte

    // Wait for the 5 header echo bytes in RX FIFO and discard them.
    while ((rd32(QSPI1_FIFO_STAT) >> 8) & 0xFF) < 5 {
        core::hint::spin_loop();
    }
    for _ in 0..5 {
        let _ = rd8(QSPI1_RX_FIFO);
    }

    // 4. Clock out and read `len` data bytes (1st clock byte already sent above).
    let mut tx_sent = 1usize;
    let mut rx_got = 0usize;
    while rx_got < len {
        let stat = rd32(QSPI1_FIFO_STAT);
        let tx_cnt = (stat >> 16) & 0xFF;
        let rx_cnt = (stat >> 8) & 0xFF;
        if tx_sent < len && tx_cnt < 16 {
            wr8(QSPI1_TX_FIFO, 0x00);
            tx_sent += 1;
        }
        if rx_cnt > 0 {
            dst[rx_got] = rd8(QSPI1_RX_FIFO);
            rx_got += 1;
        }
    }

    wr8(QSPI1_BASE + 0x38, 0); // Deassert CS
    dsb();
}

/// Read `len` bytes from Chip 1 flash into `dst_addr` via 256-byte PIO chunks.
unsafe fn qspi1_copy_to_ram(flash_addr: u32, dst_addr: usize, len: usize) {
    let mut buf = [0u8; CHUNK_SIZE];
    let mut done = 0usize;
    while done < len {
        let chunk = (len - done).min(CHUNK_SIZE);
        qspi1_read_pio(flash_addr + done as u32, &mut buf[..chunk]);
        for i in 0..chunk {
            wr8(dst_addr + done + i, buf[i]);
        }
        done += chunk;
    }
    dsb();
}

fn name_matches(entry: &[u8], expected: &[u8]) -> bool {
    if entry.len() < 16 || expected.len() > 16 {
        return false;
    }
    if &entry[..expected.len()] != expected {
        return false;
    }
    expected.len() == 16 || entry[expected.len()] == 0
}

fn le_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Publish stock IPC anchor words and load `idm1.bin` -> `0x0110_0000` and
/// `sdram1.bin` -> `0x6100_0000` from Chip 1 QSPI NOR flash (`0x4003_2000`).
unsafe fn load_core1_from_qspi1() -> bool {
    // 1. Publish stock IPC mailbox anchor words (KernelMainEntry / InitIpcKernelPrep).
    wr32(IPC_CORE1_MBOX_HDR, IPC_CORE1_MBOX_ANCHOR as u32);
    wr32(IPC_CORE1_MBOX_TAIL, CORE1_SDRAM_BASE as u32);
    wr32(IPC_CORE1_ANCHOR_PTR, IPC_CORE1_MBOX_ANCHOR as u32);
    dmb();

    // 2. Read first 256 bytes of Chip 1 NOR flash via PIO to locate the "QSPI" TOC.
    let mut hdr = [0u8; CHUNK_SIZE];
    qspi1_read_pio(0, &mut hdr);

    // Handle either container-prefixed ("QSPI" at +0x20) or header-at-0 ("QSPI" at +0x00).
    // TOC file_off values are relative to the "QSPI" header (App1 0x6009A668 adds +0x20).
    let qspi_off: usize = if &hdr[0x20..0x24] == b"QSPI" {
        0x20
    } else if &hdr[0x00..0x04] == b"QSPI" {
        0x00
    } else {
        return false;
    };

    let entry_size = le_u32(&hdr, qspi_off + 0x08) as usize;
    let num_files = le_u32(&hdr, qspi_off + 0x0C) as usize;
    if entry_size != 32 || num_files == 0 || num_files > 6 {
        return false;
    }

    let toc_base = qspi_off + 0x10;
    let mut loaded_idm1 = false;
    let mut loaded_sdram1 = false;

    for i in 0..num_files {
        let e = &hdr[toc_base + i * 32..toc_base + (i + 1) * 32];
        let file_off = (qspi_off as u32).wrapping_add(le_u32(e, 16));
        let file_len = le_u32(e, 20) as usize;

        if name_matches(e, b"idm1.bin") && file_len > 0 && file_len <= CORE1_ITCM_SIZE {
            qspi1_copy_to_ram(file_off, CORE1_ITCM_BASE, file_len);
            loaded_idm1 = true;
        } else if name_matches(e, b"sdram1.bin") && file_len > 0 && file_len <= MAX_SDRAM1_COPY {
            qspi1_copy_to_ram(file_off, CORE1_SDRAM_BASE, file_len);
            loaded_sdram1 = true;
        }
    }

    loaded_idm1 && loaded_sdram1
}

/// Release Core 1 from reset (`App1` ITCM0 kernel `0x0101_0E10..0x0101_0E3C`,
/// called via veneer `0x6001_A8F0` directly after loading `idm1.bin` + `sdram1.bin`).
unsafe fn release_core1() {
    // 1. Enable Core 1 boot/remap gate at 0x4005_1000.
    wr32(0x4005_1054, 1);
    wr32(0x4005_1058, rd32(0x4005_1058) & !1);
    dsb();

    // 2. Pulse Core 1 reset (0x4005_4028 bit 0): clear (assert) then set (release).
    let v = rd32(0x4005_4028);
    wr32(0x4005_4028, v & !1);
    dsb();
    let v = rd32(0x4005_4028);
    wr32(0x4005_4028, v | 1);
    dsb();
}
