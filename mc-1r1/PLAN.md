# mc-1r1: Clean-Room Rust Firmware Plan for Roland MC-101

## 1. Objective & Compatibility Contract

`mc-1r1` is a Rust reimplementation of the two application firmware images for the Roland MC-101 (BMC / ESC2 dual-core ARM Cortex-M4 r0p1 SoC, `CPUID 0x410FC241`):

1. **Core 0 (`mc1r1-core0`)** — replaces `RPG69_C0A_up.bin` (`App1_Main`, host OS, UI, sequencer, MIDI, USB, SD card, Core 1 loader).
2. **Core 1 (`mc1r1-core1` + `mc1r1-core1-sdram`)** — replaces `RPG69_C1A_up.bin` (`idm1.bin` + `sdram1.bin`, real-time Zen-Core sound engine & DSP coprocessor).

**Interface Swap Goal:** Either core's Rust firmware must match the stock firmware's external and inter-core interfaces so that **either Core 0 or Core 1 can be swapped independently** with the stock v1.82 binary and the device continues to boot and function identically, with equal or better real-time performance.

---

## 2. Review of Existing Reverse-Engineering Documentation

Based on `docs/`, `8-track/8-TRACK.md`, and `tools/ghidra/worklog/` (12,392 functions in `all_fns.txt`; batches through #461 documented in `tools/ghidra/specs/` and `CHECKPOINT.md`), the following subsystems are already mapped:

### 2.1 Boot, Packaging & Memory Map (Verified)
- **Container Formats:**
  - `RPG69_C0A_up.bin`: 96-byte header (`App1_Main`, target `0x6000_0000`, Word 20 = payload IEEE CRC-32, Word 21 = `0x0000_0000` for unencrypted DMA load by `App0`/BootROM), payload, 64-byte footer (CRC-32 + 48-byte build tag). Repackaged by `tools/package_c0a_firmware.py`.
  - `RPG69_C1A_up.bin`: QSPI container (`"QSPI"` magic at `0x20`, 32-byte TOC entries for `idm1.bin` and `sdram1.bin`, `sum16le` per-file checksums, IEEE CRC-32 + 48-byte build tag). Repackaged by `8-track/tools/repack_qspi_container.py`.
- **Core 0 Entry & Memory:**
  - `App0` loads `App1_Main` into SDRAM at `0x6000_0000` and jumps to `0x6000_0001` (Thumb).
  - Stock CRT0 (`ScatterLoadAndLaunchKernel` @ `0x6000_0040`) uses `DMAC0` (`0x4000_1000`) to relocate vector table + fast kernel to Core 0 ITCM (`0x0100_0000`, 512 KiB), sets `MSP = 0x0105_0800`, sets `SCB->VTOR = 0x0100_0000`, and jumps to `KernelMainEntry` (`0x0100_8F28`).
- **Core 1 Entry & Memory (Hardware-Verified):**
  - `RPG69_C1A_up.bin` has a 32-byte container header (`0x00..0x1F`) followed by the `"QSPI"` TOC at `+0x20`. All TOC `file_off` values (`0x1000` for `idm1.bin`, etc.) are **relative to `+0x20`**, so `App1` (`0x6009_A668`) reads from `file_off + 0x20`.
  - `idm1.bin` loads directly at `0x0110_0000` (256 KiB ITCM, locally aliased at `0x0000_0000`) with the Cortex-M4 vector table starting at `0x0110_0000` (`MSP = 0x200A_91C8`, `Reset` at `+0x04`).
  - `sdram1.bin` loads directly at `0x6100_0000` (entry code at `0x6100_0000`).
  - Core 1 `.data`/`.bss`/audio buffers live in shared System SRAM (`0x2002_0000..0x200A_9000`).
  - Chip 1 QSPI NOR Flash is accessed via `QSPI1` (`0x4003_2000`, bit-band `0x4264_0000`), not an AXI XIP window (`0x0C00_0000` is an SMC1 window onto SDRAM1).

### 2.2 RTOS, Peripherals & DSP (Partially to Well Documented)
- **Core 0 RTOS:** µT-Kernel 2.00.01 (`docs/core0_tkernel_boot_tree.md`) — 47-function boot tree, TCB/ready-queue/timer/semaphore/eventflag/mailbox/cyclic/alarm modules fully mapped.
- **DMA & Crypto & CRC:** `DMAC0` (`0x4000_1000`), `DMAC1` (`0x4003_0000`), `PL080 Audio DMAC` (`0x4003_1000`), `3DES` (`0x4002_0000`), `CRC32` (`0x4010_5000`).
- **Audio Crossbar & Streaming:** `0x4002_66xx` router slots, `0x4002_62xx` bitband enables, `0x4002_6688` / `0x4002_66BC` streaming DMA FIFOs (`8-track/8-TRACK.md`).
- **Core 1 DSP & Drivers:** `CBmcDspDrv` (`0x0112_C000..0x0112_D56B`), `Core1BootInit` (`0x0110_1A1A`), `KernelCtl` (`0x4110_0000`), `BMC DSP` (`0x4006_0000..0x4007_5000`).
- **IPC, Panel Link & Display (`mc-1r1/DISPLAY.md`):**
  - `0x0114_0000..0x0114_2000+` IPC mailbox SRAM, `0x60F0_0000`/`0x60F0_0004`/`0x60F0_0008` SDRAM anchor words, `0x2000_03E0` (`g_pIpcCore1AnchorPtr`), `0x0100_031C` (`g_pfnIpcMboxHandler = 0x0100_5DAC`), `0x4110_0000` (`KernelCtl`: `+0x34` busy, `+0x40` chan, `+0x50` ack, `+0x200` irq-mask mirror), `0x4112_2000` (`Periph12`).
  - **Core 1 Boot (Hardware-Verified):** Core 0 reads `idm1.bin` and `sdram1.bin` from Chip 1 QSPI NOR (`0x4003_2000`, pure-PIO `0x0B` Fast Read after enabling `*(0x4264_0000) = 1`) into `0x0110_0000` and `0x6100_0000`, then releases Core 1 from reset via `App1` ITCM0 kernel `0x0101_0E10` (`*(0x4005_1054) = 1`, `*(0x4005_1058) &= ~1`, pulse `*(0x4005_4028)` bit 0 low then high).
  - **16×2 LCD & Backlight (`mc-1r1/DISPLAY.md` §6 & §9, Hardware-Verified):** Driven directly from Core 0 via **Serial Channel 7 (`0x4002_2C00`)** + RS GPIO bit-band **`0x424C_4374`** (`0` = cmd, `1` = data) using an ST7032-compatible command set (`38 39 14 76 56 6B(200ms) 38 06 0C 38`, row0 `0x80` + 16B, row1 `0xC0` + 16B), with Ch7 clock `*(0x4002_9504) = 3`, Ch7 pinmux (`0x4002_656C..657C`), `desc[0] = 0x0147`, status poll bits 8/9 (`0x100`/`0x200`), and backlight pinmux `0x4002_676C = 0x806` + **`0x4002_5028`** (bits 1 & 5) + **`0x4002_5300`** (bit 1 = 0, bit 9 = 1 steady ON). (`g_abDispBuf0` `0x6096_2A34` is a separate 53×35 diagnostic grid.)

---

## 3. Workspace Structure

```
mc-1r1/
├── Cargo.toml              # Workspace (thumbv7em-none-eabihf, no_std)
├── Makefile                # Self-contained build, test & packaging entrypoint
├── .cargo/config.toml      # arm-none-eabi-ld + -nostdlib
├── PLAN.md                 # This implementation plan & constraints
├── DISPLAY.md              # Verified panel/SCI/Core 1 bring-up findings
├── README.md               # Build & packaging instructions
├── shared/                 # mc1r1-shared: memory map, FPU init, IPC primitives
├── core0/                  # mc1r1-core0: SDRAM @ 0x6000_0000 + ITCM @ 0x0100_0000
├── core1/                  # mc1r1-core1: single ELF split by xtask into idm1.bin + sdram1.bin
└── xtask/                  # mc1r1-xtask: build, constraint checks, C0A/C1A packager
```

## 3.6 Drop-In Compatibility Constraints

To guarantee that **either Core 0 (`RPG69_C0A_up.bin`) or Core 1 (`RPG69_C1A_up.bin`)** can be swapped independently alongside stock v1.82 firmware, `mc-1r1` must satisfy the following constraints (items marked **[Build-Enforced]** are verified automatically by linker `ASSERT`s and `xtask`):

1. **Shared Memory Partitioning (Strict Non-Overlap) [Build-Enforced]:**
   - **Core 0 SDRAM**: `0x6000_0000..0x602C_C000` (strictly below `g_bDispDirty` `0x602C_C114`, `g_abDispBuf0` `0x6096_2A34`, and the `0x60F0_0000..0x60FF_FFFF` shared IPC pool).
   - **Core 0 ITCM0**: `0x0100_0000..0x0105_0000` (strictly below initial `MSP = 0x0105_0800`).
   - **Core 0 SRAM0**: `0x2000_0000..0x2001_FFFF` (strictly below Core 1 SRAM).
   - **Core 1 ITCM1 (`idm1.bin`)**: `0x0110_0000..0x0114_0000` (≤ 256 KiB), Cortex-M4 vector table at `0x0110_0000` (`MSP = 0x200A_91C8`), stored in `RPG69_C1A_up.bin` at `0x20 + toc.file_off`.
   - **Core 1 SRAM1**: `0x2002_0000..0x200A_9000` (strictly above Core 0 SRAM and below Core 1 initial stack).
   - **Core 1 SDRAM1 (`sdram1.bin`)**: `0x6100_0000..0x6170_0000` (strictly below wave RAM `0x6180_0000`), entry at `0x6100_0000`, stored in `RPG69_C1A_up.bin` at `0x20 + toc.file_off`.
2. **Entry & Container Contracts [Build-Enforced]:**
   - Core 0 ELF entry at `0x6000_0001` (Thumb); `RPG69_C0A_up.bin` 16-byte aligned payload, header Word 20 payload CRC-32, Word 21 `0x0000_0000` plaintext DMA mode, IEEE 802.3 CRC-32 trailer, `Roland RPG69_C0A...` build tag.
   - `RPG69_C1A_up.bin` 4 MiB QSPI container (`0x003F_FF60`), `"QSPI"` header, 4 KiB-aligned `idm1.bin` + `sdram1.bin`, `sum16le` per-region checksums, IEEE 802.3 CRC-32 trailer, `Roland RPG69_C1A...` build tag.
3. **FPU Enable Before Hard-Float Execution [Implemented in CRT0]:**
   - Both cores compile for `thumbv7em-none-eabihf` and must enable `CP10`/`CP11` in `SCB->CPACR` (`0xE000_ED88 |= 0x00F0_0000`) and `SCB->FPCCR = 0xC000_017A` in `Reset_Handler`.
4. **Boot-Time Handshakes & Peripheral Ownership:**
   - Swapping Core 1 with stock Core 0 requires Core 1 to initialize `PINMUX` (`0x4002_6704..0x4002_67F0`), `SCI |= 0x31`, `g_dwCore1DspReadyFlag` (`0x2008_00D8 = 1`), and `KernelCtl` (`0x4110_0000`) / `SEV`.
   - Swapping Core 0 with stock Core 1 requires Core 0 to publish `0x60F0_0000`/`0x60F0_0004`/`0x2000_03E0`, seed `0x0114_0000`, zero wave RAM (`0x6180_0000`), and speak `KernelCtl`/`Periph12`.
5. **Hardware Minefields:**
   - Never write `0x4002_6468`/`0x4002_646C` (triggers immediate power shutdown on MC-101).
   - Never read `>= 0x0120_4000` (Boot ROM boundary) or unmapped ITCM ranges (bus timeout/lockup).
   - Never modify QSPI0 Sector 0 (`0x0000_0000..0x000B_FFFF`, preserves `[SHIFT]` SD recovery).
6. **Read-Only Content & External Interfaces:**
   - Parse stock `RPG69_C0C` (QSPI0), `RPG69_C1C` (Chip 1 AXI `0x0C40_0000`), and SD card project formats identically.
   - Preserve USB VID/PID (`0x0582`), USB-MIDI, 20-ch USB Audio (`0x4002_66xx` crossbar & `0x4002_6688`/`0x4002_66BC` FIFOs), 5-pin DIN MIDI, and Roland SysEx/ZEN-Core parameter maps.
   - Keep Core 1 audio DMA ISRs (`PL080` `0x4003_1000`, 44.1 kHz) in ITCM with zero heap allocation.

---

## 3.5 Framework & Runtime Strategy

Because the Roland BMC (ESC2) is a proprietary SoC with no off-the-shelf vendor HAL/PAC and uses non-standard boot/vector layouts on both cores, `mc-1r1` uses a **hybrid per-core runtime strategy**:

- **Phase 0 (Bring-Up):** Zero external dependencies (`core` + `core::arch::asm!` only) on both cores to keep early hardware bring-up independent of linker/runtime assumptions.
- **Core 0 (`embassy-executor` + `embassy-sync` + `embassy-time`):**
  - Core 0 handles I/O-heavy, concurrent subsystems (SD/MMC filesystem, QSPI0 asset loading, Sub-CPU serial link, USB-MIDI/Audio, 5-pin DIN MIDI, display UI, and sequencer).
  - `embassy-executor` (hardware-agnostic) provides `async/await` with multi-priority preemption (`InterruptExecutor` for low-jitter MIDI/sequencer/IPC, thread-mode `Executor` for UI/storage) without per-task stack RAM overhead.
  - Our custom CRT0 (`0x6000_0000` entry -> ITCM `0x0100_0000` vector relocation) initializes hardware and launches the Embassy executors, backed by a custom `embassy_time_driver::Driver` on `DSP_MICROS_CNT` (`0x4010_B000`) + SysTick.
- **Core 1 (Hand-coded bare-metal Rust, optionally RTIC):**
  - Stock Core 1 runs **no RTOS**—only hard-real-time audio synthesis driven by `PL080` Audio DMAC (`0x4003_1000`) ping-pong interrupts, `IRQ22` IPC interrupts, and a `WFI` heartbeat loop.
  - Hand-coded bare-metal Rust (`no_std`, lock-free SPSC queues, direct FPU/ITCM placement) avoids fighting `cortex-m-rt` assumptions around the 32-byte zero header + `0x0110_0020` vector table and the `idm1.bin`/`sdram1.bin` split.
  - **RTIC** remains a zero-overhead option for Core 1 later if compile-time `BASEPRI` resource-sharing guarantees between `IRQ22` and `PL080` DMA ISRs are desired.

---

## 4. Phased Implementation Plan

### Phase 0: Bare-Metal Bring-Up & Hello World (**COMPLETE — HARDWARE-VERIFIED 2026-11-22**)
- **Core 0 SDRAM (`0x6000_0000`) + ITCM0 (`0x0100_0000`):** Boots from `0x6000_0001`, relocates vector table + `.itcm` to `0x0100_0000`, sets `MSP = 0x0105_0800` and `VTOR = 0x0100_0000`, enables FPU, displays `"Hello, MC-1R1!"` on physical 16×2 LCD row 0 from ITCM0, loads `idm1.bin` + `sdram1.bin` via `QSPI1` PIO (`0x4003_2000`), releases Core 1 (`0x4005_1054`/`0x4005_1058`/`0x4005_4028`), and polls `HelloMailbox` (`0x0114_0800`).
- **Core 1 ITCM1 (`0x0110_0000`) + SDRAM1 (`0x6100_0000`):** Boots from `idm1.bin` vector table at `0x0110_0000`, initializes `.data`/`.bss` in `SRAM1`, enables FPU, waits for `HELLO_MAGIC`, jumps to `sdram1_entry` at `0x6100_0000`, posts `"Hello from C1!"` to `HelloMailbox` (displayed on LCD row 1), and parks in `WFI`.

### Phase 1: Core 0 Hardware Bring-Up & Async/RTOS Executor
- Implement `SYSCTRL` (`0x4000_0000`) & PLL (`0x4005_4000`) verification/clocking.
- Implement `DMAC0` (`0x4000_1000`) fast scatter-copy and `CRC32` (`0x4010_5000`).
- Implement SysTick (1 ms) + `DSP_MICROS_CNT` (`0x4010_B000`) timebase.
- Build a lightweight `no_std` priority scheduler / RTOS executor (matching µT-Kernel's deterministic IRQ-driven preemption without requiring binary µT-Kernel ABI compatibility internally).

### Phase 2: Stock-Compatible Core 1 Loader & Inter-Core IPC
- Core 0: Load `idm1.bin` from Chip 1 / QSPI (`0x0110_0000`) and `sdram1.bin` (`0x6100_0000`), initialize `0x60F0_0000..0x60F0_0008` and `0x2000_03E0`, and release Core 1 reset via `SYSCTRL`.
- Replace `HelloMailbox` with the exact stock `0x0114_0000` ring-buffer + `0x4110_0000` (`KernelCtl`) + `IRQ22` / `0x0100_5DAC` doorbell protocol so either core can talk to the stock counterpart.

### Phase 3: Physical Display, Sub-CPU & Storage
- Implement the physical 2-line display driver (wiring `core0/src/display.rs` to the hardware MMIO / Sub-CPU link).
- Implement the Sub-CPU serial/SPI protocol (buttons, pads, encoders, pots, LEDs).
- Implement `QSPI0` (`0x4000_4000`) asset reader (`RPG69_C0C` tones/kits/init) and SD/MMC filesystem driver for projects and WAV/SMF import/export.

### Phase 4: MIDI, USB & Audio Crossbar
- 5-pin DIN MIDI UART driver.
- USB Device stack (CDC/Vendor, USB-MIDI, 20-channel USB Audio matching `0x4002_66xx` crossbar & `0x4002_6688`/`0x4002_66BC` streaming FIFOs).
- Sequencer, clip launcher, parameter state, and UI state machine.

### Phase 5: Core 1 Zen-Core DSP Engine
- `PL080` Audio DMAC (`0x4003_1000`) ping-pong double-buffered ISR in `0x2002_0000`.
- `BMC DSP` hardware accelerator driver (`0x4006_0000..0x4007_5000`) and `CBmcDspDrv` DRAM-IO.
- Direct AXI `0x0C00_0000` wavetable/patch streaming (`RPG69_C1C`), voice allocation, envelopes, LFOs, filters, MFX, and mixer.

### Phase 6: Cross-Swap & Performance Verification
- Test **Rust Core 0 + Stock Core 1** on hardware.
- Test **Stock Core 0 + Rust Core 1** on hardware.
- Profile audio callback worst-case execution time (WCET) against stock DSP budgets.

---

## 5. Key Missing Information (Must Be Discovered Before Completion)

The following hardware interfaces and binary contracts are **not yet documented** in the current reverse-engineering corpus (`docs/`, `8-track/`, `tools/ghidra/worklog/`) and must be resolved before `mc-1r1` can be completed:

1. **STM32G0 Sub-CPU Front-Panel Input Protocol (Buttons/Pads/Encoders/LEDs):**
   - The 16×2 LCD and backlight are now fully solved (`0x4002_2C00` + `0x424C_4374` + `0x4002_5300`). The remaining Sub-CPU gap is the exact framing/CRC for button/pad/knob input and LED matrix packets over `SCI` (`0x4002_6000`).
2. **Hardware Inter-Core Doorbell / IRQ Trigger:**
   - `KernelCtl` (`0x4110_0000`: `+0x34` busy, `+0x40` chan, `+0x50` ack) is mapped; exact NVIC IRQ numbers and edge-trigger semantics for `0x0100_5DAC` (Core 0) and `IRQ22` (Core 1) need verification.
3. **Stock `0x0114_0000` / `0x60F0_0000` / `0x4110_0000` IPC Message Schema:**
   - Complete command IDs, payload structs, and ring-buffer head/tail invariants between Core 0 and Core 1 (needed for single-core swap compatibility).
4. **USB Controller MMIO Map:**
   - Base address, endpoint FIFO layout, DMA descriptors, and IRQ numbers for USB-MIDI and 20-channel USB Audio.
5. **SD/MMC Card Controller MMIO Map:**
   - Base address, clock/bus-width setup, DMA, and card-detect GPIO.
6. **5-Pin DIN MIDI UART & Audio Codec (DAC/ADC) Initialization:**
   - Exact UART instance index (`0x4002_1000 + idx*0x400` or `0x4005_3800 + idx*0x400`) for DIN MIDI, and I2C/SPI/GPIO sequence for the external audio codec/DAC/ADC.
7. **Full `BMC DSP` (`0x4006_0000..0x4007_5000`) & `PL080` (`0x4003_1000`) Register Spec:**
   - Channel/voice slot layout, sample format, and hardware MFX/filter register programming.
8. **SYSCTRL (`0x4000_0000`) Warm Reset (Optional):**
   - Only needed if software-triggered Core 1 warm restart is required; not used by stock cold boot.
