//! Verified Roland BMC (ESC2) SoC Hardware Memory Map (MC-101 / RPG69).
//!
//! Addresses are derived from `docs/system_architecture_and_boot_flow.md`,
//! `docs/core0_boot_and_memory_architecture.md`, and `docs/mc101_core1_structural_map.md`.

/// Core 0 ITCM (512 KiB): Vector table at 0x0100_0000, fast kernel at 0x0100_0400.
pub const CORE0_ITCM_BASE: usize = 0x0100_0000;
pub const CORE0_ITCM_SIZE: usize = 512 * 1024;
pub const CORE0_INITIAL_MSP: u32 = 0x0105_0800;

/// Core 1 ITCM (256 KiB): 32-byte zero header at +0x00, vector table at +0x20.
/// Also aliased locally on Core 1 at 0x0000_0000..0x0003_FFFF.
pub const CORE1_ITCM_BASE: usize = 0x0110_0000;
pub const CORE1_ITCM_SIZE: usize = 256 * 1024;
pub const CORE1_INITIAL_MSP: u32 = 0x200A_91C8;

/// Inter-Core IPC Mailbox SRAM (4 KiB+ at 0x0114_0000; Core 1 also uses 0x0114_2000).
pub const IPC_MBOX_BASE: usize = 0x0114_0000;
pub const IPC_MBOX_SIZE: usize = 4 * 1024;
/// One-shot handoff magic written by `mc-101-boot` when `[c1a] image` is pre-loaded into
/// `0x0110_0000` (`idm1.bin`) and `0x6100_0000` (`sdram1.bin`).
pub const CORE1_PRELOADED_MAGIC_ADDR: usize = IPC_MBOX_BASE + 0x0FF0;
pub const CORE1_PRELOADED_MAGIC: u32 = 0x4331_5344; // "C1SD"

/// Silicon Mask Boot ROM (16 KiB, read-only; reading >= 0x0120_4000 times out).
pub const BOOTROM_BASE: usize = 0x0120_0000;
pub const BOOTROM_SIZE: usize = 16 * 1024;

/// Direct AXI Aperture for QSPI1 NOR Flash (16 MiB, Core 1 / DMAC1).
pub const QSPI1_AXI_BASE: usize = 0x0C00_0000;
pub const QSPI1_AXI_SIZE: usize = 16 * 1024 * 1024;

/// Main Shared System SRAM (768 KiB+).
pub const SYSTEM_SRAM_BASE: usize = 0x2000_0000;
pub const SYSTEM_SRAM_SIZE: usize = 768 * 1024;

/// Core 0 -> Core 1 IPC Anchor Pointer in System SRAM.
pub const IPC_CORE1_ANCHOR_PTR: usize = 0x2000_03E0;

/// Physical External SDRAM (32 MiB total, shared across AXI matrix).
pub const SDRAM_BASE: usize = 0x6000_0000;
pub const SDRAM_SIZE: usize = 32 * 1024 * 1024;

/// Core 0 App1_Main load & entry address in SDRAM.
pub const CORE0_SDRAM_BASE: usize = 0x6000_0000;

/// Core 0 <-> Core 1 shared IPC header/tail words in SDRAM.
pub const IPC_CORE1_MBOX_HDR: usize = 0x60F0_0000;
pub const IPC_CORE1_MBOX_TAIL: usize = 0x60F0_0004;
pub const IPC_CORE1_MBOX_ANCHOR: usize = 0x60F0_0008;

/// Core 1 sdram1.bin load & entry address in SDRAM (entry code at +0x20).
pub const CORE1_SDRAM_BASE: usize = 0x6100_0000;

// -----------------------------------------------------------------------------
// Peripheral MMIO Bases
// -----------------------------------------------------------------------------

/// SYSCTRL / Clock & Reset Management.
/// Note: No vendor image (App0/App1/idm1/sdram1) writes 0x4000_0000; Core 1
/// boots autonomously via mask ROM / hardware.
pub const SYSCTRL_BASE: usize = 0x4000_0000;

/// DSP Peripheral IPC Core-A / Core-B bases.
pub const DSP_IPC_COREA_BASE: usize = 0x4000_2000;
pub const DSP_IPC_COREB_BASE: usize = 0x4003_1000;

/// Master System DMA Controller 0 (used by Core 0 CRT0 scatter-load).
pub const DMAC0_BASE: usize = 0x4000_1000;

/// QSPI0 Hardware Controller (Chip 0 Flash).
pub const QSPI0_BASE: usize = 0x4000_4000;

/// System Interconnect & Bus Remap.
pub const REMAP_BASE: usize = 0x4000_5000;

/// Hardware DES/3DES Cipher Engine.
pub const CIPHER_BASE: usize = 0x4002_0000;

/// Primary UART Controller (used by Boot ROM ISP & Core 0 console).
pub const UART0_BASE: usize = 0x4002_1000;

/// SCI Serial / Panel-Link & System Strapping Block (`0x4002_6000`).
/// - `+0x00`: SCI control word (`|= 0x110` in update mode)
/// - `+0x10`: control / ROM serial-bootloader enable (bit 0)
/// - `+0x30`: tick rate (units/sec) word (`g_dwClkRegWd40026030`)
/// - `+0x60`: 16-bit data FIFO window
/// - `+0x200`: TX-DMA kick/doorbell (bit 3)
pub const SCI_BASE: usize = 0x4002_6000;
pub const SYS_STATUS_BASE: usize = 0x4002_6000;

/// Per-pin alternate-function pinmux words (`0x4002_6704..0x4002_67F0`).
pub const PINMUX_BASE: usize = 0x4002_6704;

/// Master System DMA Controller 1.
pub const DMAC1_BASE: usize = 0x4003_0000;

/// PL080 Audio DMA Controller.
pub const AUDIO_DMAC_BASE: usize = 0x4003_1000;

/// QSPI1 Hardware Controller (Chip 1 Flash).
pub const QSPI1_BASE: usize = 0x4003_2000;

/// Boot Handshake / ITCM Remap Flag.
pub const BOOT_REMAP_FLAG: usize = 0x4005_1050;

/// System PLL Configuration.
pub const PLL_CFG_BASE: usize = 0x4005_4000;

/// Roland BMC DSP Sound Processing Engine (0x4006_0000..0x4007_5000).
pub const BMC_DSP_BASE: usize = 0x4006_0000;

/// Hardware IEEE 802.3 CRC-32 Peripheral.
pub const CRC32_BASE: usize = 0x4010_5000;

/// Hardware Microsecond Counter (unmapped in stock Ghidra, verified on HW).
pub const DSP_MICROS_CNT: usize = 0x4010_B000;

/// KernelCtl Inter-Core Control & SWI Window (0x4110_0000..0x4110_0FFF).
/// - `+0x34`: busy word
/// - `+0x40`: DMA doorbell channel port
/// - `+0x50`: ack strobe
/// - `+0x200`: DSP IRQ-mask mirror
pub const KERNEL_CTL_BASE: usize = 0x4110_0000;

/// Periph12 Command Block Window (`0x4112_2000`, status `0x4112_1028`).
pub const PERIPH12_STATUS: usize = 0x4112_1028;
pub const PERIPH12_WIN: usize = 0x4112_2000;

/// Shared SDRAM 53-column Diagnostic Text Grid (`g_abDispBuf0`, stride `0x35`).
pub const DISP_BUF0_BASE: usize = 0x6096_2A34;
pub const DISP_GRID_COLS: usize = 53;
pub const DISP_GRID_STRIDE: usize = 0x35;
pub const DISP_GRID_SIZE: usize = 0x73F;

/// Display Dirty Flag Byte (`g_bDispDirty`).
pub const DISP_DIRTY_FLAG: usize = 0x602C_C114;

// -----------------------------------------------------------------------------
// Physical 16x2 Character LCD (ST7032-compatible) & Backlight
// Verified from App0_UPD (`mc-1r1/DISPLAY.md` §6 + Capstone check of 0x6000_39B8 / 0x6000_729E).
// -----------------------------------------------------------------------------

/// Serial Channel 7 SPI/LCD controller (`0x4002_1000 + 7 * 0x400`).
pub const LCD_CH7_BASE: usize = 0x4002_2C00;

/// LCD Register Select (RS) GPIO bit-band alias: `0` = Command, `1` = Data.
pub const LCD_RS_BITBAND: usize = 0x424C_4374;

/// LCD Backlight peripheral clock/enable (`0x4002_5028` bits 1 & 5) and
/// PWM / steady-ON control (`0x4002_5300` bit 1 = PWM mode, bit 9 = steady ON).
pub const LCD_BL_CLK_REG: usize = 0x4002_5028;
pub const LCD_BL_CTL_REG: usize = 0x4002_5300;

// -----------------------------------------------------------------------------
// Soft Power Switch & Hardware Power-Cut Control (MC-101)
// Verified from App1_Main: 0x6001_AE50 / 0x6001_AE7C (init), 0x6001_AB34 /
// 0x6001_CB84 (pin 0x19 input), 0x6009_1EE8 / 0x6009_1FB4 (500 ms debounce),
// and 0x6001_B174 / 0x6001_B184 (audio mute + power-cut latches).
// -----------------------------------------------------------------------------

/// Rear power switch GPIO pin 0x19 (25) pinmux (`= 6` for GPIO input).
pub const PWR_SW_PINMUX: usize = 0x4002_646C;
/// Rear power switch GPIO input bit-band alias (`0x4002_6204` bit 25): `1` = ON, `0` = OFF.
pub const PWR_SW_BITBAND: usize = 0x424C_40E4;

/// Power-cut latch A & B pinmux (`= 0x800` for GPIO output; latch bit-bands must be 0 first!).
pub const PWR_OFF_PINMUX_A: usize = 0x4002_6468;
pub const PWR_OFF_PINMUX_B: usize = 0x4002_6464;
/// Power-cut latch A & B output bit-band aliases (`0x4002_6200` bits 24 & 23): write `1` to cut power.
pub const PWR_OFF_BB_A: usize = 0x424C_4060;
pub const PWR_OFF_BB_B: usize = 0x424C_405C;

/// Audio DAC mute GPIO bit-band alias (`App1 0x6001_B184`: `0` = muted, `1` = unmuted).
pub const AUDIO_MUTE_BB: usize = 0x424C_4504;

// -----------------------------------------------------------------------------
// Front-Panel Sub-CPU Serial Link (Channel 4, `0x4002_2000`)
// Verified from App1_Main: `0x6001_B074` (pinmux), `0x600A_9E1E..0x600A_9E3A`
// (Sub-CPU NRST/BOOT0 reset), `0x6002_72E0` / `0x601D_0378` (Ch4 115200 baud),
// `0x6004_7EDE..0x6004_7EEA` (4-byte TX init/query), `0x0101_2F5C` (RX buttons).
// -----------------------------------------------------------------------------

/// Serial Channel 4 controller (`0x4002_1000 + 4 * 0x400`) connected to the Sub-CPU.
pub const PANEL_CH4_BASE: usize = 0x4002_2000;
/// Sub-CPU Channel 4 pinmux & reset/boot0 registers (`App1 0x6001_B074`).
pub const PANEL_RX_PINMUX: usize = 0x4002_6558;
pub const PANEL_RX_MUX: usize = 0x4002_6A30;
pub const PANEL_TX_PINMUX: usize = 0x4002_655C;
pub const PANEL_BOOT0_PINMUX: usize = 0x4002_6560;
pub const PANEL_NRST_PINMUX: usize = 0x4002_6564;
/// Sub-CPU BOOT0 GPIO bit-band (`0` = normal app, `1` = STM32 ROM bootloader).
pub const PANEL_BOOT0_BB: usize = 0x424C_4360;
/// Sub-CPU NRST GPIO bit-band (`0` = hold in reset, `1` = release).
pub const PANEL_NRST_BB: usize = 0x424C_4364;

// -----------------------------------------------------------------------------
// SD/MMC Host Interface (Panasonic/Renesas SDHI, `0x4000_5000`)
// Verified from App1 ITCM0 `0x0100_35F0..0x0100_4E20` (`*[0x6031_10E4]`) and
// pinmux `App1 0x6001_AF98`.
// -----------------------------------------------------------------------------

/// SDHI Controller Base (`0x4000_5000`, 32-bit stride = 2x classic 16-bit SDHI).
pub const SDHI_BASE: usize = 0x4000_5000;
/// SDHI Companion DMA Base (`*[0x6031_10E8]`).
pub const SDHI_DMA_BASE: usize = 0x4000_3000;

// -----------------------------------------------------------------------------
// USB 2.0 High-Speed / Full-Speed Device Controller (`0x4006_2000`)
// Verified from Boot ROM (`0x0120_07E2..0x0120_092E`, `0x0120_3280..0x0120_3978`)
// and App1 (`0x6002_28B0`, `0x6002_66D6`).
// -----------------------------------------------------------------------------

/// USB 2.0 Device Controller register base (`0x4006_2000`).
pub const USB_BASE: usize = 0x4006_2000;
/// USB VBUS pinmux register (`= 5`).
pub const USB_VBUS_PINMUX: usize = 0x4002_6808;
/// USB / PHY auxiliary interrupt control (`0x4005_0000`).
pub const USB_AUX_BASE: usize = 0x4005_0000;

/// Write a single bit via ARM Cortex-M4 peripheral bit-band alias.
#[inline(always)]
pub unsafe fn bitband_write(addr: usize, bit: u32, val: u32) {
    let region = addr & 0xF000_0000;
    let offset = addr & 0x000F_FFFF;
    let alias = region + 0x0200_0000 + (offset << 5) + ((bit as usize) << 2);
    core::ptr::write_volatile(alias as *mut u32, val & 1);
}

/// ARM Cortex-M4 System Control Block (SCB) registers.
pub const SCB_VTOR: usize = 0xE000_ED08;
pub const SCB_AIRCR: usize = 0xE000_ED0C;
pub const SCB_CCR: usize = 0xE000_ED14;
pub const SCB_CFSR: usize = 0xE000_ED28;
pub const SCB_HFSR: usize = 0xE000_ED2C;
pub const SCB_CPACR: usize = 0xE000_ED88;
pub const SCB_FPCCR: usize = 0xE000_EF34;

/// Enable Cortex-M4F CP10/CP11 (full access) and configure FPCCR lazy stacking
/// (`0xC000_017A`, matching stock `KernelMainEntry`). Must run before any FPU
/// instruction on `thumbv7em-none-eabihf`.
#[inline(always)]
pub unsafe fn enable_fpu() {
    let cpacr = SCB_CPACR as *mut u32;
    let val = core::ptr::read_volatile(cpacr);
    core::ptr::write_volatile(cpacr, val | (0xF << 20));
    core::ptr::write_volatile(SCB_FPCCR as *mut u32, 0xC000_017A);
    core::arch::asm!("dsb sy", "isb", options(nostack, preserves_flags));
}
