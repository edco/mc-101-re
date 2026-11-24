# MC-101 USB-Storage-Mode Hardware Trace

Source images (all in `vendor/mc101_sys_v182/`):
- `extracts/RPG69_C0A/App1_Main_decrypted.bin` — core0 app, file offset = addr − 0x60000000
- `extracts/sector0/App0_UPD/App0_UPD_decrypted.bin` — recovery updater (base 0x60000000, len 0x30850)
- `extracts/sector0/part2_app0_bootloader.bin` — App0 BMC loader (file off 0x10 → ITCM 0x01000000, len 0x3F30)
- `extracts/RPG69_C1A/idm1.bin` / `sdram1.bin` — core1 DSP engine (ITCM 0x01100000+0x20 / SDRAM 0x61000000+0x20)
- `dumps/bootrom_full_16k.bin` — boot ROM (USB engine documented in `docs/core0_tkernel_boot_tree.md` #356)

This is a READ-ONLY trace; every claim below is anchored to a verifiable address.

---

## Part A — Sub-CPU (key-scan) interface

**A.1 Sub-CPU identity and update path.** The updater overlay (App0_UPD)
contains the sub-CPU firmware update strings:
`RPG69_SUB_up.bin` @0x60000F60, ` Up SUB-CPU ... ` @0x60001190, and the
per-target list (`update core0 loader/updater/app/data`, `update core1
app/data`, `update wave 0/1`, then `Up SUB-CPU`). The sub-CPU is
therefore a self-contained MCU whose firmware is streamed to it in a
separate update step — not a flash-attached device (the App0 BMC loader
at ITCM 0x01000000 is QSPI/SFDP flash-only; it contains no serial
transport code, only `SFDP`/`QRY3` flash-query strings).

**A.2 Serial transport = SSI0-class block @ 0x40060000.** In App0_UPD,
the boot bring-up routine @0x60030722 (pool @0x600307E8) does:
1. NVIC clear-pending + set-enable loop for IRQ 0..239 (0xE000E180/280),
2. vector base +8 store, 0x40020000[+0x10] |= 0x20 and [0] |= 8 (clock
   enable leg), delay 300,
3. **0x40061000[+0x50] = 1** (baud/brg lane),
4. **0x40050000[0] = (x & 0x700) | 0x0705FA00** (word-select 5,
   bitrate-select 0; the same block as the SCI9 ISP UART),
5. data/status lane pair **0x40060000[0]/[+4]** (bit 5 = rx-ready:
   `bl 0x6003069C(0/1); r0 = [0x40060004] & 0x20`),
6. priority word `0xE000EC00[0x100+4*irq]` (IPRIPL).

This is a 3-word serial channel family: channel n data @0x40060000 +
n*0x800, status @+4, baud @0x40061000+0x50. App0_UPD's pool cluster
(0x40060000, 0x40061804, 0x40060804 @0x600307F0-0x600307F8) shows all
three channels. **This is the sub-CPU link** used during
`Up SUB-CPU ...`.

**A.3 App1 runtime does NOT touch 0x40060000 directly.** A full
peripheral census of App1_Main (word scan of the 3.4 MB image) shows
zero literal words at 0x40060000/0x40060800/0x40061000, and zero
movw/movt encodings for 0x4006xxxx in the whole image. Key/panel traffic
in App1 goes through the core-to-core **bridge block 0x40062000**
(handshake word 0x8000 @+0x120/+0x128, event words [+8]/[+4], EP-kick
legs +0x118+8n, mask 0xFFFDF00F — see CHECKPOINT #253/#254) and kernel
ITCM veneers. Panel state is not addressable from the App1 image.

**A.4 Boot-mode screen.** The STORAGE MODE screen painter is
0x6006589C: it reads the boot-mode byte at `[obj+0x9F0]` and paints
`  STORAGE MODE  ` (string 0x602A7414, pool cell 0x600658F4) via the
draw svc pair 0x6003CB9E/0x6003CBB2. The `= Upd  Sub Cpu =` line
(string 0x602A7467, xref 0x60065FBC) belongs to the update screen,
together with `Erasing.../Writing.../Verifying...` (0x602A747E+).
The update worker at 0x6002596C polls device state via kernel veneer
**0x6001A0EE** (`movw ip,#0x4D2F; movt ip,#0x100` → **ITCM kernel
0x01004D2E**, thumb bit set; verified with capstone), result −1/0/1,
and drives the erase/write/verify UI states.

**A.5 Open (Part A).** No App1-visible button table exists; the earlier
"boot-button accessor" veneers 0x6001A4CC/0x6001A4D6 were re-checked
this pass and are **SysEx note-bitmap** services (ITCM 0x010103BA
`SysExNoteLaneCount` / 0x010103F4 `SysExNoteBitTest`), not keys. Key
scan → sub-CPU → SSI0 → core-side path beyond the bridge words is not
decoded in the App1 image.

---

## Part B — SD / MMC interface

**B.1 File layer in App1 (core0) is FAT-based.** App1 contains
`FAT12/FAT16/FAT32` label strings @0x6000412C (consumed by
CdcParMapConvCore 0x60003C38 as patch-name templates — coincidental),
UI strings `< Mounted SD >` 0x602A6E4F / `< No SD Card >` 0x602A6E60
(painter 0x6005AE20, mount byte at `[obj+0x4C]`, sector data `obj+0xC`),
`Format SD CARD?` 0x602AB0D4, `No SD CARD` 0x602AB1F4, `SD CARD`
0x6005005A, sector-map/dump debug views 0x6006831E+,
`CPlaySdCard::Play [%s]` 0x6003E330 (`src\sys\sd_sample_play.cpp`
0x6003D7A0). The SD sample player allocates a **0x400 sector buffer**
(ctor 0x6003E46C: `mov r0,#0x400; bl 0x60019F4A` alloc).

**B.2 Block/file access is kernel-svc mediated.** App1 file I/O goes
through ITCM-kernel veneers: `thunk_FUN_01012056` (file read, used by
`ReadDataHeaderBlock` 0x60094B54 and `ReadBlockPayloadData`
0x60094ABA — the SVZa `Data Block Header/Table` import/export layer),
and the CPlaySdCard ctor file-open leg 0x6003E474 `bl 0x600171F8` +
stream svc 0x6002C8F6/0x6002C934 (open modes 0/1). App1_Main itself
contains **no SD controller MMIO base** (census of the whole image:
no 0x4006F000/0x4006E000/SDHI-style windows, no movw/movt 0x4006Fxxx).

**B.3 SD host evidence in the ITCM kernel device band.** The C0 ITCM
kernel (loaded at 0x01000400 from App1 file 0x312B9C; the
0x01010000-0x0103xxxx veneer targets live inside this same block —
decodable from the App1 image) contains an SD-card command/capacity
service cluster at **0x01004D2E-0x01004E1A** (symbols
EXT_FUN_01004D2E/4D5C/4DA4):
- 0x01004D2E: `r0 = *(0x60311114); r0 -= 0x14; return *(r0+4)` — device
  ready/state query (the 0x6001A0EE veneer target above).
- 0x01004D5C: device object `*(0x603110E4)`; `obj[+4] == 0x400` → 1;
  `obj[+4] == 0x401` → set `obj[+0x1E4]=2`, 0.
- 0x01004D84/0x01004DA4: command-word `obj[+0x1E4]` set 2 / clear 0.
- 0x01004DB0: **SD CSD capacity decode**: gate on ready-flag `*(0x60311114)-0x10 == 1`;
  card-type nibble `[r0]&0xF`: type 2 (SDHC) → block count
  `[r2+0xA] >> 4` (22-bit CSD v2 `C_SIZE` block-count field, with
  `lsl #14` scaling path); type 0/1 (SDsc) → `C_SIZE = (ldrh[r1+0xA]
  >>7 &0x7F)+1`, `C_SIZE_MULT = ldrh[r1+0xC]>>6 &0xF` (`ubfx` pair —
  standard MMC/SD CSD v1 geometry) → capacity = (C_SIZE+1)·2^C_SIZE_MULT.
This is the core0-side proxy for the SD card. Pool cells:
device-object ptr 0x603110E4 (shared with the `McDev*` device family),
ready cell 0x60311114.

**B.4 SDHI0 base in core1 address map.** `sdram1.bin` (core1 SDRAM
engine, vaddr = off + 0x61000000 − 0x20) contains a peripheral-region
table (entries 0x4006F000, 0x400DC000, 0x40147000, 0x401B0000, mask
0xC0800000, plus a 0x3F99C000.. run of SDRAM regions) at pool
0x6115BBD8 / 0x611662B4 / 0x6116FFF0. **SDHI0 = 0x4006F000** is a real
core1-view peripheral. idm1.bin contains **no** SDHI base (word census
and movw/movt scan: 0 hits) and no FatFs strings; idm1 is the DSP audio
engine (`CORE1 Version/Build` strings, MASTER/SLAVE DSP, Legato HC).
sdram1 is DSP coefficient/patch code (its 0x401xxxxx census hits are
float constants).

**B.5 Open (Part B).** The FatFs/diskio module that owns SDHI0
0x4006F000 is not present as literal-pool code in any decoded image
(idm1, sdram1, App1, kernel copy, App0_UPD all negative). The command
object at 0x603110E4 (state 0x400/0x401, cmd word +0x1E4) is populated
by code outside the decoded bands — most consistent with a core1/boot
region owning SDHI and core0 using the kernel services. Card-detect,
CMD17/18/24/25 sequences, and SD clock/width setup are NOT localized.

---

## Part C — USB Device Controller + USB-Storage mode

**C.1 Enumeration identity (from App1 descriptor blob 0x602CA000-0x602CA820):**
- Device descriptor @**0x602CA01C**: USB 2.0, class 00, **idVendor 0x0582
  (Roland), idProduct 0x022C (MC-101)**, bcdDevice 0x0000,
  strings iManuf=1/iProduct=2/iSerial=3, 1 config.
- String descriptors (UTF-16): `Roland` 0x602CA07A, `MC-101` 0x602CA088,
  serial `000000000001` 0x602CA096.
- Boot-mode CDC-ACM configs @0x602CA034 / 0x602CA054 (len 0x20, iface
  02/02/00, bulk EP 0x06 OUT + 0x87 IN, packet 0x40 / 0x200) — matches
  boot ROM VID/PID 0x0582:0x018F/0x0190 family (docs/boot_rom_analysis.md).

**C.2 The four app-mode configurations** (all idVendor 0x0582:0x022C):
| addr | wTotalLen | ifaces | content |
|---|---|---|---|
| 0x602CA178 | 0xF9 | 4 | USB Audio 2.0 class (01/01/20) + AC 0x0350 + CS AS 0x0624/0x0101/0x0100 + GS endpoint; `MC-101` audio strings 0x602CA27A-0x602CA2D2 (`Audio OUT`/`Audio IN`/`MIDI I/O`) |
| 0x602CA324 | 0x135 | 4 | audio variant + Roland VC (0x24F1/0x1604 vendor CS) |
| 0x602CA45C | 0xAF | 3 | `BMC Sub Board USB Audio` / `Generic Audio MIDI 2.0/1.0` (factory/service, strings 0x602CA512+) |
| 0x602CA694 | 0x11A | 4 | vendor-OS set (`0x24F1 0x0100`, `0x0624 F1 04 16 00`, extended-features `0x0418 0x0144AC` MSFT-compatible-id pattern) |
| **0x602CA7B0** | **0x2D** | **4** | **USB-Storage-mode set: 4 × vendor-specific interfaces (class ff, subclass 02, protocol 01), no endpoints in this descriptor; strings `Roland`/`MC-101`/`VASVMS` @0x602CA7E6-0x602CA806** |

**C.3 VASVMS = the USB-Storage Mode function.** `VASVMS` (Roland
"MC-101 VAS Virtual Mass Storage") is string index 3 of the 4-interface
ff/02/01 configuration; the descriptor tail table @0x602CA81C
{0x602CA7E0, 0x602CA7E4, 0x602CA7F4, 0x602CA804, 0x602CA820} indexes
the string set. The companion device-qualifier-shaped template
`09 02 2D 00 04 01 00 80 FA` + 4×(09 04 n 00 00 FF FF 00 00) at
0x602CA7B0 is the config sent in storage mode. The vendor-class EP
traffic seen in the adjacent sets (bulk EP 0x0D OUT / 0x8E IN with
`0x24F1 0x1604` CS, packet 0x16/0x8, `0x0418 0x0144AC` extended
compat id) is the Roland vendor channel that hosts VAS; the MSC payload
rides that channel (the 0xff/02/01 interfaces carry no in-endpoints in
the 0x2D set itself).

**C.4 Boot→MSC init leg.** `DspCoreInitSvc` 0x60022340 (extent
0x6002340-0x600674D): nMode==1 → init gates → prints
`Start USB Device MSC.` (string 0x60026768, xref from 0x600266D6) →
`DefineIrqVector(0x14, 0xB, 2)` + NVIC IPR writes incl.
`g_bNvicIprIrq107Lane` → `DrvOpsTableInitCore` 0x60002138 →
`UsbOpenFrameCore` 0x600020E6 → `UsbSci9MaskClearSvc` 0x600228B0
(0x40050000[+0x00..+0x18] mask-clear legs, [+0x10] |= 4 — the
USB/SCI9-shared block; SCI9 is the 0x40050000 ISP UART shared with
boot). **USB IRQ set = 20 / 107 / 108 (0x14/0x6B/0x6C)** — consistent
with the boot ROM USB band (IRQ 0x14/0xCA/0xC9 code family, #356).

**C.5 Core0 driver stack (App1 band 0x60000F90-0x60002510, batch #366):**
`UsbOpenFrameCore` opens frame {dev, 1, cb 0x600022F4, ep 0x1F, size
0x1000} through **kernel veneer 0x600199E6 (USB open) / 0x600199F0
(USB xfer)**, handle stored in `g_dwOpenHdlWd6031116C`. Ops table
`g_pfnDrvOpsA-H` 0x60001EEF/1F1B/1F4F/1F69/1F83/1F9D/2039/2127 →
`DrvInitCallCore` 0x60022F90; hw windows 0x40054000 (flash ctrl),
0x40024000, 0x40026030. `UsbQueueScanCore` 0x600024B4 scans 9 queue
slots `g_dwCdc1-9Wd60AD9E88`.

**C.6 MMIO map for USB:**
- **0x40080000** `g_dwUsbBridgeBlk` — USB bridge block (App1 words
  +0x8, +0x400/4, +0x500/4, +0x600/4, +0x700/4, +0x800, +0x810, +0x818,
  +0x900, +0x910, +0xE40: 8 channel-pair windows at 0x400+n*0x100
  status/ctl + shared 0x810/0x818/0x910; 36 refs in core1 sdram1 —
  core1 services it).
- **0x40062000** — core0↔core1 IPC/bridge: handshake 0x8000 @+0x120/28,
  event words [+8]/[+4] (mask 0xFF000000), EP-kick legs +0x118+8n
  (0x8000 set/poll, mask 0xFFFDF00F), reset pair +0x160/164, SR word
  +0x204; **shared with the updater**: App0_UPD uses words 0x40062100
  (0x60016584), 0x4006211C+mask 0xFFFDF00F (0x6001FC38), 0x60062180
  (0x600153D8), 0x60062204 (0x60021890) — same protocol in recovery
  mode; boot ROM USB uses the 0x400101A0-0x400101C0 CB window
  (boot_rom_analysis.md).
- **0x40050000** — USB/SCI9 shared block: `UsbSci9MaskClearSvc`
  0x600228B0 writes +0x00/+0x04/+0x10/+0x14/+0x18 (bic 4 legs,
  +0x10 |= 4) + SRTRIM kick `DwordStoreSvc(0x40026808, 5)`.
- **0x40054028** `g_dwDspCtl54028 |= 8` = USB-busy flag.
- Boot ROM USB engine 0x01200300-0x01200C33 (`BootRomUsbIrqHdlr`
  reg-walk base 0x01203970 −0xF0, +0x28/+0xC pair, 0x8000 set/poll) —
  #356.

**C.7 Storage-mode user flow (strings).** `STORAGE MODE` boot screen
(0x6006589C painter, boot byte [obj+0x9F0]); app-side
`< Mounted SD >` / sector meters (0x6005AE20 painter, `Sector Use
%5d/%5d(Free %5d) Remain %5d` 0x60059460); PC side sees VASVMS. The
MSC payload path app→kernel: 9 CDC queue slots (g_dwCdc1-9 Wd60AD9E88)
+ 0x1000-byte transfer window per open frame (UsbOpenFrameCore).

**C.8 Open (Part C).** The UDC register block itself is not in any
core0-side pool (no 0x4008xxxx controller window beyond the bridge;
the 0x40080000 words are bridge shared-mem, not UDC registers) — the
UDC is driven from core1/bootROM via the 0x40062000 handshake +
0x40080000 bridge. The exact `DspCoreInitSvc` MSC tail fragments
(0x6002B8A-0x600674D DEFERRED sub-extents) remain per-band decoded in
the worklog, not re-verified end-to-end.

---

## Summary model

```
Buttons/keypad (sub-CPU MCU) --SSI0 0x40060000/0x40061000--> updater path (App0_UPD)
                                                        \--> (runtime: not App1-visible)
SD card --SDHI0 0x4006F000 (core1 view; owner module NOT localized)-- diskio
        --file svcs (ITCM kernel 0x01004D2E cmd/capacity band, veneers 0x6001A0EE/0x01012056)-->
            App1: SVZa Data-Block IO (0x60094B54), CPlaySdCard (0x6003E46C), mount UI (0x6005AE20)
USB --boot: bootROM CDC (0x0582:0x018F/0x0190, IRQ 0x14 family, CB window 0x400101A0)
    --app: 0x0582:0x022C; cfg 0x2D 4×ff/02/01 "VASVMS" (storage) @0x602CA7B0
           App1 band 0x60000F90 (UsbOpenFrameCore→veneer 0x600199E6/F0)
           bridge 0x40062000 + USB bridge 0x40080000, IRQ 20/107/108
```

Verifications performed this pass: veneer 0x6001A0EE → 0x01004D2E
(capstone, thumb-bit corrected); pool literals 0x60311114/0x603110E4
(ITCM file off = 0x312B9C + (vaddr − 0x01000400)); App0 BMC map
(file off 0x10 → 0x01000000, per setup_ghidra_layout.py:148); App1
peripheral census (word + movw/movt encodings); descriptor decode of
0x602CA000-0x602CA820 byte-by-byte.
