//! Shared `#![no_std]` parsers for `/firmware/firmware.ini` and `MC1P` binary patch files.

/// Magic bytes for verified binary patch files (`"MC1P"`).
pub const MC1P_MAGIC: [u8; 4] = *b"MC1P";
/// Supported `MC1P` format version (v2: checksum-verified `Write`, `Copy`, `Fill`, `WriteRaw`).
pub const MC1P_VERSION: u16 = 2;
/// Size of the `MC1P` file header in bytes.
pub const MC1P_HEADER_LEN: usize = 16;

/// Maximum length of a normalized SD card path in `firmware.ini`.
pub const MAX_PATH_LEN: usize = 96;
/// Maximum number of patch files per core in `firmware.ini`.
pub const MAX_PATCHES_PER_CORE: usize = 4;

/// `MC1P` v2 record kind.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchKind {
    /// Verify `crc32_ieee(mem[addr..addr+len]) == expected_crc32`, then write `patch[len]`.
    Write = 0,
    /// Copy `len` bytes from `src_addr` (`u32`) to `addr` (`memmove` semantics).
    Copy = 1,
    /// Fill `len` bytes at `addr` with `fill_byte` (`u8`).
    Fill = 2,
    /// Write `data[len]` directly to `addr` without pre-image check (for free memory regions).
    WriteRaw = 3,
}

impl PatchKind {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Write),
            1 => Some(Self::Copy),
            2 => Some(Self::Fill),
            3 => Some(Self::WriteRaw),
            _ => None,
        }
    }
}

/// Parsed `MC1P` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mc1pHeader {
    pub version: u16,
    pub record_count: u16,
    pub flags: u32,
    pub crc32: u32,
}

/// Errors returned when validating or applying an `MC1P` patch file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchError {
    TooShort,
    BadMagic,
    UnsupportedVersion,
    BadCrc,
    TruncatedRecord,
    UnknownKind,
    TrailingBytes,
    ExpectedMismatch { index: u16, addr: u32 },
    ReadFailed { index: u16, addr: u32 },
}

/// Standard IEEE 802.3 CRC-32 (`0xEDB8_8320`).
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[inline]
fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Parse and verify an `MC1P` v2 file's header, CRC-32, record framing, and every `Write`
/// record's `expected_crc32` via `read_mem(addr, offset)` without modifying any target memory.
pub fn verify_mc1p<F>(data: &[u8], mut read_mem: F) -> Result<Mc1pHeader, PatchError>
where
    F: FnMut(u32, usize) -> Option<u8>,
{
    if data.len() < MC1P_HEADER_LEN {
        return Err(PatchError::TooShort);
    }
    if data[0..4] != MC1P_MAGIC {
        return Err(PatchError::BadMagic);
    }
    let version = rd_u16(data, 4);
    if version != MC1P_VERSION {
        return Err(PatchError::UnsupportedVersion);
    }
    let record_count = rd_u16(data, 6);
    let flags = rd_u32(data, 8);
    let crc32 = rd_u32(data, 12);
    if crc32_ieee(&data[MC1P_HEADER_LEN..]) != crc32 {
        return Err(PatchError::BadCrc);
    }

    let mut off = MC1P_HEADER_LEN;
    for index in 0..record_count {
        if off + 8 > data.len() {
            return Err(PatchError::TruncatedRecord);
        }
        let addr = rd_u32(data, off);
        let len = rd_u16(data, off + 4) as usize;
        let kind = PatchKind::from_u16(rd_u16(data, off + 6)).ok_or(PatchError::UnknownKind)?;
        off += 8;

        match kind {
            PatchKind::Write => {
                let need = 4usize.checked_add(len).ok_or(PatchError::TruncatedRecord)?;
                if off + need > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                let expected_crc = rd_u32(data, off);
                let mut crc: u32 = 0xFFFF_FFFF;
                for i in 0..len {
                    let b = read_mem(addr, i).ok_or(PatchError::ReadFailed { index, addr })?;
                    crc ^= b as u32;
                    for _ in 0..8 {
                        let mask = (crc & 1).wrapping_neg();
                        crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                    }
                }
                if !crc != expected_crc {
                    return Err(PatchError::ExpectedMismatch { index, addr });
                }
                off += need;
            }
            PatchKind::Copy | PatchKind::Fill => {
                if off + 4 > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                off += 4;
            }
            PatchKind::WriteRaw => {
                if off + len > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                off += len;
            }
        }
    }

    if off != data.len() {
        return Err(PatchError::TrailingBytes);
    }

    Ok(Mc1pHeader {
        version,
        record_count,
        flags,
        crc32,
    })
}

/// Operation emitted by [`apply_mc1p`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchOp<'a> {
    Write { addr: u32, bytes: &'a [u8] },
    Copy { dst: u32, src: u32, len: usize },
    Fill { dst: u32, byte: u8, len: usize },
}

/// Apply an already-verified `MC1P` v2 patch file.
pub fn apply_mc1p<F>(data: &[u8], mut on_op: F) -> Result<u16, PatchError>
where
    F: FnMut(PatchOp<'_>),
{
    if data.len() < MC1P_HEADER_LEN {
        return Err(PatchError::TooShort);
    }
    let record_count = rd_u16(data, 6);
    let mut off = MC1P_HEADER_LEN;

    for _ in 0..record_count {
        if off + 8 > data.len() {
            return Err(PatchError::TruncatedRecord);
        }
        let addr = rd_u32(data, off);
        let len = rd_u16(data, off + 4) as usize;
        let kind = PatchKind::from_u16(rd_u16(data, off + 6)).ok_or(PatchError::UnknownKind)?;
        off += 8;

        match kind {
            PatchKind::Write => {
                let need = 4usize.checked_add(len).ok_or(PatchError::TruncatedRecord)?;
                if off + need > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                let patch = &data[off + 4..off + need];
                on_op(PatchOp::Write { addr, bytes: patch });
                off += need;
            }
            PatchKind::Copy => {
                if off + 4 > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                let src_addr = rd_u32(data, off);
                on_op(PatchOp::Copy {
                    dst: addr,
                    src: src_addr,
                    len,
                });
                off += 4;
            }
            PatchKind::Fill => {
                if off + 4 > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                let fill_byte = data[off];
                on_op(PatchOp::Fill {
                    dst: addr,
                    byte: fill_byte,
                    len,
                });
                off += 4;
            }
            PatchKind::WriteRaw => {
                if off + len > data.len() {
                    return Err(PatchError::TruncatedRecord);
                }
                let bytes = &data[off..off + len];
                on_op(PatchOp::Write { addr, bytes });
                off += len;
            }
        }
    }

    Ok(record_count)
}

// -----------------------------------------------------------------------------
// /firmware/firmware.ini Parser
// -----------------------------------------------------------------------------

/// Fixed-capacity normalized SD path (e.g. `firmware/mc1r1/mc1r1-0.bin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathBuf96 {
    buf: [u8; MAX_PATH_LEN],
    len: u8,
}

impl PathBuf96 {
    pub const fn empty() -> Self {
        Self {
            buf: [0; MAX_PATH_LEN],
            len: 0,
        }
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        let mut s = raw.trim();
        // Strip optional surrounding quotes.
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            s = &s[1..s.len() - 1];
        }
        s = s.trim().trim_start_matches('/');
        if s.is_empty() || s.len() > MAX_PATH_LEN || s.contains("..") || s.contains('\\') {
            return None;
        }
        let mut out = Self::empty();
        out.buf[..s.len()].copy_from_slice(s.as_bytes());
        out.len = s.len() as u8;
        Some(out)
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

/// Boot configuration for one core (`[c0a]` or `[c1a]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreBootConfig {
    pub image: Option<PathBuf96>,
    pub patches: [Option<PathBuf96>; MAX_PATCHES_PER_CORE],
    pub patch_count: usize,
}

impl CoreBootConfig {
    pub const fn empty() -> Self {
        Self {
            image: None,
            patches: [None; MAX_PATCHES_PER_CORE],
            patch_count: 0,
        }
    }

    pub fn patches(&self) -> impl Iterator<Item = &str> {
        self.patches[..self.patch_count]
            .iter()
            .filter_map(|p| p.as_ref().map(|x| x.as_str()))
    }

    fn push_patch(&mut self, path: PathBuf96) {
        if self.patch_count < MAX_PATCHES_PER_CORE {
            self.patches[self.patch_count] = Some(path);
            self.patch_count += 1;
        }
    }
}

/// Parsed `/firmware/firmware.ini`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirmwareConfig {
    pub c0a: CoreBootConfig,
    pub c1a: CoreBootConfig,
}

impl FirmwareConfig {
    pub const fn empty() -> Self {
        Self {
            c0a: CoreBootConfig::empty(),
            c1a: CoreBootConfig::empty(),
        }
    }

    pub fn parse(text: &str) -> Self {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Section {
            None,
            C0a,
            C1a,
        }

        let mut cfg = Self::empty();
        let mut section = Section::None;

        for raw_line in text.lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let sec = line[1..line.len() - 1].trim();
                if sec.eq_ignore_ascii_case("c0a") {
                    section = Section::C0a;
                } else if sec.eq_ignore_ascii_case("c1a") {
                    section = Section::C1a;
                } else {
                    section = Section::None;
                }
                continue;
            }
            let Some((raw_key, raw_val)) = line.split_once('=') else {
                continue;
            };
            let key = raw_key.trim();
            let val = raw_val.trim();
            let target = match section {
                Section::C0a => &mut cfg.c0a,
                Section::C1a => &mut cfg.c1a,
                Section::None => continue,
            };

            if key.eq_ignore_ascii_case("image") || key.eq_ignore_ascii_case("firmware") {
                if !val.is_empty() {
                    if let Some(p) = PathBuf96::from_str(val) {
                        target.image = Some(p);
                    }
                }
            } else if key.eq_ignore_ascii_case("patch") {
                for item in val.split(',') {
                    if let Some(p) = PathBuf96::from_str(item) {
                        target.push_patch(p);
                    }
                }
            }
        }

        cfg
    }
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, b) in line.bytes().enumerate() {
        if b == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && (b == b'#' || b == b';') {
            return &line[..idx];
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_firmware_ini_both_cores() {
        let ini = r#"
# MC-101 SD Bootloader configuration
[c0a]
image = "/firmware/mc1r1/mc1r1-0.bin"
patch = /firmware/mc107/mc107-0.pat, /firmware/extra/fix.pat ; inline comment

[C1A]
image = firmware/mc1r1/mc1r1-1.bin
patch = /firmware/mc107/mc107-1.pat
"#;
        let cfg = FirmwareConfig::parse(ini);
        assert_eq!(
            cfg.c0a.image.as_ref().map(|p| p.as_str()),
            Some("firmware/mc1r1/mc1r1-0.bin")
        );
        let p0: [&str; 2] = [
            cfg.c0a.patches[0].as_ref().unwrap().as_str(),
            cfg.c0a.patches[1].as_ref().unwrap().as_str(),
        ];
        assert_eq!(
            p0,
            ["firmware/mc107/mc107-0.pat", "firmware/extra/fix.pat"]
        );
        assert_eq!(
            cfg.c1a.image.as_ref().map(|p| p.as_str()),
            Some("firmware/mc1r1/mc1r1-1.bin")
        );
        assert_eq!(
            cfg.c1a.patches[0].as_ref().unwrap().as_str(),
            "firmware/mc107/mc107-1.pat"
        );
    }

    #[test]
    fn verify_and_apply_mc1p_roundtrip() {
        // Build a 3-record MC1P v2:
        //   0: Write(addr=4, exp_crc=crc32([1,2]), patch=[9,8]) -> 8 + 4 + 2 = 14 B
        //   1: Fill(addr=8, byte=0x55, len=4) -> 8 + 4 = 12 B
        //   2: WriteRaw(addr=12, bytes=[0xAA, 0xBB]) -> 8 + 2 = 10 B
        let mut body = [0u8; 36];
        let exp_crc = crc32_ieee(&[1, 2]);
        // Record 0: Write
        body[0..4].copy_from_slice(&4u32.to_le_bytes());
        body[4..6].copy_from_slice(&2u16.to_le_bytes());
        body[6..8].copy_from_slice(&0u16.to_le_bytes());
        body[8..12].copy_from_slice(&exp_crc.to_le_bytes());
        body[12..14].copy_from_slice(&[9, 8]);
        // Record 1: Fill
        body[14..18].copy_from_slice(&8u32.to_le_bytes());
        body[18..20].copy_from_slice(&4u16.to_le_bytes());
        body[20..22].copy_from_slice(&2u16.to_le_bytes());
        body[22..26].copy_from_slice(&[0x55, 0, 0, 0]);
        // Record 2: WriteRaw
        body[26..30].copy_from_slice(&12u32.to_le_bytes());
        body[30..32].copy_from_slice(&2u16.to_le_bytes());
        body[32..34].copy_from_slice(&3u16.to_le_bytes());
        body[34..36].copy_from_slice(&[0xAA, 0xBB]);

        let mut pkt = [0u8; 52];
        pkt[0..4].copy_from_slice(b"MC1P");
        pkt[4..6].copy_from_slice(&2u16.to_le_bytes());
        pkt[6..8].copy_from_slice(&3u16.to_le_bytes());
        let crc = crc32_ieee(&body);
        pkt[12..16].copy_from_slice(&crc.to_le_bytes());
        pkt[16..52].copy_from_slice(&body);

        let mut mem = [0u8; 16];
        mem[4..6].copy_from_slice(&[1, 2]);

        let hdr = verify_mc1p(&pkt, |addr, i| mem.get(addr as usize + i).copied()).unwrap();
        assert_eq!(hdr.record_count, 3);

        apply_mc1p(&pkt, |op| match op {
            PatchOp::Write { addr, bytes } => {
                mem[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
            }
            PatchOp::Copy { dst, src, len } => {
                mem.copy_within(src as usize..src as usize + len, dst as usize);
            }
            PatchOp::Fill { dst, byte, len } => {
                mem[dst as usize..dst as usize + len].fill(byte);
            }
        })
        .unwrap();
        assert_eq!(&mem[4..14], &[9, 8, 0, 0, 0x55, 0x55, 0x55, 0x55, 0xAA, 0xBB]);
    }
}
