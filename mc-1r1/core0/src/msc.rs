//! Async USB Mass Storage Class (BOT + SCSI) over `embassy-usb` + SDHI DMA Ping-Pong.
//!
//! Features:
//! - Spec-compliant BOT 13-case STALL / `wait_unstalled()` synchronization (fixes Linux
//!   `udisks2`/Thunar initial mount delays on optional SCSI commands like `INQUIRY EVPD=1`,
//!   `REPORT_LUNS`, `READ_CAPACITY_16`, `GET_EVENT_STATUS_NOTIFICATION`).
//! - Dual 16 KiB 4-byte-aligned SDRAM ping-pong buffers overlapping `DMAC0 CH3` (SDHI)
//!   and `DMAC0 CH1` (USB 2.0 High-Speed Bulk) transfers via `embassy_futures::join::join`.
//! - Runtime SD ownership arbitration (`StorageMode::Firmware` vs `StorageMode::UsbMsc`)
//!   over `embassy_sync::mutex::Mutex<NoopRawMutex, SdCard>`.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_futures::join::join;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_usb::control::{InResponse, OutResponse, Recipient, Request, RequestType};
use embassy_usb::Handler;
use embassy_usb_driver::{Endpoint, EndpointError, EndpointOut};

use crate::hal::BlockDevice;
use crate::sd::SdCard;
use crate::usb::{BmcEndpointIn, BmcEndpointOut};

const CBW_SIG: u32 = 0x4342_5355; // "USBC" (LE)
const CSW_SIG: u32 = 0x5342_5355; // "USBS" (LE)

const CSW_STATUS_PASSED: u8 = 0x00;
const CSW_STATUS_FAILED: u8 = 0x01;

// SCSI opcodes
const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_START_STOP_UNIT: u8 = 0x1B;
const SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
const SCSI_READ_FORMAT_CAPACITIES: u8 = 0x23;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2A;
const SCSI_VERIFY_10: u8 = 0x2F;
const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;
const SCSI_MODE_SENSE_10: u8 = 0x5A;

// Standard 36-byte SCSI INQUIRY response
const INQUIRY_DATA: [u8; 36] = [
    0x00, // Direct-access block device
    0x80, // Removable medium
    0x02, // SPC / SCSI-2
    0x02, // Response data format
    31,   // Additional length
    0x00, 0x00, 0x00,
    // Vendor ID (8 bytes): "Roland  "
    b'R', b'o', b'l', b'a', b'n', b'd', b' ', b' ',
    // Product ID (16 bytes): "MC-1R1 SD Card  "
    b'M', b'C', b'-', b'1', b'R', b'1', b' ', b'S', b'D', b' ', b'C', b'a', b'r', b'd', b' ', b' ',
    // Product Revision (4 bytes): "0100"
    b'0', b'1', b'0', b'0',
];

/// Ping-pong DMA chunk size: 64 KiB (128 × 512-byte sectors) per buffer in SDRAM.
const CHUNK_SECTORS: u32 = 128;
const CHUNK_BYTES: usize = (CHUNK_SECTORS as usize) * 512;

#[repr(C, align(32))]
struct PingPongBufs {
    a: [u8; CHUNK_BYTES],
    b: [u8; CHUNK_BYTES],
}
static mut PING_PONG: PingPongBufs = PingPongBufs {
    a: [0u8; CHUNK_BYTES],
    b: [0u8; CHUNK_BYTES],
};

#[repr(C, align(4))]
struct CbwBuf([u8; 512]);
static mut CBW_RX_BUF: CbwBuf = CbwBuf([0u8; 512]);

// -----------------------------------------------------------------------------
// Runtime SD Ownership Arbitration (Normal Firmware Mode vs. USB MSC Mode)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StorageMode {
    /// Core 0 firmware (`embedded-sdmmc`) owns the SD card; USB MSC reports NOT READY.
    Firmware = 0,
    /// USB Mass Storage holds exclusive access to the SD card.
    UsbMsc = 1,
}

static STORAGE_MODE: AtomicU8 = AtomicU8::new(StorageMode::Firmware as u8);

pub fn set_storage_mode(mode: StorageMode) {
    STORAGE_MODE.store(mode as u8, Ordering::Release);
}

pub fn storage_mode() -> StorageMode {
    if STORAGE_MODE.load(Ordering::Acquire) == StorageMode::UsbMsc as u8 {
        StorageMode::UsbMsc
    } else {
        StorageMode::Firmware
    }
}

/// Core 0 shared SD card mutex for safe runtime arbitration between firmware FS and USB MSC.
pub type SharedSd = Mutex<NoopRawMutex, SdCard>;

// -----------------------------------------------------------------------------
// embassy-usb Control Handler for MSC Class Requests & Configured State
// -----------------------------------------------------------------------------

static BOMSR_PENDING: AtomicBool = AtomicBool::new(false);
static USB_CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn is_usb_configured() -> bool {
    USB_CONFIGURED.load(Ordering::Acquire)
}

pub struct MscControlHandler {
    max_lun: [u8; 1],
}

impl MscControlHandler {
    pub const fn new() -> Self {
        Self { max_lun: [0u8] }
    }
}

impl Handler for MscControlHandler {
    fn reset(&mut self) {
        USB_CONFIGURED.store(false, Ordering::Release);
    }

    fn configured(&mut self, configured: bool) {
        USB_CONFIGURED.store(configured, Ordering::Release);
    }

    fn control_in<'a>(&'a mut self, req: Request, _buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        if req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.request == 0xFE
            && req.length >= 1
        {
            return Some(InResponse::Accepted(&self.max_lun));
        }
        None
    }

    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        if req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface
            && req.request == 0xFF
            && req.length == 0
        {
            BOMSR_PENDING.store(true, Ordering::Release);
            return Some(OutResponse::Accepted);
        }
        None
    }
}

// -----------------------------------------------------------------------------
// Async BOT + SCSI Engine
// -----------------------------------------------------------------------------

async fn read_exact_out(ep_out: &mut BmcEndpointOut, buf: &mut [u8]) -> Result<(), EndpointError> {
    let mut got = 0;
    let mps = ep_out.negotiated_mps();
    while got < buf.len() {
        let want = (buf.len() - got).min(mps);
        let n = ep_out.read(&mut buf[got..got + want]).await?;
        got += n;
        if n < mps && got < buf.len() {
            // Host terminated transfer early with a short packet.
            return Err(EndpointError::Disabled);
        }
    }
    Ok(())
}

pub struct AsyncMsc {
    read_only: bool,
    sense_key: u8,
    asc: u8,
    ascq: u8,
}

impl AsyncMsc {
    pub const fn new(read_only: bool) -> Self {
        Self {
            read_only,
            sense_key: 0,
            asc: 0,
            ascq: 0,
        }
    }

    fn set_sense(&mut self, key: u8, asc: u8, ascq: u8) {
        self.sense_key = key;
        self.asc = asc;
        self.ascq = ascq;
    }

    fn clear_sense(&mut self) {
        self.sense_key = 0;
        self.asc = 0;
        self.ascq = 0;
    }

    /// Run the async MSC BOT loop until the endpoints are disabled or reset.
    pub async fn run(
        &mut self,
        ep_out: &mut BmcEndpointOut,
        ep_in: &mut BmcEndpointIn,
        sd_mutex: &SharedSd,
    ) {
        loop {
            ep_out.wait_enabled().await;
            ep_in.wait_enabled().await;

            // SAFETY: Single async MSC task owns CBW_RX_BUF exclusively.
            let cbw = unsafe { &mut *(&raw mut CBW_RX_BUF.0) };
            let n = match ep_out.read(cbw).await {
                Ok(n) => n,
                Err(_) => continue,
            };

            if BOMSR_PENDING.swap(false, Ordering::AcqRel) {
                self.clear_sense();
            }

            if n < 31 {
                ep_in.set_stalled(true);
                ep_out.set_stalled(true);
                ep_in.wait_unstalled().await;
                continue;
            }

            let sig = u32::from_le_bytes([cbw[0], cbw[1], cbw[2], cbw[3]]);
            if sig != CBW_SIG {
                ep_in.set_stalled(true);
                ep_out.set_stalled(true);
                ep_in.wait_unstalled().await;
                continue;
            }

            let tag = u32::from_le_bytes([cbw[4], cbw[5], cbw[6], cbw[7]]);
            let expected_len = u32::from_le_bytes([cbw[8], cbw[9], cbw[10], cbw[11]]);
            let flags = cbw[12];
            let mut cdb = [0u8; 16];
            cdb.copy_from_slice(&cbw[15..31]);

            let (status, residue, stalled_in) = self
                .handle_scsi(ep_out, ep_in, sd_mutex, &cdb, expected_len, flags)
                .await;

            // BOT 13-Case Rule: if we stalled Bulk-IN during the data phase, we MUST wait
            // until the host sends CLEAR_FEATURE(ENDPOINT_HALT) before queuing the CSW!
            if stalled_in {
                ep_in.wait_unstalled().await;
            }

            let mut csw = [0u8; 13];
            csw[0..4].copy_from_slice(&CSW_SIG.to_le_bytes());
            csw[4..8].copy_from_slice(&tag.to_le_bytes());
            csw[8..12].copy_from_slice(&residue.to_le_bytes());
            csw[12] = status;
            let _ = ep_in.write_all(&csw).await;
        }
    }

    async fn handle_scsi(
        &mut self,
        ep_out: &mut BmcEndpointOut,
        ep_in: &mut BmcEndpointIn,
        sd_mutex: &SharedSd,
        cdb: &[u8; 16],
        expected_len: u32,
        flags: u8,
    ) -> (u8, u32, bool) {
        let dir_in = (flags & 0x80) != 0;

        // Commands that do not require active media access:
        match cdb[0] {
            SCSI_INQUIRY => {
                // Reject EVPD=1 (Vital Product Data pages) cleanly with INVALID FIELD IN CDB.
                if (cdb[1] & 0x01) != 0 {
                    self.set_sense(0x05, 0x24, 0x00);
                    if expected_len > 0 && dir_in {
                        ep_in.set_stalled(true);
                        return (CSW_STATUS_FAILED, expected_len, true);
                    }
                    return (CSW_STATUS_FAILED, expected_len, false);
                }
                self.clear_sense();
                let n = (expected_len as usize).min(INQUIRY_DATA.len());
                if ep_in.write_all(&INQUIRY_DATA[..n]).await.is_ok() {
                    return (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false);
                }
                return (CSW_STATUS_FAILED, expected_len, false);
            }

            SCSI_REQUEST_SENSE => {
                let mut resp = [0u8; 18];
                resp[0] = 0x70;
                resp[2] = self.sense_key;
                resp[7] = 10;
                resp[12] = self.asc;
                resp[13] = self.ascq;
                self.clear_sense();
                let n = (expected_len as usize).min(resp.len());
                if ep_in.write_all(&resp[..n]).await.is_ok() {
                    return (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false);
                }
                return (CSW_STATUS_FAILED, expected_len, false);
            }

            _ => {}
        }

        // Runtime arbitration gate: only touch SD when StorageMode::UsbMsc is active.
        if storage_mode() != StorageMode::UsbMsc {
            self.set_sense(0x02, 0x3A, 0x00); // NOT READY - MEDIUM NOT PRESENT
            if expected_len > 0 && dir_in {
                ep_in.set_stalled(true);
                return (CSW_STATUS_FAILED, expected_len, true);
            }
            return (CSW_STATUS_FAILED, expected_len, false);
        }

        let mut sd = sd_mutex.lock().await;
        match cdb[0] {
            SCSI_TEST_UNIT_READY => {
                if sd.is_card_present() && sd.block_count() > 0 {
                    self.clear_sense();
                    (CSW_STATUS_PASSED, 0, false)
                } else {
                    self.set_sense(0x02, 0x3A, 0x00);
                    (CSW_STATUS_FAILED, 0, false)
                }
            }

            SCSI_READ_CAPACITY_10 => {
                let blocks = sd.block_count();
                if blocks == 0 {
                    self.set_sense(0x02, 0x3A, 0x00);
                    if expected_len > 0 && dir_in {
                        ep_in.set_stalled(true);
                        return (CSW_STATUS_FAILED, expected_len, true);
                    }
                    return (CSW_STATUS_FAILED, expected_len, false);
                }
                let last_lba = (blocks - 1).min(u32::MAX as u64) as u32;
                let mut resp = [0u8; 8];
                resp[0..4].copy_from_slice(&last_lba.to_be_bytes());
                resp[4..8].copy_from_slice(&512u32.to_be_bytes());
                self.clear_sense();
                let n = (expected_len as usize).min(resp.len());
                if ep_in.write_all(&resp[..n]).await.is_ok() {
                    (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false)
                } else {
                    (CSW_STATUS_FAILED, expected_len, false)
                }
            }

            SCSI_READ_FORMAT_CAPACITIES => {
                let blocks = sd.block_count().min(u32::MAX as u64) as u32;
                let mut resp = [0u8; 12];
                resp[3] = 8;
                resp[4..8].copy_from_slice(&blocks.to_be_bytes());
                resp[8] = 0x02;
                resp[10] = 0x02; // 512 bytes/block
                self.clear_sense();
                let n = (expected_len as usize).min(resp.len());
                if ep_in.write_all(&resp[..n]).await.is_ok() {
                    (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false)
                } else {
                    (CSW_STATUS_FAILED, expected_len, false)
                }
            }

            SCSI_MODE_SENSE_6 => {
                let wp = if self.read_only { 0x80 } else { 0x00 };
                let resp = [3u8, 0x00, wp, 0x00];
                self.clear_sense();
                let n = (expected_len as usize).min(resp.len());
                if ep_in.write_all(&resp[..n]).await.is_ok() {
                    (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false)
                } else {
                    (CSW_STATUS_FAILED, expected_len, false)
                }
            }

            SCSI_MODE_SENSE_10 => {
                let wp = if self.read_only { 0x80 } else { 0x00 };
                let resp = [0x00, 0x06, 0x00, wp, 0x00, 0x00, 0x00, 0x00];
                self.clear_sense();
                let n = (expected_len as usize).min(resp.len());
                if ep_in.write_all(&resp[..n]).await.is_ok() {
                    (CSW_STATUS_PASSED, expected_len.saturating_sub(n as u32), false)
                } else {
                    (CSW_STATUS_FAILED, expected_len, false)
                }
            }

            SCSI_READ_10 => {
                let start_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                let total_blocks = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
                if total_blocks == 0 {
                    self.clear_sense();
                    return (CSW_STATUS_PASSED, expected_len, false);
                }

                // SAFETY: Single async MSC task owns PING_PONG exclusively.
                let (buf_a, buf_b) = unsafe {
                    (
                        &mut *(&raw mut PING_PONG.a),
                        &mut *(&raw mut PING_PONG.b),
                    )
                };

                let mut lba = start_lba;
                let mut rem = total_blocks;
                let mut sent = 0u32;

                // Prime first DMA chunk from SD into buf_a.
                let first_blocks = rem.min(CHUNK_SECTORS);
                let first_bytes = (first_blocks as usize) * 512;
                if sd.read_blocks_dma(lba, &mut buf_a[..first_bytes]).await.is_err() {
                    self.set_sense(0x03, 0x11, 0x00);
                    ep_in.set_stalled(true);
                    return (CSW_STATUS_FAILED, expected_len, true);
                }
                lba += first_blocks;
                rem -= first_blocks;

                let mut cur_is_a = true;
                let mut cur_bytes = first_bytes;

                // Ping-pong: start SDHI DMA read of next chunk first, then stream current chunk over USB Bulk IN.
                while rem > 0 {
                    let next_blocks = rem.min(CHUNK_SECTORS);
                    let next_bytes = (next_blocks as usize) * 512;
                    let (sd_res, usb_res) = if cur_is_a {
                        join(
                            sd.read_blocks_dma(lba, &mut buf_b[..next_bytes]),
                            ep_in.write_all(&buf_a[..cur_bytes]),
                        )
                        .await
                    } else {
                        join(
                            sd.read_blocks_dma(lba, &mut buf_a[..next_bytes]),
                            ep_in.write_all(&buf_b[..cur_bytes]),
                        )
                        .await
                    };

                    if usb_res.is_err() {
                        return (CSW_STATUS_FAILED, expected_len.saturating_sub(sent), false);
                    }
                    sent += cur_bytes as u32;
                    if sd_res.is_err() {
                        self.set_sense(0x03, 0x11, 0x00);
                        ep_in.set_stalled(true);
                        return (CSW_STATUS_FAILED, expected_len.saturating_sub(sent), true);
                    }

                    lba += next_blocks;
                    rem -= next_blocks;
                    cur_is_a = !cur_is_a;
                    cur_bytes = next_bytes;
                }

                // Flush final chunk to USB Bulk IN.
                let last_slice = if cur_is_a {
                    &buf_a[..cur_bytes]
                } else {
                    &buf_b[..cur_bytes]
                };
                if ep_in.write_all(last_slice).await.is_err() {
                    return (CSW_STATUS_FAILED, expected_len.saturating_sub(sent), false);
                }
                sent += cur_bytes as u32;
                self.clear_sense();
                (CSW_STATUS_PASSED, expected_len.saturating_sub(sent), false)
            }

            SCSI_WRITE_10 => {
                let start_lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                let total_blocks = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;

                if self.read_only {
                    self.set_sense(0x07, 0x27, 0x00); // DATA PROTECT
                    ep_out.set_stalled(true);
                    return (CSW_STATUS_FAILED, expected_len, false);
                }
                if total_blocks == 0 {
                    self.clear_sense();
                    return (CSW_STATUS_PASSED, expected_len, false);
                }

                let (buf_a, buf_b) = unsafe {
                    (
                        &mut *(&raw mut PING_PONG.a),
                        &mut *(&raw mut PING_PONG.b),
                    )
                };

                let mut lba = start_lba;
                let mut rem = total_blocks;
                let mut recvd = 0u32;

                // Prime first chunk from USB Bulk OUT into buf_a.
                let first_blocks = rem.min(CHUNK_SECTORS);
                let first_bytes = (first_blocks as usize) * 512;
                if read_exact_out(ep_out, &mut buf_a[..first_bytes]).await.is_err() {
                    self.set_sense(0x04, 0x00, 0x00);
                    return (CSW_STATUS_FAILED, expected_len, false);
                }
                recvd += first_bytes as u32;
                rem -= first_blocks;

                let mut cur_is_a = true;
                let mut cur_lba = lba;
                let mut cur_blocks = first_blocks;
                let mut cur_bytes = first_bytes;
                lba += first_blocks;

                // Ping-pong: overlap SDHI DMA write of current chunk with USB Bulk OUT of next chunk.
                while rem > 0 {
                    let next_blocks = rem.min(CHUNK_SECTORS);
                    let next_bytes = (next_blocks as usize) * 512;
                    let (sd_res, usb_res) = if cur_is_a {
                        join(
                            sd.write_blocks_dma(cur_lba, &buf_a[..cur_bytes]),
                            read_exact_out(ep_out, &mut buf_b[..next_bytes]),
                        )
                        .await
                    } else {
                        join(
                            sd.write_blocks_dma(cur_lba, &buf_b[..cur_bytes]),
                            read_exact_out(ep_out, &mut buf_a[..next_bytes]),
                        )
                        .await
                    };

                    if sd_res.is_err() {
                        self.set_sense(0x03, 0x0C, 0x00);
                        return (CSW_STATUS_FAILED, expected_len.saturating_sub(recvd), false);
                    }
                    if usb_res.is_err() {
                        self.set_sense(0x04, 0x00, 0x00);
                        return (CSW_STATUS_FAILED, expected_len.saturating_sub(recvd), false);
                    }
                    recvd += next_bytes as u32;

                    cur_lba = lba;
                    cur_blocks = next_blocks;
                    cur_bytes = next_bytes;
                    lba += next_blocks;
                    rem -= next_blocks;
                    cur_is_a = !cur_is_a;
                }

                // Write final chunk to SD.
                let _ = cur_blocks;
                let last_slice = if cur_is_a {
                    &buf_a[..cur_bytes]
                } else {
                    &buf_b[..cur_bytes]
                };
                if sd.write_blocks_dma(cur_lba, last_slice).await.is_err() {
                    self.set_sense(0x03, 0x0C, 0x00);
                    return (CSW_STATUS_FAILED, expected_len.saturating_sub(recvd), false);
                }
                self.clear_sense();
                (CSW_STATUS_PASSED, expected_len.saturating_sub(recvd), false)
            }

            SCSI_START_STOP_UNIT => {
                let _ = sd.flush();
                // LoEj=1 (bit 1), Start=0 (bit 0) -> Host ejected the volume; hand SD back to firmware.
                if (cdb[4] & 0x03) == 0x02 {
                    set_storage_mode(StorageMode::Firmware);
                }
                self.clear_sense();
                (CSW_STATUS_PASSED, 0, false)
            }

            SCSI_SYNCHRONIZE_CACHE_10 => {
                let _ = sd.flush();
                self.clear_sense();
                (CSW_STATUS_PASSED, 0, false)
            }

            SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL | SCSI_VERIFY_10 => {
                self.clear_sense();
                (CSW_STATUS_PASSED, 0, false)
            }

            _ => {
                self.set_sense(0x05, 0x20, 0x00); // ILLEGAL REQUEST - INVALID COMMAND OPCODE
                if expected_len > 0 && dir_in {
                    ep_in.set_stalled(true);
                    (CSW_STATUS_FAILED, expected_len, true)
                } else if expected_len > 0 {
                    ep_out.set_stalled(true);
                    (CSW_STATUS_FAILED, expected_len, false)
                } else {
                    (CSW_STATUS_FAILED, 0, false)
                }
            }
        }
    }
}
