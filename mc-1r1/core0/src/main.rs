//! MC-101 Core 0 ("mc1r1-core0") — Bare-Metal CRT0 + Safe HAL + Embassy Runtime.
//!
//! Matches the stock `RPG69_C0A_up.bin` (`App1_Main`) entry contract:
//! - Loaded to SDRAM `0x6000_0000`, entered in Thumb mode at `0x6000_0001`.
//! - Relocates vector table + ITCM code to `0x0100_0000`.
//! - Sets `MSP = 0x0105_0800` and `SCB->VTOR = 0x0100_0000`.
//! - Takes exclusive peripheral ownership via `hal::Peripherals::take()`,
//!   starts the 1 ms SysTick `embassy-time` driver, and runs async tasks on
//!   `embassy_executor::Executor` in 100% safe Rust.

#![no_std]
#![no_main]

mod clocks;
mod crc32;
mod display;
mod dmac;
mod fs;
mod hal;
mod loader;
mod msc;
mod panel;
mod power;
mod sd;
mod time;
mod usb;

use core::arch::asm;
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
use mc1r1_shared::ipc::{HelloMailbox, MAX_MSG_LEN};
use mc1r1_shared::memmap::{
    enable_fpu, CORE0_INITIAL_MSP, CORE0_ITCM_BASE, CORE1_PRELOADED_MAGIC,
    CORE1_PRELOADED_MAGIC_ADDR, SCB_VTOR,
};
use static_cell::StaticCell;

use display::{LcdCh7, COLS};
use fs::{probe_fat_volume, FatVolumeSummary, FsError};
use hal::{BlockDevice, Peripherals, TextDisplay};
use msc::{
    is_media_ejected, is_usb_configured, set_storage_mode, AsyncMsc, MscControlHandler, SharedSd,
    StorageMode,
};
use panel::{button, button_name, PanelCh4, PanelEvent};
use power::PowerCtl;
use sd::{SdCard, SdError};
use usb::UsbDevice;

extern "C" {
    static __sitcm: u32;
    static __eitcm: u32;
    static __siitcm: u32;
    static mut __sbss: u32;
    static mut __ebss: u32;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union Vector {
    handler: unsafe extern "C" fn(),
    reserved: u32,
}

#[repr(C, align(512))]
pub struct AlignedVectorTable([Vector; 128]);

const fn build_vector_table() -> AlignedVectorTable {
    let mut t = [Vector {
        handler: Default_Handler,
    }; 128];
    t[0] = Vector {
        reserved: CORE0_INITIAL_MSP,
    };
    t[1] = Vector {
        handler: Reset_Entry,
    };
    t[2] = Vector {
        handler: Default_Handler,
    };
    t[3] = Vector {
        handler: HardFault_Handler,
    };
    t[4] = Vector {
        handler: Default_Handler,
    };
    t[5] = Vector {
        handler: Default_Handler,
    };
    t[6] = Vector {
        handler: Default_Handler,
    };
    t[7] = Vector { reserved: 0 };
    t[8] = Vector { reserved: 0 };
    t[9] = Vector { reserved: 0 };
    t[10] = Vector { reserved: 0 };
    t[11] = Vector {
        handler: Default_Handler,
    };
    t[12] = Vector {
        handler: Default_Handler,
    };
    t[13] = Vector { reserved: 0 };
    t[14] = Vector {
        handler: Default_Handler,
    };
    t[15] = Vector {
        handler: time::SysTick_Handler,
    };
    // External NVIC IRQs (index = 16 + irq_num):
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

/// Core 0 Cortex-M4 Vector Table (128 entries, 512-byte VTOR aligned, relocated to `0x0100_0000`).
#[link_section = ".vector_table"]
#[no_mangle]
pub static VECTOR_TABLE: AlignedVectorTable = build_vector_table();

#[no_mangle]
pub unsafe extern "C" fn Reset_Entry() {
    Reset_Handler()
}

/// Entry point at `0x6000_0000` (jumped to at `0x6000_0001` by App0/BootROM).
#[link_section = ".text.entry"]
#[no_mangle]
pub unsafe extern "C" fn Reset_Handler() -> ! {
    // 1. Copy vector table + ITCM code from SDRAM load image to 0x0100_0000.
    let mut src = &__siitcm as *const u32;
    let mut dst = &__sitcm as *const u32 as *mut u32;
    let end = &__eitcm as *const u32 as *mut u32;
    while dst < end {
        core::ptr::write_volatile(dst, core::ptr::read_volatile(src));
        src = src.add(1);
        dst = dst.add(1);
    }

    // 2. Zero-initialize .bss in SDRAM.
    let mut bss = &raw mut __sbss;
    let bss_end = &raw mut __ebss;
    while bss < bss_end {
        core::ptr::write_volatile(bss, 0);
        bss = bss.add(1);
    }

    // 3. Enable FPU (CP10/CP11 + lazy stacking) before any hard-float code.
    enable_fpu();

    // 4. Set MSP = 0x0105_0800 and VTOR = 0x0100_0000.
    core::ptr::write_volatile(SCB_VTOR as *mut u32, CORE0_ITCM_BASE as u32);
    asm!(
        "dsb sy",
        "isb",
        "msr msp, {msp}",
        "isb",
        msp = in(reg) CORE0_INITIAL_MSP,
        options(nostack)
    );

    kernel_main()
}

// -----------------------------------------------------------------------------
// Safe Application & Async Embassy Tasks
// -----------------------------------------------------------------------------

enum DisplayCmd {
    WriteLine { row: usize, len: usize, buf: [u8; COLS] },
    Shutdown(PowerCtl),
}

static DISPLAY_CH: Channel<CriticalSectionRawMutex, DisplayCmd, 4> = Channel::new();
static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static SHUTDOWN_REQ: AtomicBool = AtomicBool::new(false);
static STORAGE_QUIESCED: AtomicBool = AtomicBool::new(true);

/// Executed from relocated Core 0 ITCM (`0x0100_0040+`).
#[link_section = ".itcm"]
#[inline(never)]
#[no_mangle]
fn itcm_show_banner(lcd: &mut LcdCh7) {
    lcd.init();
    lcd.write_line(0, b"Hello, MC-1R1!");
    lcd.flush();
}

#[embassy_executor::task]
async fn display_task(mut lcd: LcdCh7) {
    loop {
        match DISPLAY_CH.receive().await {
            DisplayCmd::WriteLine { row, len, buf } => {
                lcd.write_line(row, &buf[..len]);
                lcd.flush();
            }
            DisplayCmd::Shutdown(mut pwr) => {
                SHUTDOWN_REQ.store(true, Ordering::Release);
                // Allow usb_msc_task to flush SD and detach USB before cutting power.
                let deadline = time::now_ms().wrapping_add(1000);
                while !STORAGE_QUIESCED.load(Ordering::Acquire) && time::now_ms() < deadline {
                    Timer::after_millis(5).await;
                }
                pwr.safe_shutdown(&mut lcd, || {});
            }
        }
    }
}

#[embassy_executor::task]
async fn core1_ipc_task() {
    let mut rx_buf = [0u8; MAX_MSG_LEN];
    loop {
        // SAFETY: HelloMailbox is at fixed shared SRAM 0x0114_0800 and accessed
        // exclusively by this task on Core 0 via atomic acquire/release protocol.
        let msg_len = unsafe { HelloMailbox::get().poll_core0(&mut rx_buf) };
        if let Some(len) = msg_len {
            let mut line = [b' '; COLS];
            let n = len.min(COLS);
            line[..n].copy_from_slice(&rx_buf[..n]);
            DISPLAY_CH
                .send(DisplayCmd::WriteLine {
                    row: 1,
                    len: n,
                    buf: line,
                })
                .await;
        }
        Timer::after_millis(5).await;
    }
}

#[embassy_executor::task]
async fn power_monitor_task(mut pwr: PowerCtl) {
    let mut ticker = Ticker::every(Duration::from_millis(power::POLL_INTERVAL_MS));
    loop {
        ticker.next().await;
        if pwr.poll_tick() {
            DISPLAY_CH.send(DisplayCmd::Shutdown(pwr)).await;
            return;
        }
    }
}

fn fmt_panel_event(ev: PanelEvent) -> [u8; COLS] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let (tag, id) = match ev {
        PanelEvent::Press(id) => (b"DN", id),
        PanelEvent::Release(id) => (b"UP", id),
    };
    let mut line = *b"BTN XX: 00      ";
    line[4] = tag[0];
    line[5] = tag[1];
    line[8] = HEX[((id >> 4) & 0xF) as usize];
    line[9] = HEX[(id & 0xF) as usize];
    let name = button_name(id);
    let n = name.len().min(6);
    line[10..10 + n].copy_from_slice(&name[..n]);
    line
}

#[embassy_executor::task]
async fn panel_task(mut panel: PanelCh4) {
    let mut ticker = Ticker::every(Duration::from_millis(2));
    loop {
        ticker.next().await;
        while let Some(ev) = panel.poll_event() {
            let line = fmt_panel_event(ev);
            DISPLAY_CH
                .send(DisplayCmd::WriteLine {
                    row: 1,
                    len: COLS,
                    buf: line,
                })
                .await;
        }
    }
}

fn fmt_sd_status(mb: u32, sig: u16) -> [u8; COLS] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut line = *b"SD 00000MB XXXX ";
    let mut n = mb;
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
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    match err {
        SdError::NoCard => *b" < No SD Card > ",
        SdError::Timeout => *b"SD ERR: TIMEOUT ",
        SdError::NotMounted => *b"SD ERR: UNMOUNT ",
        SdError::InitFailed(cmd, code) => {
            let mut l = *b"SD I00 E:XXXX   ";
            l[4] = HEX[((cmd >> 4) & 0xF) as usize];
            l[5] = HEX[(cmd & 0xF) as usize];
            l[9] = HEX[((code >> 12) & 0xF) as usize];
            l[10] = HEX[((code >> 8) & 0xF) as usize];
            l[11] = HEX[((code >> 4) & 0xF) as usize];
            l[12] = HEX[(code & 0xF) as usize];
            l
        }
        SdError::CmdError(bits) => {
            let mut l = *b"SD CMD ERR XXXX ";
            l[11] = HEX[((bits >> 12) & 0xF) as usize];
            l[12] = HEX[((bits >> 8) & 0xF) as usize];
            l[13] = HEX[((bits >> 4) & 0xF) as usize];
            l[14] = HEX[(bits & 0xF) as usize];
            l
        }
    }
}

fn fmt_sd_usb_status(mb: u32, high_speed: bool, read_only: bool) -> [u8; COLS] {
    let mut line = *b"SD 00000MB HS RW";
    let mut n = mb;
    for i in (3..8).rev() {
        line[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    if !high_speed {
        line[11] = b'F';
    }
    if read_only {
        line[15] = b'O';
    }
    line
}

fn fmt_fat_summary(sum: FatVolumeSummary) -> [u8; COLS] {
    // Format: "FAT32 F00 D00 R+" (or "R-" if /ROLAND directory is absent).
    let mut line = *b"FAT32 F00 D00 R-";
    let f = sum.root_files.min(99);
    line[7] = b'0' + (f / 10) as u8;
    line[8] = b'0' + (f % 10) as u8;
    let d = sum.root_dirs.min(99);
    line[11] = b'0' + (d / 10) as u8;
    line[12] = b'0' + (d % 10) as u8;
    if sum.has_roland_dir {
        line[15] = b'+';
    }
    line
}

fn fmt_fat_err(err: FsError) -> [u8; COLS] {
    match err {
        FsError::Sd(e) => fmt_sd_err(e),
        FsError::BusyUsbMsc => *b"FAT32: USB BUSY ",
        FsError::VolumeOpen => *b"FAT32: VOL ERR  ",
        FsError::RootDirOpen => *b"FAT32: ROOT ERR ",
        FsError::DirIter => *b"FAT32: DIR ERR  ",
        FsError::FileOpen => *b"FAT32: OPEN ERR ",
        FsError::FileIo => *b"FAT32: IO ERR   ",
    }
}

static SHARED_SD: StaticCell<SharedSd> = StaticCell::new();

#[embassy_executor::task]
async fn usb_msc_task(usb: UsbDevice, sd: SdCard, mb: u32) {
    const READ_ONLY: bool = false;
    STORAGE_QUIESCED.store(false, Ordering::Release);
    set_storage_mode(StorageMode::UsbMsc);

    let sd_mutex: &'static SharedSd = SHARED_SD.init(Mutex::new(sd));

    let mut cfg = Config::new(0x0582, 0x022C); // Roland MC-101
    cfg.manufacturer = Some("Roland");
    cfg.product = Some("MC-1R1");
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

    let mut func = builder.function(0x08, 0x06, 0x50); // Mass Storage / SCSI / BOT
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
                    fmt_sd_usb_status(mb, usb::is_high_speed(), READ_ONLY)
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

fn kernel_main() -> ! {
    let mut p = Peripherals::take().expect("Peripherals already taken");

    p.power.init();
    p.dmac0.reset_and_enable();
    time::init(p.clocks.sysclk_hz());
    p.clocks.init_audio_pll_44k1();

    itcm_show_banner(&mut p.lcd);

    // Initialize Sub-CPU AFTER LCD init (which blocks ~210 ms) and immediately
    // drain the Sub-CPU's initial state dump so Channel 4's RX FIFO never overruns.
    p.panel.init();
    let boot_scan_end = time::now_ms().wrapping_add(150);
    while time::now_ms() < boot_scan_end {
        while p.panel.poll_event().is_some() {}
    }

    let storage_mode = p.panel.is_held(button::PROJECT);
    let mut mounted_mb: Option<u32> = None;
    if storage_mode {
        p.lcd.write_line(0, b"  STORAGE MODE  ");
        match p.sd.mount() {
            Ok(info) => {
                let mut mbr = [0u8; 512];
                match p.sd.read_block(0, &mut mbr) {
                    Ok(()) => {
                        let mb = (info.block_count / 2048) as u32;
                        let sig = u16::from_be_bytes([mbr[510], mbr[511]]);
                        let line = fmt_sd_status(mb, sig);
                        p.lcd.write_line(1, &line);
                        mounted_mb = Some(mb);
                    }
                    Err(e) => {
                        let line = fmt_sd_err(e);
                        p.lcd.write_line(1, &line);
                    }
                }
            }
            Err(e) => {
                let line = fmt_sd_err(e);
                p.lcd.write_line(1, &line);
            }
        }
        p.lcd.flush();
    } else {
        // Normal boot: Core 0 firmware owns SD via embedded-sdmmc (StorageMode::Firmware).
        set_storage_mode(StorageMode::Firmware);
        let fat_line = match probe_fat_volume(&mut p.sd) {
            Ok(summary) => fmt_fat_summary(summary),
            Err(e) => fmt_fat_err(e),
        };
        p.lcd.write_line(0, &fat_line);
        p.lcd.flush();

        // Initialize inter-core mailbox, load Core 1 (idm1 + sdram1) unless pre-loaded
        // by mc-101-boot (`CORE1_PRELOADED_MAGIC`), and release Core 1.
        // SAFETY: Called once before core1_ipc_task starts and before releasing Core 1.
        unsafe { HelloMailbox::get().init_core0() };
        p.qspi1.init();
        let preloaded = unsafe {
            let ptr = CORE1_PRELOADED_MAGIC_ADDR as *mut u32;
            let ok = core::ptr::read_volatile(ptr) == CORE1_PRELOADED_MAGIC;
            if ok {
                core::ptr::write_volatile(ptr, 0);
            }
            ok
        };
        p.qspi1.publish_ipc_anchors();
        if preloaded || p.qspi1.load_core1() {
            p.qspi1.release_core1();
        }
    }

    let executor = EXECUTOR.init(Executor::new());
    let lcd = p.lcd;
    let panel = p.panel;
    let power = p.power;
    let sd = p.sd;
    let usb = p.usb;
    executor.run(|spawner| {
        spawner.spawn(display_task(lcd).unwrap());
        spawner.spawn(power_monitor_task(power).unwrap());
        if storage_mode {
            if let Some(mb) = mounted_mb {
                spawner.spawn(usb_msc_task(usb, sd, mb).unwrap());
            }
        } else {
            let _sd_mutex: &'static SharedSd = SHARED_SD.init(Mutex::new(sd));
            spawner.spawn(core1_ipc_task().unwrap());
            spawner.spawn(panel_task(panel).unwrap());
        }
    });
}

unsafe fn fault_banner(tag: &[u8; 16]) -> ! {
    usb::force_disconnect();
    let mut lcd = LcdCh7::new();
    lcd.write_line(0, tag);
    lcd.flush();
    let mut pwr = PowerCtl::new();
    loop {
        if pwr.poll_tick() {
            pwr.safe_shutdown(&mut lcd, || {});
        }
        for _ in 0..100_000u32 {
            core::hint::spin_loop();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn HardFault_Handler() {
    fault_banner(b"FAULT: HARDFAULT");
}

#[no_mangle]
pub unsafe extern "C" fn Default_Handler() {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let ipsr: u32;
    asm!("mrs {}, ipsr", out(reg) ipsr, options(nomem, nostack, preserves_flags));
    let mut msg = *b"FAULT: IRQ 0x00 ";
    msg[13] = HEX[((ipsr >> 4) & 0xF) as usize];
    msg[14] = HEX[(ipsr & 0xF) as usize];
    fault_banner(&msg);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    unsafe { fault_banner(b"FAULT: PANIC    ") }
}
