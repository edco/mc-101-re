//! `mc-101-boot` — SD Card Bootloader & Runtime Binary Patcher for the Roland MC-101.
//!
//! Designed to be appended to stock `App1_Main` (`0x6035_65B0`, hooked from `0x6000_001C`)
//! or booted standalone at `0x6000_0000`.
//!
//! On entry, a 64-byte position-independent Thumb-2 stub copies `.itcm` -> `0x0100_0000`
//! and `.data` -> `0x2000_0000`, zeroes `.bss`, and jumps to `bootloader_main()` in ITCM,
//! leaving all 32 MiB of SDRAM (`0x6000_0000..0x61FF_FFFF`) free for loading images.
//!
//! Reads `/firmware/firmware.ini`:
//! - `[c0a]` `image = ...` / `patch = ...`
//! - `[c1a]` `image = ...` / `patch = ...`

#![no_std]
#![no_main]

mod c1a_stub;
mod clocks;
mod display;
mod dmac;
mod fs;
mod hal;
mod msc;
mod panel;
mod power;
mod sd;
mod time;
mod usb;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Executor;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Ticker, Timer};
use embassy_usb::{Builder, Config};
use mc1r1_shared::firmware_cfg::{apply_mc1p, verify_mc1p, FirmwareConfig, PatchOp};
use mc1r1_shared::ipc::{dsb, isb};
use mc1r1_shared::memmap::{
    CORE0_INITIAL_MSP, CORE0_ITCM_BASE, CORE0_SDRAM_BASE, CORE1_ITCM_BASE,
    CORE1_PRELOADED_MAGIC, CORE1_PRELOADED_MAGIC_ADDR, SCB_VTOR,
};
use static_cell::StaticCell;

use display::{LcdCh7, COLS};
use hal::{BlockDevice, Peripherals, TextDisplay};
use msc::{
    is_media_ejected, is_usb_configured, set_storage_mode, AsyncMsc, MscControlHandler, SharedSd,
    StorageMode,
};
use panel::button;
use power::PowerCtl;
use sd::{SdCard, SdError};
use usb::UsbDevice;

const NVIC_ICER0: usize = 0xE000_E180;
const NVIC_ICER1: usize = 0xE000_E184;
const APP1_STOCK_PREINIT: usize = 0x6000_0199; // 0x6000_0198 | 1 (Thumb)
const C0A_MAX_IMAGE_SIZE: usize = 0x0080_0000; // 8 MiB max

static mut SAVED_LR: u32 = 0;
static mut INI_BUF: [u8; 4096] = [0u8; 4096];

extern "C" {
    static __sitcm: u32;
    static __eitcm: u32;
    static __sdata: u32;
    static __edata: u32;
    static mut __sbss: u32;
    static mut __ebss: u32;
}

// 64-byte position-independent entry stub placed at offset 0 of the bootloader binary.
global_asm!(
    r#"
    .syntax unified
    .thumb
    .section .text.entry,"ax",%progbits
    .align 2
    .global Reset_Handler
_boot_entry_base:
    .thumb_func
Reset_Handler:
    mov r4, lr
    movs r0, #0
    msr basepri, r0
    adr r0, _boot_entry_base
    add.w r0, r0, #0x100
    ldr r1, =__sitcm
    ldr r2, =__eitcm
1:  cmp r1, r2
    bhs 2f
    ldr r3, [r0], #4
    str r3, [r1], #4
    b 1b
2:  ldr r1, =__sdata
    ldr r2, =__edata
3:  cmp r1, r2
    bhs 4f
    ldr r3, [r0], #4
    str r3, [r1], #4
    b 3b
4:  ldr r1, =__sbss
    ldr r2, =__ebss
    movs r3, #0
5:  cmp r1, r2
    bhs 6f
    str r3, [r1], #4
    b 5b
6:  ldr r0, ={saved_lr}
    str r4, [r0]
    ldr r0, =0xE000ED88
    ldr r1, [r0]
    orr.w r1, r1, #0x00F00000
    str r1, [r0]
    ldr r0, ={vtor}
    ldr r1, ={itcm_base}
    str r1, [r0]
    dsb
    isb
    ldr r0, ={main_fn}
    bx r0
    .ltorg
    "#,
    saved_lr = sym SAVED_LR,
    vtor = const SCB_VTOR,
    itcm_base = const CORE0_ITCM_BASE,
    main_fn = sym bootloader_main,
);

#[repr(C)]
#[derive(Clone, Copy)]
pub union Vector {
    handler: unsafe extern "C" fn(),
    reserved: u32,
}

#[repr(C, align(512))]
pub struct AlignedVectorTable([Vector; 128]);

const fn build_vector_table() -> AlignedVectorTable {
    extern "C" {
        fn Reset_Handler();
    }
    let mut t = [Vector {
        handler: Default_Handler,
    }; 128];
    t[0] = Vector {
        reserved: CORE0_INITIAL_MSP,
    };
    t[1] = Vector {
        handler: Reset_Handler,
    };
    t[3] = Vector {
        handler: HardFault_Handler,
    };
    t[15] = Vector {
        handler: time::SysTick_Handler,
    };
    t[16 + usb::USB_IRQ_NUM] = Vector {
        handler: usb::USB_IRQ_Handler,
    };
    t[16 + sd::SD_DMA_IRQ_NUM] = Vector {
        handler: sd::SD_DMA_IRQ_Handler,
    };
    t[16 + 107] = Vector {
        handler: usb::USB_IRQ_Handler,
    };
    t[16 + 108] = Vector {
        handler: usb::USB_IRQ_Handler,
    };
    AlignedVectorTable(t)
}

#[link_section = ".vector_table"]
#[no_mangle]
pub static VECTOR_TABLE: AlignedVectorTable = build_vector_table();

#[link_section = ".itcm"]
#[no_mangle]
pub extern "C" fn bootloader_main() -> ! {
    unsafe {
        mc1r1_shared::memmap::enable_fpu();
    }

    let Some(mut p) = Peripherals::take() else {
        loop {
            core::hint::spin_loop();
        }
    };

    let sysclk_hz = p.clocks.sysclk_hz();
    time::init(sysclk_hz);
    p.power.init();
    p.dmac0.reset_and_enable();
    p.lcd.init();
    show_status(&mut p.lcd, b"MC-101 BOOT v0.2", b"PROBING PANEL...");

    p.panel.init();
    let boot_scan_end = time::now_ms().wrapping_add(120);
    while time::now_ms() < boot_scan_end {
        while p.panel.poll_event().is_some() {}
    }

    if p.panel.is_held(button::MULTI_FX) {
        enter_usb_storage_mode(p);
    }

    show_status(&mut p.lcd, b"MC-101 BOOT v0.2", b"PROBING SD...   ");

    let mut c0a_from_sd = false;
    let mut c0a_sd_len: usize = 0;
    unsafe {
        core::ptr::write_volatile(CORE1_PRELOADED_MAGIC_ADDR as *mut u32, 0);
    }

    if p.sd.mount().is_ok() {
        let vol_mgr = embedded_sdmmc::VolumeManager::new(fs::SdRef(&p.sd), fs::BootTimeSource);
        let bfs_res = fs::BootFs::open(&vol_mgr);
        if let Ok(bfs) = bfs_res {
            let ini_buf = unsafe { &mut *(&raw mut INI_BUF) };
            if let Ok(ini_len) = bfs.read_file_into("firmware/firmware.ini", ini_buf) {
                if let Ok(ini_text) = core::str::from_utf8(&ini_buf[..ini_len]) {
                    let cfg = FirmwareConfig::parse(ini_text);

                    // 1. Process [c0a] image (if specified).
                    if let Some(img_path) = cfg.c0a.image.as_ref() {
                        show_status(&mut p.lcd, b"BOOT: LOAD C0A  ", img_path.as_str().as_bytes());
                        match load_c0a_image(&bfs, img_path.as_str()) {
                            Ok(n) => {
                                c0a_from_sd = true;
                                c0a_sd_len = n;
                            }
                            Err(msg) => {
                                fault_halt(&mut p.lcd, &mut p.power, b"ERR: C0A IMAGE  ", msg)
                            }
                        }
                    }

                    // 2. Process [c0a] patches (if specified).
                    for pat_path in cfg.c0a.patches() {
                        show_status(&mut p.lcd, b"BOOT: PATCH C0A ", pat_path.as_bytes());
                        if let Err(msg) = apply_c0a_patch(&bfs, pat_path) {
                            fault_halt(&mut p.lcd, &mut p.power, b"ERR: C0A PATCH  ", msg);
                        }
                    }

                    // 3. Process [c1a] image / patches:
                    //    - For stock App1_Main: stage in SDRAM/CAVE-02 so App1_Main loads/patches
                    //      Core 1 right before releasing reset at 0x600A_A56C.
                    //    - For custom Core 0 (e.g. mc-1r1): if [c1a] image is set, load idm1.bin ->
                    //      0x0110_0000 and sdram1.bin -> 0x6100_0000, apply any [c1a] patches, and
                    //      set CORE1_PRELOADED_MAGIC so Core 0 skips QSPI1 load_core1().
                    let is_stock = (!c0a_from_sd || c0a_sd_len >= 0x000B_0000)
                        && unsafe { c1a_stub::is_stock_app1() };
                    let has_c1a = cfg.c1a.image.is_some() || cfg.c1a.patch_count > 0;
                    if has_c1a && is_stock {
                        let mut idm1_len = None;
                        if let Some(c1a_img) = cfg.c1a.image.as_ref() {
                            show_status(
                                &mut p.lcd,
                                b"BOOT: LOAD C1A  ",
                                c1a_img.as_str().as_bytes(),
                            );
                            match stage_c1a_image(&bfs, c1a_img.as_str()) {
                                Ok(len) => idm1_len = Some(len),
                                Err(msg) => {
                                    fault_halt(&mut p.lcd, &mut p.power, b"ERR: C1A IMAGE  ", msg)
                                }
                            }
                        }

                        let mut staged_patches = false;
                        if cfg.c1a.patch_count > 0 {
                            show_status(&mut p.lcd, b"BOOT: STAGE C1A ", b"MC1P PATCHES    ");
                            match stage_c1a_patches(&bfs, &cfg.c1a, idm1_len.is_some()) {
                                Ok(any) => staged_patches = any,
                                Err(msg) => {
                                    fault_halt(&mut p.lcd, &mut p.power, b"ERR: C1A PATCH  ", msg)
                                }
                            }
                        }

                        unsafe {
                            c1a_stub::install_stock_c1a_hooks(idm1_len, staged_patches);
                        }
                    } else if let Some(c1a_img) = cfg.c1a.image.as_ref() {
                        show_status(
                            &mut p.lcd,
                            b"BOOT: PRELD C1A ",
                            c1a_img.as_str().as_bytes(),
                        );
                        let idm1_len = match stage_c1a_image(&bfs, c1a_img.as_str()) {
                            Ok(len) => len,
                            Err(msg) => {
                                fault_halt(&mut p.lcd, &mut p.power, b"ERR: C1A IMAGE  ", msg)
                            }
                        };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                c1a_stub::STAGED_IDM1_ADDR as *const u8,
                                CORE1_ITCM_BASE as *mut u8,
                                idm1_len as usize,
                            );
                        }
                        for pat_path in cfg.c1a.patches() {
                            show_status(&mut p.lcd, b"BOOT: PATCH C1A ", pat_path.as_bytes());
                            if let Err(msg) = apply_c0a_patch(&bfs, pat_path) {
                                fault_halt(&mut p.lcd, &mut p.power, b"ERR: C1A PATCH  ", msg);
                            }
                        }
                        unsafe {
                            core::ptr::write_volatile(
                                CORE1_PRELOADED_MAGIC_ADDR as *mut u32,
                                CORE1_PRELOADED_MAGIC,
                            );
                        }
                    }
                }
            }
        }
    }

    handoff_to_target(c0a_from_sd);
}

fn load_c0a_image(bfs: &fs::BootFs<'_>, path: &str) -> Result<usize, &'static [u8]> {
    let mut dst = CORE0_SDRAM_BASE;
    let mut total = 0usize;
    let mut header_checked = false;
    let mut expected_payload_len: Option<usize> = None;
    let mut first_block = [0u8; 96];
    let mut first_fill = 0usize;

    let mut overflow = false;
    let mut bad_container = false;

    bfs.stream_file(path, |chunk| {
        if !header_checked {
            let take = (96 - first_fill).min(chunk.len());
            first_block[first_fill..first_fill + take].copy_from_slice(&chunk[..take]);
            first_fill += take;
            if first_fill < 96 {
                return true;
            }
            header_checked = true;

            // Detect RPG69_C0A_up.bin 96-byte container ("App1_Main" at +0x00).
            if &first_block[0..9] == b"App1_Main" {
                let control = u32::from_le_bytes(first_block[0x54..0x58].try_into().unwrap());
                if control != 0 {
                    bad_container = true;
                    return false;
                }
                let plen =
                    u32::from_le_bytes(first_block[0x44..0x48].try_into().unwrap()) as usize;
                expected_payload_len = Some(plen);
                let rem = &chunk[take..];
                let to_copy = rem.len().min(plen);
                if to_copy > C0A_MAX_IMAGE_SIZE {
                    overflow = true;
                    return false;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(rem.as_ptr(), dst as *mut u8, to_copy);
                }
                dst += to_copy;
                total += to_copy;
                return total < plen;
            } else {
                // Raw binary: write buffered first_block + remainder of chunk.
                if first_fill + (chunk.len() - take) > C0A_MAX_IMAGE_SIZE {
                    overflow = true;
                    return false;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(first_block.as_ptr(), dst as *mut u8, first_fill);
                    dst += first_fill;
                    total += first_fill;
                    let rem = &chunk[take..];
                    core::ptr::copy_nonoverlapping(rem.as_ptr(), dst as *mut u8, rem.len());
                    dst += rem.len();
                    total += rem.len();
                }
                return true;
            }
        }

        let remaining = match expected_payload_len {
            Some(plen) => plen.saturating_sub(total),
            None => C0A_MAX_IMAGE_SIZE.saturating_sub(total),
        };
        let to_copy = chunk.len().min(remaining);
        if total + to_copy > C0A_MAX_IMAGE_SIZE {
            overflow = true;
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst as *mut u8, to_copy);
        }
        dst += to_copy;
        total += to_copy;
        if let Some(plen) = expected_payload_len {
            total < plen
        } else {
            true
        }
    })
    .map_err(fs_err_msg)?;

    if bad_container {
        return Err(b"C0A ENCRYPTED   ");
    }
    if overflow || total < 64 {
        return Err(b"BAD C0A SIZE    ");
    }
    Ok(total)
}

fn apply_c0a_patch(bfs: &fs::BootFs<'_>, path: &str) -> Result<(), &'static [u8]> {
    let mut total = 0usize;
    let mut overflow = false;
    bfs.stream_file(path, |chunk| {
        if total + chunk.len() > c1a_stub::C1A_TEMP_MAX {
            overflow = true;
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (c1a_stub::C1A_TEMP_BUF + total) as *mut u8,
                chunk.len(),
            );
        }
        total += chunk.len();
        true
    })
    .map_err(fs_err_msg)?;
    if overflow {
        return Err(b"C0A PAT TOO BIG ");
    }
    let slice = unsafe { core::slice::from_raw_parts(c1a_stub::C1A_TEMP_BUF as *const u8, total) };

    verify_mc1p(slice, |addr, i| {
        Some(unsafe { core::ptr::read_volatile((addr as usize + i) as *const u8) })
    })
    .map_err(|_| &b"C0A PAT VERIFY  "[..])?;

    apply_mc1p(slice, |op| unsafe {
        match op {
            PatchOp::Write { addr, bytes } => {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
            }
            PatchOp::Copy { dst, src, len } => {
                core::ptr::copy(src as *const u8, dst as *mut u8, len);
            }
            PatchOp::Fill { dst, byte, len } => {
                core::ptr::write_bytes(dst as *mut u8, byte, len);
            }
        }
    })
    .map_err(|_| &b"C0A PAT APPLY   "[..])?;

    Ok(())
}

fn stage_c1a_image(bfs: &fs::BootFs<'_>, path: &str) -> Result<u32, &'static [u8]> {
    let mut total = 0usize;
    let mut overflow = false;
    bfs.stream_file(path, |chunk| {
        if total + chunk.len() > c1a_stub::C1A_TEMP_MAX {
            overflow = true;
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (c1a_stub::C1A_TEMP_BUF + total) as *mut u8,
                chunk.len(),
            );
        }
        total += chunk.len();
        true
    })
    .map_err(fs_err_msg)?;

    if overflow {
        return Err(b"C1A TOO LARGE   ");
    }
    unsafe { c1a_stub::parse_and_stage_c1a(total) }.ok_or(&b"BAD C1A TOC     "[..])
}

fn stage_c1a_patches(
    bfs: &fs::BootFs<'_>,
    c1a_cfg: &mc1r1_shared::firmware_cfg::CoreBootConfig,
    verify_against_ram: bool,
) -> Result<bool, &'static [u8]> {
    let mut cursor = c1a_stub::STAGED_PATCH_ADDR;
    let end_limit = c1a_stub::STAGED_PATCH_ADDR + c1a_stub::STAGED_PATCH_MAX - 4;
    let mut any = false;

    for pat_path in c1a_cfg.patches() {
        let mut len = 0usize;
        let mut overflow = false;
        bfs.stream_file(pat_path, |chunk| {
            if cursor + 4 + len + chunk.len() > end_limit {
                overflow = true;
                return false;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    (cursor + 4 + len) as *mut u8,
                    chunk.len(),
                );
            }
            len += chunk.len();
            true
        })
        .map_err(fs_err_msg)?;
        if overflow {
            return Err(b"C1A PAT OOM     ");
        }
        let slice = unsafe { core::slice::from_raw_parts((cursor + 4) as *const u8, len) };

        // Always verify CRC-32 and framing; also verify expected bytes if Core 1 RAM is staged.
        verify_mc1p(slice, |addr, i| {
            if verify_against_ram {
                let phys = if (addr as usize) >= 0x0110_0000 && (addr as usize) < 0x0114_0000 {
                    c1a_stub::STAGED_IDM1_ADDR + ((addr as usize) - 0x0110_0000)
                } else {
                    addr as usize
                };
                Some(unsafe { core::ptr::read_volatile((phys + i) as *const u8) })
            } else {
                // Defer expected-byte check until App1_Main loads QSPI1.
                None
            }
        })
        .or_else(|e| {
            if !verify_against_ram
                && matches!(
                    e,
                    mc1r1_shared::firmware_cfg::PatchError::ReadFailed { .. }
                )
            {
                Ok(mc1r1_shared::firmware_cfg::Mc1pHeader {
                    version: 1,
                    record_count: 0,
                    flags: 0,
                    crc32: 0,
                })
            } else {
                Err(&b"C1A PAT VERIFY  "[..])
            }
        })?;

        let aligned = (len + 3) & !3;
        unsafe {
            core::ptr::write_volatile(cursor as *mut u32, len as u32);
        }
        cursor += 4 + aligned;
        any = true;
    }

    unsafe {
        core::ptr::write_volatile(cursor as *mut u32, 0);
    }
    Ok(any)
}

fn fs_err_msg(e: fs::FsError) -> &'static [u8] {
    match e {
        fs::FsError::Sd(_) => b"FS: SD IO ERR   ",
        fs::FsError::VolumeOpen(b'T') => b"FS: VOL TIMEOUT ",
        fs::FsError::VolumeOpen(b'C') => b"FS: VOL CMD ERR ",
        fs::FsError::VolumeOpen(b'F') => b"FS: VOL FMT ERR ",
        fs::FsError::VolumeOpen(_) => b"FS: VOL OPEN ERR",
        fs::FsError::DirOpen => b"FS: DIR OPEN ERR",
        fs::FsError::FileOpen => b"FS: FILE NOT FND",
        fs::FsError::FileRead => b"FS: FILE RD ERR ",
        fs::FsError::TooLarge => b"FS: TOO LARGE   ",
        fs::FsError::BadPath => b"FS: BAD PATH    ",
    }
}

fn handoff_to_target(c0a_from_sd: bool) -> ! {
    unsafe {
        asm!("cpsid i", options(nomem, nostack, preserves_flags));
        time::deinit();
        core::ptr::write_volatile(NVIC_ICER0 as *mut u32, 0xFFFF_FFFF);
        core::ptr::write_volatile(NVIC_ICER1 as *mut u32, 0xFFFF_FFFF);
        let basepri: u32 = if c0a_from_sd { 0 } else { 0x10 };
        asm!("msr basepri, {}", in(reg) basepri, options(nomem, nostack, preserves_flags));
        dsb();
        isb();
        // App1_Main expects PRIMASK=0 (with BASEPRI=0x10); leaving PRIMASK=1 hangs RTOS.
        asm!("cpsie i", options(nomem, nostack, preserves_flags));

        if c0a_from_sd {
            let entry: extern "C" fn() -> ! =
                core::mem::transmute((CORE0_SDRAM_BASE | 1) as usize);
            entry();
        } else {
            let preinit: extern "C" fn() = core::mem::transmute(APP1_STOCK_PREINIT);
            preinit();
            let ret: extern "C" fn() -> ! = core::mem::transmute(SAVED_LR as usize);
            ret();
        }
    }
}

fn show_status(lcd: &mut display::LcdCh7, row0: &[u8], row1: &[u8]) {
    let mut buf = [b' '; COLS];
    let n0 = row0.len().min(COLS);
    buf[..n0].copy_from_slice(&row0[..n0]);
    lcd.write_line(0, &buf);
    buf.fill(b' ');
    let n1 = row1.len().min(COLS);
    buf[..n1].copy_from_slice(&row1[..n1]);
    lcd.write_line(1, &buf);
    lcd.flush();
}

fn fault_halt(
    lcd: &mut display::LcdCh7,
    power: &mut power::PowerCtl,
    row0: &[u8],
    row1: &[u8],
) -> ! {
    show_status(lcd, row0, row1);
    loop {
        if power.poll_tick() {
            power.safe_shutdown(lcd, || {});
        }
        time::delay_ms(20);
    }
}

enum DisplayCmd {
    WriteLine { row: usize, len: usize, buf: [u8; COLS] },
    Shutdown(PowerCtl),
}

static DISPLAY_CH: Channel<CriticalSectionRawMutex, DisplayCmd, 4> = Channel::new();
static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static SHARED_SD: StaticCell<SharedSd> = StaticCell::new();
static SHUTDOWN_REQ: AtomicBool = AtomicBool::new(false);
static STORAGE_QUIESCED: AtomicBool = AtomicBool::new(true);

#[embassy_executor::task]
async fn display_task(mut lcd: LcdCh7) {
    loop {
        match DISPLAY_CH.receive().await {
            DisplayCmd::WriteLine { row, len, buf } => {
                lcd.write_line(row, &buf[..len]);
                lcd.flush();
            }
            DisplayCmd::Shutdown(mut power) => {
                for _ in 0..50 {
                    if STORAGE_QUIESCED.load(Ordering::Acquire) {
                        break;
                    }
                    Timer::after_millis(10).await;
                }
                power.safe_shutdown(&mut lcd, || {});
            }
        }
    }
}

#[embassy_executor::task]
async fn power_monitor_task(mut power: PowerCtl) {
    let mut ticker = Ticker::every(Duration::from_millis(power::POLL_INTERVAL_MS));
    loop {
        ticker.next().await;
        if power.poll_tick() {
            SHUTDOWN_REQ.store(true, Ordering::Release);
            let _ = DISPLAY_CH.try_send(DisplayCmd::Shutdown(power));
            break;
        }
    }
}

#[embassy_executor::task]
async fn usb_msc_task(usb: UsbDevice, sd: SdCard, mb: u32) {
    const READ_ONLY: bool = false;
    STORAGE_QUIESCED.store(false, Ordering::Release);
    set_storage_mode(StorageMode::UsbMsc);

    let sd_mutex: &'static SharedSd = SHARED_SD.init(Mutex::new(sd));

    let mut cfg = Config::new(0x0582, 0x022C);
    cfg.manufacturer = Some("Roland");
    cfg.product = Some("MC-101 BOOT");
    cfg.serial_number = Some("000000000001");
    cfg.device_release = 0x0100;
    cfg.device_class = 0x00;
    cfg.device_sub_class = 0x00;
    cfg.device_protocol = 0x00;
    cfg.composite_with_iads = false;
    cfg.max_power = 0;
    cfg.self_powered = true;
    cfg.max_packet_size_0 = 64;

    let mut config_desc = [0u8; 128];
    let mut bos_desc = [0u8; 64];
    let mut msos_desc = [0u8; 64];
    let mut control_buf = [0u8; 128];
    let mut ctrl_handler = MscControlHandler::new();

    let mut builder = Builder::new(
        usb,
        cfg,
        &mut config_desc,
        &mut bos_desc,
        &mut msos_desc,
        &mut control_buf,
    );
    builder.handler(&mut ctrl_handler);

    let mut func = builder.function(0x08, 0x06, 0x50);
    let mut iface = func.interface();
    let mut alt = iface.alt_setting(0x08, 0x06, 0x50, None);
    let mut ep_out = alt.endpoint_bulk_out(None, 512);
    let mut ep_in = alt.endpoint_bulk_in(None, 512);
    drop(func);

    let mut usb_dev = builder.build();
    let mut msc = AsyncMsc::new(READ_ONLY);

    let usb_and_msc = join(
        usb_dev.run(),
        msc.run(&mut ep_out, &mut ep_in, sd_mutex),
    );

    let monitor = async {
        let mut was_configured = false;
        let mut was_ejected = false;
        loop {
            if SHUTDOWN_REQ.load(Ordering::Acquire) {
                break;
            }
            let cfg_now = is_usb_configured() && !usb::is_suspended();
            let ej_now = is_media_ejected();
            if cfg_now && (!was_configured || (was_ejected && !ej_now)) {
                was_configured = true;
                was_ejected = ej_now;
                let line = if ej_now {
                    *b"USB: EJECTED    "
                } else {
                    fmt_sd_usb_status(mb, usb::is_high_speed())
                };
                let _ = DISPLAY_CH.try_send(DisplayCmd::WriteLine {
                    row: 1,
                    len: COLS,
                    buf: line,
                });
            } else if cfg_now && ej_now && !was_ejected {
                was_ejected = true;
                let _ = DISPLAY_CH.try_send(DisplayCmd::WriteLine {
                    row: 1,
                    len: COLS,
                    buf: *b"USB: EJECTED    ",
                });
            } else if !cfg_now && was_configured {
                was_configured = false;
                was_ejected = ej_now;
                let _ = DISPLAY_CH.try_send(DisplayCmd::WriteLine {
                    row: 1,
                    len: COLS,
                    buf: *b"USB: DETACHED   ",
                });
            }
            Timer::after_millis(20).await;
        }
    };

    match select(usb_and_msc, monitor).await {
        Either::First(_) | Either::Second(_) => {
            set_storage_mode(StorageMode::Firmware);
            {
                let mut sd_guard = sd_mutex.lock().await;
                sd_guard.abort_dma();
                let _ = sd_guard.flush();
            }
            usb::force_disconnect();
            STORAGE_QUIESCED.store(true, Ordering::Release);
        }
    }
}

fn fmt_sd_usb_status(mb: u32, hs: bool) -> [u8; COLS] {
    let mut line = *b"SD:00000MB USBFS";
    let mut n = mb.min(99_999);
    for i in (3..8).rev() {
        line[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    if hs {
        line[14] = b'H';
    }
    line
}

fn fmt_sd_status(mb: u32, sig: u16) -> [u8; COLS] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut line = *b"SD:00000MB 0000 ";
    let mut n = mb.min(99_999);
    for i in (3..8).rev() {
        line[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    line[11] = HEX[((sig >> 12) & 0xF) as usize];
    line[12] = HEX[((sig >> 8) & 0xF) as usize];
    line[13] = HEX[((sig >> 4) & 0xF) as usize];
    line[14] = HEX[(sig & 0xF) as usize];
    line
}

fn fmt_sd_err(err: SdError) -> [u8; COLS] {
    match err {
        SdError::NoCard => *b"SD: NO CARD     ",
        SdError::Timeout => *b"SD: TIMEOUT     ",
        SdError::NotMounted => *b"SD: NOT MOUNTED ",
        SdError::InitFailed(_, _) => *b"SD: INIT ERR    ",
        SdError::CmdError(_) => *b"SD: CMD ERR     ",
    }
}

fn enter_usb_storage_mode(mut p: Peripherals) -> ! {
    p.lcd.write_line(0, b"  STORAGE MODE  ");
    let mounted_mb = match p.sd.mount() {
        Ok(info) => {
            let mut mbr = [0u8; 512];
            match p.sd.read_block(0, &mut mbr) {
                Ok(()) => {
                    let mb = (info.block_count / 2048) as u32;
                    let sig = u16::from_be_bytes([mbr[510], mbr[511]]);
                    let line = fmt_sd_status(mb, sig);
                    p.lcd.write_line(1, &line);
                    Some(mb)
                }
                Err(e) => {
                    let line = fmt_sd_err(e);
                    p.lcd.write_line(1, &line);
                    None
                }
            }
        }
        Err(e) => {
            let line = fmt_sd_err(e);
            p.lcd.write_line(1, &line);
            None
        }
    };
    p.lcd.flush();

    let executor = EXECUTOR.init(Executor::new());
    let lcd = p.lcd;
    let power = p.power;
    let sd = p.sd;
    let usb = p.usb;
    executor.run(|spawner| {
        spawner.spawn(display_task(lcd).unwrap());
        spawner.spawn(power_monitor_task(power).unwrap());
        if let Some(mb) = mounted_mb {
            spawner.spawn(usb_msc_task(usb, sd, mb).unwrap());
        }
    });
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub unsafe extern "C" fn Default_Handler() {
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub unsafe extern "C" fn HardFault_Handler() {
    loop {
        core::hint::spin_loop();
    }
}
