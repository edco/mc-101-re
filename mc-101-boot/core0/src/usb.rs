//! BMC / ESC2 USB 2.0 High-Speed / Full-Speed `embassy-usb-driver` Implementation (`0x4006_2000`).
//!
//! Hardware sequences verified from Silicon Boot ROM (`0x0120_031C..0x0120_092E`,
//! `0x0120_3280..0x0120_3978`) and `App1_Main` (`0x6002_28B0`, `0x6002_66D6`).
//!
//! Hardware quirks handled transparently for `embassy-usb`:
//! - Silicon intercepts `SET_ADDRESS`, `SET_CONFIGURATION`, `GET_STATUS`, and
//!   `CLEAR_FEATURE(ENDPOINT_HALT)` without raising `INT_SETUP`.
//!   * On `INT_SET_CONFIG` (bit 26), `BmcControlPipe::setup()` synthesizes the
//!     8-byte `SET_CONFIGURATION(1)` SETUP packet and makes `accept()` a no-op so
//!     `embassy-usb` transitions to `Configured` cleanly.
//!   * `wait_unstalled()` polls hardware `USB_EPn_CTRL` bit 1 via `Timer::after_millis(1)`
//!     until silicon clears the halt on host `CLEAR_FEATURE(ENDPOINT_HALT)`.
//! - Negotiated HS (512 B) vs. FS (64 B) speed is tracked on `INT_ENUM_DONE` and
//!   applied to both the hardware EP config RAM (`0x4006_2200+`) and outgoing FS
//!   configuration descriptors.
//! - `DMAC0 CH1` (`0x4000_1020`) transfers directly from/to 4-byte-aligned SDRAM/SRAM
//!   slices (zero-copy) and bounces stack/ITCM or unaligned slices through `USB_DMA_BUF`.

#![allow(dead_code)]

use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering};
use core::task::Poll;

use embassy_sync::waitqueue::AtomicWaker;
use embassy_time::Timer;
use embassy_usb_driver::{
    Bus, ControlPipe, Direction, Driver, Endpoint, EndpointAddress, EndpointAllocError,
    EndpointError, EndpointIn, EndpointInfo, EndpointOut, EndpointType, Event, Unsupported,
};
use mc1r1_shared::ipc::dsb;
use mc1r1_shared::memmap::{DMAC0_BASE, PLL_CFG_BASE, USB_AUX_BASE, USB_BASE, USB_VBUS_PINMUX};

// USB 2.0 Device Controller registers (`0x4006_2000`)
const USB_CTRL: usize = USB_BASE + 0x000;
const USB_CFG: usize = USB_BASE + 0x004;
const USB_INT_STAT: usize = USB_BASE + 0x008;
const USB_EP_INT_MASK: usize = USB_BASE + 0x00C;
const USB_EP_INT_STAT: usize = USB_BASE + 0x010;
const USB_EP_INT_EN: usize = USB_BASE + 0x014;
const USB_SETUP_LATCH: usize = USB_BASE + 0x070;
const USB_TIMING_CFG: usize = USB_BASE + 0x0F0;
const USB_IFACE_CFG: usize = USB_BASE + 0x0FC;

const USB_EP0_CTRL: usize = USB_BASE + 0x100;
const USB_EP0_STAT: usize = USB_BASE + 0x104;
const USB_EP_MISC_MASK: usize = USB_BASE + 0x178;
const USB_EP_MISC_STAT: usize = USB_BASE + 0x17C;

const USB_EP0_IN_FIFO: usize = USB_BASE + 0x180;
const USB_EP0_OUT_FIFO: usize = USB_BASE + 0x1C0;
const USB_CFG_RAM_BASE: usize = USB_BASE + 0x200;

// NVIC registers (Cortex-M4)
const NVIC_ISER0: usize = 0xE000_E100;
const NVIC_ICER0: usize = 0xE000_E180;
const NVIC_IPR0: usize = 0xE000_E400;
pub const USB_IRQ_NUM: usize = 20;

// DMAC0 Channel 1 (`0x4000_1020`, dedicated to USB FIFO bursts)
const DMAC0_CH1: usize = DMAC0_BASE + 0x20;
const DMAC_CFG0_BASE: u32 = 0xA080_0000;
const DMAC_CFG1_WR_FIFO: u32 = 0x0900_0000;
const DMAC_CFG1_RD_FIFO: u32 = 0x0A00_0000;

// USB_INT_STAT (+0x008) interrupt bits (Write-0-to-Clear, verified from BootROM 0x3544..0x3740 & App0_UPD 0x6001_F5FC..0x6001_F6B6)
const INT_VBUS_BIT31: u32 = 1 << 31;
const INT_SET_CONFIG: u32 = 1 << 30;
const INT_BUS_RESET: u32 = 1 << 29;
const INT_ENUM_DONE: u32 = 1 << 28;
const INT_SETUP: u32 = 1 << 27;
const INT_SOF: u32 = 1 << 26; // Start-of-Frame (8 kHz on HS; masked by 0x84FF_BFFF)
const INT_SUSPEND: u32 = 1 << 25;
const INT_RESUME: u32 = 1 << 24;

// Endpoint interrupt mask bits in USB_EPn_CTRL (bits [23:16]: 1 = masked).
const EP_IRQ_MASK_ALL: u32 = 0x00FF_0000;

// Hardware endpoint config RAM slot table (from Boot ROM `0x0120_3E60`)
const HW_EP_SLOT: [u8; 15] = [1, 0, 0, 2, 4, 5, 7, 8, 9, 10, 0, 0, 11, 12, 22];

/// 512-byte Core 0 SDRAM bounce buffer (right after `PING_PONG` at `0x6010_0000..0x6012_0000`)
/// so `DMAC0 CH1` never targets `SRAM0` (`0x2000_xxxx`), matching `mc-1r1` SDRAM DMA.
const USB_DMA_BUF_SDRAM: usize = 0x6012_0000;

// Interrupt-driven wakers & shared hardware state
static BUS_WAKER: AtomicWaker = AtomicWaker::new();
static EP0_WAKER: AtomicWaker = AtomicWaker::new();
static EP_WAKERS: [AtomicWaker; 15] = [const { AtomicWaker::new() }; 15];

static BUS_EVENTS: AtomicU32 = AtomicU32::new(0);
const EV_RESET: u32 = 1 << 0;
const EV_SUSPEND: u32 = 1 << 1;
const EV_RESUME: u32 = 1 << 2;

static HIGH_SPEED: AtomicBool = AtomicBool::new(true);
static CONFIGURED: AtomicBool = AtomicBool::new(false);
static USB_SUSPENDED: AtomicBool = AtomicBool::new(false);
static EP_ENABLED_MASK: AtomicU32 = AtomicU32::new(0);
static EP_ALLOC_MASK: AtomicU32 = AtomicU32::new(0);
static RESET_EPOCH: AtomicU32 = AtomicU32::new(0);

// Allocated endpoint metadata (for re-programming 0x4006_2200+ on INT_ENUM_DONE)
static EP_DIR_IN_MASK: AtomicU32 = AtomicU32::new(0);
static EP_TYPE_WORDS: [AtomicU8; 15] = [const { AtomicU8::new(0) }; 15];
static EP_MPS_WORDS: [AtomicU16; 15] = [const { AtomicU16::new(0) }; 15];

// Pending EP0 SETUP state (latched by USB_IRQ_Handler)
static SETUP_READY: AtomicBool = AtomicBool::new(false);
static SYNTH_SET_CONFIG: AtomicBool = AtomicBool::new(false);
static SETUP_W0: AtomicU32 = AtomicU32::new(0);
static SETUP_W1: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
unsafe fn rd32(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

#[inline(always)]
unsafe fn wr32(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline(always)]
const fn ep_ctrl_addr(ep: usize) -> usize {
    USB_BASE + 0x100 + ep * 8
}

#[inline(always)]
const fn ep_stat_addr(ep: usize) -> usize {
    USB_BASE + 0x104 + ep * 8
}

#[inline(always)]
const fn ep_in_fifo_addr(ep: usize) -> usize {
    USB_BASE + 0x180 + ep * 4
}

#[inline(always)]
const fn ep_out_fifo_addr(ep: usize) -> usize {
    USB_BASE + 0x1C0 + ep * 4
}

#[inline(always)]
const fn ep_out_len_addr(ep: usize) -> usize {
    // Boot ROM 0x0120_063E: EP3 -> +0x88; App0_UPD 0x6001_FAF2: +0x70 + ep*8
    USB_BASE + 0x70 + ep * 8
}

#[inline(always)]
fn is_dmac_accessible_aligned(ptr: *const u8) -> bool {
    let a = ptr as usize;
    (a & 3) == 0 && (0x6000_0000..0x6100_0000).contains(&a)
}

unsafe fn nvic_enable_irq(irq: usize, prio: u8) {
    let ipr = (NVIC_IPR0 + irq) as *mut u8;
    core::ptr::write_volatile(ipr, prio << 4);
    let iser = (NVIC_ISER0 + (irq / 32) * 4) as *mut u32;
    core::ptr::write_volatile(iser, 1u32 << (irq % 32));
    dsb();
}

unsafe fn nvic_disable_irq(irq: usize) {
    let icer = (NVIC_ICER0 + (irq / 32) * 4) as *mut u32;
    core::ptr::write_volatile(icer, 1u32 << (irq % 32));
    dsb();
}

/// Program hardware endpoint config RAM (`0x4006_2200+`) from allocated endpoints,
/// respecting negotiated HS vs FS max packet size.
unsafe fn program_hw_ep_table(high_speed: bool) {
    wr32(USB_CTRL, rd32(USB_CTRL) | 0x0001_0000);
    wr32(USB_CFG_RAM_BASE + 0x00, 0x0120_0120);
    wr32(USB_CFG_RAM_BASE + 0x04, 64u32 << 19); // EP0: 64 bytes

    // Single interface 0, alt 0 (generalizes if more interfaces are added later).
    wr32(USB_IFACE_CFG, (1u32 << 24) | (1u32 << 2));
    wr32(USB_CFG, rd32(USB_CFG) | 0x18);

    let alloc = EP_ALLOC_MASK.load(Ordering::Acquire);
    let dir_in = EP_DIR_IN_MASK.load(Ordering::Acquire);
    for ep in 1..15usize {
        if (alloc & (1 << ep)) == 0 {
            continue;
        }
        let slot = HW_EP_SLOT[ep] as usize;
        if slot == 0 {
            continue;
        }
        let is_in = ((dir_in >> ep) & 1) as u32;
        let ep_type = (EP_TYPE_WORDS[ep].load(Ordering::Relaxed) & 3) as u32;
        let req_mps = EP_MPS_WORDS[ep].load(Ordering::Relaxed) as u32;
        let mps = if !high_speed && ep_type == (EndpointType::Bulk as u32) {
            req_mps.min(64)
        } else {
            req_mps
        };
        let word: u32 = (ep as u32)
            | (is_in << 4)
            | (ep_type << 5)
            | (1 << 7) // bConfigurationValue = 1
            | (0 << 11) // bInterfaceNumber = 0
            | (0 << 15) // bAlternateSetting = 0
            | (mps << 19);
        wr32(USB_CFG_RAM_BASE + slot * 4, word);
    }

    wr32(USB_CTRL, rd32(USB_CTRL) & !0x0001_0000);
    dsb();
}

/// Flush allocated bulk/interrupt endpoint FIFOs on `INT_ENUM_DONE` (`Boot ROM 0x0120_38BA`).
unsafe fn reset_allocated_endpoints() {
    // Keep EP0 masked in USB_EP_INT_MASK (EP0 SETUP arrives via INT_SETUP in USB_INT_STAT;
    // unmasking EP0 in USB_EP_INT_MASK causes an IRQ 20 storm while EP0 is serviced async).
    wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) | 1);
    wr32(USB_EP0_CTRL, rd32(USB_EP0_CTRL) & !0x20000);

    let alloc = EP_ALLOC_MASK.load(Ordering::Acquire);
    for ep in 1..15usize {
        if (alloc & (1 << ep)) == 0 {
            continue;
        }
        let ctrl = ep_ctrl_addr(ep);
        wr32(ctrl, rd32(ctrl) | 0x8000);
        for _ in 0..50_000u32 {
            if (rd32(ctrl) & 0x8000) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // Clear stall/control flags (`0xFFFE_F00F` like Boot ROM) and mask all EP IRQ bits [23:16]
        // until BmcEndpointOut::read() / BmcEndpointIn::write() explicitly awaits them.
        let v = (rd32(ctrl) & 0xFF00_F00F) | EP_IRQ_MASK_ALL;
        wr32(ctrl, v);
    }

    // Mask all 7 misc/stall/clear-halt IRQ bits in 0x4006_2178 and clear 0x4006_217C
    // (leaving 0x4006_2178 unmasked without clearing 0x4006_217C causes an IRQ 20 storm!).
    wr32(USB_EP_MISC_MASK, rd32(USB_EP_MISC_MASK) | 0x7F);
    wr32(USB_EP_MISC_STAT, rd32(USB_EP_MISC_STAT) & !0x7F);
    wr32(USB_EP_INT_EN, rd32(USB_EP_INT_EN) & !alloc);
    // Keep ALL endpoint IRQs masked in USB_EP_INT_MASK until read()/write() awaits them.
    wr32(USB_EP_INT_MASK, 0xFFFF_FFFF);
    dsb();
}

/// DMA write from `buf` to USB IN FIFO `fifo_addr` (zero-copy when 4-byte-aligned in SDRAM/SRAM).
unsafe fn dma_write_fifo(fifo_addr: usize, buf: &[u8]) {
    let len = buf.len().min(512);
    if len == 0 {
        return;
    }
    let src_ptr = if is_dmac_accessible_aligned(buf.as_ptr()) {
        buf.as_ptr() as u32
    } else {
        let dst = USB_DMA_BUF_SDRAM as *mut u8;
        core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, len);
        dst as u32
    };

    let words = len >> 2;
    let rem = len & 3;
    wr32(DMAC0_CH1 + 0x0C, fifo_addr as u32);
    wr32(DMAC0_CH1 + 0x08, src_ptr);
    dsb();

    if words > 0 {
        wr32(DMAC0_CH1 + 0x04, DMAC_CFG1_WR_FIFO);
        wr32(DMAC0_CH1 + 0x00, DMAC_CFG0_BASE | ((words - 1) as u32 & 0xFFFF));
        for _ in 0..500_000u32 {
            if (rd32(DMAC0_CH1 + 0x00) & (1 << 29)) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }
    if rem > 0 {
        wr32(DMAC0_CH1 + 0x04, DMAC_CFG1_WR_FIFO & !0x0D00_0000);
        wr32(DMAC0_CH1 + 0x00, DMAC_CFG0_BASE | ((rem - 1) as u32 & 0xFFFF));
        for _ in 0..500_000u32 {
            if (rd32(DMAC0_CH1 + 0x00) & (1 << 29)) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }
    wr32(DMAC0_CH1 + 0x04, rd32(DMAC0_CH1 + 0x04) & !0x0007_0000);
    dsb();
}

/// DMA read from USB OUT FIFO `fifo_addr` to `buf` (zero-copy when 4-byte-aligned in SDRAM/SRAM).
unsafe fn dma_read_fifo(fifo_addr: usize, buf: &mut [u8]) {
    let len = buf.len().min(512);
    if len == 0 {
        return;
    }
    let direct = is_dmac_accessible_aligned(buf.as_ptr());
    let dst_ptr = if direct {
        buf.as_mut_ptr() as u32
    } else {
        USB_DMA_BUF_SDRAM as u32
    };

    let words = len >> 2;
    let rem = len & 3;
    wr32(DMAC0_CH1 + 0x0C, dst_ptr);
    wr32(DMAC0_CH1 + 0x08, fifo_addr as u32);
    dsb();

    if words > 0 {
        wr32(DMAC0_CH1 + 0x04, DMAC_CFG1_RD_FIFO);
        wr32(DMAC0_CH1 + 0x00, DMAC_CFG0_BASE | ((words - 1) as u32 & 0xFFFF));
        for _ in 0..500_000u32 {
            if (rd32(DMAC0_CH1 + 0x00) & (1 << 29)) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }
    if rem > 0 {
        wr32(USB_CTRL, rd32(USB_CTRL) & !3);
        wr32(DMAC0_CH1 + 0x04, DMAC_CFG1_RD_FIFO & !0x0C00_0000);
        wr32(DMAC0_CH1 + 0x00, DMAC_CFG0_BASE | ((rem - 1) as u32 & 0xFFFF));
        for _ in 0..500_000u32 {
            if (rd32(DMAC0_CH1 + 0x00) & (1 << 29)) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        wr32(USB_CTRL, (rd32(USB_CTRL) & !3) | 2);
    }
    wr32(DMAC0_CH1 + 0x04, rd32(DMAC0_CH1 + 0x04) & !0x0007_0000);
    dsb();

    if !direct {
        let src = USB_DMA_BUF_SDRAM as *const u8;
        core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), len);
    }
}

/// NVIC IRQ 20 (USB 2.0 Global + Endpoint Interrupt Handler, placed in ITCM0).
#[link_section = ".itcm"]
#[no_mangle]
pub unsafe extern "C" fn USB_IRQ_Handler() {
    let status = rd32(USB_INT_STAT);
    let mask = rd32(USB_CFG);
    let pending = status & (0xFF00_0000 & !mask);

    // App0_UPD 0x6001_F62E..0x6001_F634: Bit 31 of USB_INT_STAT must be cleared (W0C: !bit).
    if (pending & INT_VBUS_BIT31) != 0 {
        wr32(USB_INT_STAT, !INT_VBUS_BIT31);
    }

    if (pending & INT_BUS_RESET) != 0 {
        wr32(USB_INT_STAT, !INT_BUS_RESET);
        wr32(USB_TIMING_CFG, 0x0100_5F30);
        wr32(USB_EP0_CTRL, rd32(USB_EP0_CTRL) | 3);
        for _ in 0..50_000u32 {
            if (rd32(USB_EP0_CTRL) & 3) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        CONFIGURED.store(false, Ordering::Release);
        USB_SUSPENDED.store(false, Ordering::Release);
        EP_ENABLED_MASK.store(0, Ordering::Release);
        SETUP_READY.store(false, Ordering::Release);
        SYNTH_SET_CONFIG.store(false, Ordering::Release);
        RESET_EPOCH.fetch_add(1, Ordering::AcqRel);
        BUS_EVENTS.fetch_or(EV_RESET, Ordering::Release);
        BUS_WAKER.wake();
        EP0_WAKER.wake();
        for w in &EP_WAKERS {
            w.wake();
        }
    }

    if (pending & INT_ENUM_DONE) != 0 {
        wr32(USB_INT_STAT, !INT_ENUM_DONE);
        let spd = (rd32(USB_INT_STAT) >> 16) & 3;
        let hs = spd != 1;
        HIGH_SPEED.store(hs, Ordering::Release);
        program_hw_ep_table(hs);
        reset_allocated_endpoints();
    }

    if (pending & INT_SETUP) != 0 {
        USB_SUSPENDED.store(false, Ordering::Release);
        wr32(USB_INT_STAT, !INT_SETUP);
        // Clear any leftover EP0 STALL from a previous rejected SETUP request
        // (e.g. GET_DESCRIPTOR(DEVICE_QUALIFIER) or BOS on macOS).
        let ep0_ctrl = rd32(USB_EP0_CTRL);
        if (ep0_ctrl & 4) != 0 {
            wr32(USB_EP0_CTRL, ep0_ctrl & !4);
        }
        for _ in 0..50_000u32 {
            if (rd32(USB_EP0_STAT) & 0x20000) != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let _ = rd32(USB_SETUP_LATCH);
        let w0 = rd32(USB_EP0_OUT_FIFO);
        let w1 = rd32(USB_EP0_OUT_FIFO);
        wr32(USB_EP0_STAT, 0x00E7_0010);
        SETUP_W0.store(w0, Ordering::Relaxed);
        SETUP_W1.store(w1, Ordering::Relaxed);
        SETUP_READY.store(true, Ordering::Release);
        EP0_WAKER.wake();
    }

    if (pending & INT_SOF) != 0 {
        wr32(USB_CFG, rd32(USB_CFG) | INT_SOF);
        wr32(USB_INT_STAT, !INT_SOF);
    }

    if (pending & INT_SET_CONFIG) != 0 {
        wr32(USB_INT_STAT, !INT_SET_CONFIG);
        USB_SUSPENDED.store(false, Ordering::Release);
        CONFIGURED.store(true, Ordering::Release);
        SYNTH_SET_CONFIG.store(true, Ordering::Release);
        EP0_WAKER.wake();
    }

    // Clear SUSPEND / RESUME in hardware (W0C). Do NOT emit Event::Suspend to
    // embassy-usb: embassy-usb exits run_until_suspend() on Event::Suspend and stops polling
    // ControlPipe::setup() (EP0), which causes Linux usb-storage GET_MAX_LUN to time out for 10s!
    if (pending & INT_SUSPEND) != 0 {
        wr32(USB_INT_STAT, !INT_SUSPEND);
        USB_SUSPENDED.store(true, Ordering::Release);
    }
    if (pending & INT_RESUME) != 0 {
        wr32(USB_INT_STAT, !INT_RESUME);
        USB_SUSPENDED.store(false, Ordering::Release);
    }

    // Clear any pending misc/stall/clear-halt status bits in 0x4006_217C (W0C: App0_UPD 0x6001_F6D2).
    let misc_stat = rd32(USB_EP_MISC_STAT) & 0x7F;
    if misc_stat != 0 {
        wr32(USB_EP_MISC_STAT, !misc_stat);
    }

    // Wake any endpoint that raised an IRQ, and immediately mask it in USB_EP_INT_MASK
    // (master per-EP gate) so no EP status bit can ever storm IRQ 20.
    let ep_pending = rd32(USB_EP_INT_STAT) & !rd32(USB_EP_INT_MASK);
    if ep_pending != 0 {
        wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) | ep_pending);
        let active = ep_pending & EP_ALLOC_MASK.load(Ordering::Relaxed);
        for ep in 1..15usize {
            if (active & (1 << ep)) != 0 {
                EP_WAKERS[ep].wake();
            }
        }
    }

    // Clear any unhandled EP/misc IRQ sources without permanently disabling NVIC IRQ 20,
    // so USB can still receive INT_BUS_RESET / INT_SETUP and recover cleanly.
    static EMPTY_IRQ_STREAK: AtomicU32 = AtomicU32::new(0);
    if pending == 0 && ep_pending == 0 && misc_stat == 0 {
        if EMPTY_IRQ_STREAK.fetch_add(1, Ordering::Relaxed) >= 64 {
            let misc_all = rd32(USB_EP_MISC_STAT);
            if misc_all != 0 {
                wr32(USB_EP_MISC_STAT, !misc_all);
            }
            wr32(USB_EP_INT_MASK, 0xFFFF_FFFF);
            wr32(USB_EP_MISC_MASK, rd32(USB_EP_MISC_MASK) | 0x7F);
            EMPTY_IRQ_STREAK.store(0, Ordering::Relaxed);
        }
    } else {
        EMPTY_IRQ_STREAK.store(0, Ordering::Relaxed);
    }
}

/// Query negotiated USB speed (`true` = High-Speed 480 Mbps, `false` = Full-Speed 12 Mbps).
pub fn is_high_speed() -> bool {
    HIGH_SPEED.load(Ordering::Acquire)
}

/// Query whether the USB bus is currently suspended / unplugged.
pub fn is_suspended() -> bool {
    USB_SUSPENDED.load(Ordering::Acquire)
}

/// Set or clear hardware STALL on any endpoint `ep_idx` (`USB_EPn_CTRL` bit 1).
pub fn ep_set_stalled(ep_idx: usize, stalled: bool) {
    if ep_idx == 0 || ep_idx >= 15 {
        return;
    }
    // SAFETY: Critical-section protected RMW on dedicated endpoint control register.
    critical_section::with(|_| unsafe {
        let ctrl = ep_ctrl_addr(ep_idx);
        if stalled {
            wr32(ctrl, rd32(ctrl) | 2);
        } else {
            wr32(ctrl, rd32(ctrl) & !2);
        }
        dsb();
    });
}

/// Flush endpoint FIFO, clear STALL, and reset endpoint IRQ state (used on `BOMSR`).
pub fn flush_ep(ep_idx: usize) {
    if ep_idx == 0 || ep_idx >= 15 {
        return;
    }
    critical_section::with(|_| unsafe {
        let ctrl = ep_ctrl_addr(ep_idx);
        wr32(ctrl, rd32(ctrl) | 0x8000);
        for _ in 0..10_000u32 {
            if (rd32(ctrl) & 0x8000) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let v = (rd32(ctrl) & 0xFF00_F00F) | EP_IRQ_MASK_ALL;
        wr32(ctrl, v);
        wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) | (1u32 << ep_idx));
        dsb();
    });
    EP_WAKERS[ep_idx].wake();
}

/// Query hardware STALL bit (`USB_EPn_CTRL` bit 1) on any endpoint `ep_idx`.
pub fn ep_is_stalled(ep_idx: usize) -> bool {
    if ep_idx == 0 || ep_idx >= 15 {
        return false;
    }
    // SAFETY: Volatile read of endpoint control register.
    unsafe { (rd32(ep_ctrl_addr(ep_idx)) & 2) != 0 }
}

/// Asynchronously wait until silicon clears the endpoint's STALL bit (via host
/// `CLEAR_FEATURE(ENDPOINT_HALT)`), bounded to 500 ms so a stuck host never wedges MSC.
pub async fn wait_ep_unstalled(ep_idx: usize) {
    for _ in 0..500u16 {
        if !ep_is_stalled(ep_idx) || !CONFIGURED.load(Ordering::Acquire) {
            return;
        }
        Timer::after_millis(1).await;
    }
    ep_set_stalled(ep_idx, false);
}

/// Emergency hardware detach (called by `power::safe_shutdown`).
pub fn force_disconnect() {
    // SAFETY: Safe during shutdown to detach D+ pull-up and silence USB IRQ.
    unsafe {
        nvic_disable_irq(USB_IRQ_NUM);
        wr32(USB_CFG, rd32(USB_CFG) | 0x20);
        wr32(USB_CTRL, rd32(USB_CTRL) | 8);
        dsb();
    }
}

// -----------------------------------------------------------------------------
// embassy_usb_driver::{Driver, Bus, ControlPipe, Endpoint, EndpointIn, EndpointOut}
// -----------------------------------------------------------------------------

/// Owned BMC USB 2.0 Device Controller peripheral (`Peripherals::take()` singleton).
pub struct UsbDevice {
    next_out_ep: usize,
    next_in_ep: usize,
}

impl UsbDevice {
    pub(crate) const fn new() -> Self {
        // Default allocation starts at Boot ROM's verified Bulk slots: EP3 OUT, EP4 IN.
        Self {
            next_out_ep: 3,
            next_in_ep: 4,
        }
    }

    fn alloc_ep(
        &mut self,
        dir: Direction,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<EndpointInfo, EndpointAllocError> {
        let ep = if let Some(addr) = ep_addr {
            addr.index()
        } else {
            let mut candidate = match dir {
                Direction::Out => self.next_out_ep,
                Direction::In => self.next_in_ep,
            };
            let alloc = EP_ALLOC_MASK.load(Ordering::Relaxed);
            while candidate < 15 && (HW_EP_SLOT[candidate] == 0 || (alloc & (1 << candidate)) != 0) {
                candidate += 1;
            }
            candidate
        };

        if ep == 0 || ep >= 15 || HW_EP_SLOT[ep] == 0 {
            return Err(EndpointAllocError);
        }
        let bit = 1u32 << ep;
        if (EP_ALLOC_MASK.load(Ordering::Relaxed) & bit) != 0 {
            return Err(EndpointAllocError);
        }
        EP_ALLOC_MASK.fetch_or(bit, Ordering::Relaxed);
        if dir == Direction::In {
            EP_DIR_IN_MASK.fetch_or(bit, Ordering::Relaxed);
        }
        EP_TYPE_WORDS[ep].store(ep_type as u8, Ordering::Relaxed);
        EP_MPS_WORDS[ep].store(max_packet_size, Ordering::Relaxed);

        Ok(EndpointInfo {
            addr: EndpointAddress::from_parts(ep, dir),
            ep_type,
            max_packet_size,
            interval_ms,
        })
    }
}

impl<'a> Driver<'a> for UsbDevice {
    type EndpointOut = BmcEndpointOut;
    type EndpointIn = BmcEndpointIn;
    type ControlPipe = BmcControlPipe;
    type Bus = BmcBus;

    fn alloc_endpoint_out(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointOut, EndpointAllocError> {
        let info = self.alloc_ep(Direction::Out, ep_type, ep_addr, max_packet_size, interval_ms)?;
        Ok(BmcEndpointOut { info })
    }

    fn alloc_endpoint_in(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointIn, EndpointAllocError> {
        let info = self.alloc_ep(Direction::In, ep_type, ep_addr, max_packet_size, interval_ms)?;
        Ok(BmcEndpointIn { info })
    }

    fn start(self, _control_max_packet_size: u16) -> (Self::Bus, Self::ControlPipe) {
        (
            BmcBus {
                enabled: false,
                power_notified: false,
            },
            BmcControlPipe {
                last_setup: [0u8; 8],
                synthetic_active: false,
            },
        )
    }
}

pub struct BmcBus {
    enabled: bool,
    power_notified: bool,
}

impl Bus for BmcBus {
    async fn enable(&mut self) {
        self.enabled = true;
        BUS_EVENTS.store(0, Ordering::Release);
        CONFIGURED.store(false, Ordering::Release);
        USB_SUSPENDED.store(false, Ordering::Release);
        EP_ENABLED_MASK.store(0, Ordering::Release);
        SETUP_READY.store(false, Ordering::Release);
        SYNTH_SET_CONFIG.store(false, Ordering::Release);

        // SAFETY: Exclusive ownership of USB_BASE via UsbDevice::start().
        unsafe {
            let gate = PLL_CFG_BASE + 0x28;
            wr32(gate, rd32(gate) | 8);

            wr32(USB_VBUS_PINMUX, 5);
            wr32(USB_AUX_BASE + 0x04, rd32(USB_AUX_BASE + 0x04) & !4);
            wr32(USB_AUX_BASE + 0x14, rd32(USB_AUX_BASE + 0x14) & !4);
            wr32(USB_AUX_BASE + 0x18, rd32(USB_AUX_BASE + 0x18) & !4);
            wr32(USB_AUX_BASE + 0x10, rd32(USB_AUX_BASE + 0x10) | 4);
            wr32(USB_AUX_BASE + 0x00, rd32(USB_AUX_BASE + 0x00) & !4);

            wr32(USB_CTRL, rd32(USB_CTRL) | 8);
            dsb();
        }
        Timer::after_millis(5).await;

        unsafe {
            wr32(USB_CFG, rd32(USB_CFG) | 0x0080_0000);
            // Match BootROM (0x3804) & App0_UPD (0x6001_F828): mask bit 31 (VBUS) & bit 26 (SOF).
            wr32(USB_CFG, (rd32(USB_CFG) | 0x8400_0000) & 0x84FF_BFFF);
            wr32(USB_CTRL, (rd32(USB_CTRL) & !3) | 2);
            wr32(USB_INT_STAT, 0); // Clear all latched W0C global IRQ flags (App0_UPD 0x6001_F4EE)
            dsb();
        }
        Timer::after_millis(3).await;

        unsafe {
            for _ in 0..100_000u32 {
                if (rd32(USB_INT_STAT) & 0x8000) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            program_hw_ep_table(true);
            nvic_enable_irq(USB_IRQ_NUM, 6);
            wr32(USB_CFG, rd32(USB_CFG) & !0x20); // Attach D+ pull-up
            dsb();
        }
    }

    async fn disable(&mut self) {
        self.enabled = false;
        CONFIGURED.store(false, Ordering::Release);
        USB_SUSPENDED.store(false, Ordering::Release);
        EP_ENABLED_MASK.store(0, Ordering::Release);
        force_disconnect();
    }

    async fn poll(&mut self) -> Event {
        if !self.power_notified {
            self.power_notified = true;
            return Event::PowerDetected;
        }
        poll_fn(|cx| {
            BUS_WAKER.register(cx.waker());
            let ev = BUS_EVENTS.swap(0, Ordering::AcqRel);
            if (ev & EV_RESET) != 0 {
                Poll::Ready(Event::Reset)
            } else if (ev & EV_SUSPEND) != 0 {
                Poll::Ready(Event::Suspend)
            } else if (ev & EV_RESUME) != 0 {
                Poll::Ready(Event::Resume)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn endpoint_set_enabled(&mut self, ep_addr: EndpointAddress, enabled: bool) {
        let ep = ep_addr.index();
        if ep == 0 || ep >= 15 {
            return;
        }
        if enabled {
            EP_ENABLED_MASK.fetch_or(1 << ep, Ordering::Release);
            EP_WAKERS[ep].wake();
        } else {
            EP_ENABLED_MASK.fetch_and(!(1 << ep), Ordering::Release);
        }
    }

    fn endpoint_set_stalled(&mut self, ep_addr: EndpointAddress, stalled: bool) {
        ep_set_stalled(ep_addr.index(), stalled);
    }

    fn endpoint_is_stalled(&mut self, ep_addr: EndpointAddress) -> bool {
        ep_is_stalled(ep_addr.index())
    }

    fn force_reset(&mut self) -> Result<(), Unsupported> {
        // SAFETY: Soft disconnect / reconnect pulse.
        unsafe {
            wr32(USB_CFG, rd32(USB_CFG) | 0x20);
            dsb();
            wr32(USB_CFG, rd32(USB_CFG) & !0x20);
            dsb();
        }
        Ok(())
    }

    async fn remote_wakeup(&mut self) -> Result<(), Unsupported> {
        Err(Unsupported)
    }
}

pub struct BmcControlPipe {
    last_setup: [u8; 8],
    synthetic_active: bool,
}

impl BmcControlPipe {
    async fn wait_ep0_stat_bit(&self, bit_mask: u32, epoch: u32) -> bool {
        let deadline = crate::time::now_ms().wrapping_add(100);
        loop {
            if RESET_EPOCH.load(Ordering::Acquire) != epoch || SETUP_READY.load(Ordering::Acquire) {
                return false;
            }
            // SAFETY: Volatile read of EP0 status register.
            if unsafe { (rd32(USB_EP0_STAT) & bit_mask) != 0 } {
                return true;
            }
            if crate::time::now_ms() >= deadline {
                return false;
            }
            embassy_futures::yield_now().await;
        }
    }

    async fn send_ep0_in_raw(&self, data: &[u8], last: bool) -> Result<(), EndpointError> {
        let epoch = RESET_EPOCH.load(Ordering::Acquire);
        let mut offset = 0;
        let len = data.len();
        loop {
            let chunk = (len - offset).min(64);
            if !self.wait_ep0_stat_bit(0x08, epoch).await {
                return Err(EndpointError::Disabled);
            }
            if chunk > 0 {
                // SAFETY: EP0 IN FIFO is ready and owned by ControlPipe.
                unsafe { dma_write_fifo(USB_EP0_IN_FIFO, &data[offset..offset + chunk]) };
            }
            if !self.wait_ep0_stat_bit(0x08, epoch).await {
                return Err(EndpointError::Disabled);
            }
            // SAFETY: Commit EP0 IN packet.
            unsafe { wr32(USB_EP0_STAT, 0x00E7_0008) };
            offset += chunk;
            if offset >= len {
                break;
            }
        }
        if last && len > 0 {
            if !self.wait_ep0_stat_bit(0x10, epoch).await {
                return Err(EndpointError::Disabled);
            }
            // SAFETY: Acknowledge EP0 status OUT stage.
            unsafe { wr32(USB_EP0_STAT, 0x00E7_0010) };
        }
        Ok(())
    }
}

impl ControlPipe for BmcControlPipe {
    fn max_packet_size(&self) -> usize {
        64
    }

    async fn setup(&mut self) -> [u8; 8] {
        poll_fn(|cx| {
            EP0_WAKER.register(cx.waker());
            if SYNTH_SET_CONFIG.swap(false, Ordering::AcqRel) {
                // Deliver synthesized SET_CONFIGURATION(1) before any subsequent class SETUP
                // (e.g. GET_MAX_LUN) so embassy-usb is Configured first.
                let pkt = [0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
                self.last_setup = pkt;
                self.synthetic_active = true;
                Poll::Ready(pkt)
            } else if SETUP_READY.swap(false, Ordering::AcqRel) {
                let w0 = SETUP_W0.load(Ordering::Relaxed).to_le_bytes();
                let w1 = SETUP_W1.load(Ordering::Relaxed).to_le_bytes();
                let mut pkt = [0u8; 8];
                pkt[0..4].copy_from_slice(&w0);
                pkt[4..8].copy_from_slice(&w1);
                self.last_setup = pkt;
                self.synthetic_active = false;
                Poll::Ready(pkt)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn data_out(
        &mut self,
        buf: &mut [u8],
        _first: bool,
        _last: bool,
    ) -> Result<usize, EndpointError> {
        let epoch = RESET_EPOCH.load(Ordering::Acquire);
        if !self.wait_ep0_stat_bit(0x0001_0000, epoch).await {
            return Err(EndpointError::Disabled);
        }
        // SAFETY: EP0 OUT FIFO holds host control-OUT payload.
        unsafe {
            wr32(USB_EP0_STAT, 0x00FA_6000);
            let pkt_len = (rd32(ep_out_len_addr(0)) & 0x7FF) as usize;
            if pkt_len > buf.len() {
                let mut sink = [0u8; 64];
                let mut rem = pkt_len;
                while rem > 0 {
                    let n = rem.min(sink.len());
                    dma_read_fifo(USB_EP0_OUT_FIFO, &mut sink[..n]);
                    rem -= n;
                }
                wr32(USB_EP0_STAT, 0x00FB_6008);
                return Err(EndpointError::BufferOverflow);
            }
            if pkt_len > 0 {
                dma_read_fifo(USB_EP0_OUT_FIFO, &mut buf[..pkt_len]);
            }
            wr32(USB_EP0_STAT, 0x00FB_6008);
            dsb();
            Ok(pkt_len)
        }
    }

    async fn data_in(
        &mut self,
        data: &[u8],
        first: bool,
        last: bool,
    ) -> Result<(), EndpointError> {
        if self.synthetic_active {
            return Ok(());
        }
        // Guardrail #2: if enumerated at Full-Speed, clamp Bulk endpoint wMaxPacketSize
        // in GET_DESCRIPTOR(Configuration) from 512 -> 64 so FS hosts accept the descriptor.
        let is_cfg_desc = first
            && (self.last_setup[0] & 0x60) == 0
            && self.last_setup[1] == 6
            && self.last_setup[3] == 2;
        if is_cfg_desc && !is_high_speed() && data.len() <= 128 {
            let mut patched = [0u8; 128];
            let n = data.len();
            patched[..n].copy_from_slice(data);
            let mut pos = 0;
            while pos + 2 <= n {
                let dlen = patched[pos] as usize;
                if dlen < 2 || pos + dlen > n {
                    break;
                }
                if patched[pos + 1] == 0x05 && dlen >= 7 && (patched[pos + 3] & 3) == 2 {
                    let mps = u16::from_le_bytes([patched[pos + 4], patched[pos + 5]]);
                    if mps > 64 {
                        patched[pos + 4] = 64;
                        patched[pos + 5] = 0;
                    }
                }
                pos += dlen;
            }
            self.send_ep0_in_raw(&patched[..n], last).await
        } else {
            self.send_ep0_in_raw(data, last).await
        }
    }

    async fn accept(&mut self) {
        if self.synthetic_active {
            self.synthetic_active = false;
            return;
        }
        // ZLP IN status stage for 0-length host-to-device control requests (e.g. BOMSR).
        let _ = self.send_ep0_in_raw(&[], true).await;
    }

    async fn reject(&mut self) {
        if self.synthetic_active {
            self.synthetic_active = false;
            return;
        }
        unsafe {
            wr32(USB_EP0_CTRL, rd32(USB_EP0_CTRL) | 4);
            dsb();
        }
    }

    async fn accept_set_address(&mut self, _addr: u8) {
        // Handled automatically by BMC silicon.
    }
}

pub struct BmcEndpointOut {
    info: EndpointInfo,
}

impl BmcEndpointOut {
    pub fn set_stalled(&self, stalled: bool) {
        ep_set_stalled(self.info.addr.index(), stalled);
    }

    pub async fn wait_unstalled(&self) {
        wait_ep_unstalled(self.info.addr.index()).await;
    }

    pub fn negotiated_mps(&self) -> usize {
        if is_high_speed() {
            self.info.max_packet_size as usize
        } else {
            (self.info.max_packet_size as usize).min(64)
        }
    }
}

impl Endpoint for BmcEndpointOut {
    fn info(&self) -> &EndpointInfo {
        &self.info
    }

    async fn wait_enabled(&mut self) {
        let ep = self.info.addr.index();
        poll_fn(|cx| {
            EP_WAKERS[ep].register(cx.waker());
            if (EP_ENABLED_MASK.load(Ordering::Acquire) & (1 << ep)) != 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

impl EndpointOut for BmcEndpointOut {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EndpointError> {
        let ep = self.info.addr.index();
        let ctrl = ep_ctrl_addr(ep);
        let stat = ep_stat_addr(ep);
        let fifo = ep_out_fifo_addr(ep);
        let len_reg = ep_out_len_addr(ep);

        poll_fn(|cx| {
            EP_WAKERS[ep].register(cx.waker());
            if (EP_ENABLED_MASK.load(Ordering::Acquire) & (1 << ep)) == 0 {
                return Poll::Ready(Err(EndpointError::Disabled));
            }
            // SAFETY: Dedicated endpoint registers owned by this EndpointOut.
            unsafe {
                let st = rd32(stat);
                if (st & 0x0001_0000) == 0 {
                    // Unmask ONLY OUT packet ready (bit 16) + this EP in USB_EP_INT_MASK.
                    critical_section::with(|_| {
                        wr32(ctrl, (rd32(ctrl) | EP_IRQ_MASK_ALL) & !0x0001_0000);
                        wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) & !(1u32 << ep));
                    });
                    return Poll::Pending;
                }
                wr32(stat, 0x00FA_6000);
                if (rd32(stat) & 0x08) == 0 {
                    critical_section::with(|_| {
                        wr32(ctrl, (rd32(ctrl) | EP_IRQ_MASK_ALL) & !0x0001_0000);
                        wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) & !(1u32 << ep));
                    });
                    return Poll::Pending;
                }
                let pkt_len = (rd32(len_reg) & 0x7FF) as usize;
                let _ = rd32(stat);
                if pkt_len > buf.len() {
                    let mut sink = [0u8; 64];
                    let mut rem = pkt_len;
                    while rem > 0 {
                        let n = rem.min(sink.len());
                        dma_read_fifo(fifo, &mut sink[..n]);
                        rem -= n;
                    }
                    wr32(stat, 0x00FB_6008);
                    return Poll::Ready(Err(EndpointError::BufferOverflow));
                }
                if pkt_len > 0 {
                    dma_read_fifo(fifo, &mut buf[..pkt_len]);
                }
                wr32(stat, 0x00FB_6008);
                dsb();
                Poll::Ready(Ok(pkt_len))
            }
        })
        .await
    }
}

pub struct BmcEndpointIn {
    info: EndpointInfo,
}

impl BmcEndpointIn {
    pub fn set_stalled(&self, stalled: bool) {
        ep_set_stalled(self.info.addr.index(), stalled);
    }

    pub async fn wait_unstalled(&self) {
        wait_ep_unstalled(self.info.addr.index()).await;
    }

    pub fn negotiated_mps(&self) -> usize {
        if is_high_speed() {
            self.info.max_packet_size as usize
        } else {
            (self.info.max_packet_size as usize).min(64)
        }
    }

    /// Write an arbitrary buffer in negotiated HS (512 B) or FS (64 B) chunks.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        let mps = self.negotiated_mps();
        for chunk in data.chunks(mps) {
            self.write(chunk).await?;
        }
        Ok(())
    }
}

impl Endpoint for BmcEndpointIn {
    fn info(&self) -> &EndpointInfo {
        &self.info
    }

    async fn wait_enabled(&mut self) {
        let ep = self.info.addr.index();
        poll_fn(|cx| {
            EP_WAKERS[ep].register(cx.waker());
            if (EP_ENABLED_MASK.load(Ordering::Acquire) & (1 << ep)) != 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

impl EndpointIn for BmcEndpointIn {
    async fn write(&mut self, buf: &[u8]) -> Result<(), EndpointError> {
        let ep = self.info.addr.index();
        let mps = self.negotiated_mps();
        if buf.len() > mps {
            return Err(EndpointError::BufferOverflow);
        }
        let ctrl = ep_ctrl_addr(ep);
        let stat = ep_stat_addr(ep);
        let fifo = ep_in_fifo_addr(ep);

        poll_fn(|cx| {
            EP_WAKERS[ep].register(cx.waker());
            if (EP_ENABLED_MASK.load(Ordering::Acquire) & (1 << ep)) == 0 {
                return Poll::Ready(Err(EndpointError::Disabled));
            }
            // SAFETY: Dedicated endpoint registers owned by this EndpointIn.
            unsafe {
                if (rd32(stat) & 0x04) == 0 {
                    // App0_UPD 0x6001_F892 (0xFFFE_F00F) & 0x6001_6240: bit 16 (0x0001_0000) is the
                    // packet-complete IRQ bit for BOTH Bulk OUT and Bulk IN endpoints.
                    // Clear any stale bit 16 (W0C: !0x0001_000C = 0xFFFE_FFF3), re-check FIFO empty
                    // (0x04) to avoid a race, and unmask bit 16 + this EP in USB_EP_INT_MASK.
                    wr32(stat, 0x00FA_6000);
                    if (rd32(stat) & 0x04) == 0 {
                        critical_section::with(|_| {
                            wr32(ctrl, (rd32(ctrl) | EP_IRQ_MASK_ALL) & !0x0001_0000);
                            wr32(USB_EP_INT_MASK, rd32(USB_EP_INT_MASK) & !(1u32 << ep));
                        });
                        return Poll::Pending;
                    }
                }
                if !buf.is_empty() {
                    dma_write_fifo(fifo, buf);
                }
                for _ in 0..50_000u32 {
                    if (rd32(stat) & 0x04) != 0 {
                        break;
                    }
                    if (EP_ENABLED_MASK.load(Ordering::Acquire) & (1 << ep)) == 0 {
                        return Poll::Ready(Err(EndpointError::Disabled));
                    }
                    core::hint::spin_loop();
                }
                // Commit IN packet (bit 2 = 0x04) and clear bit 16 + bit 18 (0x00FA_6004).
                wr32(stat, 0x00FA_6004);
                dsb();
                Poll::Ready(Ok(()))
            }
        })
        .await
    }
}
