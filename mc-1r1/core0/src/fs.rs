//! Core 0 FAT16/FAT32 Filesystem Layer (`embedded-sdmmc`) over SDHI Companion DMA.
//!
//! Arbitrates SD card access with USB Mass Storage Mode via `msc::storage_mode()`
//! (`StorageMode::Firmware` vs. `StorageMode::UsbMsc`) and provides:
//! - [`Mc1r1TimeSource`]: monotonic FAT timestamp generator backed by `time::now_ms()`.
//! - [`ArbitratedSd`]: `embedded_sdmmc::BlockDevice` wrapper around `&SdCard` / `&mut SdCard`
//!   that refuses I/O while `StorageMode::UsbMsc` owns the SD card.
//! - [`FatVolumeSummary`] & [`probe_fat_volume`]: mounts Partition 0 (`VolumeIdx(0)`),
//!   scans the root directory (and `/ROLAND` if present), and returns entry statistics.
//! - [`read_root_file`] & [`write_root_file`]: convenience helpers for project/asset I/O.

#![allow(dead_code)]

use core::ops::ControlFlow;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice as SdmmcBlockDevice, BlockIdx, Mode, TimeSource, Timestamp,
    VolumeIdx, VolumeManager,
};

use crate::msc::{storage_mode, StorageMode};
use crate::sd::{SdCard, SdError};
use crate::time;

/// Monotonic `embedded-sdmmc` [`TimeSource`] derived from Core 0's 1 kHz SysTick clock
/// ( anchored at `2026-01-01 00:00:00` since the MC-101 has no battery-backed RTC).
#[derive(Clone, Copy, Debug, Default)]
pub struct Mc1r1TimeSource;

impl TimeSource for Mc1r1TimeSource {
    fn get_timestamp(&self) -> Timestamp {
        let total_secs = time::now_ms() / 1_000;
        let seconds = (total_secs % 60) as u8;
        let minutes = ((total_secs / 60) % 60) as u8;
        let hours = ((total_secs / 3_600) % 24) as u8;
        let days = ((total_secs / 86_400) % 28) as u8;
        Timestamp {
            year_since_1970: 56, // 2026 - 1970
            zero_indexed_month: 0,
            zero_indexed_day: days,
            hours,
            minutes,
            seconds,
        }
    }
}

/// Arbitrated `embedded_sdmmc::BlockDevice` borrower that enforces `StorageMode::Firmware`.
pub struct ArbitratedSd<'a>(pub &'a SdCard);

impl<'a> SdmmcBlockDevice for ArbitratedSd<'a> {
    type Error = SdError;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        if storage_mode() != StorageMode::Firmware {
            return Err(SdError::NotMounted);
        }
        self.0.read(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        if storage_mode() != StorageMode::Firmware {
            return Err(SdError::NotMounted);
        }
        self.0.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        if storage_mode() != StorageMode::Firmware {
            return Err(SdError::NotMounted);
        }
        self.0.num_blocks()
    }
}

/// Summary of the mounted FAT16/FAT32 volume on Partition 0 (`VolumeIdx(0)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FatVolumeSummary {
    /// Total capacity in MiB.
    pub capacity_mb: u32,
    /// Number of regular files in the root directory (`/`).
    pub root_files: u16,
    /// Number of subdirectories in the root directory (`/`).
    pub root_dirs: u16,
    /// Whether the standard `/ROLAND` directory exists in the root directory.
    pub has_roland_dir: bool,
    /// Number of entries inside `/ROLAND` (if present).
    pub roland_entries: u16,
}

/// High-level filesystem error combining SDHI hardware errors and `embedded-sdmmc` FAT errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    Sd(SdError),
    BusyUsbMsc,
    VolumeOpen,
    RootDirOpen,
    DirIter,
    FileOpen,
    FileIo,
}

/// Mount the SD card (if not already mounted), open FAT Partition 0 (`VolumeIdx(0)`) via
/// `embedded-sdmmc`, and scan the root directory (plus `/ROLAND` if present).
pub fn probe_fat_volume(sd: &mut SdCard) -> Result<FatVolumeSummary, FsError> {
    if storage_mode() != StorageMode::Firmware {
        return Err(FsError::BusyUsbMsc);
    }
    let info = match sd.info() {
        Some(i) => i,
        None => sd.mount().map_err(FsError::Sd)?,
    };
    let capacity_mb = (info.block_count / 2048) as u32;

    let vol_mgr = VolumeManager::new(ArbitratedSd(sd), Mc1r1TimeSource);
    let volume = vol_mgr
        .open_volume(VolumeIdx(0))
        .map_err(|_| FsError::VolumeOpen)?;
    let root_dir = volume.open_root_dir().map_err(|_| FsError::RootDirOpen)?;

    let mut root_files = 0u16;
    let mut root_dirs = 0u16;
    let mut has_roland_dir = false;

    root_dir
        .iterate_dir(|entry| {
            if entry.attributes.is_volume() || entry.attributes.is_hidden() {
                return ControlFlow::Continue(());
            }
            if entry.attributes.is_directory() {
                root_dirs = root_dirs.saturating_add(1);
                if entry.name.base_name() == b"ROLAND" && entry.name.extension() == b"" {
                    has_roland_dir = true;
                }
            } else {
                root_files = root_files.saturating_add(1);
            }
            ControlFlow::Continue(())
        })
        .map_err(|_| FsError::DirIter)?;

    let mut roland_entries = 0u16;
    if has_roland_dir {
        if let Ok(roland_dir) = root_dir.open_dir("ROLAND") {
            let _ = roland_dir.iterate_dir(|entry| {
                let base = entry.name.base_name();
                if base != b"." && base != b".." && !entry.attributes.is_volume() {
                    roland_entries = roland_entries.saturating_add(1);
                }
                ControlFlow::Continue(())
            });
        }
    }

    Ok(FatVolumeSummary {
        capacity_mb,
        root_files,
        root_dirs,
        has_roland_dir,
        roland_entries,
    })
}

/// Read up to `buf.len()` bytes from an 8.3 file `name` in the root directory of Volume 0.
pub fn read_root_file(sd: &SdCard, name: &str, buf: &mut [u8]) -> Result<usize, FsError> {
    if storage_mode() != StorageMode::Firmware {
        return Err(FsError::BusyUsbMsc);
    }
    let vol_mgr = VolumeManager::new(ArbitratedSd(sd), Mc1r1TimeSource);
    let volume = vol_mgr
        .open_volume(VolumeIdx(0))
        .map_err(|_| FsError::VolumeOpen)?;
    let root_dir = volume.open_root_dir().map_err(|_| FsError::RootDirOpen)?;
    let file = root_dir
        .open_file_in_dir(name, Mode::ReadOnly)
        .map_err(|_| FsError::FileOpen)?;

    let mut total = 0usize;
    while total < buf.len() && !file.is_eof() {
        let n = file.read(&mut buf[total..]).map_err(|_| FsError::FileIo)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// Create or overwrite an 8.3 file `name` in the root directory of Volume 0 and flush it to disk.
pub fn write_root_file(sd: &SdCard, name: &str, data: &[u8]) -> Result<(), FsError> {
    if storage_mode() != StorageMode::Firmware {
        return Err(FsError::BusyUsbMsc);
    }
    let vol_mgr = VolumeManager::new(ArbitratedSd(sd), Mc1r1TimeSource);
    let volume = vol_mgr
        .open_volume(VolumeIdx(0))
        .map_err(|_| FsError::VolumeOpen)?;
    let root_dir = volume.open_root_dir().map_err(|_| FsError::RootDirOpen)?;
    let file = root_dir
        .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| FsError::FileOpen)?;
    file.write(data).map_err(|_| FsError::FileIo)?;
    file.flush().map_err(|_| FsError::FileIo)?;
    Ok(())
}
