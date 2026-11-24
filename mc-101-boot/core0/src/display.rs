//! Physical 16x2 Character LCD (ST7032-compatible) & Backlight Driver for MC-101 Core 0.
//!
//! Verified from `App0_UPD_decrypted.bin` (`mc-1r1/DISPLAY.md` §6 + disassembly of
//! `0x6000_39B8`, `0x6000_6DF8`, `0x6000_6E86`, `0x6000_6F60`, `0x6000_7260`, `0x6000_729E`):
//! - **Channel 7 Serial/SPI**: `0x4002_2C00` (`+0x08` TX data, `+0x04` status bit 8 & bit 9,
//!   `+0x18` TX FIFO level `<= 0x3C`).
//! - **RS pin**: bit-band `0x424C_4374` (`0` = command, `1` = data).
//! - **Backlight**: `0x4002_5028` bits 1 & 5 enabled; `0x4002_5300` bit 1 = 0 (disable PWM),
//!   bit 9 = 1 (100% steady ON).
//! - **Wake sequence (contrast `0x26`)**: `38 39 14 76 56 6B(200ms) 38 06 0C 38`.
//! - **Text flush**: `cmd(0x80)` + 16 ASCII bytes (row 0), `cmd(0xC0)` + 16 ASCII bytes (row 1).

use crate::hal::TextDisplay;
use crate::time::delay_ms;
use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::{bitband_write, LCD_BL_CLK_REG, LCD_BL_CTL_REG, LCD_CH7_BASE, LCD_RS_BITBAND};

pub const COLS: usize = 16;
pub const ROWS: usize = 2;

const BOOT_CONTRAST: u8 = 0x26;

/// Owned handle to the physical 16x2 ST7032 LCD on Serial Channel 7 (`0x4002_2C00`).
pub struct LcdCh7 {
    lines: [[u8; COLS]; ROWS],
    dirty: bool,
}

#[inline]
unsafe fn ch7_write_byte(rs_data: bool, byte: u8) {
    // Set RS pin via bit-band alias (0 = command, 1 = data).
    core::ptr::write_volatile(LCD_RS_BITBAND as *mut u32, if rs_data { 1 } else { 0 });

    // Gate on TX FIFO level ([0x4002_2C18] & 0xFF <= 0x3C).
    let fifo_reg = (LCD_CH7_BASE + 0x18) as *const u32;
    while (core::ptr::read_volatile(fifo_reg) & 0xFF) > 0x3C {
        core::hint::spin_loop();
    }

    // Push byte to TX data register (0x4002_2C08).
    core::ptr::write_volatile((LCD_CH7_BASE + 0x08) as *mut u32, byte as u32);

    // Wait for status bit 8 (0x100) then bit 9 (0x200) in 0x4002_2C04
    // (verified from 0x6000_39CC: `lsls r1, #0x17; bpl` and `lsls r1, #0x16; bpl`).
    let status_reg = (LCD_CH7_BASE + 0x04) as *const u32;
    loop {
        let st = core::ptr::read_volatile(status_reg);
        if (st & 0x100) != 0 && (st & 0x200) != 0 {
            break;
        }
    }
}

#[inline]
unsafe fn lcd_cmd(b: u8) {
    ch7_write_byte(false, b);
}

#[inline]
unsafe fn lcd_data(b: u8) {
    ch7_write_byte(true, b);
}

unsafe fn init_pinmux_and_clocks() {
    #[inline(always)]
    unsafe fn wr(addr: usize, val: u32) {
        core::ptr::write_volatile(addr as *mut u32, val);
    }

    // From App0_UPD 0x6000_363E(0) & 0x6000_3682(1): board GPIO enables on 0x4002_6200.
    bitband_write(0x4002_6200, 0x18, 0);
    bitband_write(0x4002_6200, 0x17, 1);
    bitband_write(0x4002_6200, 0x15, 0);

    // From App0_UPD 0x6000_3754: peripheral gates & Channel 7 SPI / panel pinmux.
    bitband_write(0x4002_6200, 0, 1);
    bitband_write(0x4002_6200, 1, 1);
    bitband_write(0x4002_6200, 2, 1);
    bitband_write(0x4002_6200, 3, 1);
    bitband_write(0x4002_6208, 12, 1);
    bitband_write(0x4002_6208, 13, 1);

    wr(0x4002_6660, 0x800);
    wr(0x4002_6664, 0x800);
    wr(0x4002_6668, 0x800);
    wr(0x4002_666C, 0x800);
    wr(0x4002_6700, 0x800);
    wr(0x4002_6704, 0x800);

    // From App0_UPD 0x6000_3748 (called at entry of 0x6000_7260):
    // Route physical LCD backlight pin to 0x4002_5300 PWM/drive block (AF = 0x806).
    wr(0x4002_676C, 0x806);
    dsb();
}

unsafe fn init_backlight_on() {
    // Enable backlight peripheral bits (0x6000_7260) and set steady 100% ON
    // (0x6000_729E duty >= 100 branch: PWM bit 1 = 0, drive bit 9 = 1).
    bitband_write(LCD_BL_CLK_REG, 5, 1);
    bitband_write(LCD_BL_CLK_REG, 1, 1);
    bitband_write(LCD_BL_CTL_REG, 1, 0);
    bitband_write(LCD_BL_CTL_REG, 9, 1);
    dsb();
}

unsafe fn init_channel7() {
    #[inline(always)]
    unsafe fn wr(addr: usize, val: u32) {
        core::ptr::write_volatile(addr as *mut u32, val);
    }

    // Exact pre-init callback cb0 (0x6000_6E68 -> 0x6000_36FE):
    // 1. Enable Channel 7 peripheral clock/reset.
    wr(0x4002_9504, 3);

    // 2. Channel 7 SPI + RS + LCD reset pinmux (0x6000_36FE).
    wr(0x4002_657C, 0x803);
    wr(0x4002_6578, 0x803);
    wr(0x4002_6574, 0x800);
    wr(0x4002_6570, 0x806);
    wr(0x4002_656C, 0x800);
    bitband_write(0x4002_6218, 0x1B, 1);
    dsb();

    // Exact register write order & desc values from App0_UPD 0x6000_3A14(7, 0x6002_3694..AC):
    // desc = [0x0147, 0x0000, 0x0001, 0x0167, 0x2000, 0x0001, 0x0005]
    wr(LCD_CH7_BASE + 0x00, 0xFFFF_8000); // soft reset
    wr(LCD_CH7_BASE + 0x14, 0x000C);      // FIFO reset
    wr(LCD_CH7_BASE + 0x14, 0x0001);      // desc[4]
    wr(LCD_CH7_BASE + 0x18, 0x2000);      // desc[8]
    wr(LCD_CH7_BASE + 0x04, 0x0000);      // desc[2]
    wr(LCD_CH7_BASE + 0x0C, 0x0167);      // desc[6] (clock divisor)
    wr(LCD_CH7_BASE + 0xD0, 0x0005);      // desc[0xC]
    wr(LCD_CH7_BASE + 0xC0, 0x0001);      // desc[0xA]
    wr(LCD_CH7_BASE + 0x00, 0x0147);      // desc[0] (enable Channel 7 SPI!)
    dsb();
}

unsafe fn lcd_wake(contrast: u8) {
    // Exact ST7032 / Sitronix wake sequence from App0_UPD 0x6000_6F60(0x26).
    let c_lo = 0x70 | (contrast & 0x0F);
    let c_hi = 0x54 | ((contrast >> 4) & 0x03);

    lcd_cmd(0x38);
    delay_ms(1);
    lcd_cmd(0x39);
    delay_ms(1);
    lcd_cmd(0x14);
    delay_ms(1);
    lcd_cmd(c_lo);
    delay_ms(1);
    lcd_cmd(c_hi);
    delay_ms(1);
    lcd_cmd(0x6B);
    delay_ms(200);
    lcd_cmd(0x38);
    delay_ms(1);
    lcd_cmd(0x06);
    delay_ms(1);
    lcd_cmd(0x0C);
    delay_ms(1);
    lcd_cmd(0x38);
    delay_ms(1);
}

impl LcdCh7 {
    pub(crate) const fn new() -> Self {
        Self {
            lines: [[b' '; COLS]; ROWS],
            dirty: false,
        }
    }

    /// Initialize Channel 7 SPI, turn on the LCD backlight, run the ST7032
    /// wake sequence, and clear both 16-character rows.
    pub fn init(&mut self) {
        // SAFETY: Exclusive ownership of Channel 7 & backlight registers guaranteed by Peripherals::take().
        unsafe {
            init_pinmux_and_clocks();
            init_backlight_on();
            init_channel7();
            lcd_wake(BOOT_CONTRAST);
        }
        self.clear();
        self.flush();
    }

    pub fn clear(&mut self) {
        for row in self.lines.iter_mut() {
            row.fill(b' ');
        }
        self.dirty = true;
    }
}

impl TextDisplay for LcdCh7 {
    /// Write an ASCII string to `row` (0 or 1), padded/truncated to 16 chars.
    fn write_line(&mut self, row: usize, text: &[u8]) {
        if row >= ROWS {
            return;
        }
        self.lines[row].fill(b' ');
        let n = text.len().min(COLS);
        for i in 0..n {
            let c = text[i] & 0x7F;
            self.lines[row][i] = if (0x20..=0x7D).contains(&c) { c } else { b' ' };
        }
        self.dirty = true;
    }

    /// Flush both 16-character rows to the physical 16x2 LCD over Channel 7.
    fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        // SAFETY: Exclusive ownership of Channel 7 guaranteed by Peripherals::take().
        unsafe {
            lcd_cmd(0x80);
            for col in 0..COLS {
                lcd_data(self.lines[0][col]);
            }
            lcd_cmd(0xC0);
            for col in 0..COLS {
                lcd_data(self.lines[1][col]);
            }
        }
        self.dirty = false;
    }

    /// Turn off the LCD display panel and backlight cleanly during shutdown.
    fn power_off(&mut self) {
        // SAFETY: Exclusive ownership of Channel 7 & backlight registers guaranteed by Peripherals::take().
        unsafe {
            lcd_cmd(0x08); // ST7032 Display OFF
            bitband_write(LCD_BL_CTL_REG, 9, 0); // Steady backlight OFF
            bitband_write(LCD_BL_CTL_REG, 1, 0); // PWM backlight OFF
            dsb();
        }
    }
}
