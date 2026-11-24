//! Panasonic/Renesas SDHI SD/MMC Block Device Driver (`0x4000_5000`).
//!
//! Register map verified from `App1` ITCM0 (`0x0100_35F0..0x0100_4E20`,
//! `*[0x6031_10E4] = 0x4000_5000`) and pinmux `App1 0x6001_AF98`:
//! - `+0x000`: `SD_CMD`      - `+0x008/+0x00C`: `SD_ARG0`/`SD_ARG1`
//! - `+0x010`: `SD_STOP`     - `+0x014`: `SD_SECCNT`
//! - `+0x018..+0x034`: `SD_RSP10..SD_RSP76`
//! - `+0x038`: `SD_INFO1` (bit0 RESP_END, bit2 ACCESS_END, bit5 CD)
//! - `+0x03C`: `SD_INFO2` (bits0..6 ERR, bit8 BRE, bit9 BWE, bit14 CBSY)
//! - `+0x048`: `SD_CLK_CTRL` (bits0..7 div, bit8 SCLKEN)
//! - `+0x04C`: `SD_SIZE`     - `+0x050`: `SD_OPTION` (bit15: 1=1bit, 0=4bit)
//! - `+0x060`: `SD_BUF0` (16-bit PIO FIFO)
//! - `+0x1B0`: `CC_EXT_MODE` (bit1=0 PIO) - `+0x1C0`: `SOFT_RST`

#![allow(dead_code)]

use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::task::Poll;

use crate::hal::BlockDevice;
use crate::time;
use embassy_futures::select::{select, Either};
use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::Timer;
use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::{DMAC0_BASE, PLL_CFG_BASE, SDHI_BASE, SDHI_DMA_BASE};

// DMAC0 Channel 3 (SDHI companion DMA, verified from App0_UPD 0x600079F2 / 0x60007A44 / 0x60014C5A)
const DMAC0_CH3: usize = DMAC0_BASE + 0x10 + 3 * 0x10; // 0x4000_1040
const DMAC0_CH3_EXT: usize = DMAC0_BASE + 0x90 + 3 * 12; // 0x4000_10B4
pub const SD_DMA_IRQ_NUM: usize = 34; // 3 + 0x1F (App0_UPD 0x60014C6C)

const NVIC_ISER1: usize = 0xE000_E104;
const NVIC_ICER1: usize = 0xE000_E184;
const NVIC_IPR0: usize = 0xE000_E400;

static SD_DMA_WAKER: AtomicWaker = AtomicWaker::new();
static SD_DMA_DONE: AtomicBool = AtomicBool::new(false);
static SD_DMA_ERR: AtomicU8 = AtomicU8::new(0);

/// NVIC IRQ 34 (`DMAC0 CH3` SDHI DMA completion handler, placed in ITCM0).
#[link_section = ".itcm"]
#[no_mangle]
pub unsafe extern "C" fn SD_DMA_IRQ_Handler() {
    let cfg = rd32(DMAC0_CH3 + 0x04);
    let st = cfg & 0x70000;
    if st != 0 {
        wr32(DMAC0_CH3 + 0x04, cfg & !0x70000);
        if st != 0x50000 {
            SD_DMA_ERR.store((st >> 16) as u8, Ordering::Release);
        }
        SD_DMA_DONE.store(true, Ordering::Release);
        SD_DMA_WAKER.wake();
    } else {
        // Guard against an unacknowledged IRQ 34 storm freezing Core 0.
        wr32(NVIC_ICER1, 1u32 << (SD_DMA_IRQ_NUM - 32));
        dsb();
    }
}

// SDHI register offsets (32-bit stride = 2x classic 16-bit SDHI).
const SD_CMD: usize = SDHI_BASE + 0x000;
const SD_PORTSEL: usize = SDHI_BASE + 0x004;
const SD_ARG0: usize = SDHI_BASE + 0x008;
const SD_ARG1: usize = SDHI_BASE + 0x00C;
const SD_STOP: usize = SDHI_BASE + 0x010;
const SD_SECCNT: usize = SDHI_BASE + 0x014;
const SD_RSP10: usize = SDHI_BASE + 0x018;
const SD_RSP32: usize = SDHI_BASE + 0x020;
const SD_RSP54: usize = SDHI_BASE + 0x028;
const SD_RSP76: usize = SDHI_BASE + 0x030;
const SD_INFO1: usize = SDHI_BASE + 0x038;
const SD_INFO2: usize = SDHI_BASE + 0x03C;
const SD_INFO1_MASK: usize = SDHI_BASE + 0x040;
const SD_INFO2_MASK: usize = SDHI_BASE + 0x044;
const SD_CLK_CTRL: usize = SDHI_BASE + 0x048;
const SD_SIZE: usize = SDHI_BASE + 0x04C;
const SD_OPTION: usize = SDHI_BASE + 0x050;
const SD_BUF0: usize = SDHI_BASE + 0x060;
const CC_EXT_MODE: usize = SDHI_BASE + 0x1B0;
const SOFT_RST: usize = SDHI_BASE + 0x1C0;
const SD_PWR_CTRL: usize = SDHI_BASE + 0x1E4;
const SD_EXT_MASK: usize = SDHI_BASE + 0x1F8;
const CLK_EXT: usize = SDHI_BASE + 0x200;
const SD_SIZE_EXT: usize = SDHI_BASE + 0x20C;

// SD_INFO1 bits
const INFO1_RESP_END: u32 = 1 << 0;
const INFO1_ACCESS_END: u32 = 1 << 2;
const INFO1_CD: u32 = 1 << 5;

// SD_INFO2 bits
const INFO2_ERR_MASK: u32 = 0x007F;
const INFO2_BRE: u32 = 1 << 8;
const INFO2_BWE: u32 = 1 << 9;
const INFO2_CBSY: u32 = 1 << 14;

// SD_CMD response / data flags (standard Renesas/Panasonic SDHI)
const CMD_RESP_NONE: u32 = 0x0300;
const CMD_RESP_R1: u32 = 0x0400;
const CMD_RESP_R1B: u32 = 0x0500;
const CMD_RESP_R2: u32 = 0x0600;
const CMD_RESP_R3: u32 = 0x0700;
const CMD_DATA: u32 = 0x0800;
const CMD_READ: u32 = 0x1000;
const CMD_ACMD: u32 = 0x0040;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdError {
    NoCard,
    Timeout,
    CmdError(u16),
    InitFailed(u8, u16),
    NotMounted,
}

impl core::fmt::Display for SdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SdError::NoCard => write!(f, "no SD card"),
            SdError::Timeout => write!(f, "SD timeout"),
            SdError::CmdError(c) => write!(f, "SD cmd error 0x{:04X}", c),
            SdError::InitFailed(step, c) => write!(f, "SD init step {} err 0x{:04X}", step, c),
            SdError::NotMounted => write!(f, "SD not mounted"),
        }
    }
}

impl core::error::Error for SdError {}

/// 32-byte-aligned 4 KiB (8-sector) SDRAM bounce buffer for synchronous `embedded-sdmmc`
/// `DMAC0 CH3` transfers (safe even when caller buffers reside on the Core 0 ITCM stack).
const FS_DMA_SECTORS: usize = 8;
#[repr(C, align(32))]
struct FsDmaBuf([u8; FS_DMA_SECTORS * 512]);
static mut FS_DMA_BUF: FsDmaBuf = FsDmaBuf([0u8; FS_DMA_SECTORS * 512]);

#[derive(Clone, Copy, Debug)]
pub struct SdCardInfo {
    pub rca: u16,
    pub is_sdhc: bool,
    pub block_count: u64,
}

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline(always)]
unsafe fn rd16(addr: usize) -> u16 {
    core::ptr::read_volatile(addr as *const u16)
}

#[inline(always)]
unsafe fn wr16(addr: usize, val: u16) {
    core::ptr::write_volatile(addr as *mut u16, val);
}

/// Owned handle to the MC-101 SD/MMC host controller (`0x4000_5000`).
pub struct SdCard {
    info: Option<SdCardInfo>,
}

impl SdCard {
    pub(crate) const fn new() -> Self {
        Self { info: None }
    }

    /// Configure SDHI pinmux (`App1 0x6001_AF98`) and reset the SDHI host (`0x0100_3994`).
    pub fn init_host(&mut self) {
        self.info = None;
        // SAFETY: Exclusive ownership of SDHI & pinmux guaranteed by Peripherals::take().
        unsafe {
            // 1. SDHI pinmux (`App1 0x6001_AF98`), clock gate (`0x6001_AAE8`: 0x4005_4028 |= 0x40),
            //    and SD card 3.3V power switch (`0x6001_B120(1)` / `0x6001_B160(1)`).
            for addr in [
                0x4002_6690usize,
                0x4002_6694,
                0x4002_6698,
                0x4002_669C,
                0x4002_66A0,
            ] {
                wr32(addr, 0x0C02);
            }
            wr32(0x4002_6674, 0x0802);
            wr32(0x4002_66AC, 0x0802);
            wr32(0x4002_66A4, 0x0000);
            wr32(0x4002_66A8, 0x0000);
            wr32(0x4002_6864, 0x0000);
            wr32(0x4002_6868, 0x0000);

            let gate = PLL_CFG_BASE + 0x28; // 0x4005_4028
            wr32(gate, rd32(gate) | 0x40);

            // SD power rail GPIOs matching App0_UPD 0x600035F0..3616 + 0x60003682(1):
            // bit 20 (0x424C_4050) = 1, bit 21 (0x424C_4054) = 0.
            wr32(0x424C_4050, 1);
            wr32(0x4002_6458, 0x0800);
            wr32(0x424C_4054, 0);
            wr32(0x4002_645C, 0x0800);

            // Select SDHI Port 0 (`App0_UPD 0x600083BA`) and enable SDHI built-in Port 0
            // card power switch (`App0_UPD 0x60008FAC`: `SDHI + 0x1E4 = 1`).
            wr32(SD_PORTSEL, 0);
            wr32(SD_PWR_CTRL, 1);
            dsb();
            time::delay_ms(50);

            // 2. Software reset pulse (`App1 0x0100_399C` / `App0_UPD 0x60007BE4`)
            wr32(SOFT_RST, rd32(SOFT_RST) | 1);
            wr32(SOFT_RST, rd32(SOFT_RST) & !1);
            wr32(SOFT_RST, rd32(SOFT_RST) | 1);
            wr32(SOFT_RST, rd32(SOFT_RST) | 6);

            // 3. Preserve SD_OPTION[14:8] (RMW like App1 0x0100_397C/39DC), 1-bit mode,
            //    and clear SD_INFO1/SD_INFO2 status (`App1 0x0100_3D0E..3D1C`).
            wr32(SD_INFO1_MASK, 0xFFFF_FFFF);
            wr32(SD_INFO2_MASK, 0xFFFF_FFFF);
            wr32(SD_EXT_MASK, 0xFFFF);
            wr32(SD_INFO1, rd32(SD_INFO1) & 0x04A0);
            wr32(SD_INFO2, rd32(SD_INFO2) & 0x4080);

            let opt = (rd32(SD_OPTION) & !0x000F) | 0x0006 | 0x8000; // 1-bit, mode 6
            wr32(SD_OPTION, opt);
            wr32(SD_STOP, rd32(SD_STOP) | 0x0100);
            wr32(SD_SIZE, 512);
            wr32(SD_SIZE_EXT, 512);
            wr32(CC_EXT_MODE, rd32(CC_EXT_MODE) & !2); // PIO mode

            // 4. Identification clock (~152 kHz: div = 0x40, SCLKEN = 0x100; App1 0x0100_3910(215)).
            self.set_clock_div(0x40);

            // Enable NVIC IRQ 34 (DMAC0 CH3 SDHI DMA completion).
            core::ptr::write_volatile((NVIC_IPR0 + SD_DMA_IRQ_NUM) as *mut u8, 5 << 4);
            core::ptr::write_volatile(NVIC_ISER1 as *mut u32, 1u32 << (SD_DMA_IRQ_NUM - 32));
            dsb();
        }
    }

    unsafe fn set_clock_div(&mut self, div: u8) {
        wr32(SD_CLK_CTRL, rd32(SD_CLK_CTRL) & !0x100);
        wr32(CLK_EXT, rd32(CLK_EXT) & !0x10);
        let cur = (rd32(SD_CLK_CTRL) & !0xFF) | (div as u32);
        wr32(SD_CLK_CTRL, cur);
        wr32(SD_CLK_CTRL, cur | 0x100);
        dsb();
    }

    /// Enable 1:1 SD High-Speed (~39-50 MHz) clock bypass (`App0_UPD 0x6000_7BC2..0x6000_7BCA`:
    /// `CLK_EXT |= 0x10`, `SD_CLK_CTRL[7:0] = 0xFF`, `SCLKEN = 0x100`).
    unsafe fn set_clock_high_speed(&mut self) {
        wr32(SD_CLK_CTRL, rd32(SD_CLK_CTRL) & !0x100);
        wr32(CLK_EXT, rd32(CLK_EXT) | 0x10);
        let cur = (rd32(SD_CLK_CTRL) & !0xFF) | 0xFF;
        wr32(SD_CLK_CTRL, cur);
        wr32(SD_CLK_CTRL, cur | 0x100);
        dsb();
    }

    /// Issue 64-byte PIO `CMD6` (`SWITCH_FUNC`, `0x1C06`) and read the 64-byte switch status
    /// block (`App0_UPD 0x6001_4820..0x6001_486A`).
    unsafe fn send_cmd6_pio(&mut self, arg: u32, status: &mut [u8; 64]) -> Result<(), SdError> {
        wr32(SD_SECCNT, 1);
        self.send_cmd(CMD_RESP_R1 | CMD_DATA | CMD_READ | 6, arg)?;

        let deadline = time::now_ms().wrapping_add(500);
        loop {
            let i2 = rd32(SD_INFO2);
            if (i2 & INFO2_ERR_MASK) != 0 {
                let err = (i2 & INFO2_ERR_MASK) as u16;
                wr32(SD_INFO2, !INFO2_ERR_MASK);
                return Err(SdError::CmdError(err));
            }
            if (i2 & INFO2_BRE) != 0 {
                wr32(SD_INFO2, !INFO2_BRE);
                break;
            }
            if time::now_ms() >= deadline {
                return Err(SdError::Timeout);
            }
            core::hint::spin_loop();
        }

        for i in 0..32 {
            let hw = rd16(SD_BUF0);
            status[i * 2] = (hw & 0xFF) as u8;
            status[i * 2 + 1] = (hw >> 8) as u8;
        }

        let deadline = time::now_ms().wrapping_add(500);
        while (rd32(SD_INFO1) & INFO1_ACCESS_END) == 0 {
            if time::now_ms() >= deadline {
                return Err(SdError::Timeout);
            }
            core::hint::spin_loop();
        }
        wr32(SD_INFO1, !INFO1_ACCESS_END);
        self.wait_not_busy()
    }

    /// Probe and switch the SD card to High-Speed mode via `CMD6` (`0x00FF_FFF1` check,
    /// `0x80FF_FFF1` switch), then enable `CLK_EXT |= 0x10` (`App0_UPD 0x6000_856C..0x6000_863E`).
    unsafe fn try_switch_high_speed(&mut self) {
        let mut status = [0u8; 64];
        if self.send_cmd(CMD_RESP_R1 | 16, 64).is_err() {
            return;
        }
        wr32(SD_SIZE, 64);
        wr32(SD_SIZE_EXT, 64);

        let check_ok = self.send_cmd6_pio(0x00FF_FFF1, &mut status).is_ok();
        time::delay_ms(10);

        let mut switched = false;
        if check_ok && (status[16] & 0x0F) == 1 && (status[13] & 0x02) != 0 {
            status.fill(0);
            if self.send_cmd6_pio(0x80FF_FFF1, &mut status).is_ok() {
                time::delay_ms(10);
                if (status[16] & 0x0F) == 1 {
                    switched = true;
                }
            }
        }

        wr32(SD_SIZE, 512);
        wr32(SD_SIZE_EXT, 512);
        let _ = self.send_cmd(CMD_RESP_R1 | 16, 512);

        if switched {
            self.set_clock_high_speed();
        }
    }

    unsafe fn wait_not_busy(&self) -> Result<(), SdError> {
        let deadline = time::now_ms().wrapping_add(500);
        while (rd32(SD_INFO2) & INFO2_CBSY) != 0 {
            if time::now_ms() >= deadline {
                return Err(SdError::Timeout);
            }
            core::hint::spin_loop();
        }
        Ok(())
    }

    unsafe fn send_cmd(&self, cmd: u32, arg: u32) -> Result<u32, SdError> {
        self.wait_not_busy()?;

        // Clear previous status flags.
        wr32(SD_INFO1, !(INFO1_RESP_END | INFO1_ACCESS_END));
        wr32(SD_INFO2, !(INFO2_ERR_MASK | INFO2_BRE | INFO2_BWE));

        // Exact command write order from App1 0x0100_3A64.
        wr32(SD_OPTION, (rd32(SD_OPTION) & !0xF0) | 0xE0);
        wr32(CC_EXT_MODE, rd32(CC_EXT_MODE) & !2);
        wr32(SD_ARG0, arg & 0xFFFF);
        wr32(SD_ARG1, (arg >> 16) & 0xFFFF);
        wr32(SD_CMD, cmd);
        dsb();

        let deadline = time::now_ms().wrapping_add(500);
        loop {
            let i1 = rd32(SD_INFO1);
            let i2 = rd32(SD_INFO2);
            if (i2 & INFO2_ERR_MASK) != 0 {
                let err = (i2 & INFO2_ERR_MASK) as u16;
                wr32(SD_INFO2, !INFO2_ERR_MASK);
                return Err(SdError::CmdError(err));
            }
            if (i1 & INFO1_RESP_END) != 0 {
                wr32(SD_INFO1, !INFO1_RESP_END);
                let lo = rd32(SD_RSP10) & 0xFFFF;
                let hi = rd32(SD_RSP10 + 4) & 0xFFFF;
                return Ok((hi << 16) | lo);
            }
            if time::now_ms() >= deadline {
                return Err(SdError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    unsafe fn send_acmd(
        &mut self,
        rca: u16,
        acmd_no_flag: u32,
        arg: u32,
    ) -> Result<u32, (u8, SdError)> {
        // Exact App0_UPD sequence (0x600086CA + 0x600086EE):
        // Issue CMD55 (0x0437) manually, then the ACMD with CMD_ACMD (0x0040) set
        // (e.g. 0x0769 for ACMD41, 0x0446 for ACMD6).
        self.send_cmd(CMD_RESP_R1 | 55, (rca as u32) << 16)
            .map_err(|e| (55u8, e))?;
        self.send_cmd(acmd_no_flag | CMD_ACMD, arg)
            .map_err(|e| ((acmd_no_flag & 0x3F) as u8, e))
    }

    fn err_code(e: SdError) -> u16 {
        match e {
            SdError::CmdError(b) => b,
            SdError::Timeout => 0xEEEE,
            _ => 0xFFFF,
        }
    }

    /// Initialize and identify the inserted SD/SDHC/SDXC card (4-bit mode, fast clock).
    pub fn mount(&mut self) -> Result<SdCardInfo, SdError> {
        self.init_host();
        if !self.is_card_present() {
            return Err(SdError::NoCard);
        }
        // Allow >= 74 SD clock cycles after clock enable (10 ms @ 152 kHz = ~1520 cycles).
        time::delay_ms(10);

        // SAFETY: Exclusive ownership of SDHI guaranteed by Peripherals::take().
        unsafe {
            // 1. CMD0: GO_IDLE_STATE
            self.send_cmd(CMD_RESP_NONE | 0, 0)
                .map_err(|e| SdError::InitFailed(0, Self::err_code(e)))?;

            // 2. CMD8: SEND_IF_COND (2.7-3.6V, check pattern 0xAA).
            //    If CMD8 fails (SDv1 card), re-issue CMD0 like App0_UPD 0x60008680.
            let v2 = self.send_cmd(CMD_RESP_R1 | 8, 0x0000_01AA).is_ok();
            if !v2 {
                let _ = self.send_cmd(CMD_RESP_NONE | 0, 0);
            }

            // 3. ACMD41: SD_SEND_OP_COND until card ready (OCR bit 31 == 1)
            let hcs = if v2 { 0x40FC_0000u32 } else { 0x00FC_0000u32 };
            let deadline = time::now_ms().wrapping_add(1500);
            let ocr = loop {
                let r = self
                    .send_acmd(0, CMD_RESP_R3 | 41, hcs)
                    .map_err(|(step, e)| SdError::InitFailed(step, Self::err_code(e)))?;
                if (r & (1 << 31)) != 0 {
                    break r;
                }
                if time::now_ms() >= deadline {
                    return Err(SdError::InitFailed(41, 0x7000));
                }
                time::delay_ms(5);
            };
            let is_sdhc = (ocr & (1 << 30)) != 0;

            // 4. CMD2: ALL_SEND_CID
            self.send_cmd(CMD_RESP_R2 | 2, 0)
                .map_err(|e| SdError::InitFailed(2, Self::err_code(e)))?;

            // 5. CMD3: SEND_RELATIVE_ADDR
            let r6 = self
                .send_cmd(CMD_RESP_R1 | 3, 0)
                .map_err(|e| SdError::InitFailed(3, Self::err_code(e)))?;
            let rca = (r6 >> 16) as u16;

            // 6. CMD9: SEND_CSD -> decode card capacity in 512-byte blocks
            self.send_cmd(CMD_RESP_R2 | 9, (rca as u32) << 16)
                .map_err(|e| SdError::InitFailed(9, Self::err_code(e)))?;
            let block_count = self.decode_csd_blocks();

            // 7. CMD7: SELECT_CARD
            self.send_cmd(CMD_RESP_R1B | 7, (rca as u32) << 16)
                .map_err(|e| SdError::InitFailed(7, Self::err_code(e)))?;

            // 8. ACMD6(2): Switch to 4-bit bus width on card & host (`SD_OPTION` bit15 = 0).
            if self.send_acmd(rca, CMD_RESP_R1 | 6, 2).is_ok() {
                wr32(SD_OPTION, rd32(SD_OPTION) & !0x8000);
            }

            // 9. Switch to Default-Speed data clock (div = 0x00 = /2 -> ~19.5-25 MHz), set 512-byte
            //    block length, and attempt App0_UPD CMD6 High-Speed (~39-50 MHz 1:1 bypass) upgrade.
            self.set_clock_div(0x00);
            let _ = self.send_cmd(CMD_RESP_R1 | 16, 512);
            if v2 {
                self.try_switch_high_speed();
            }

            let info = SdCardInfo {
                rca,
                is_sdhc,
                block_count,
            };
            self.info = Some(info);
            Ok(info)
        }
    }

    /// Decode 128-bit CSD response (`SD_RSP10..SD_RSP76`, shifted down 8 bits by SDHI)
    /// into total 512-byte block count.
    unsafe fn decode_csd_blocks(&self) -> u64 {
        // SDHI stores bits [127:8] of the R2 response in RSP76..RSP10 (120 bits).
        // Each 32-bit word is split across two 16-bit registers at +0/+4.
        let r0 = (rd32(SD_RSP10) & 0xFFFF) | ((rd32(SD_RSP10 + 4) & 0xFFFF) << 16);
        let r1 = (rd32(SD_RSP32) & 0xFFFF) | ((rd32(SD_RSP32 + 4) & 0xFFFF) << 16);
        let r2 = (rd32(SD_RSP54) & 0xFFFF) | ((rd32(SD_RSP54 + 4) & 0xFFFF) << 16);
        let r3 = (rd32(SD_RSP76) & 0xFFFF) | ((rd32(SD_RSP76 + 4) & 0xFFFF) << 16);

        // CSD_STRUCTURE is bits [127:126] -> bits [23:22] of r3 (since shifted right 8).
        let csd_ver = (r3 >> 22) & 0x3;
        if csd_ver == 1 {
            // CSD v2.0 (SDHC/SDXC): C_SIZE is bits [69:48] -> shifted right 8 = [61:40]
            // = bits [29:8] of r1 (22 bits).
            let c_size = ((r1 >> 8) & 0x003F_FFFF) as u64;
            (c_size + 1) * 1024
        } else {
            // CSD v1.0 (SDSC):
            // READ_BL_LEN [83:80] -> [75:72] = r2[11:8]
            // C_SIZE [73:62] -> [65:54] = (r2[1:0] << 10) | r1[31:22]
            // C_SIZE_MULT [49:47] -> [41:39] = r1[9:7]
            let _ = r0;
            let read_bl_len = (r2 >> 8) & 0xF;
            let c_size = (((r2 & 0x3) << 10) | ((r1 >> 22) & 0x3FF)) as u64;
            let c_size_mult = ((r1 >> 7) & 0x7) as u32;
            let mult = 1u64 << (c_size_mult + 2);
            let block_len = 1u64 << read_bl_len;
            ((c_size + 1) * mult * block_len) / 512
        }
    }

    pub fn info(&self) -> Option<SdCardInfo> {
        self.info
    }

    /// Issue an SDHI command in DMA mode (`CC_EXT_MODE = 2`, matching `App0_UPD 0x60007C94`).
    unsafe fn send_cmd_dma(&self, cmd: u32, arg: u32) -> Result<u32, SdError> {
        self.wait_not_busy()?;
        wr32(SD_INFO1, !(INFO1_RESP_END | INFO1_ACCESS_END));
        wr32(SD_INFO2, !(INFO2_ERR_MASK | INFO2_BRE | INFO2_BWE));
        wr32(SD_OPTION, (rd32(SD_OPTION) & !0xF0) | 0xE0);
        wr32(CC_EXT_MODE, 2); // Enable SDHI companion DMA mode
        wr32(SD_ARG0, arg & 0xFFFF);
        wr32(SD_ARG1, (arg >> 16) & 0xFFFF);
        wr32(SD_CMD, cmd);
        dsb();

        let deadline = time::now_ms().wrapping_add(500);
        loop {
            let i1 = rd32(SD_INFO1);
            let i2 = rd32(SD_INFO2);
            if (i2 & INFO2_ERR_MASK) != 0 {
                let err = (i2 & INFO2_ERR_MASK) as u16;
                wr32(SD_INFO2, !INFO2_ERR_MASK);
                wr32(CC_EXT_MODE, rd32(CC_EXT_MODE) & !2);
                return Err(SdError::CmdError(err));
            }
            if (i1 & INFO1_RESP_END) != 0 {
                wr32(SD_INFO1, !INFO1_RESP_END);
                let lo = rd32(SD_RSP10) & 0xFFFF;
                let hi = rd32(SD_RSP10 + 4) & 0xFFFF;
                return Ok((hi << 16) | lo);
            }
            if time::now_ms() >= deadline {
                wr32(CC_EXT_MODE, rd32(CC_EXT_MODE) & !2);
                return Err(SdError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// Start `DMAC0 CH3` 32-byte burst DMA between `SDHI_DMA_BASE` (`0x4000_3000`) and SDRAM.
    unsafe fn start_sd_dma(&self, ram_addr: u32, byte_len: usize, is_read: bool) {
        SD_DMA_DONE.store(false, Ordering::Release);
        SD_DMA_ERR.store(0, Ordering::Release);
        let bursts = ((byte_len >> 5) - 1) as u32 & 0xFFFF; // 32-byte bursts
        wr32(CLK_EXT, rd32(CLK_EXT) | 8);
        if is_read {
            wr32(DMAC0_CH3 + 0x00, 0x11C0_0000 | bursts);
            wr32(DMAC0_CH3 + 0x04, 0x0A18_0000);
            wr32(DMAC0_CH3 + 0x08, SDHI_DMA_BASE as u32);
            wr32(DMAC0_CH3 + 0x0C, ram_addr);
        } else {
            wr32(DMAC0_CH3 + 0x00, 0x10C0_0000 | bursts);
            wr32(DMAC0_CH3 + 0x04, 0x0918_0000);
            wr32(DMAC0_CH3 + 0x08, ram_addr);
            wr32(DMAC0_CH3 + 0x0C, SDHI_DMA_BASE as u32);
        }
        wr32(DMAC0_CH3_EXT + 0x00, 0);
        wr32(DMAC0_CH3_EXT + 0x04, 0);
        wr32(DMAC0_CH3_EXT + 0x08, 0);
        dsb();
        wr32(DMAC0_CH3 + 0x00, rd32(DMAC0_CH3 + 0x00) | 0x8000_0000);
        dsb();
    }

    unsafe fn stop_sd_dma(&self) {
        wr32(DMAC0_CH3 + 0x00, rd32(DMAC0_CH3 + 0x00) & !0xA000_0000);
        wr32(CC_EXT_MODE, rd32(CC_EXT_MODE) & !2);
        dsb();
    }

    /// Async multi-block DMA read into a 32-byte-aligned SDRAM/SRAM slice (`buf.len()` must be a
    /// non-zero multiple of 512 bytes, up to 64 KiB). Woken by NVIC IRQ 34.
    pub async fn read_blocks_dma(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), SdError> {
        let count = (buf.len() / 512) as u32;
        if count == 0 || (buf.len() & 511) != 0 {
            return Ok(());
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let addr = if info.is_sdhc { lba } else { lba.wrapping_mul(512) };

        // SAFETY: Exclusive ownership of SDHI + DMAC0 CH3; caller provides 32B-aligned SDRAM buffer.
        // Hardware-verified on MC-101 (`sda: sda1`): arm DMAC0 CH3 *before* SD_CMD, with
        // SD_STOP = 0x0100 (hardware auto-CMD12) and CMD17 = 0x0011 / CMD18 = 0x0012.
        unsafe {
            wr32(SD_SIZE, 512);
            wr32(SD_SIZE_EXT, 512);
            wr32(SD_STOP, 0x0100);
            wr32(SD_SECCNT, count);
            self.start_sd_dma(buf.as_mut_ptr() as u32, buf.len(), true);
            let cmd = if count == 1 {
                0x0011 // DMA CMD17 (hardware auto-decode)
            } else {
                0x0012 // DMA CMD18 (hardware auto-decode + auto-CMD12)
            };
            if let Err(e) = self.send_cmd_dma(cmd, addr) {
                self.stop_sd_dma();
                return Err(e);
            }
        }

        // Await IRQ 34 (DMAC0 CH3 completion) with a 2 s safety timeout.
        let dma_wait = poll_fn(|cx| {
            SD_DMA_WAKER.register(cx.waker());
            if SD_DMA_DONE.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        if matches!(select(dma_wait, Timer::after_millis(2000)).await, Either::Second(())) {
            unsafe {
                self.stop_sd_dma();
                wr32(SD_STOP, 0x0101);
                let _ = self.wait_not_busy();
            }
            return Err(SdError::Timeout);
        }

        unsafe {
            self.stop_sd_dma();
            let err_code = SD_DMA_ERR.load(Ordering::Acquire);
            let deadline = time::now_ms().wrapping_add(500);
            while (rd32(SD_INFO1) & INFO1_ACCESS_END) == 0 {
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }
            wr32(SD_INFO1, !INFO1_ACCESS_END);
            self.wait_not_busy()?;
            if err_code != 0 {
                return Err(SdError::CmdError(err_code as u16));
            }
        }
        Ok(())
    }

    /// Async multi-block DMA write from a 32-byte-aligned SDRAM/SRAM slice (`buf.len()` must be a
    /// non-zero multiple of 512 bytes, up to 64 KiB). Woken by NVIC IRQ 34.
    pub async fn write_blocks_dma(&mut self, lba: u32, buf: &[u8]) -> Result<(), SdError> {
        let count = (buf.len() / 512) as u32;
        if count == 0 || (buf.len() & 511) != 0 {
            return Ok(());
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let addr = if info.is_sdhc { lba } else { lba.wrapping_mul(512) };

        // SAFETY: Exclusive ownership of SDHI + DMAC0 CH3; caller provides 32B-aligned SDRAM buffer.
        // Arm DMAC0 CH3 *before* SD_CMD, with SD_STOP = 0x0100 and CMD24 = 0x0018 / CMD25 = 0x0019.
        unsafe {
            wr32(SD_SIZE, 512);
            wr32(SD_SIZE_EXT, 512);
            wr32(SD_STOP, 0x0100);
            wr32(SD_SECCNT, count);
            self.start_sd_dma(buf.as_ptr() as u32, buf.len(), false);
            let cmd = if count == 1 {
                0x0018 // DMA CMD24 (hardware auto-decode)
            } else {
                0x0019 // DMA CMD25 (hardware auto-decode + auto-CMD12)
            };
            if let Err(e) = self.send_cmd_dma(cmd, addr) {
                self.stop_sd_dma();
                return Err(e);
            }
        }

        // Await IRQ 34 (DMAC0 CH3 completion) with a 2 s safety timeout.
        let dma_wait = poll_fn(|cx| {
            SD_DMA_WAKER.register(cx.waker());
            if SD_DMA_DONE.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        if matches!(select(dma_wait, Timer::after_millis(2000)).await, Either::Second(())) {
            unsafe {
                self.stop_sd_dma();
                wr32(SD_STOP, 0x0101);
                let _ = self.wait_not_busy();
            }
            return Err(SdError::Timeout);
        }

        unsafe {
            self.stop_sd_dma();
            let err_code = SD_DMA_ERR.load(Ordering::Acquire);
            let deadline = time::now_ms().wrapping_add(1000);
            loop {
                let i1 = rd32(SD_INFO1);
                let i2 = rd32(SD_INFO2);
                if (i2 & INFO2_ERR_MASK) != 0 {
                    let err = (i2 & INFO2_ERR_MASK) as u16;
                    wr32(SD_INFO2, !INFO2_ERR_MASK);
                    return Err(SdError::CmdError(err));
                }
                if (i1 & INFO1_ACCESS_END) != 0 {
                    wr32(SD_INFO1, !INFO1_ACCESS_END);
                    break;
                }
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }
            self.wait_not_busy()?;
            if err_code != 0 {
                return Err(SdError::CmdError(err_code as u16));
            }
        }
        Ok(())
    }

    /// Synchronous `DMAC0 CH3` multi-block read for `embedded-sdmmc::BlockDevice`.
    /// Bounces through 32-byte-aligned SDRAM (`FS_DMA_BUF`) so stack/ITCM `Block` slices work safely.
    pub fn read_blocks_sync_dma(
        &self,
        lba: u32,
        blocks: &mut [embedded_sdmmc::Block],
    ) -> Result<(), SdError> {
        if blocks.is_empty() {
            return Ok(());
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let mut done = 0usize;

        // SAFETY: Exclusive SDHI + DMAC0 CH3 access guaranteed by SdCard ownership / SharedSd mutex.
        unsafe {
            let dma_buf = &mut *(&raw mut FS_DMA_BUF.0);
            while done < blocks.len() {
                let chunk = (blocks.len() - done).min(FS_DMA_SECTORS);
                let byte_len = chunk * 512;
                let cur_lba = lba.wrapping_add(done as u32);
                let addr = if info.is_sdhc {
                    cur_lba
                } else {
                    cur_lba.wrapping_mul(512)
                };

                wr32(SD_SIZE, 512);
                wr32(SD_SIZE_EXT, 512);
                wr32(SD_STOP, 0x0100);
                wr32(SD_SECCNT, chunk as u32);
                self.start_sd_dma(dma_buf.as_mut_ptr() as u32, byte_len, true);
                let cmd = if chunk == 1 { 0x0011 } else { 0x0012 };
                if let Err(e) = self.send_cmd_dma(cmd, addr) {
                    self.stop_sd_dma();
                    return Err(e);
                }

                let deadline = time::now_ms().wrapping_add(1000);
                while !SD_DMA_DONE.load(Ordering::Acquire) {
                    if time::now_ms() >= deadline {
                        self.stop_sd_dma();
                        wr32(SD_STOP, 0x0101);
                        let _ = self.wait_not_busy();
                        return Err(SdError::Timeout);
                    }
                    core::hint::spin_loop();
                }

                self.stop_sd_dma();
                let err_code = SD_DMA_ERR.load(Ordering::Acquire);
                let deadline = time::now_ms().wrapping_add(500);
                while (rd32(SD_INFO1) & INFO1_ACCESS_END) == 0 {
                    if time::now_ms() >= deadline {
                        return Err(SdError::Timeout);
                    }
                    core::hint::spin_loop();
                }
                wr32(SD_INFO1, !INFO1_ACCESS_END);
                self.wait_not_busy()?;
                if err_code != 0 {
                    return Err(SdError::CmdError(err_code as u16));
                }

                for i in 0..chunk {
                    blocks[done + i]
                        .contents
                        .copy_from_slice(&dma_buf[i * 512..(i + 1) * 512]);
                }
                done += chunk;
            }
        }
        Ok(())
    }

    /// Synchronous `DMAC0 CH3` multi-block write for `embedded-sdmmc::BlockDevice`.
    /// Bounces through 32-byte-aligned SDRAM (`FS_DMA_BUF`) so stack/ITCM `Block` slices work safely.
    pub fn write_blocks_sync_dma(
        &self,
        lba: u32,
        blocks: &[embedded_sdmmc::Block],
    ) -> Result<(), SdError> {
        if blocks.is_empty() {
            return Ok(());
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let mut done = 0usize;

        // SAFETY: Exclusive SDHI + DMAC0 CH3 access guaranteed by SdCard ownership / SharedSd mutex.
        unsafe {
            let dma_buf = &mut *(&raw mut FS_DMA_BUF.0);
            while done < blocks.len() {
                let chunk = (blocks.len() - done).min(FS_DMA_SECTORS);
                let byte_len = chunk * 512;
                for i in 0..chunk {
                    dma_buf[i * 512..(i + 1) * 512]
                        .copy_from_slice(&blocks[done + i].contents);
                }

                let cur_lba = lba.wrapping_add(done as u32);
                let addr = if info.is_sdhc {
                    cur_lba
                } else {
                    cur_lba.wrapping_mul(512)
                };

                wr32(SD_SIZE, 512);
                wr32(SD_SIZE_EXT, 512);
                wr32(SD_STOP, 0x0100);
                wr32(SD_SECCNT, chunk as u32);
                self.start_sd_dma(dma_buf.as_ptr() as u32, byte_len, false);
                let cmd = if chunk == 1 { 0x0018 } else { 0x0019 };
                if let Err(e) = self.send_cmd_dma(cmd, addr) {
                    self.stop_sd_dma();
                    return Err(e);
                }

                let deadline = time::now_ms().wrapping_add(1000);
                while !SD_DMA_DONE.load(Ordering::Acquire) {
                    if time::now_ms() >= deadline {
                        self.stop_sd_dma();
                        wr32(SD_STOP, 0x0101);
                        let _ = self.wait_not_busy();
                        return Err(SdError::Timeout);
                    }
                    core::hint::spin_loop();
                }

                self.stop_sd_dma();
                let err_code = SD_DMA_ERR.load(Ordering::Acquire);
                let deadline = time::now_ms().wrapping_add(1000);
                loop {
                    let i1 = rd32(SD_INFO1);
                    let i2 = rd32(SD_INFO2);
                    if (i2 & INFO2_ERR_MASK) != 0 {
                        let err = (i2 & INFO2_ERR_MASK) as u16;
                        wr32(SD_INFO2, !INFO2_ERR_MASK);
                        return Err(SdError::CmdError(err));
                    }
                    if (i1 & INFO1_ACCESS_END) != 0 {
                        wr32(SD_INFO1, !INFO1_ACCESS_END);
                        break;
                    }
                    if time::now_ms() >= deadline {
                        return Err(SdError::Timeout);
                    }
                    core::hint::spin_loop();
                }
                self.wait_not_busy()?;
                if err_code != 0 {
                    return Err(SdError::CmdError(err_code as u16));
                }
                done += chunk;
            }
        }
        Ok(())
    }
}

impl embedded_sdmmc::BlockDevice for SdCard {
    type Error = SdError;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.read_blocks_sync_dma(start_block_idx.0, blocks)
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.write_blocks_sync_dma(start_block_idx.0, blocks)
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        let info = self.info.ok_or(SdError::NotMounted)?;
        let count = info.block_count.min(u32::MAX as u64) as u32;
        Ok(embedded_sdmmc::BlockCount(count))
    }
}

impl BlockDevice for SdCard {
    type Error = SdError;

    #[inline]
    fn is_card_present(&self) -> bool {
        // SAFETY: Read-only SD_INFO1.CD (bit 5) query (`App1 0x0100_3690`).
        unsafe { (rd32(SD_INFO1) & INFO1_CD) != 0 }
    }

    fn block_count(&self) -> u64 {
        self.info.map(|i| i.block_count).unwrap_or(0)
    }

    fn read_block(&mut self, lba: u32, buf: &mut [u8; 512]) -> Result<(), Self::Error> {
        let info = self.info.ok_or(SdError::NotMounted)?;
        let addr = if info.is_sdhc { lba } else { lba.wrapping_mul(512) };

        // SAFETY: Exclusive ownership of SDHI guaranteed by Peripherals::take().
        unsafe {
            wr32(SD_SIZE, 512);
            wr32(SD_SECCNT, 1);
            self.send_cmd(CMD_RESP_R1 | CMD_DATA | CMD_READ | 17, addr)?;

            // Wait for BRE (Buffer Read Enable, SD_INFO2 bit 8).
            let deadline = time::now_ms().wrapping_add(500);
            loop {
                let i2 = rd32(SD_INFO2);
                if (i2 & INFO2_ERR_MASK) != 0 {
                    let err = (i2 & INFO2_ERR_MASK) as u16;
                    wr32(SD_INFO2, !INFO2_ERR_MASK);
                    return Err(SdError::CmdError(err));
                }
                if (i2 & INFO2_BRE) != 0 {
                    wr32(SD_INFO2, !INFO2_BRE);
                    break;
                }
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }

            // Read 256 x 16-bit halfwords from SD_BUF0 (`App1 0x0100_3772`).
            for i in 0..256 {
                let hw = rd16(SD_BUF0);
                buf[i * 2] = (hw & 0xFF) as u8;
                buf[i * 2 + 1] = (hw >> 8) as u8;
            }

            // Wait for ACCESS_END (SD_INFO1 bit 2).
            let deadline = time::now_ms().wrapping_add(500);
            while (rd32(SD_INFO1) & INFO1_ACCESS_END) == 0 {
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }
            wr32(SD_INFO1, !INFO1_ACCESS_END);
            Ok(())
        }
    }

    fn write_block(&mut self, lba: u32, buf: &[u8; 512]) -> Result<(), Self::Error> {
        let info = self.info.ok_or(SdError::NotMounted)?;
        let addr = if info.is_sdhc { lba } else { lba.wrapping_mul(512) };

        // SAFETY: Exclusive ownership of SDHI guaranteed by Peripherals::take().
        unsafe {
            wr32(SD_SIZE, 512);
            wr32(SD_SECCNT, 1);
            self.send_cmd(CMD_RESP_R1 | CMD_DATA | 24, addr)?;

            // Wait for BWE (Buffer Write Enable, SD_INFO2 bit 9).
            let deadline = time::now_ms().wrapping_add(500);
            loop {
                let i2 = rd32(SD_INFO2);
                if (i2 & INFO2_ERR_MASK) != 0 {
                    let err = (i2 & INFO2_ERR_MASK) as u16;
                    wr32(SD_INFO2, !INFO2_ERR_MASK);
                    return Err(SdError::CmdError(err));
                }
                if (i2 & INFO2_BWE) != 0 {
                    wr32(SD_INFO2, !INFO2_BWE);
                    break;
                }
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }

            // Write 256 x 16-bit halfwords to SD_BUF0.
            for i in 0..256 {
                let hw = u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                wr16(SD_BUF0, hw);
            }

            // Wait for ACCESS_END (SD_INFO1 bit 2) and check for write errors.
            let deadline = time::now_ms().wrapping_add(1000);
            loop {
                let i1 = rd32(SD_INFO1);
                let i2 = rd32(SD_INFO2);
                if (i2 & INFO2_ERR_MASK) != 0 {
                    let err = (i2 & INFO2_ERR_MASK) as u16;
                    wr32(SD_INFO2, !INFO2_ERR_MASK);
                    return Err(SdError::CmdError(err));
                }
                if (i1 & INFO1_ACCESS_END) != 0 {
                    wr32(SD_INFO1, !INFO1_ACCESS_END);
                    break;
                }
                if time::now_ms() >= deadline {
                    return Err(SdError::Timeout);
                }
                core::hint::spin_loop();
            }
            self.wait_not_busy()?;
            Ok(())
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // SAFETY: Exclusive ownership of SDHI; wait until card finishes any busy state.
        unsafe { self.wait_not_busy() }
    }

    /// Multi-block streaming read using SDHI `CMD18` (`0x7C12`, auto-CMD12 enabled in `SD_STOP`).
    fn read_blocks<F>(&mut self, lba: u32, count: u32, mut on_block: F) -> Result<u32, Self::Error>
    where
        F: FnMut(&[u8; 512]) -> bool,
    {
        if count == 0 {
            return Ok(0);
        }
        if count == 1 {
            let mut buf = [0u8; 512];
            self.read_block(lba, &mut buf)?;
            return Ok(if on_block(&buf) { 1 } else { 0 });
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let mut buf = [0u8; 512];
        let mut done = 0u32;

        // SAFETY: Exclusive ownership of SDHI guaranteed by Peripherals::take().
        unsafe {
            while done < count {
                let chunk = (count - done).min(128);
                let cur_lba = lba + done;
                let addr = if info.is_sdhc {
                    cur_lba
                } else {
                    cur_lba.wrapping_mul(512)
                };

                wr32(SD_SIZE, 512);
                wr32(SD_SIZE_EXT, 512);
                wr32(SD_STOP, 0x0100); // Auto-CMD12 enabled
                wr32(SD_SECCNT, chunk);
                // Renesas SDHI CMD18: R1 | DATA | READ | MULTI (0x2000) | 18 = 0x3C12 (auto-CMD12).
                if self.send_cmd(CMD_RESP_R1 | CMD_DATA | CMD_READ | 0x2000 | 18, addr).is_err() {
                    // Fallback to single-block CMD17 if multi-block command is rejected.
                    let _ = self.wait_not_busy();
                    for i in done..count {
                        self.read_block(lba + i, &mut buf)?;
                        if !on_block(&buf) {
                            return Ok(i);
                        }
                    }
                    return Ok(count);
                }

                let mut aborted = false;
                for _ in 0..chunk {
                    let deadline = time::now_ms().wrapping_add(500);
                    loop {
                        let i2 = rd32(SD_INFO2);
                        if (i2 & INFO2_ERR_MASK) != 0 {
                            let err = (i2 & INFO2_ERR_MASK) as u16;
                            wr32(SD_INFO2, !INFO2_ERR_MASK);
                            wr32(SD_STOP, 0x0101);
                            let _ = self.wait_not_busy();
                            return Err(SdError::CmdError(err));
                        }
                        if (i2 & INFO2_BRE) != 0 {
                            wr32(SD_INFO2, !INFO2_BRE);
                            break;
                        }
                        if time::now_ms() >= deadline {
                            wr32(SD_STOP, 0x0101);
                            return Err(SdError::Timeout);
                        }
                        core::hint::spin_loop();
                    }

                    for i in 0..256 {
                        let hw = rd16(SD_BUF0);
                        buf[i * 2] = (hw & 0xFF) as u8;
                        buf[i * 2 + 1] = (hw >> 8) as u8;
                    }

                    if !on_block(&buf) {
                        aborted = true;
                        wr32(SD_STOP, 0x0101);
                        break;
                    }
                    done += 1;
                }

                let deadline = time::now_ms().wrapping_add(500);
                while (rd32(SD_INFO1) & INFO1_ACCESS_END) == 0 {
                    if time::now_ms() >= deadline {
                        break;
                    }
                    core::hint::spin_loop();
                }
                wr32(SD_INFO1, !INFO1_ACCESS_END);
                if aborted {
                    let _ = self.wait_not_busy();
                    return Ok(done);
                }
            }
            Ok(done)
        }
    }

    /// Multi-block streaming write using SDHI `CMD25` (`0x6C19`, auto-CMD12 enabled in `SD_STOP`).
    fn write_blocks<F>(&mut self, lba: u32, count: u32, mut get_block: F) -> Result<u32, Self::Error>
    where
        F: FnMut(&mut [u8; 512]) -> bool,
    {
        if count == 0 {
            return Ok(0);
        }
        if count == 1 {
            let mut buf = [0u8; 512];
            if !get_block(&mut buf) {
                return Ok(0);
            }
            self.write_block(lba, &buf)?;
            return Ok(1);
        }
        let info = self.info.ok_or(SdError::NotMounted)?;
        let mut buf = [0u8; 512];
        let mut done = 0u32;

        // SAFETY: Exclusive ownership of SDHI guaranteed by Peripherals::take().
        unsafe {
            while done < count {
                let chunk = (count - done).min(128);
                let cur_lba = lba + done;
                let addr = if info.is_sdhc {
                    cur_lba
                } else {
                    cur_lba.wrapping_mul(512)
                };

                wr32(SD_SIZE, 512);
                wr32(SD_SIZE_EXT, 512);
                wr32(SD_STOP, 0x0100); // Auto-CMD12 enabled
                wr32(SD_SECCNT, chunk);
                // Renesas SDHI CMD25: R1 | DATA | MULTI (0x2000) | 25 = 0x2C19 (auto-CMD12).
                if self.send_cmd(CMD_RESP_R1 | CMD_DATA | 0x2000 | 25, addr).is_err() {
                    let _ = self.wait_not_busy();
                    for i in done..count {
                        if !get_block(&mut buf) {
                            return Ok(i);
                        }
                        self.write_block(lba + i, &buf)?;
                    }
                    return Ok(count);
                }

                let mut aborted = false;
                for _ in 0..chunk {
                    if !get_block(&mut buf) {
                        aborted = true;
                        wr32(SD_STOP, 0x0101);
                        break;
                    }

                    let deadline = time::now_ms().wrapping_add(1000);
                    loop {
                        let i2 = rd32(SD_INFO2);
                        if (i2 & INFO2_ERR_MASK) != 0 {
                            let err = (i2 & INFO2_ERR_MASK) as u16;
                            wr32(SD_INFO2, !INFO2_ERR_MASK);
                            wr32(SD_STOP, 0x0101);
                            let _ = self.wait_not_busy();
                            return Err(SdError::CmdError(err));
                        }
                        if (i2 & INFO2_BWE) != 0 {
                            wr32(SD_INFO2, !INFO2_BWE);
                            break;
                        }
                        if time::now_ms() >= deadline {
                            wr32(SD_STOP, 0x0101);
                            return Err(SdError::Timeout);
                        }
                        core::hint::spin_loop();
                    }

                    for i in 0..256 {
                        let hw = u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                        wr16(SD_BUF0, hw);
                    }
                    done += 1;
                }

                let deadline = time::now_ms().wrapping_add(1000);
                loop {
                    let i1 = rd32(SD_INFO1);
                    let i2 = rd32(SD_INFO2);
                    if (i2 & INFO2_ERR_MASK) != 0 {
                        let err = (i2 & INFO2_ERR_MASK) as u16;
                        wr32(SD_INFO2, !INFO2_ERR_MASK);
                        return Err(SdError::CmdError(err));
                    }
                    if (i1 & INFO1_ACCESS_END) != 0 {
                        wr32(SD_INFO1, !INFO1_ACCESS_END);
                        break;
                    }
                    if time::now_ms() >= deadline {
                        return Err(SdError::Timeout);
                    }
                    core::hint::spin_loop();
                }
                self.wait_not_busy()?;
                if aborted {
                    return Ok(done);
                }
            }
            Ok(done)
        }
    }
}
