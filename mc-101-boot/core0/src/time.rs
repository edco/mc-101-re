//! Core 0 Hardware Timebase & `embassy-time` Driver (1 ms Cortex-M4 SysTick).
//!
//! Drives both `now_ms()` / `delay_ms()` and `embassy_time::{Timer, Ticker}`
//! at 1 kHz (`tick-hz-1_000`) from the standard ARMv7E-M SysTick interrupt
//! (`VECTOR_TABLE[15]`).

use core::arch::asm;
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::Waker;

use critical_section::Mutex;
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use mc1r1_shared::ipc::{dsb, wfi};

const SYST_CSR: usize = 0xE000_E010;
const SYST_RVR: usize = 0xE000_E014;
const SYST_CVR: usize = 0xE000_E018;

const CSR_ENABLE: u32 = 1 << 0;
const CSR_TICKINT: u32 = 1 << 1;
const CSR_CLKSOURCE: u32 = 1 << 2;

static TICKS_LO: AtomicU32 = AtomicU32::new(0);
static TICKS_HI: AtomicU32 = AtomicU32::new(0);
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static WDT_ARMED: AtomicBool = AtomicBool::new(false);
static WDT_STALL_MS: AtomicU32 = AtomicU32::new(0);

pub fn wdt_arm() {
    WDT_STALL_MS.store(0, Ordering::Release);
    WDT_ARMED.store(true, Ordering::Release);
}

pub fn wdt_feed() {
    WDT_STALL_MS.store(0, Ordering::Release);
}

struct SysTickDriver {
    queue: Mutex<RefCell<Queue>>,
    next_alarm: Mutex<Cell<u64>>,
}

impl SysTickDriver {
    const fn new() -> Self {
        Self {
            queue: Mutex::new(RefCell::new(Queue::new())),
            next_alarm: Mutex::new(Cell::new(u64::MAX)),
        }
    }

    fn on_tick(&self, now: u64) {
        critical_section::with(|cs| {
            let alarm = self.next_alarm.borrow(cs).get();
            if now >= alarm {
                let mut queue = self.queue.borrow(cs).borrow_mut();
                let next = queue.next_expiration(now);
                self.next_alarm.borrow(cs).set(next);
            }
        });
    }
}

impl Driver for SysTickDriver {
    fn now(&self) -> u64 {
        now_ms()
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            if queue.schedule_wake(at, waker) {
                let now = now_ms();
                let next = queue.next_expiration(now);
                self.next_alarm.borrow(cs).set(next);
            }
        });
    }
}

embassy_time_driver::time_driver_impl!(static DRIVER: SysTickDriver = SysTickDriver::new());

/// Configure Cortex-M4 SysTick for a 1 ms periodic interrupt using `sysclk_hz`.
pub fn init(sysclk_hz: u32) {
    let reload = (sysclk_hz / 1_000).saturating_sub(1).clamp(1, 0x00FF_FFFF);

    // SAFETY: Standard Cortex-M4 SysTick + SHPR3 registers; called once during Core 0 boot.
    unsafe {
        // Guarantee SysTick has highest programmable priority (0x00 in SHPR3[31:24])
        // so it can preempt USB_IRQ_Handler (prio 6) and SD_DMA_IRQ_Handler (prio 4).
        let shpr3 = core::ptr::read_volatile(0xE000_ED20 as *const u32);
        core::ptr::write_volatile(0xE000_ED20 as *mut u32, shpr3 & 0x00FF_FFFF);
        core::ptr::write_volatile(SYST_CSR as *mut u32, 0);
        core::ptr::write_volatile(SYST_RVR as *mut u32, reload);
        core::ptr::write_volatile(SYST_CVR as *mut u32, 0);
        dsb();
        core::ptr::write_volatile(
            SYST_CSR as *mut u32,
            CSR_ENABLE | CSR_TICKINT | CSR_CLKSOURCE,
        );
        dsb();
        INITIALIZED.store(true, Ordering::Release);
        asm!("cpsie i", options(nomem, nostack, preserves_flags));
    }
}

/// Disable SysTick before handing off to the target firmware.
pub fn deinit() {
    INITIALIZED.store(false, Ordering::Release);
    unsafe {
        core::ptr::write_volatile(SYST_CSR as *mut u32, 0);
        core::ptr::write_volatile(SYST_CVR as *mut u32, 0);
        dsb();
    }
}

/// Cortex-M4 SysTick exception handler (1 ms tick) with 2 s software watchdog.
#[link_section = ".itcm"]
#[no_mangle]
pub unsafe extern "C" fn systick_rust(frame: *const u32) {
    let prev = TICKS_LO.fetch_add(1, Ordering::Relaxed);
    let lo = prev.wrapping_add(1);
    let hi = if prev == u32::MAX {
        TICKS_HI.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    } else {
        TICKS_HI.load(Ordering::Relaxed)
    };
    let now = ((hi as u64) << 32) | (lo as u64);
    DRIVER.on_tick(now);

    if WDT_ARMED.load(Ordering::Relaxed) {
        let stalled = WDT_STALL_MS.fetch_add(1, Ordering::Relaxed) + 1;
        if stalled >= 2000 {
            WDT_ARMED.store(false, Ordering::Relaxed);
            let lr = core::ptr::read_volatile(frame.add(5));
            let pc = core::ptr::read_volatile(frame.add(6));
            crate::wdt_report(pc, lr);
        }
    }
}

core::arch::global_asm!(
    r#"
    .syntax unified
    .thumb
    .section .itcm,"ax",%progbits
    .align 2
    .global SysTick_Handler
    .thumb_func
SysTick_Handler:
    tst lr, #4
    ite eq
    mrseq r0, msp
    mrsne r0, psp
    b systick_rust
    "#
);

/// Monotonic milliseconds elapsed since `time::init()`.
pub fn now_ms() -> u64 {
    loop {
        let hi1 = TICKS_HI.load(Ordering::Acquire);
        let lo = TICKS_LO.load(Ordering::Acquire);
        let hi2 = TICKS_HI.load(Ordering::Acquire);
        if hi1 == hi2 {
            return ((hi1 as u64) << 32) | (lo as u64);
        }
    }
}

/// Blocking sleep for `ms` milliseconds (used for hardware settle delays
/// during early init and shutdown; async tasks should use `embassy_time::Timer`).
pub fn delay_ms(ms: u32) {
    if ms == 0 {
        return;
    }
    if INITIALIZED.load(Ordering::Acquire) {
        let target = now_ms().wrapping_add(ms as u64);
        while now_ms() < target {
            wfi();
        }
    } else {
        for _ in 0..ms {
            for _ in 0..30_000u32 {
                core::hint::spin_loop();
            }
        }
    }
}
