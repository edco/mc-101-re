//! Bootloader FAT16/FAT32 Filesystem Helper (`embedded-sdmmc`) over SDHI DMA.

#![allow(dead_code)]

use core::ops::ControlFlow;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice as SdmmcBlockDevice, BlockIdx, Directory, Error as SdmmcError,
    LfnBuffer, Mode, ShortFileName, TimeSource, Timestamp, Volume, VolumeIdx, VolumeManager,
};

use crate::sd::{SdCard, SdError};
use crate::time;

pub struct SdRef<'a>(pub &'a SdCard);

impl<'a> SdmmcBlockDevice for SdRef<'a> {
    type Error = SdError;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        self.0.read(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        self.0.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.0.num_blocks()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BootTimeSource;

impl TimeSource for BootTimeSource {
    fn get_timestamp(&self) -> Timestamp {
        let total_secs = time::now_ms() / 1_000;
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 0,
            zero_indexed_day: ((total_secs / 86_400) % 28) as u8,
            hours: ((total_secs / 3_600) % 24) as u8,
            minutes: ((total_secs / 60) % 60) as u8,
            seconds: (total_secs % 60) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    Sd(SdError),
    VolumeOpen(u8),
    DirOpen,
    FileOpen,
    FileRead,
    TooLarge,
    BadPath,
}

fn classify_sdmmc_err(e: &SdmmcError<SdError>) -> u8 {
    match e {
        SdmmcError::DeviceError(SdError::Timeout) => b'T',
        SdmmcError::DeviceError(SdError::CmdError(_)) => b'C',
        SdmmcError::DeviceError(SdError::NoCard) => b'N',
        SdmmcError::DeviceError(_) => b'D',
        SdmmcError::FormatError(_) => b'F',
        SdmmcError::NoSuchVolume => b'V',
        _ => b'?',
    }
}

/// Mounted FAT volume handle reused across all bootloader file reads.
pub struct BootFs<'a> {
    volume: Volume<'a, SdRef<'a>, BootTimeSource, 4, 4, 1>,
}

impl<'a> BootFs<'a> {
    pub fn open(
        vol_mgr: &'a VolumeManager<SdRef<'a>, BootTimeSource, 4, 4, 1>,
    ) -> Result<Self, FsError> {
        let volume = vol_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|e| FsError::VolumeOpen(classify_sdmmc_err(&e)))?;
        Ok(Self { volume })
    }

    pub fn read_file_into(&self, rel_path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
        let mut total = 0usize;
        let mut overflow = false;
        self.stream_file(rel_path, |chunk| {
            let end = total + chunk.len();
            if end > buf.len() {
                overflow = true;
                return false;
            }
            buf[total..end].copy_from_slice(chunk);
            total = end;
            true
        })?;
        if overflow {
            return Err(FsError::TooLarge);
        }
        Ok(total)
    }

    pub fn stream_file<F>(&self, rel_path: &str, mut on_chunk: F) -> Result<usize, FsError>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let trimmed = rel_path.trim().trim_start_matches('/');
        if trimmed.is_empty() {
            return Err(FsError::BadPath);
        }

        let mut dir = self
            .volume
            .open_root_dir()
            .map_err(|_| FsError::DirOpen)?;

        let mut parts = trimmed.split('/').filter(|p| !p.is_empty()).peekable();
        let mut file_name = None;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                file_name = Some(part);
            } else {
                dir = match dir.open_dir(part) {
                    Ok(d) => d,
                    Err(_) => {
                        let sfn = find_entry_lfn(&dir, part, true).ok_or(FsError::DirOpen)?;
                        dir.open_dir(&sfn).map_err(|_| FsError::DirOpen)?
                    }
                };
            }
        }
        let Some(name) = file_name else {
            return Err(FsError::BadPath);
        };

        let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
            Ok(f) => f,
            Err(_) => {
                let sfn = find_entry_lfn(&dir, name, false).ok_or(FsError::FileOpen)?;
                dir.open_file_in_dir(&sfn, Mode::ReadOnly)
                    .map_err(|_| FsError::FileOpen)?
            }
        };
        let mut chunk_buf = [0u8; 4096];
        let mut total = 0usize;
        loop {
            let n = file.read(&mut chunk_buf).map_err(|_| FsError::FileRead)?;
            if n == 0 {
                break;
            }
            total += n;
            if !on_chunk(&chunk_buf[..n]) {
                break;
            }
        }
        Ok(total)
    }
}

fn find_entry_lfn(
    dir: &Directory<'_, SdRef<'_>, BootTimeSource, 4, 4, 1>,
    target: &str,
    want_dir: bool,
) -> Option<ShortFileName> {
    let mut lfn_storage = [0u8; 128];
    let mut lfn_buf = LfnBuffer::new(&mut lfn_storage);
    let mut found: Option<ShortFileName> = None;
    let _ = dir.iterate_dir_lfn(&mut lfn_buf, |entry, lfn| {
        if entry.attributes.is_directory() != want_dir {
            return ControlFlow::Continue(());
        }
        if let Some(long_name) = lfn {
            if long_name.eq_ignore_ascii_case(target) {
                found = Some(entry.name.clone());
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });
    found
}
