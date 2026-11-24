//! MC-101 Core 1 ("mc1r1-core1") — Unified ITCM + SDRAM Bare-Metal Firmware.
//!
//! Linked as a single ELF and split by `xtask` into `idm1.bin` (`0x0110_0000`)
//! and `sdram1.bin` (`0x6100_0000`) for stock `RPG69_C1A_up.bin` QSPI TOC
//! compatibility:
//! - `idm1.bin`: 32-byte zero header at `0x0110_0000..0x0110_001F`, Cortex-M4
//!   vector table at `0x0110_0020` (`MSP = 0x200A_91C8`), hot code/ISRs.
//! - `sdram1.bin`: 32-byte zero header at `0x6100_0000..0x6100_001F`, cold DSP
//!   code/tables starting at `0x6100_0020` (`#[link_section = ".sdram.*"]`).
//! - `.data`/`.bss` reside strictly in Core 1's SRAM window (`0x2002_0000+`),
//!   never overlapping Core 0's `0x2000_0000..0x2001_FFFF`.

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::sync::atomic::Ordering;
use mc1r1_shared::ipc::{dsb, wfi, HelloMailbox, HELLO_MAGIC};
use mc1r1_shared::memmap::{enable_fpu, CORE1_INITIAL_MSP, SCI_BASE};

extern "C" {
    static __sidata: u32;
    static mut __sdata: u32;
    static mut __edata: u32;
    static mut __sbss: u32;
    static mut __ebss: u32;
}

#[repr(C)]
pub union Vector {
    handler: unsafe extern "C" fn(),
    reserved: u32,
}

/// Core 1 Cortex-M4 Vector Table (placed at `0x0110_0020`).
#[link_section = ".vector_table"]
#[no_mangle]
pub static VECTOR_TABLE: [Vector; 16] = [
    Vector { reserved: CORE1_INITIAL_MSP },    // 0: Initial MSP (0x200A_91C8)
    Vector { handler: Reset_Entry },           // 1: Reset
    Vector { handler: Default_Handler },       // 2: NMI
    Vector { handler: HardFault_Handler },     // 3: HardFault
    Vector { handler: Default_Handler },       // 4: MemManage
    Vector { handler: Default_Handler },       // 5: BusFault
    Vector { handler: Default_Handler },       // 6: UsageFault
    Vector { reserved: 0 },                    // 7: Reserved
    Vector { reserved: 0 },                    // 8: Reserved
    Vector { reserved: 0 },                    // 9: Reserved
    Vector { reserved: 0 },                    // 10: Reserved
    Vector { handler: Default_Handler },       // 11: SVCall
    Vector { handler: Default_Handler },       // 12: DebugMonitor
    Vector { reserved: 0 },                    // 13: Reserved
    Vector { handler: Default_Handler },       // 14: PendSV
    Vector { handler: Default_Handler },       // 15: SysTick
];

#[no_mangle]
pub unsafe extern "C" fn Reset_Entry() {
    Reset_Handler()
}

#[no_mangle]
pub unsafe extern "C" fn Reset_Handler() -> ! {
    // 1. Copy .data from ITCM1 load image to Core 1 SRAM (0x2002_0000+).
    let mut src = &__sidata as *const u32;
    let mut dst = &raw mut __sdata;
    let data_end = &raw mut __edata;
    while dst < data_end {
        core::ptr::write_volatile(dst, core::ptr::read_volatile(src));
        src = src.add(1);
        dst = dst.add(1);
    }

    // 2. Zero-initialize .bss in Core 1 SRAM.
    let mut bss = &raw mut __sbss;
    let bss_end = &raw mut __ebss;
    while bss < bss_end {
        core::ptr::write_volatile(bss, 0);
        bss = bss.add(1);
    }

    // 3. Enable FPU (CP10/CP11 + lazy stacking) before any hard-float code.
    enable_fpu();

    core1_main()
}

fn core1_main() -> ! {
    let mbox = unsafe { HelloMailbox::get() };

    // Wait until Core 0 initializes the bring-up mailbox.
    while mbox.magic.load(Ordering::Acquire) != HELLO_MAGIC {
        core::hint::spin_loop();
    }

    // Jump from ITCM1 (idm1.bin) into SDRAM1 (sdram1.bin) to post the greeting,
    // proving both Core 1 ITCM and Core 1 SDRAM loaded and executed.
    sdram1_entry(mbox)
}

/// Replicates the verified `Core1BootModeSvc` (`0x6100_0C40`) pinmux writes
/// and `Core1DevRegSetup` (`0x6103_238C`) `SCI |= 0x31` enable from
/// `mc-1r1/DISPLAY.md` §4.3 so the panel/SCI link comes up.
///
/// Note: Never touches `0x4002_6468`/`0x4002_646C` (MC-101 power-off minefield).
#[allow(dead_code)]
#[link_section = ".sdram"]
#[inline(never)]
unsafe fn init_pinmux_and_sci() {
    #[inline(always)]
    unsafe fn wr(addr: usize, val: u32) {
        core::ptr::write_volatile(addr as *mut u32, val);
    }

    // 0x4002_65B0..=0x4002_65EC (stride 4) = 0xC01
    let mut a = 0x4002_65B0usize;
    while a <= 0x4002_65EC {
        wr(a, 0xC01);
        a += 4;
    }

    // 0x4002_6600..=0x4002_6638 (stride 4) = 0x801
    a = 0x4002_6600;
    while a <= 0x4002_6638 {
        wr(a, 0x801);
        a += 4;
    }
    wr(0x4002_6640, 0x801);
    wr(0x4002_6648, 0x801);
    wr(0x4002_664C, 0x801);
    wr(0x4002_6650, 0x801);
    wr(0x4002_6654, 0x801);

    // 0x4002_6704..=0x4002_6758 (stride 4) = 0x801, 0x4002_6768 = 0x801
    a = 0x4002_6704;
    while a <= 0x4002_6758 {
        wr(a, 0x801);
        a += 4;
    }
    wr(0x4002_6768, 0x801);

    // 0x4002_6780..=0x4002_67BC (stride 4) = 0xC01
    a = 0x4002_6780;
    while a <= 0x4002_67BC {
        wr(a, 0xC01);
        a += 4;
    }

    // 0x4002_67C0..=0x4002_67F0
    wr(0x4002_67C0, 0x801);
    wr(0x4002_67C4, 0x801);
    wr(0x4002_67C8, 0x800);
    wr(0x4002_67CC, 0x800);
    wr(0x4002_67D0, 0x802);
    wr(0x4002_67D4, 0x802);
    wr(0x4002_67D8, 0x802);
    wr(0x4002_67DC, 0x806);
    wr(0x4002_67E0, 0x801);
    wr(0x4002_67E4, 0x801);
    wr(0x4002_67E8, 0x801);
    wr(0x4002_67EC, 0x801);
    wr(0x4002_67F0, 0x801);

    // SCI control enable: 0x4002_6000 |= 0x31
    let sci = SCI_BASE as *mut u32;
    let cur = core::ptr::read_volatile(sci);
    core::ptr::write_volatile(sci, cur | 0x31);
    dsb();
}

/// Core 1 SDRAM DSP entry point at `0x6100_0000` (extracted into `sdram1.bin`).
#[link_section = ".sdram.entry"]
#[no_mangle]
#[inline(never)]
pub extern "C" fn sdram1_entry(mbox: &mut HelloMailbox) -> ! {
    mbox.post_from_core1(b"Hello from C1!");
    loop {
        wfi();
    }
}

#[no_mangle]
pub unsafe extern "C" fn HardFault_Handler() {
    loop {
        wfi();
    }
}

#[no_mangle]
pub unsafe extern "C" fn Default_Handler() {
    loop {
        wfi();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        wfi();
    }
}
