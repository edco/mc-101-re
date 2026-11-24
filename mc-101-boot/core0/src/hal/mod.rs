//! Core 0 Hardware Abstraction Layer (HAL) for `mc-101-boot`.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::clocks::Clocks;
use crate::display::LcdCh7;
use crate::dmac::Dmac0;
use crate::panel::PanelCh4;
use crate::power::PowerCtl;
use crate::sd::SdCard;
use crate::usb::UsbDevice;

pub trait TextDisplay {
    fn write_line(&mut self, row: usize, text: &[u8]);
    fn flush(&mut self);
    fn power_off(&mut self);
}

#[allow(dead_code)]
pub trait BlockDevice {
    type Error;
    fn is_card_present(&self) -> bool;
    fn block_count(&self) -> u64;
    fn read_block(&mut self, lba: u32, buf: &mut [u8; 512]) -> Result<(), Self::Error>;
    fn write_block(&mut self, lba: u32, buf: &[u8; 512]) -> Result<(), Self::Error>;
    fn flush(&mut self) -> Result<(), Self::Error>;

    fn read_blocks<F>(&mut self, lba: u32, count: u32, mut on_block: F) -> Result<u32, Self::Error>
    where
        F: FnMut(&[u8; 512]) -> bool,
    {
        let mut buf = [0u8; 512];
        for i in 0..count {
            self.read_block(lba + i, &mut buf)?;
            if !on_block(&buf) {
                return Ok(i);
            }
        }
        Ok(count)
    }

    fn write_blocks<F>(&mut self, lba: u32, count: u32, mut get_block: F) -> Result<u32, Self::Error>
    where
        F: FnMut(&mut [u8; 512]) -> bool,
    {
        let mut buf = [0u8; 512];
        for i in 0..count {
            if !get_block(&mut buf) {
                return Ok(i);
            }
            self.write_block(lba + i, &buf)?;
        }
        Ok(count)
    }
}

#[allow(dead_code)]
pub struct Peripherals {
    pub clocks: Clocks,
    pub lcd: LcdCh7,
    pub panel: PanelCh4,
    pub power: PowerCtl,
    pub sd: SdCard,
    pub usb: UsbDevice,
    pub dmac0: Dmac0,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

impl Peripherals {
    pub fn take() -> Option<Self> {
        if TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(Self {
                clocks: Clocks::new(),
                lcd: LcdCh7::new(),
                panel: PanelCh4::new(),
                power: PowerCtl::new(),
                sd: SdCard::new(),
                usb: UsbDevice::new(),
                dmac0: Dmac0::new(),
            })
        } else {
            None
        }
    }
}

struct SingleCoreCriticalSection;
critical_section::set_impl!(SingleCoreCriticalSection);

unsafe impl critical_section::Impl for SingleCoreCriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        let primask: u32;
        asm!("mrs {}, primask", out(reg) primask, options(nomem, nostack, preserves_flags));
        asm!("cpsid i", options(nomem, nostack, preserves_flags));
        (primask & 1) == 0
    }

    unsafe fn release(was_enabled: critical_section::RawRestoreState) {
        if was_enabled {
            asm!("cpsie i", options(nomem, nostack, preserves_flags));
        }
    }
}
