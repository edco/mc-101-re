//! Front-Panel Sub-CPU Serial Driver (Channel 4, `0x4002_2000`).
//!
//! Hardware contract verified from `App1_Main`:
//! - Pinmux (`0x6001_B074`): `0x4002_6558 = 0`, `0x4002_6A30 = 6`,
//!   `0x4002_655C = 0x806`, `0x4002_6560 = 0x800` (`BOOT0` @ `0x424C_4360`),
//!   `0x4002_6564 = 0x800` (`NRST` @ `0x424C_4364`).
//! - Sub-CPU reset (`0x600A_9E1E..0x600A_9E3A`): `NRST = 0`, `BOOT0 = 0`,
//!   wait 10 ms, `NRST = 1`, wait 300 ms.
//! - Channel 4 config (`0x601D_0378` via `0x6002_72E0` -> `0x6001_E3F0`):
//!   `desc = [0xB301, 0x0000, 0x0803, 0x030C, 0x2000]` (115200 baud).
//! - Boot init TX commands (`0x6004_7EDE..0x6004_7EEA` -> `0x600A_0918`):
//!   `[0x80, 0x28, 0x00, 0x26]`, `[0x80, 0x20, 0x00, 0x0D]`,
//!   `[0x80, 0x0C, 0x00, 0x00]`, `[0x80, 0x14, 0x00, 0x00]`.
//! - Wire RX (`0x0102_41B4` + `0x0101_2F5C`): 4-byte binary packets starting
//!   with bit 7 set (`0x81` = Release, `0x82` = Press, byte 1 = `button_id`),
//!   or 10-byte Roland SysEx `F0 41 60 00 <msb> <b0> <b1> <b2> <b3> F7`.

#![allow(dead_code)]

use crate::time;
use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::{
    PANEL_BOOT0_BB, PANEL_BOOT0_PINMUX, PANEL_CH4_BASE, PANEL_NRST_BB, PANEL_NRST_PINMUX,
    PANEL_RX_MUX, PANEL_RX_PINMUX, PANEL_TX_PINMUX,
};

/// Known MC-101 logical button IDs (`docs/core0_boot_and_memory_architecture.md` §5.2).
pub mod button {
    pub const SHIFT: u8 = 0x00;
    pub const PROJECT: u8 = 0x01;
    pub const MULTI_FX: u8 = 0x0A;
    pub const SOUND: u8 = 0x0B;
    pub const FILTER: u8 = 0x0C;
    pub const MOD: u8 = 0x0D;
    pub const ENTER: u8 = 0x11;
    pub const CLIP: u8 = 0x12;
}

/// Decoded event from the Sub-CPU front-panel stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelEvent {
    Press(u8),
    Release(u8),
}

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

/// Owned handle to the Sub-CPU serial link on Channel 4 (`0x4002_2000`).
pub struct PanelCh4 {
    pkt: [u8; 4],
    pkt_len: usize,
    sysex_buf: [u8; 10],
    sysex_len: usize,
    in_sysex: bool,
    held_mask: u64,
    rx_bytes: u32,
}

impl PanelCh4 {
    pub(crate) const fn new() -> Self {
        Self {
            pkt: [0; 4],
            pkt_len: 0,
            sysex_buf: [0; 10],
            sysex_len: 0,
            in_sysex: false,
            held_mask: 0,
            rx_bytes: 0,
        }
    }

    /// Initialize Channel 4 pinmux, reset the STM32G0 Sub-CPU, configure
    /// Channel 4 (`0x601D_0378`, 115200 baud), and send the 4 boot-query packets.
    pub fn init(&mut self) {
        // SAFETY: Exclusive ownership of Channel 4 & Sub-CPU GPIOs guaranteed by Peripherals::take().
        unsafe {
            // 1. Pinmux (`App1 0x6001_B074`)
            wr32(PANEL_RX_PINMUX, 0x0000);
            wr32(PANEL_RX_MUX, 0x0006);
            wr32(PANEL_TX_PINMUX, 0x0806);
            wr32(PANEL_BOOT0_BB, 0);
            wr32(PANEL_BOOT0_PINMUX, 0x0800);
            wr32(PANEL_NRST_BB, 0);
            wr32(PANEL_NRST_PINMUX, 0x0800);
            dsb();

            // 2. Channel 4 hardware config (`App1 0x6002_72E0` -> `0x6001_E41E` with `0x601D_0378`)
            wr32(PANEL_CH4_BASE + 0x00, 0xFFFF_8000); // reset
            wr32(PANEL_CH4_BASE + 0x14, 0x000C);
            wr32(PANEL_CH4_BASE + 0x14, 0x0803); // desc[4]
            wr32(PANEL_CH4_BASE + 0x18, 0x2000); // desc[8] FIFO threshold
            wr32(PANEL_CH4_BASE + 0x04, 0x0000); // desc[2]
            wr32(PANEL_CH4_BASE + 0x0C, 0x030C); // desc[6] baud divisor (115200)
            wr32(PANEL_CH4_BASE + 0x00, 0xB301); // desc[0] enable
            dsb();

            // 3. Reset Sub-CPU (`App1 0x600A_9E1E..0x600A_9E3A`):
            //    NRST=0, BOOT0=0 -> 10 ms -> NRST=1 -> 300 ms boot settle.
            wr32(PANEL_NRST_BB, 0);
            wr32(PANEL_BOOT0_BB, 0);
            dsb();
            time::delay_ms(10);
            wr32(PANEL_NRST_BB, 1);
            dsb();
            time::delay_ms(300);
        }

        // 4. Send the 4 Sub-CPU wake & initial-key-query commands (`App1 0x6004_7EDE..0x6004_7EEA`).
        self.tx_bytes(&[0x80, 0x28, 0x00, 0x26]); // 0x600A_07B4(0, 0x26)
        self.tx_bytes(&[0x80, 0x20, 0x00, 0x0D]); // 0x600A_0814()
        self.tx_bytes(&[0x80, 0x0C, 0x00, 0x00]); // 0x600A_083A()
        self.tx_bytes(&[0x80, 0x14, 0x00, 0x00]); // 0x600A_08D4()
    }

    fn tx_bytes(&mut self, bytes: &[u8]) {
        // SAFETY: Exclusive ownership of Channel 4 registers guaranteed by Peripherals::take().
        unsafe {
            for &b in bytes {
                while (rd32(PANEL_CH4_BASE + 0x18) & 0xFF) > 0x3C {
                    core::hint::spin_loop();
                }
                wr32(PANEL_CH4_BASE + 0x08, b as u32);
            }
            dsb();
        }
    }

    /// Returns `true` if logical `button_id` (`0..=0x25`) is currently held.
    #[inline]
    pub fn is_held(&self, button_id: u8) -> bool {
        if button_id > 0x25 {
            return false;
        }
        (self.held_mask & (1u64 << button_id)) != 0
    }

    /// Raw 64-bit bitmask of currently held buttons.
    #[inline]
    pub fn held_mask(&self) -> u64 {
        self.held_mask
    }

    /// Total raw bytes received from Channel 4.
    #[inline]
    pub fn rx_bytes(&self) -> u32 {
        self.rx_bytes
    }

    /// Poll Channel 4 RX FIFO and return the next decoded `PanelEvent`, if any.
    pub fn poll_event(&mut self) -> Option<PanelEvent> {
        loop {
            let byte = self.read_rx_byte()?;
            self.rx_bytes = self.rx_bytes.wrapping_add(1);
            if let Some(ev) = self.feed_byte(byte) {
                return Some(ev);
            }
        }
    }

    fn read_rx_byte(&mut self) -> Option<u8> {
        // SAFETY: Exclusive ownership of Channel 4 registers guaranteed by Peripherals::take().
        unsafe {
            let st = rd32(PANEL_CH4_BASE + 0x04);
            if (st & 0x3800) != 0 {
                wr32(PANEL_CH4_BASE + 0x04, st | 0x8000);
                let ctl = rd32(PANEL_CH4_BASE + 0x14);
                wr32(PANEL_CH4_BASE + 0x14, ctl | 0x08);
                let ctl = rd32(PANEL_CH4_BASE + 0x14);
                wr32(PANEL_CH4_BASE + 0x14, ctl | 0x02);
            }
            if (rd32(PANEL_CH4_BASE + 0x18) & 0xFF00) == 0 {
                return None;
            }
            Some((rd32(PANEL_CH4_BASE + 0x08) & 0xFF) as u8)
        }
    }

    fn feed_byte(&mut self, b: u8) -> Option<PanelEvent> {
        // Path 1: 10-byte Roland SysEx framing (`F0 41 60 00 <msb> <b0..b3> F7`).
        if b == 0xF0 {
            self.sysex_buf[0] = 0xF0;
            self.sysex_len = 1;
            self.in_sysex = true;
            self.pkt_len = 0;
            return None;
        }
        if self.in_sysex {
            if b >= 0xF8 {
                return None;
            }
            if self.sysex_len < self.sysex_buf.len() {
                self.sysex_buf[self.sysex_len] = b;
                self.sysex_len += 1;
            } else {
                self.in_sysex = false;
                self.sysex_len = 0;
                return None;
            }
            if b == 0xF7 {
                self.in_sysex = false;
                let len = self.sysex_len;
                self.sysex_len = 0;
                return self.decode_sysex(len);
            }
            return None;
        }

        // Path 2: Native 4-byte Sub-CPU binary packet (byte 0 has bit 7 set: 0x80..0x9F).
        if (b & 0x80) != 0 {
            self.pkt[0] = b;
            self.pkt_len = 1;
            return None;
        }
        if self.pkt_len > 0 && self.pkt_len < 4 {
            self.pkt[self.pkt_len] = b;
            self.pkt_len += 1;
            if self.pkt_len == 4 {
                self.pkt_len = 0;
                return self.decode_word(u32::from_le_bytes(self.pkt));
            }
        }
        None
    }

    fn decode_sysex(&mut self, len: usize) -> Option<PanelEvent> {
        if len != 10
            || self.sysex_buf[0] != 0xF0
            || self.sysex_buf[1] != 0x41
            || self.sysex_buf[2] != 0x60
            || self.sysex_buf[3] != 0x00
            || self.sysex_buf[9] != 0xF7
        {
            return None;
        }
        let msb = self.sysex_buf[4];
        let out = [
            self.sysex_buf[5] | ((msb << 4) & 0x80),
            self.sysex_buf[6] | ((msb << 5) & 0x80),
            self.sysex_buf[7] | ((msb << 6) & 0x80),
            self.sysex_buf[8] | ((msb << 7) & 0x80),
        ];
        self.decode_word(u32::from_le_bytes(out))
    }

    fn decode_word(&mut self, word: u32) -> Option<PanelEvent> {
        let cmd = (word & 0x1F) as u8;
        let btn = ((word >> 8) & 0x7F) as u8;
        match cmd {
            1 => {
                if btn <= 0x25 {
                    self.held_mask &= !(1u64 << btn);
                }
                Some(PanelEvent::Release(btn))
            }
            2 => {
                if btn <= 0x25 {
                    self.held_mask |= 1u64 << btn;
                }
                Some(PanelEvent::Press(btn))
            }
            _ => None,
        }
    }
}

/// Short human-readable label (up to 6 ASCII chars) for known MC-101 buttons.
pub fn button_name(id: u8) -> &'static [u8] {
    match id {
        button::SHIFT => b"SHIFT ",
        button::PROJECT => b"PROJ  ",
        button::MULTI_FX => b"MFX   ",
        button::SOUND => b"SOUND ",
        button::FILTER => b"FILTER",
        button::MOD => b"MOD   ",
        button::ENTER => b"ENTER ",
        button::CLIP => b"CLIP  ",
        _ => b"KEY   ",
    }
}
