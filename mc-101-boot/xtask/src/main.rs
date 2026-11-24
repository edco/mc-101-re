//! Build, ELF verifier, `App1_Main` bootloader appender, and `MC1P` patch generator for `mc-101-boot`.

mod des3;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const TARGET_TRIPLE: &str = "thumbv7em-none-eabihf";
const C0A_HEADER_LEN: usize = 96;
const C0A_FOOTER_LEN: usize = 64;
const C0A_SDRAM_BASE: u32 = 0x6000_0000;
const C0A_ENTRY_OFFSET: u32 = 0x000C_0040;
const C0A_PAYLOAD_FLASH_OFFSET: u32 = 0x000C_0060;
const C0A_CONTROL_NO_DECRYPT: u32 = 0x0000_0000;
const C0A_APP_NAME: &[u8] = b"App1_Main";
const C0A_BUILD_TIMESTAMP: &[u8] = b"2023/05/17 22:54";
const C0A_VERSION: &[u8] = b"0.010001";
const C0A_BUILD_TAG: &[u8] = b"Roland RPG69_C0AVER.1.82-BLD.14423 0517 22 54 58";

const STOCK_APP1_LEN: usize = 0x0035_65B0;
const STOCK_HOOK_VA: usize = 0x6000_001C;
const STOCK_HOOK_EXPECTED: [u8; 4] = [0x00, 0xF0, 0xBC, 0xF8]; // bl #0x6000_0198

fn main() -> ExitCode {
    match build_and_package() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot determine mc-101-boot workspace root".to_string())
}

fn build_and_package() -> Result<(), String> {
    let root = workspace_root()?;
    let target_release = root.join("target").join(TARGET_TRIPLE).join("release");
    let dist_dir = root.join("target").join("dist");
    fs::create_dir_all(&dist_dir).map_err(|e| format!("mkdir {}: {e}", dist_dir.display()))?;

    println!("[1/4] Building mc101-boot ({TARGET_TRIPLE}, --release)...");
    run_cmd(
        Command::new("cargo")
            .current_dir(&root)
            .args(["build", "--release", "--target", TARGET_TRIPLE, "-p", "mc101-boot"]),
    )?;

    let elf_path = target_release.join("mc101-boot");
    let boot_bin_path = dist_dir.join("mc101-boot.bin");

    println!("[2/4] Extracting flat bootloader binary...");
    objcopy_bin(&elf_path, &boot_bin_path)?;
    let mut boot_raw = fs::read(&boot_bin_path).map_err(|e| format!("read boot.bin: {e}"))?;
    if boot_raw.len() < 64 {
        return Err("mc101-boot.bin unexpectedly small".into());
    }
    while boot_raw.len() % 16 != 0 {
        boot_raw.push(0x00);
    }
    println!(
        "      mc101-boot.bin: {} bytes ({:.1} KiB)",
        boot_raw.len(),
        boot_raw.len() as f64 / 1024.0
    );

    // Package standalone container (for testing SD loading directly).
    let standalone = package_c0a(&boot_raw)?;
    let standalone_path = dist_dir.join("RPG69_C0A_standalone_up.bin");
    fs::write(&standalone_path, &standalone)
        .map_err(|e| format!("write {}: {e}", standalone_path.display()))?;

    println!("[3/4] Checking for stock App1_Main_decrypted.bin to build dual-boot image...");
    let stock_path = root
        .parent()
        .unwrap()
        .join("vendor/mc101_sys_v182/extracts/RPG69_C0A/App1_Main_decrypted.bin");
    if stock_path.exists() {
        let mut app1 = fs::read(&stock_path).map_err(|e| format!("read stock App1: {e}"))?;
        if app1.len() != STOCK_APP1_LEN {
            return Err(format!(
                "Expected App1_Main_decrypted.bin len 0x{STOCK_APP1_LEN:x}, got 0x{:x}",
                app1.len()
            ));
        }
        let hook_off = STOCK_HOOK_VA - (C0A_SDRAM_BASE as usize);
        if app1[hook_off..hook_off + 4] != STOCK_HOOK_EXPECTED {
            return Err("Unexpected bytes at 0x6000_001C in App1_Main_decrypted.bin".into());
        }
        let boot_va = (C0A_SDRAM_BASE as usize) + app1.len();
        let bl = encode_bl(STOCK_HOOK_VA, boot_va);
        app1[hook_off..hook_off + 4].copy_from_slice(&bl);
        app1.extend_from_slice(&boot_raw);

        let patched_raw_path = dist_dir.join("App1_Main_boot.bin");
        fs::write(&patched_raw_path, &app1)
            .map_err(|e| format!("write {}: {e}", patched_raw_path.display()))?;

        let c0a_boot = package_c0a(&app1)?;
        let c0a_boot_path = dist_dir.join("RPG69_C0A_up.bin");
        fs::write(&c0a_boot_path, &c0a_boot)
            .map_err(|e| format!("write {}: {e}", c0a_boot_path.display()))?;
        println!(
            "      Packaged {} ({} bytes, boot stub at 0x{boot_va:08X})",
            c0a_boot_path.display(),
            c0a_boot.len()
        );

        let stock_enc_path = root
            .parent()
            .unwrap()
            .join("vendor/mc101_sys_v182/RPG69_C0A_up.bin");
        let bootrom_path = root
            .parent()
            .unwrap()
            .join("vendor/mc101_sys_v182/dumps/bootrom_full_16k.bin");
        if stock_enc_path.exists() && bootrom_path.exists() {
            let xorpatch = generate_and_verify_xorpatch(
                &stock_enc_path,
                &bootrom_path,
                &app1,
                &boot_raw,
            )?;
            let xp_dist = dist_dir.join("mc101-boot.xorpatch");
            fs::write(&xp_dist, &xorpatch)
                .map_err(|e| format!("write {}: {e}", xp_dist.display()))?;
            let mc107_docs = root.parent().unwrap().join("mc-107/docs");
            if mc107_docs.exists() {
                let _ = fs::write(mc107_docs.join("mc101-boot.xorpatch"), &xorpatch);
            }
            println!(
                "      Generated & verified {} ({} bytes, 0 vendor bytes)",
                xp_dist.display(),
                xorpatch.len()
            );
        }
    } else {
        println!(
            "      Stock App1_Main_decrypted.bin not found at {}; skipped combined image.",
            stock_path.display()
        );
    }

    Ok(())
}

fn generate_and_verify_xorpatch(
    stock_enc_path: &Path,
    bootrom_path: &Path,
    app1_boot_dec: &[u8],
    boot_raw: &[u8],
) -> Result<Vec<u8>, String> {
    let stock_enc = fs::read(stock_enc_path).map_err(|e| format!("read stock enc: {e}"))?;
    let rom = fs::read(bootrom_path).map_err(|e| format!("read bootrom: {e}"))?;
    let des3 = des3::Mc101Des3::from_bootrom(&rom, 0xB6)?;

    // Sanity check: decrypt first 32 bytes of stock_enc and verify against app1_boot_dec[0..24]
    let dec0 = des3.decrypt_ecb(&stock_enc[0x60..0x80]);
    if dec0[..24] != app1_boot_dec[..24] {
        return Err("3DES-ECB key/decrypt sanity check failed on block 0..2".into());
    }

    // Encrypt only the changed hook block (VA 0x6000_0018..0x6000_001F -> file off 0x78..0x7F)
    // and our clean-room boot_raw payload.
    let patched_block3_enc = des3.encrypt_ecb(&app1_boot_dec[0x18..0x20]);
    let mut hook_xor = [0u8; 8];
    for i in 0..8 {
        hook_xor[i] = patched_block3_enc[i] ^ stock_enc[0x78 + i];
    }
    let boot_blob_enc = des3.encrypt_ecb(boot_raw);

    // Build updated 96-byte header (keeping Word 21 = 0xB602_0001 encrypted mode!)
    let mut new_hdr = [0u8; C0A_HEADER_LEN];
    new_hdr.copy_from_slice(&stock_enc[..C0A_HEADER_LEN]);
    let new_payload_len = (STOCK_APP1_LEN + boot_raw.len()) as u32;
    new_hdr[0x44..0x48].copy_from_slice(&new_payload_len.to_le_bytes());

    // Word 20 (+0x50) is CRC-32 of the *decrypted* payload (checked by App0 after 3DES decrypt).
    let dec_payload_crc = crc32_ieee(app1_boot_dec);
    new_hdr[0x50..0x54].copy_from_slice(&dec_payload_crc.to_le_bytes());

    let mut full_enc = Vec::with_capacity(C0A_HEADER_LEN + new_payload_len as usize + C0A_FOOTER_LEN);
    full_enc.extend_from_slice(&new_hdr);
    full_enc.extend_from_slice(&stock_enc[0x60..0x60 + STOCK_APP1_LEN]);
    for i in 0..8 {
        full_enc[0x78 + i] ^= hook_xor[i];
    }
    full_enc.extend_from_slice(&boot_blob_enc);

    let mut footer = [0xFFu8; C0A_FOOTER_LEN];
    footer[0..12].copy_from_slice(&[0u8; 12]);
    footer[16..16 + C0A_BUILD_TAG.len()].copy_from_slice(C0A_BUILD_TAG);
    full_enc.extend_from_slice(&footer);
    let total_crc = crc32_ieee(&full_enc[..full_enc.len() - C0A_FOOTER_LEN + 12]);
    let crc_off = full_enc.len() - C0A_FOOTER_LEN + 12;
    full_enc[crc_off..crc_off + 4].copy_from_slice(&total_crc.to_le_bytes());
    footer[12..16].copy_from_slice(&total_crc.to_le_bytes());

    // Verify full decryption of patched encrypted image matches app1_boot_dec byte-for-byte!
    let full_dec = des3.decrypt_ecb(&full_enc[0x60..0x60 + new_payload_len as usize]);
    if full_dec != app1_boot_dec {
        return Err("Encrypted roundtrip verification failed vs App1_Main_boot.bin".into());
    }

    // Build MC1X v2 patch file (zero vendor bytes):
    //   0..4:     "MC1X"
    //   4..6:     version = 2 (u16)
    //   6..8:     reserved = 0 (u16)
    //   8..40:    stock_sha256 (32 B)
    //   40..72:   output_sha256 (32 B)
    //   72..76:   stock_payload_len (u32 = 0x003565B0)
    //   76..172:  header_xor (96 B)
    //   172..176: hook_offset (u32 = 0x78)
    //   176..184: hook_xor (8 B)
    //   184..188: boot_blob_len (u32)
    //   188..188+boot_blob_len: boot_blob_enc
    //   tail 64 B: footer
    let stock_sha = sha256(&stock_enc);
    let output_sha = sha256(&full_enc);
    let mut header_xor = [0u8; C0A_HEADER_LEN];
    for i in 0..C0A_HEADER_LEN {
        header_xor[i] = new_hdr[i] ^ stock_enc[i];
    }

    let mut xp = Vec::with_capacity(188 + boot_blob_enc.len() + C0A_FOOTER_LEN);
    xp.extend_from_slice(b"MC1X");
    xp.extend_from_slice(&2u16.to_le_bytes());
    xp.extend_from_slice(&0u16.to_le_bytes());
    xp.extend_from_slice(&stock_sha);
    xp.extend_from_slice(&output_sha);
    xp.extend_from_slice(&(STOCK_APP1_LEN as u32).to_le_bytes());
    xp.extend_from_slice(&header_xor);
    xp.extend_from_slice(&0x78u32.to_le_bytes());
    xp.extend_from_slice(&hook_xor);
    xp.extend_from_slice(&(boot_blob_enc.len() as u32).to_le_bytes());
    xp.extend_from_slice(&boot_blob_enc);
    xp.extend_from_slice(&footer);

    Ok(xp)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (idx, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[idx] = h[idx].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn encode_bl(from: usize, to: usize) -> [u8; 4] {
    let offset = (to as isize) - ((from + 4) as isize);
    let s = ((offset >> 24) & 1) as u32;
    let i1 = ((offset >> 23) & 1) as u32;
    let i2 = ((offset >> 22) & 1) as u32;
    let j1 = (!i1 ^ s) & 1;
    let j2 = (!i2 ^ s) & 1;
    let imm10 = ((offset >> 12) & 0x3FF) as u32;
    let imm11 = ((offset >> 1) & 0x7FF) as u32;
    let hw1 = (0xF000 | (s << 10) | imm10) as u16;
    let hw2 = (0xD000 | (j1 << 13) | (j2 << 11) | imm11) as u16;
    let b1 = hw1.to_le_bytes();
    let b2 = hw2.to_le_bytes();
    [b1[0], b1[1], b2[0], b2[1]]
}

fn package_c0a(raw_payload: &[u8]) -> Result<Vec<u8>, String> {
    if raw_payload.is_empty() {
        return Err("Core 0 payload is empty".into());
    }
    let mut payload = raw_payload.to_vec();
    while payload.len() % 16 != 0 {
        payload.push(0);
    }
    let payload_crc = crc32_ieee(&payload);

    let mut hdr = [0u8; C0A_HEADER_LEN];
    hdr[0x00..C0A_APP_NAME.len()].copy_from_slice(C0A_APP_NAME);
    hdr[0x10..0x10 + C0A_BUILD_TIMESTAMP.len()].copy_from_slice(C0A_BUILD_TIMESTAMP);
    hdr[0x20..0x20 + C0A_VERSION.len()].copy_from_slice(C0A_VERSION);
    hdr[0x28..0x2C].copy_from_slice(&C0A_SDRAM_BASE.to_le_bytes());
    hdr[0x2C..0x30].copy_from_slice(&0u32.to_le_bytes());
    hdr[0x30..0x34].copy_from_slice(&C0A_ENTRY_OFFSET.to_le_bytes());
    hdr[0x34..0x40].fill(0xFF);
    hdr[0x40..0x44].copy_from_slice(&C0A_PAYLOAD_FLASH_OFFSET.to_le_bytes());
    hdr[0x44..0x48].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    hdr[0x48..0x4C].copy_from_slice(&C0A_SDRAM_BASE.to_le_bytes());
    hdr[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes());
    hdr[0x50..0x54].copy_from_slice(&payload_crc.to_le_bytes());
    hdr[0x54..0x58].copy_from_slice(&C0A_CONTROL_NO_DECRYPT.to_le_bytes());
    hdr[0x58..0x5C].copy_from_slice(&0u32.to_le_bytes());
    hdr[0x5C..0x60].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

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

fn crc32_ieee(data: &[u8]) -> u32 {
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

fn run_cmd(cmd: &mut Command) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {:?}: {e}", cmd.get_program()))?;
    if !status.success() {
        return Err(format!("{:?} exited with {status}", cmd.get_program()));
    }
    Ok(())
}

fn objcopy_bin(elf: &Path, out: &Path) -> Result<(), String> {
    for tool in ["arm-none-eabi-objcopy", "llvm-objcopy", "rust-objcopy"] {
        if let Ok(status) = Command::new(tool)
            .args(["-O", "binary"])
            .arg(elf)
            .arg(out)
            .status()
        {
            if status.success() {
                return Ok(());
            }
            return Err(format!("{tool} exited with {status}"));
        }
    }
    Err("no objcopy tool found (tried arm-none-eabi-objcopy, llvm-objcopy, rust-objcopy)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des3_roundtrip_and_sha256() {
        let mut fake_rom = vec![0u8; 0x3500];
        for (i, b) in fake_rom[0x23B0 + 0xB6 * 24..0x23B0 + 0xB6 * 24 + 24]
            .iter_mut()
            .enumerate()
        {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let des = des3::Mc101Des3::from_bootrom(&fake_rom, 0xB6).unwrap();
        let pt = b"Hello MC-101 3DES!!12345";
        let ct = des.encrypt_ecb(pt);
        assert_ne!(&ct[..], &pt[..]);
        assert_eq!(&des.decrypt_ecb(&ct)[..], &pt[..]);

        // Standard SHA-256("abc") vector
        let h = sha256(b"abc");
        assert_eq!(
            h,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn bl_encoding_matches_stock_preinit() {
        // Verify encode_bl(0x6000_001C, 0x6000_0198) == [0x00, 0xF0, 0xBC, 0xF8]
        assert_eq!(encode_bl(0x6000_001C, 0x6000_0198), [0x00, 0xF0, 0xBC, 0xF8]);
    }

    #[test]
    fn c0a_header_matches_app0_layout() {
        let img = package_c0a(&[0x11u8; 32]).unwrap();
        assert_eq!(&img[0x00..0x09], b"App1_Main");
        assert_eq!(&img[0x10..0x20], b"2023/05/17 22:54");
        assert_eq!(&img[0x20..0x28], b"0.010001");
        assert_eq!(u32::from_le_bytes(img[0x28..0x2C].try_into().unwrap()), 0x6000_0000);
        assert_eq!(u32::from_le_bytes(img[0x30..0x34].try_into().unwrap()), 0x000C_0040);
        assert_eq!(&img[0x34..0x40], &[0xFF; 12]);
        assert_eq!(u32::from_le_bytes(img[0x40..0x44].try_into().unwrap()), 0x000C_0060);
        assert_eq!(u32::from_le_bytes(img[0x44..0x48].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(img[0x48..0x4C].try_into().unwrap()), 0x6000_0000);
        assert_eq!(u32::from_le_bytes(img[0x54..0x58].try_into().unwrap()), 0x0000_0000);
        assert_eq!(u32::from_le_bytes(img[0x5C..0x60].try_into().unwrap()), 0xFFFF_FFFF);
    }
}
