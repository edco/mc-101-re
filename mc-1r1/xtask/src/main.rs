//! Self-contained build, constraint checker, and firmware container packager for mc-1r1.
//!
//! Builds the two embedded firmware ELFs (`mc1r1-core0`, `mc1r1-core1`) for
//! `thumbv7em-none-eabihf`, enforces drop-in memory-map and binary-layout
//! constraints on both ELFs and extracted binaries, packages:
//! - `target/dist/RPG69_C0A_up.bin` (unencrypted Core 0 App1_Main container)
//! - `target/dist/RPG69_C1A_up.bin` (Core 1 QSPI container with `idm1.bin` + `sdram1.bin`)
//! and verifies every header, TOC entry, sum16le, and IEEE 802.3 CRC-32.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

// -----------------------------------------------------------------------------
// Hardware & Drop-In Memory Constraints
// -----------------------------------------------------------------------------
const CORE0_SDRAM_START: u32 = 0x6000_0000;
const CORE0_SDRAM_LIMIT: u32 = 0x602C_C000; // Below g_bDispDirty (0x602C_C114) & g_abDispBuf0 (0x6096_2A34)
const CORE0_ITCM_START: u32 = 0x0100_0000;
const CORE0_ITCM_LIMIT: u32 = 0x0105_0000; // Below initial MSP (0x0105_0800)
const CORE0_SRAM_START: u32 = 0x2000_0000;
const CORE0_SRAM_LIMIT: u32 = 0x2002_0000; // Strictly below Core 1 SRAM (0x2002_0000+)

const CORE1_ITCM_START: u32 = 0x0110_0000;
const CORE1_ITCM_LIMIT: u32 = 0x0114_0000; // 256 KiB ITCM1 limit
const CORE1_SRAM_START: u32 = 0x2002_0000; // Strictly above Core 0 SRAM
const CORE1_SRAM_LIMIT: u32 = 0x200A_9000; // Below Core 1 initial MSP (0x200A_91C8)
const CORE1_SDRAM_START: u32 = 0x6100_0000;
const CORE1_SDRAM_LIMIT: u32 = 0x6170_0000; // Strictly below upper BSS / wave RAM (0x6180_0000)

// -----------------------------------------------------------------------------
// RPG69_C0A (Core 0 App1_Main) Constants
// -----------------------------------------------------------------------------
const C0A_HEADER_LEN: usize = 96;
const C0A_FOOTER_LEN: usize = 64;
const C0A_MAX_PAYLOAD: usize = 0x002C_C000;
const C0A_APP_NAME: &[u8] = b"App1_Main";
const C0A_BUILD_TIMESTAMP: &[u8] = b"2023/05/17 22:54";
const C0A_VERSION: &[u8] = b"0.010001";
const C0A_SDRAM_BASE: u32 = 0x6000_0000;
const C0A_ENTRY_OFFSET: u32 = 0x000C_0040;
const C0A_PAYLOAD_FLASH_OFFSET: u32 = 0x000C_0060;
const C0A_CONTROL_NO_DECRYPT: u32 = 0x0000_0000;
const C0A_BUILD_TAG: &[u8] = b"Roland RPG69_C0AVER.1.82-BLD.14423 0517 22 54 58";

// -----------------------------------------------------------------------------
// RPG69_C1A (Core 1 QSPI Filesystem) Constants
// -----------------------------------------------------------------------------
const C1A_TOTAL_LEN: usize = 0x003F_FF60; // 4,194,144 bytes (4 MiB partition)
const C1A_TRAILER_LEN: usize = 0x34; // 4-byte CRC-32 + 48-byte build tag
const C1A_FOOTER_TAG_LEN: usize = 0x30;
const C1A_DATA_END: usize = C1A_TOTAL_LEN - C1A_TRAILER_LEN; // 0x003F_FF2C
const C1A_FIRST_FILE_OFFSET: usize = 0x1000;
const C1A_ALIGN: usize = 0x1000;
const C1A_MAX_IDM1: usize = 256 * 1024;
const C1A_BUILD_TAG: &[u8] = b"Roland RPG69_C1AVER.1.82-BLD.14423 0517 22 49 32";

const TARGET_TRIPLE: &str = "thumbv7em-none-eabihf";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .ok_or("cannot determine mc-1r1 workspace root")?;
    let target_release = root.join("target").join(TARGET_TRIPLE).join("release");
    let dist_dir = root.join("target").join("dist");
    fs::create_dir_all(&dist_dir).map_err(|e| format!("mkdir {}: {e}", dist_dir.display()))?;

    println!("[1/5] Building embedded firmware ELFs ({TARGET_TRIPLE}, --release)...");
    run_cmd(
        Command::new("cargo")
            .current_dir(root)
            .args(["build", "--release", "--target", TARGET_TRIPLE]),
    )?;

    let core0_elf_path = target_release.join("mc1r1-core0");
    let core1_elf_path = target_release.join("mc1r1-core1");

    println!("[2/5] Checking ELF memory-map & non-overlap constraints...");
    verify_elf_constraints(&core0_elf_path, CoreId::Core0)?;
    verify_elf_constraints(&core1_elf_path, CoreId::Core1)?;

    println!("[3/5] Extracting raw binaries (splitting mc1r1-core1 -> idm1.bin + sdram1.bin)...");
    let core0_bin_path = dist_dir.join("mc1r1-core0.bin");
    let idm1_bin_path = dist_dir.join("idm1.bin");
    let sdram1_bin_path = dist_dir.join("sdram1.bin");

    objcopy_bin(&core0_elf_path, &core0_bin_path, &[])?;
    objcopy_bin(
        &core1_elf_path,
        &idm1_bin_path,
        &["-j", ".vector_table", "-j", ".text", "-j", ".data"],
    )?;
    objcopy_bin(&core1_elf_path, &sdram1_bin_path, &["-j", ".sdram"])?;

    let core0_raw = fs::read(&core0_bin_path).map_err(|e| format!("read core0.bin: {e}"))?;
    let idm1_raw = fs::read(&idm1_bin_path).map_err(|e| format!("read idm1.bin: {e}"))?;
    let sdram1_raw = fs::read(&sdram1_bin_path).map_err(|e| format!("read sdram1.bin: {e}"))?;

    println!("[4/5] Verifying extracted binary headers, vector tables, and size bounds...");
    verify_core0_binary(&core0_raw)?;
    verify_idm1_binary(&idm1_raw)?;
    verify_sdram1_binary(&sdram1_raw)?;

    println!("[5/5] Packaging & verifying RPG69_C0A_up.bin and RPG69_C1A_up.bin...");
    let c0a = package_c0a(&core0_raw)?;
    let c1a = package_c1a(&idm1_raw, &sdram1_raw)?;

    let c0a_path = dist_dir.join("RPG69_C0A_up.bin");
    let c1a_path = dist_dir.join("RPG69_C1A_up.bin");
    fs::write(&c0a_path, &c0a).map_err(|e| format!("write {}: {e}", c0a_path.display()))?;
    fs::write(&c1a_path, &c1a).map_err(|e| format!("write {}: {e}", c1a_path.display()))?;

    verify_c0a(&c0a)?;
    verify_c1a(&c1a)?;

    println!(
        "  OK  {} ({} bytes, payload {} bytes, CRC-32 0x{:08X})",
        c0a_path.display(),
        c0a.len(),
        c0a.len() - C0A_HEADER_LEN - C0A_FOOTER_LEN,
        read_u32_le(&c0a, c0a.len() - 0x34),
    );
    println!(
        "  OK  {} ({} bytes, idm1 {} bytes, sdram1 {} bytes, CRC-32 0x{:08X})",
        c1a_path.display(),
        c1a.len(),
        idm1_raw.len(),
        sdram1_raw.len(),
        read_u32_le(&c1a, c1a.len() - 0x34),
    );

    Ok(())
}

fn run_cmd(cmd: &mut Command) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {:?}: {e}", cmd.get_program()))?;
    if !status.success() {
        return Err(format!(
            "command {:?} exited with status {status}",
            cmd.get_program()
        ));
    }
    Ok(())
}

fn find_sysroot_rust_objcopy() -> Option<PathBuf> {
    let sysroot_out = Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
    if !sysroot_out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&sysroot_out.stdout).trim().to_string();
    let rustlib = Path::new(&sysroot).join("lib").join("rustlib");
    let entries = fs::read_dir(rustlib).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("bin").join("rust-objcopy");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn objcopy_bin(elf: &Path, out_bin: &Path, extra_args: &[&str]) -> Result<(), String> {
    for tool in ["rust-objcopy", "llvm-objcopy", "arm-none-eabi-objcopy"] {
        if let Ok(status) = Command::new(tool)
            .args(["-O", "binary"])
            .args(extra_args)
            .arg(elf)
            .arg(out_bin)
            .status()
        {
            if status.success() {
                return Ok(());
            }
            return Err(format!("{tool} exited with {status}"));
        }
    }
    if let Some(sysroot_tool) = find_sysroot_rust_objcopy() {
        let status = Command::new(&sysroot_tool)
            .args(["-O", "binary"])
            .args(extra_args)
            .arg(elf)
            .arg(out_bin)
            .status()
            .map_err(|e| format!("run {}: {e}", sysroot_tool.display()))?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("{} exited with {status}", sysroot_tool.display()));
    }
    Err("no objcopy tool found (tried rust-objcopy, llvm-objcopy, arm-none-eabi-objcopy, and sysroot rust-objcopy)".into())
}

// -----------------------------------------------------------------------------
// ELF32 & Binary Constraint Enforcement
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum CoreId {
    Core0,
    Core1,
}

/// Parse ELF32 little-endian section headers and ensure every `SHF_ALLOC`
/// section falls strictly inside the allowed memory windows for that core.
fn verify_elf_constraints(elf_path: &Path, core: CoreId) -> Result<(), String> {
    let data = fs::read(elf_path).map_err(|e| format!("read {}: {e}", elf_path.display()))?;
    if data.len() < 52 || &data[0..4] != b"\x7fELF" || data[4] != 1 || data[5] != 1 {
        return Err(format!("{}: not a valid ELF32 LE file", elf_path.display()));
    }

    let entry = read_u32_le(&data, 24);
    let shoff = read_u32_le(&data, 32) as usize;
    let shentsize = read_u16_le(&data, 46) as usize;
    let shnum = read_u16_le(&data, 48) as usize;
    let shstrndx = read_u16_le(&data, 50) as usize;

    match core {
        CoreId::Core0 => {
            if entry != (CORE0_SDRAM_START | 1) {
                return Err(format!(
                    "Core 0 ELF entry must be 0x{:08X} (Thumb Reset_Handler), got 0x{entry:08X}",
                    CORE0_SDRAM_START | 1
                ));
            }
        }
        CoreId::Core1 => {
            if entry < CORE1_ITCM_START || entry >= CORE1_ITCM_LIMIT || (entry & 1) == 0 {
                return Err(format!(
                    "Core 1 ELF entry must be Thumb in ITCM1, got 0x{entry:08X}"
                ));
            }
        }
    }

    let strtab_off = read_u32_le(&data, shoff + shstrndx * shentsize + 16) as usize;
    let strtab_size = read_u32_le(&data, shoff + shstrndx * shentsize + 20) as usize;
    let strtab = &data[strtab_off..strtab_off + strtab_size];

    let allowed: &[(u32, u32)] = match core {
        CoreId::Core0 => &[
            (CORE0_SDRAM_START, CORE0_SDRAM_LIMIT),
            (CORE0_ITCM_START, CORE0_ITCM_LIMIT),
            (CORE0_SRAM_START, CORE0_SRAM_LIMIT),
        ],
        CoreId::Core1 => &[
            (CORE1_ITCM_START, CORE1_ITCM_LIMIT),
            (CORE1_SRAM_START, CORE1_SRAM_LIMIT),
            (CORE1_SDRAM_START, CORE1_SDRAM_LIMIT),
        ],
    };

    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let name_off = read_u32_le(&data, base) as usize;
        let flags = read_u32_le(&data, base + 8);
        let addr = read_u32_le(&data, base + 12);
        let size = read_u32_le(&data, base + 20);

        const SHF_ALLOC: u32 = 0x2;
        if (flags & SHF_ALLOC) == 0 || size == 0 {
            continue;
        }

        let name_end = strtab[name_off..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| name_off + p)
            .unwrap_or(strtab.len());
        let name = String::from_utf8_lossy(&strtab[name_off..name_end]);

        let end = addr
            .checked_add(size)
            .ok_or_else(|| format!("section {name} address overflow"))?;
        let in_allowed = allowed.iter().any(|&(lo, hi)| addr >= lo && end <= hi);
        if !in_allowed {
            return Err(format!(
                "{}: section `{name}` [0x{addr:08X}..0x{end:08X}) violates drop-in memory-map bounds",
                elf_path.display()
            ));
        }
    }

    Ok(())
}

fn verify_core0_binary(bin: &[u8]) -> Result<(), String> {
    if bin.is_empty() {
        return Err("mc1r1-core0.bin is empty".into());
    }
    if bin.len() > C0A_MAX_PAYLOAD {
        return Err(format!(
            "mc1r1-core0.bin ({} bytes) exceeds 0x{:X} limit (below g_bDispDirty)",
            bin.len(),
            C0A_MAX_PAYLOAD
        ));
    }
    Ok(())
}

fn verify_idm1_binary(bin: &[u8]) -> Result<(), String> {
    if bin.len() < 0x40 {
        return Err(format!("idm1.bin too small ({} bytes)", bin.len()));
    }
    if bin.len() > C1A_MAX_IDM1 {
        return Err(format!(
            "idm1.bin ({} bytes) exceeds 256 KiB ITCM1 limit",
            bin.len()
        ));
    }
    let msp = read_u32_le(bin, 0x00);
    let reset = read_u32_le(bin, 0x04);
    if msp < CORE1_SRAM_START || msp > 0x200C_0000 || (msp & 7) != 0 {
        return Err(format!(
            "idm1.bin vector[0] MSP 0x{msp:08X} not 8-byte aligned in Core 1 SRAM"
        ));
    }
    if reset < CORE1_ITCM_START || reset >= CORE1_ITCM_LIMIT || (reset & 1) == 0 {
        return Err(format!(
            "idm1.bin vector[1] Reset 0x{reset:08X} not a Thumb address in ITCM1"
        ));
    }
    Ok(())
}

fn verify_sdram1_binary(bin: &[u8]) -> Result<(), String> {
    if bin.len() < 4 {
        return Err(format!("sdram1.bin too small ({} bytes)", bin.len()));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Checksums
// -----------------------------------------------------------------------------

/// Standard IEEE 802.3 CRC-32 (poly 0xEDB88320, init 0xFFFFFFFF, final XOR 0xFFFFFFFF).
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 16-bit little-endian word sum used by Roland QSPI TOC entries.
fn sum16le(data: &[u8]) -> Result<u16, String> {
    if data.len() % 2 != 0 {
        return Err(format!(
            "sum16le requires even-length data, got {}",
            data.len()
        ));
    }
    let mut acc: u32 = 0;
    for chunk in data.chunks_exact(2) {
        acc = acc.wrapping_add(u16::from_le_bytes([chunk[0], chunk[1]]) as u32);
    }
    Ok((acc & 0xFFFF) as u16)
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

fn write_u32_le(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn write_u16_le(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

// -----------------------------------------------------------------------------
// RPG69_C0A_up.bin Builder & Verifier
// -----------------------------------------------------------------------------

fn package_c0a(raw_payload: &[u8]) -> Result<Vec<u8>, String> {
    if raw_payload.is_empty() {
        return Err("Core 0 payload is empty".into());
    }
    // Pad payload to 16-byte boundary (required by App0_UPD; matches stock 0x3565B0).
    let padded_len = align_up(raw_payload.len(), 16);
    let mut payload = Vec::with_capacity(padded_len);
    payload.extend_from_slice(raw_payload);
    payload.resize(padded_len, 0);

    let payload_crc = crc32_ieee(&payload);

    let mut hdr = [0u8; C0A_HEADER_LEN];
    hdr[0x00..C0A_APP_NAME.len()].copy_from_slice(C0A_APP_NAME);
    hdr[0x10..0x10 + C0A_BUILD_TIMESTAMP.len()].copy_from_slice(C0A_BUILD_TIMESTAMP);
    hdr[0x20..0x20 + C0A_VERSION.len()].copy_from_slice(C0A_VERSION);
    write_u32_le(&mut hdr, 0x28, C0A_SDRAM_BASE);
    write_u32_le(&mut hdr, 0x2C, 0);
    write_u32_le(&mut hdr, 0x30, C0A_ENTRY_OFFSET);
    hdr[0x34..0x40].fill(0xFF);
    write_u32_le(&mut hdr, 0x40, C0A_PAYLOAD_FLASH_OFFSET);
    write_u32_le(&mut hdr, 0x44, payload.len() as u32);
    write_u32_le(&mut hdr, 0x48, C0A_SDRAM_BASE);
    write_u32_le(&mut hdr, 0x4C, 0);
    write_u32_le(&mut hdr, 0x50, payload_crc);
    write_u32_le(&mut hdr, 0x54, C0A_CONTROL_NO_DECRYPT);
    write_u32_le(&mut hdr, 0x58, 0);
    write_u32_le(&mut hdr, 0x5C, 0xFFFF_FFFF);

    let mut out = Vec::with_capacity(C0A_HEADER_LEN + payload.len() + C0A_FOOTER_LEN);
    out.extend_from_slice(&hdr);
    out.extend_from_slice(&payload);
    out.extend_from_slice(&[0u8; 12]);

    let trailer_crc = crc32_ieee(&out);
    out.extend_from_slice(&trailer_crc.to_le_bytes());

    let mut tag = [0u8; 48];
    tag[..C0A_BUILD_TAG.len()].copy_from_slice(C0A_BUILD_TAG);
    out.extend_from_slice(&tag);

    Ok(out)
}

fn verify_c0a(data: &[u8]) -> Result<(), String> {
    if data.len() < C0A_HEADER_LEN + C0A_FOOTER_LEN {
        return Err("C0A container too small".into());
    }
    let payload_len = read_u32_le(data, 0x44) as usize;
    let actual_payload_len = data.len() - C0A_HEADER_LEN - C0A_FOOTER_LEN;
    if payload_len != actual_payload_len {
        return Err(format!(
            "C0A payload_len mismatch: header={payload_len}, actual={actual_payload_len}"
        ));
    }
    if payload_len % 16 != 0 {
        return Err(format!(
            "C0A payload_len ({payload_len}) must be 16-byte aligned"
        ));
    }
    let payload = &data[C0A_HEADER_LEN..C0A_HEADER_LEN + payload_len];
    let w20_stored = read_u32_le(data, 0x50);
    let w20_calc = crc32_ieee(payload);
    if w20_stored != w20_calc {
        return Err(format!(
            "C0A Word 20 payload CRC mismatch: stored=0x{w20_stored:08X}, calc=0x{w20_calc:08X}"
        ));
    }
    let crc_off = data.len() - 0x34;
    let stored_crc = read_u32_le(data, crc_off);
    let calc_crc = crc32_ieee(&data[..crc_off]);
    if stored_crc != calc_crc {
        return Err(format!(
            "C0A trailer CRC-32 mismatch: stored=0x{stored_crc:08X}, calc=0x{calc_crc:08X}"
        ));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// RPG69_C1A_up.bin (QSPI Container) Builder & Verifier
// -----------------------------------------------------------------------------

fn package_c1a(idm1_raw: &[u8], sdram1_raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut idm1 = idm1_raw.to_vec();
    if idm1.len() % 2 != 0 {
        idm1.push(0);
    }
    let mut sdram1 = sdram1_raw.to_vec();
    if sdram1.len() % 2 != 0 {
        sdram1.push(0);
    }

    let idm1_off = C1A_FIRST_FILE_OFFSET;
    let sdram1_off = align_up(idm1_off + idm1.len(), C1A_ALIGN);
    // TOC offsets are relative to the "QSPI" header at +0x20 (App1 0x6009A668 adds +0x20).
    let idm1_phys = 0x20 + idm1_off;
    let sdram1_phys = 0x20 + sdram1_off;
    if sdram1_phys + sdram1.len() > C1A_DATA_END {
        return Err(format!(
            "C1A payloads overflow 4 MiB QSPI container (end=0x{:X} > max=0x{:X})",
            sdram1_phys + sdram1.len(),
            C1A_DATA_END
        ));
    }

    let mut out = vec![0u8; C1A_TOTAL_LEN];

    write_u32_le(&mut out, 0x00, (C1A_TOTAL_LEN - C1A_FOOTER_TAG_LEN) as u32);
    out[0x20..0x24].copy_from_slice(b"QSPI");
    write_u32_le(&mut out, 0x24, 0x20);
    write_u32_le(&mut out, 0x28, 32);
    write_u32_le(&mut out, 0x2C, 2);

    out[idm1_phys..idm1_phys + idm1.len()].copy_from_slice(&idm1);
    out[sdram1_phys..sdram1_phys + sdram1.len()].copy_from_slice(&sdram1);

    let idm1_csum = sum16le(&out[idm1_phys..sdram1_phys])?;
    let sdram1_csum = sum16le(&out[sdram1_phys..C1A_DATA_END])?;

    write_toc_entry(&mut out, 0x30, b"idm1.bin", idm1_off as u32, idm1.len() as u32, idm1_csum);
    write_toc_entry(
        &mut out,
        0x50,
        b"sdram1.bin",
        sdram1_off as u32,
        sdram1.len() as u32,
        sdram1_csum,
    );

    let crc = crc32_ieee(&out[..C1A_DATA_END]);
    write_u32_le(&mut out, C1A_DATA_END, crc);

    let tag_off = C1A_TOTAL_LEN - C1A_FOOTER_TAG_LEN;
    out[tag_off..tag_off + C1A_BUILD_TAG.len()].copy_from_slice(C1A_BUILD_TAG);

    Ok(out)
}

fn write_toc_entry(buf: &mut [u8], off: usize, name: &[u8], file_off: u32, len: u32, csum: u16) {
    let n = name.len().min(16);
    buf[off..off + n].copy_from_slice(&name[..n]);
    write_u32_le(buf, off + 16, file_off);
    write_u32_le(buf, off + 20, len);
    write_u16_le(buf, off + 24, csum);
    write_u16_le(buf, off + 26, 0);
    write_u32_le(buf, off + 28, 0);
}

fn verify_c1a(data: &[u8]) -> Result<(), String> {
    if data.len() < 0x70 + C1A_TRAILER_LEN {
        return Err("C1A container too small".into());
    }
    let w0 = read_u32_le(data, 0) as usize;
    if w0 != data.len() - C1A_FOOTER_TAG_LEN {
        return Err(format!(
            "C1A header word 0 mismatch: 0x{w0:08X} != 0x{:08X}",
            data.len() - C1A_FOOTER_TAG_LEN
        ));
    }
    if &data[0x20..0x24] != b"QSPI" {
        return Err("C1A missing QSPI magic at 0x20".into());
    }
    let entry_size = read_u32_le(data, 0x28) as usize;
    let num_files = read_u32_le(data, 0x2C) as usize;
    if entry_size != 32 || num_files == 0 {
        return Err(format!(
            "C1A invalid TOC header: entry_size={entry_size}, num_files={num_files}"
        ));
    }

    let crc_off = data.len() - C1A_TRAILER_LEN;
    let mut entries = Vec::with_capacity(num_files);
    for i in 0..num_files {
        let base = 0x30 + i * 32;
        let file_off = read_u32_le(data, base + 16) as usize;
        let file_len = read_u32_le(data, base + 20) as usize;
        let csum = read_u16_le(data, base + 24);
        entries.push((file_off, file_len, csum));
    }

    for i in 0..entries.len() {
        let (off, len, stored_csum) = entries[i];
        let phys_off = 0x20 + off;
        let region_end = if i + 1 < entries.len() {
            0x20 + entries[i + 1].0
        } else {
            crc_off
        };
        if phys_off + len > region_end {
            return Err(format!("C1A TOC[{i}] overflows region end"));
        }
        let calc_csum = sum16le(&data[phys_off..region_end])?;
        if stored_csum != calc_csum {
            return Err(format!(
                "C1A TOC[{i}] sum16le mismatch: stored=0x{stored_csum:04X}, calc=0x{calc_csum:04X}"
            ));
        }
    }

    let stored_crc = read_u32_le(data, crc_off);
    let calc_crc = crc32_ieee(&data[..crc_off]);
    if stored_crc != calc_crc {
        return Err(format!(
            "C1A trailer CRC-32 mismatch: stored=0x{stored_crc:08X}, calc=0x{calc_crc:08X}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_ieee_standard_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn test_c0a_roundtrip() {
        let c0a = package_c0a(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01]).unwrap();
        assert_eq!(c0a.len(), C0A_HEADER_LEN + 16 + C0A_FOOTER_LEN);
        verify_c0a(&c0a).unwrap();
    }

    #[test]
    fn test_c1a_roundtrip() {
        let c1a = package_c1a(&[0x11, 0x22, 0x33, 0x44], &[0xAA, 0xBB, 0xCC, 0xDD]).unwrap();
        assert_eq!(c1a.len(), C1A_TOTAL_LEN);
        verify_c1a(&c1a).unwrap();
    }

    #[test]
    fn test_idm1_rejects_bad_vectors_and_sram_collision() {
        let mut bad_msp = vec![0u8; 0x40];
        write_u32_le(&mut bad_msp, 0x00, 0x2001_0000); // Collides with Core 0 SRAM!
        write_u32_le(&mut bad_msp, 0x04, 0x0110_0041);
        assert!(verify_idm1_binary(&bad_msp).is_err());

        let mut bad_reset = vec![0u8; 0x40];
        write_u32_le(&mut bad_reset, 0x00, 0x200A_91C8);
        write_u32_le(&mut bad_reset, 0x04, 0x0110_0040); // Missing Thumb bit!
        assert!(verify_idm1_binary(&bad_reset).is_err());

        let mut good = vec![0u8; 0x40];
        write_u32_le(&mut good, 0x00, 0x200A_91C8);
        write_u32_le(&mut good, 0x04, 0x0110_0041);
        assert!(verify_idm1_binary(&good).is_ok());
    }
}
