//! MC-101 Soft Power Switch Monitor & Safe Shutdown (`App1` `0x6009_1EE8`).
//!
//! The rear power switch does not directly cut power; it drives GPIO input pin
//! `0x19` (bit-band `0x424C_40E4`, `1` = ON, `0` = OFF). Stock `App1_Main`
//! polls this pin every 50 ms (`0x6009_1FB4`) and requires 10 consecutive OFF
//! samples (500 ms debounce) before running its safe-shutdown sequence:
//! 1. Notify/quiesce active subsystems (flush any in-flight SD/flash writes).
//! 2. Mute audio DAC (`*(0x424C_4504) = 0`) and wait 30 ms (`0x6009_1F86`).
//! 3. Assert power-cut latches (`*(0x424C_4060) = 1; *(0x424C_405C) = 1;`,
//!    `0x6001_B174`) and halt while the regulator shuts down.

use crate::hal::TextDisplay;
use crate::time;
use mc1r1_shared::ipc::{dsb, wfi};
use mc1r1_shared::memmap::{
    AUDIO_MUTE_BB, PLL_CFG_BASE, PWR_OFF_BB_A, PWR_OFF_BB_B, PWR_OFF_PINMUX_A, PWR_OFF_PINMUX_B,
    PWR_SW_BITBAND, PWR_SW_PINMUX,
};

/// Polling interval in milliseconds (matches stock `App1 0x6009_1F34`: `0x32` = 50 ms).
pub const POLL_INTERVAL_MS: u64 = 50;

/// Consecutive OFF polls required before triggering shutdown (`0x0A` * 50 ms = 500 ms).
const DEBOUNCE_TICKS: u32 = 10;

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

/// Owned handle to the MC-101 rear power switch & hardware power-cut latches.
pub struct PowerCtl {
    off_ticks: u32,
}

impl PowerCtl {
    pub(crate) const fn new() -> Self {
        Self { off_ticks: 0 }
    }

    /// Initialize the rear power switch input (`0x4002_646C = 6`) and ensure both
    /// power-cut latches are deasserted (`0`) before configuring their pinmux as
    /// GPIO outputs (`0x800`), matching `App1 0x6001_AE50` + `0x6001_AE7C`.
    pub fn init(&mut self) {
        // SAFETY: Exclusive ownership of power GPIOs guaranteed by Peripherals::take().
        unsafe {
            wr32(PWR_OFF_BB_A, 0);
            wr32(PWR_OFF_PINMUX_A, 0x800);
            wr32(PWR_OFF_BB_B, 0);
            wr32(PWR_OFF_PINMUX_B, 0x800);
            wr32(PWR_SW_PINMUX, 6);
            dsb();
        }
    }

    /// Read the raw rear power switch state (`App1 0x6001_AB34`).
    /// Returns `true` when the switch is ON, `false` when switched OFF.
    #[inline]
    pub fn is_switch_on(&self) -> bool {
        // SAFETY: Read-only GPIO input bit-band register.
        unsafe { rd32(PWR_SW_BITBAND) != 0 }
    }

    /// Call once per `POLL_INTERVAL_MS` (50 ms). Returns `true` once the rear
    /// power switch has remained OFF for the full debounce window (500 ms).
    pub fn poll_tick(&mut self) -> bool {
        if self.is_switch_on() {
            self.off_ticks = 0;
            false
        } else {
            self.off_ticks = self.off_ticks.saturating_add(1);
            self.off_ticks >= DEBOUNCE_TICKS
        }
    }

    /// Execute a safe hardware shutdown:
    /// 1. Display `"Power Off / Shutting down..."` on the 16x2 LCD.
    /// 2. Invoke `flush_in_flight` so storage/flash/DSP subsystems can finish any
    ///    in-flight writes cleanly before power is removed.
    /// 3. Hold Core 1 in reset (`0x4005_4028 &= !1`), mute audio DAC (`0x424C_4504 = 0`),
    ///    wait 30 ms, power off the LCD/backlight, and assert the hardware power-cut
    ///    latches (`0x424C_4060 = 1`, `0x424C_405C = 1`).
    pub fn safe_shutdown<D: TextDisplay, F: FnOnce()>(
        &mut self,
        disp: &mut D,
        flush_in_flight: F,
    ) -> ! {
        disp.write_line(0, b"Power Off       ");
        disp.write_line(1, b"Shutting down...");
        disp.flush();

        // Allow any in-flight SD card / QSPI flash writes or IPC to complete.
        flush_in_flight();

        // SAFETY: Exclusive ownership of power-cut latches, DAC mute, and Core 1 reset.
        unsafe {
            let pll_ctl = PLL_CFG_BASE + 0x28; // 0x4005_4028
            wr32(pll_ctl, rd32(pll_ctl) & !1);
            dsb();

            // Mute audio DAC (`App1 0x6001_B184(1)`) and wait 30 ms anti-pop settle.
            wr32(AUDIO_MUTE_BB, 0);
            dsb();
            time::delay_ms(30);

            disp.power_off();

            // Cut hardware power (`App1 0x6001_B174`).
            wr32(PWR_OFF_BB_A, 1);
            wr32(PWR_OFF_BB_B, 1);
            dsb();
        }

        loop {
            wfi();
        }
    }
}
