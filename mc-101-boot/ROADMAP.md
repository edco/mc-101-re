# MC-101 Bootloader, Patch SDK, and MC-107 Release Plan

> Tracking document for polishing `mc-101-boot`, building the clean-room `MC1P` v2 Patch SDK, migrating `RowdyVoyeur/mc101-rex` (`8-track`), and packaging a standalone MC-107 repository with a GitHub Pages browser patcher.

---

## Architecture & Clean-Room Invariants

1. **Zero Vendor Code in Any Distributable Artifact**:
   - **Bootloader patch**: Distributed as an XOR/copy/append patch over stock encrypted `RPG69_C0A_up.bin` (`3,499,416` stock ciphertext bytes copied verbatim; only the changed header bytes, the single 8-byte 3DES-ECB hook block at `0x78..0x7F`, our encrypted `mc-101-boot` payload, and the updated 64-byte footer are supplied).
   - **Runtime `.pat` files (`MC1P` v2)**: Never embed `expected` vendor bytes; verify target firmware using `expected_crc32: u32` and construct repetitive/template blocks (e.g., `build_extra_channels` in `patch_c1a_dsp.py`) via `Copy` + targeted `Write` records.
2. **True Memory Offsets (No `0x20` Header Confusion)**:
   - Update containers (`RPG69_C0A_up.bin` 96-byte header, `RPG69_C1A_up.bin` 32-byte header before `"QSPI"`) are parsed as `(header, body)` and stripped on load.
   - All patch addresses in `MC1P` v2 are true physical RAM addresses (`0x60xx_xxxx`, `0x010x_xxxx`, `0x61xx_xxxx`, `0x011x_xxxx`).

---

## Verified Target Memory Map (Stock `App1_Main` + `sdram1.bin`)

| Core | Region | Address Range | Size | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **Core 0** | `SDRAM0_FREE` | `0x60F8_0000..0x60FF_FFFF` | **512 KiB** | Default Core 0 mod region; within `±16 MiB` of `App1_Main` (`bl`/`b.w` reachable directly). |
| **Core 0** | `ITCM0_LO` | `0x0104_30A0..0x0105_0000` | **51.8 KiB** | Between stock ITCM0 end and MSP stack; requires 8-byte `ldr pc` veneer from `0x60xx_xxxx`. |
| **Core 0** | `ITCM0_HI` | `0x0105_0800..0x0107_FFFF` | **190 KiB** | Above initial MSP stack (`0x0105_0800`); verify unused by RTOS task stacks on hardware. |
| **Core 1** | `SDRAM1_FREE` | `0x6139_2000..0x615F_FFFF` | **~2.43 MiB** | Default Core 1 mod region (above `sdram1.bin` + relocated DSP `0x6139_1BC8`). |
| **Core 1** | `C1_STAGING` | `0x6160_0000..0x617F_FFFF` | **2.0 MiB** | Temporary `mc-101-boot` Core 1 `.pat` staging buffer (freed once Core 1 releases). |
| **Core 1** | `ITCM1_FREE` | `0x0113_B084..0x0113_FFFF` | **~20 KiB** | Top of Core 1 ITCM1 above `idm1.bin`. |

---

## `MC1P` v2 Binary Patch Format

- **16-byte Header (little-endian)**:
  - `magic: [u8; 4]` = `b"MC1P"`
  - `version: u16` = `2`
  - `record_count: u16`
  - `flags: u32` = `0`
  - `crc32: u32` = IEEE 802.3 CRC-32 of all bytes after header (`data[16..]`)
- **Records**:
  - Common 8-byte prefix: `addr: u32`, `len: u16`, `kind: u16`
  - `kind = 0` (**`Write`**, verified surgical edit): `expected_crc32: u32` (4 B) + `patch: [u8; len]` — verifies `crc32_ieee(mem[addr..addr+len]) == expected_crc32` before writing.
  - `kind = 1` (**`Copy`**, `memmove` clone): `src_addr: u32` (4 B) — copies `len` bytes from `src_addr` to `addr`.
  - `kind = 2` (**`Fill`**, `.bss` / padding): `fill_byte: u8` + `[0; 3]` (4 B) — fills `len` bytes at `addr`.
  - `kind = 3` (**`WriteRaw`**, new code/data blob): `data: [u8; len]` — writes directly to `addr` without pre-check (for empty/uninitialized memory regions).

---

## Phase 1: Polish `mc-101-boot`
- [x] **Upgrade `MC1P` to v2** in `mc-101-boot/shared/src/firmware_cfg.rs` and `mc-101-boot/core0/src/c1a_stub.rs` (`Write` with `expected_crc32`, `Copy`, `Fill`, `WriteRaw`).
- [x] **Raise limits**:
  - `PATCH_BUF` in `mc-101-boot/core0/src/main.rs`: `32 KiB` → `64 KiB` (in SRAM0).
  - `STAGED_PATCH_ADDR` in `c1a_stub.rs`: `0x602D_2C00` (`5 KiB`) → `0x6160_0000` (`2 MiB`).
- [x] **Add `[MULTI FX]` USB Mass Storage mode**:
  - Restored `usb.rs`, `msc.rs` (ping-pong buffers placed in SDRAM `0x6100_0000` so SRAM0 stays free for `PATCH_BUF`), and `embassy-*` in `mc-101-boot/core0`.
  - Initializes `p.panel.init()` at boot and samples `button::MULTI_FX`; if held, enters async `embassy-executor` USB MSC mode (requires on-device hardware verification).
- [ ] **Minimal SD-boot anti-flash-read patch** (`RPG69_C0A` `TEST MENU` CRC at `0x6004_F46C` — optional; normal runtime never reads `RPG69_C0A`, and `RPG69_C1A` is already intercepted by `c1a_stub.rs`).
- [x] **Cleanup**: Removed unused `mc-1r1` leftovers (`PLAN.md`, `USB_STORAGE_HW.md`, `DISPLAY.md`).

---

## Phase 2: Patch SDK (`mkpatch` + Linker Templates)
- [x] **Commented Patch Source Format** (`mc-107/sdk/mkpatch.py` supporting `#` / `//` comments, `write` with `expected_crc32`, `hook` to ELF symbols, `copy`, `fill`, `write_raw`).
- [x] **Linker Scripts & Symbol Headers** (`mc-107/sdk/ld/`):
  - `c0-sdram.ld` (`0x60F8_0000`), `c0-itcm.ld` (`0x0104_30A0`), `c1-sdram.ld` (`0x6139_2000`), `c1-itcm.ld` (`0x0113_B084`).
  - `stock_c0.ld` providing `PROVIDE(...)` symbols with Thumb `+1` bits.

---

## Phase 3: Migrate `8-track` (from `RowdyVoyeur/mc101-rex`)
- [x] **Incorporate latest `mc101-rex/8-track`** (`external/mc101-rex-8track/8-track/`):
  - `src/sample_chop.c` (1,433 lines), `src/button_nav.c` (249 lines), 40 new Core 0 sites, and Core 1 `ROUTING_CLAMP_PATCHES` (`0x6101_EDA6`, `0x6101_F04E`).
- [x] **Eliminate zero-pad caves**:
  - `sample_chop.c`, `button_nav.c`, `CAVE-01`, `MENU-MIDI-TBL`, `DRUM-PAD-EDIT-TBL`, `SCR-TBL-EXT`, `PAD-SND-MENU-TBL`, `SMPL-EDIT-DESCS`, `DRUM-PHASE2-HOOKS`, and `NAV-LED-TRAMP` all relocated to `SDRAM0_FREE` (`0x60F8_0000..0x60FC_3300`).
- [x] **Eliminate `0x20` offset issues** in `mc107-1.pat` and `mc-107/mc107/build.py`.
- [x] **Convert `build_extra_channels`** to `5 × Copy` (`0x610B_58A4`) + `10 × WriteRaw` for the `u16` slot IDs (zero vendor bytes).
- [x] **Generate `mc107-0.pat` (`17,900` B) and `mc107-1.pat` (`712` B) (`MC1P` v2)** and host-verify against `App1_Main_decrypted.bin` and `sdram1.bin`. (On-device hardware test pending.)

---

## Phase 4: Distributable Bootloader Patch & GitHub Pages Web Patcher
- [x] **3DES-ECB Encryptor in `mc-101-boot/xtask/src/des3.rs`**:
  - Pure-Rust 3DES-ECB verified by full decrypt roundtrip against `App1_Main_boot.bin`.
  - Emits `mc101-boot.xorpatch` (`47,388` bytes, 0 vendor bytes).
- [x] **Static Browser Patcher (GitHub Pages)**:
  - `mc-107/docs/index.html` (SHA-256 verification + local in-browser `MC1X` patching + `.pat` / `firmware.ini` downloads).
- [x] **Standalone MC-107 Repository**:
  - Initialized `mc-107/` with `README.md`, `docs/`, `patches/`, `sdk/`, and `mc107/` (zero vendor files).
