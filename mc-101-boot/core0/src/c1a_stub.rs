//! Core 1 (`[c1a]`) Staging & Stock `App1_Main` `CAVE-02` Hook Installer.
//!
//! When booting stock `App1_Main` (from flash or SD) with `[c1a] image` and/or
//! `[c1a] patch` configured in `/firmware/firmware.ini`, `App1_Main` normally
//! loads Core 1 later during RTOS startup at:
//! - `0x600A_A528: bl 0x6009_A64A` (`idm1.bin` -> `0x0110_0000`)
//! - `0x600A_A548: bl 0x6009_A64A` (`sdram1.bin` -> `0x6100_0000`)
//! - `0x600A_A56C: bleq 0x6001_A8F0` (release Core 1 from reset)
//!
//! This module stages `idm1.bin` (`0x6180_0000`), `sdram1.bin` (`0x6100_0000`),
//! and/or `MC1P` patch blobs (`0x6190_0000`) before `App1_Main` starts, and
//! writes position-independent Thumb-2 trampolines into `CAVE-02`
//! (`0x602D_2B00`, untouched by `App1_Main` BSS zeroing) to apply them right
//! before Core 1 is released.

use core::arch::global_asm;

use mc1r1_shared::memmap::{CORE1_ITCM_BASE, CORE1_SDRAM_BASE};

pub const STAGED_IDM1_ADDR: usize = 0x6180_0000;
pub const C1A_TEMP_BUF: usize = 0x61A0_0000;
pub const C1A_TEMP_MAX: usize = 0x0050_0000; // 5 MiB (boot-time only; App1_Main touches >=0x6180_0000)
/// Temporary Core 1 `.pat` staging at `0x6160_0000..0x616F_FFFF` (1 MiB, untouched by `App1_Main`
/// before Core 1 release; `0x6170_0000+` is referenced by `App1_Main`).
pub const STAGED_PATCH_ADDR: usize = 0x6160_0000;
pub const STAGED_PATCH_MAX: usize = 0x0010_0000; // 1 MiB

const CAVE02_BASE: usize = 0x602D_2B00;
const APP1_LOAD_IDM1_BL: usize = 0x600A_A528;
const APP1_LOAD_SDRAM1_BL: usize = 0x600A_A548;
const APP1_RELEASE_CORE1_BL: usize = 0x600A_A56C;
const APP1_RELEASE_CORE1_FN: usize = 0x6001_A8F0;

const EXPECTED_LOAD_IDM1_BL: [u8; 4] = [0xF0, 0xF7, 0x8F, 0xF8]; // bl #0x6009a64a
const EXPECTED_LOAD_SDRAM1_BL: [u8; 4] = [0xF0, 0xF7, 0x7F, 0xF8]; // bl #0x6009a64a
const EXPECTED_RELEASE_BL: [u8; 4] = [0x70, 0xF7, 0xC0, 0xF9]; // bl #0x6001a8f0

global_asm!(
    r#"
    .syntax unified
    .thumb
    .section .rodata.c1a_stub,"a",%progbits
    .align 2
    .global _c1a_mc1p_stub_start
    .global _c1a_mc1p_stub_end
_c1a_mc1p_stub_start:
    push {{r4-r8, lr}}
    movw r4, #0x0000
    movt r4, #0x6160
1:  ldr r5, [r4], #4
    cbz r5, 9f
    ldrh r6, [r4, #6]
    adds r7, r4, #16
2:  cbz r6, 8f
    subs r6, #1
    ldr r0, [r7, #0]
    ldrh r1, [r7, #4]
    ldrh r2, [r7, #6]
    adds r7, #8
    cmp r2, #0
    bne 10f
    adds r3, r7, #4
    adds r7, r3, r1
    b 3f
10: cmp r2, #3
    bne 4f
    mov r3, r7
    adds r7, r7, r1
3:  cmp r1, #0
    beq 2b
    ldrb r2, [r3], #1
    strb r2, [r0], #1
    subs r1, #1
    b 3b
4:  cmp r2, #1
    bne 6f
    ldr r3, [r7], #4
5:  cmp r1, #0
    beq 2b
    ldrb r2, [r3], #1
    strb r2, [r0], #1
    subs r1, #1
    b 5b
6:  ldrb r2, [r7], #4
7:  cmp r1, #0
    beq 2b
    strb r2, [r0], #1
    subs r1, #1
    b 7b
8:  adds r5, #3
    bic r5, r5, #3
    add r4, r4, r5
    b 1b
9:  pop {{r4-r8, lr}}
_c1a_mc1p_stub_end:
    "#
);

extern "C" {
    static _c1a_mc1p_stub_start: u8;
    static _c1a_mc1p_stub_end: u8;
}

/// Encode a Thumb-2 `bl target` (unconditional) from `from`.
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

/// Encode a Thumb-2 `b.w target` from `from`.
fn encode_bw(from: usize, to: usize) -> [u8; 4] {
    let offset = (to as isize) - ((from + 4) as isize);
    let s = ((offset >> 24) & 1) as u32;
    let i1 = ((offset >> 23) & 1) as u32;
    let i2 = ((offset >> 22) & 1) as u32;
    let j1 = (!i1 ^ s) & 1;
    let j2 = (!i2 ^ s) & 1;
    let imm10 = ((offset >> 12) & 0x3FF) as u32;
    let imm11 = ((offset >> 1) & 0x7FF) as u32;
    let hw1 = (0xF000 | (s << 10) | imm10) as u16;
    let hw2 = (0x9000 | (j1 << 13) | (j2 << 11) | imm11) as u16;
    let b1 = hw1.to_le_bytes();
    let b2 = hw2.to_le_bytes();
    [b1[0], b1[1], b2[0], b2[1]]
}

/// Returns `true` if `0x6000_0000..` is stock `App1_Main` with the expected Core 1 loader.
pub unsafe fn is_stock_app1() -> bool {
    let b1 = core::slice::from_raw_parts(APP1_LOAD_IDM1_BL as *const u8, 4);
    let b2 = core::slice::from_raw_parts(APP1_LOAD_SDRAM1_BL as *const u8, 4);
    let b3 = core::slice::from_raw_parts(APP1_RELEASE_CORE1_BL as *const u8, 4);
    b1 == EXPECTED_LOAD_IDM1_BL && b2 == EXPECTED_LOAD_SDRAM1_BL && b3 == EXPECTED_RELEASE_BL
}

/// Parse a `"QSPI"` TOC from `C1A_TEMP_BUF` (`RPG69_C1A_up.bin`) and stage `idm1.bin` -> `0x6180_0000`
/// and `sdram1.bin` -> `0x6100_0000`, returning `Some(idm1_len)`.
pub unsafe fn parse_and_stage_c1a(len: usize) -> Option<u32> {
    let data = core::slice::from_raw_parts(C1A_TEMP_BUF as *const u8, len);
    let toc_off = if data.len() >= 0x40 && &data[0..4] == b"QSPI" {
        0usize
    } else if data.len() >= 0x60 && &data[0x20..0x24] == b"QSPI" {
        0x20usize
    } else {
        return None;
    };

    let count = u32::from_le_bytes(data[toc_off + 8..toc_off + 12].try_into().ok()?) as usize;
    let mut idm1_len = None;
    let mut has_sdram1 = false;

    for i in 0..count.min(16) {
        let eoff = toc_off + 0x10 + i * 32;
        if eoff + 32 > data.len() {
            break;
        }
        let name = &data[eoff..eoff + 16];
        let off = toc_off + u32::from_le_bytes(data[eoff + 16..eoff + 20].try_into().ok()?) as usize;
        let flen = u32::from_le_bytes(data[eoff + 20..eoff + 24].try_into().ok()?) as usize;
        if off.checked_add(flen)? > data.len() {
            return None;
        }
        if name.starts_with(b"idm1.bin\0") && flen <= 256 * 1024 {
            core::ptr::copy_nonoverlapping(
                data[off..off + flen].as_ptr(),
                STAGED_IDM1_ADDR as *mut u8,
                flen,
            );
            idm1_len = Some(flen as u32);
        } else if name.starts_with(b"sdram1.bin\0") && flen <= 0x0070_0000 {
            core::ptr::copy_nonoverlapping(
                data[off..off + flen].as_ptr(),
                CORE1_SDRAM_BASE as *mut u8,
                flen,
            );
            has_sdram1 = true;
        }
    }

    if has_sdram1 {
        idm1_len
    } else {
        None
    }
}

/// Install `CAVE-02` (`0x602D_2B00`) hooks into stock `App1_Main` SDRAM for Core 1 image/patches.
pub unsafe fn install_stock_c1a_hooks(idm1_len: Option<u32>, has_patches: bool) {
    if idm1_len.is_none() && !has_patches {
        return;
    }

    let mut cursor = CAVE02_BASE;

    if let Some(len) = idm1_len {
        let copy_idm1_addr = cursor;
        // Thumb-2 routine: copies `len` bytes from `0x6180_0000` -> `0x0110_0000`, returns `r0 = len`.
        let stub: &[u8] = &[
            0x10, 0xB5, // +0: push {r4, lr}
            0x06, 0x48, // +2: ldr r0, [pc, #24] -> +28 (0x0110_0000)
            0x06, 0x49, // +4: ldr r1, [pc, #24] -> +32 (0x6180_0000)
            0x07, 0x4A, // +6: ldr r2, [pc, #28] -> +36 (len)
            0x13, 0x46, // +8: mov r3, r2
            0x00, 0x2B, // +10: cmp r3, #0
            0x04, 0xD0, // +12: beq +8 (+24)
            0x51, 0xF8, 0x04, 0x4B, // +14: ldr r4, [r1], #4
            0x40, 0xF8, 0x04, 0x4B, // +18: str r4, [r0], #4
            0x04, 0x3B, // +22: subs r3, #4
            0xF7, 0xE7, // +24: b -18 (+10)
            0x10, 0x46, // +26: mov r0, r2 (wait: beq target was +24, let's check!)
            0x10, 0xBD, // +28: pop {r4, pc}
        ];
        // Wait: 10+2=12 (+2:14,+4:18,+4:22,+2:24,+2:26) -> mov r0, r2 is at +26!
        // Let's make it 100% exact:
        // +12: beq -> target is +26: (26 - (12+4))/2 = 5 -> 0x05, 0xD0!
        // And literals start at +32 (4-byte aligned, +30 = nop):
        // +2: ldr r0, [pc, #28] -> Align(2+4,4)+28 = 4+28 = 32
        // +4: ldr r1, [pc, #28] -> Align(4+4,4)+28 = 8+28 = 36
        // +6: ldr r2, [pc, #32] -> Align(6+4,4)+32 = 8+32 = 40
        let _ = stub;
        let exact_stub: &[u8] = &[
            0x10, 0xB5, // +0: push {r4, lr}
            0x07, 0x48, // +2: ldr r0, [pc, #28] (+32)
            0x07, 0x49, // +4: ldr r1, [pc, #28] (+36)
            0x08, 0x4A, // +6: ldr r2, [pc, #32] (+40)
            0x13, 0x46, // +8: mov r3, r2
            0x00, 0x2B, // +10: cmp r3, #0
            0x05, 0xD0, // +12: beq +10 (+26)
            0x51, 0xF8, 0x04, 0x4B, // +14: ldr r4, [r1], #4
            0x40, 0xF8, 0x04, 0x4B, // +18: str r4, [r0], #4
            0x04, 0x3B, // +22: subs r3, #4
            0xF7, 0xE7, // +24: b -18 (+10)
            0x10, 0x46, // +26: mov r0, r2
            0x10, 0xBD, // +28: pop {r4, pc}
            0x00, 0xBF, // +30: nop (4-byte align)
        ];
        core::ptr::copy_nonoverlapping(exact_stub.as_ptr(), cursor as *mut u8, exact_stub.len());
        core::ptr::write_volatile((cursor + 32) as *mut u32, CORE1_ITCM_BASE as u32);
        core::ptr::write_volatile((cursor + 36) as *mut u32, STAGED_IDM1_ADDR as u32);
        core::ptr::write_volatile((cursor + 40) as *mut u32, len);
        cursor += 44;

        // `skip_sdram1_fn`: `mov r0, r3; bx lr` (4 bytes)
        let skip_sdram1_addr = cursor;
        core::ptr::copy_nonoverlapping([0x18u8, 0x46, 0x70, 0x47].as_ptr(), cursor as *mut u8, 4);
        cursor += 4;

        let bl1 = encode_bl(APP1_LOAD_IDM1_BL, copy_idm1_addr);
        core::ptr::copy_nonoverlapping(bl1.as_ptr(), APP1_LOAD_IDM1_BL as *mut u8, 4);
        let bl2 = encode_bl(APP1_LOAD_SDRAM1_BL, skip_sdram1_addr);
        core::ptr::copy_nonoverlapping(bl2.as_ptr(), APP1_LOAD_SDRAM1_BL as *mut u8, 4);
    }

    if has_patches {
        let patch_entry = cursor;
        let stub_start = &_c1a_mc1p_stub_start as *const u8;
        let stub_end = &_c1a_mc1p_stub_end as *const u8;
        let stub_len = (stub_end as usize) - (stub_start as usize);
        core::ptr::copy_nonoverlapping(stub_start, patch_entry as *mut u8, stub_len);
        let bw_release = encode_bw(patch_entry + stub_len, APP1_RELEASE_CORE1_FN);
        core::ptr::copy_nonoverlapping(bw_release.as_ptr(), (patch_entry + stub_len) as *mut u8, 4);
        let bl3 = encode_bl(APP1_RELEASE_CORE1_BL, patch_entry);
        core::ptr::copy_nonoverlapping(bl3.as_ptr(), APP1_RELEASE_CORE1_BL as *mut u8, 4);
    }
}
