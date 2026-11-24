# DISPLAY.md — Panel Link & Core 1 Bring-Up: Verified Findings

Trace session 2026-11-22 (display-link detour, extended). All addresses are in the
Ghidra program `App1_Main_decrypted.bin` unless noted. Verification = read-through of
decompiled bodies / instruction dumps / ROM pool-cell scans.

## TL;DR

1. The BMC-side serial link to the panel Sub-CPU (STM32G0) is the **SCI block at
   `0x4002_6000`** (control word), with its **pinmux at `0x4002_67xx`**, a tick-rate
   word at `+0x30`, and a TX-DMA doorbell at `+0x200`. It is driven by **App0**
   (panel scanner), **App0UPD** (`= Upd Sub Cpu =` update mode), the **mask ROM serial
   bootloader**, and Core 1's `Core1DevRegSetup`. Core 0's App1 image never touches it.
2. **No code in the vendor images writes SYSCTRL `0x4000_0000`.** Every Ghidra
   "xref" to `0x4000_0000` in App1/idm1/sdram1 is a **soft-float literal** (high word
   of double `2.0` = `0x4000_0000`, e.g. `__hardfp_pow`, `DpfTanbDispatcher`) or an
   SDRAM controller config value. Instruction scans of `C0_ITCM_KERNEL`, App0, App0UPD
   and BOOTSTUB for `movw/movt`-built `0x4000_00xx` values returned **zero hits**.
   Core 1 boots autonomously (mask ROM, QSPI chip 1 → ITCM `0x0110_0000`); Core 0's
   role is mailbox publication + DMA loading, not a reset-release write.

## 1. BMC → STM32G0 serial hop

### 1.1 SCI MMIO block (verified register facts)

| Address | Evidence | Semantics |
|---|---|---|
| `0x4002_6000` | named `g_dwMmioSciWd40026000` | SCI control word. App0UPD Sub-CPU-update init `FUN_OVL_APP0UPD::60002616`: `|= 0x110` (enable bits 4+8) then ~5000-iteration settle delay. |
| `0x4002_6010` bit0 | mask ROM `0x0120_3BF6`: pool cell `0x0120_3C50` = `0x4002_6030`, `subs r0,#0x30` → base, `ldr/orr/str [r0,#0x10] #1` | ROM serial-bootloader SCI enable (the "re-flash via serial bootloader commands" path). |
| `0x4002_6030` | named `g_dwClkRegWd40026030` | timer tick-rate (units/second) word: `App0TimerArm 0x0100_3728` computes `window_us * (reg/1_000_000)`. 29 xrefs incl. boot ROM + App1 `6001cbb6`, `60022xxx`. |
| `0x4002_6200` bit3 | App0 `0x0100_37B6`: `[r2,#0x200] \|= 8` where r2 = SCI base via pool `0x0100_39F0` | TX kick/doorbell, paired with DMA submit `bl 0x0100_8C24` using a 7-word param block on stack: word0 = `0x10C0_0000 \| (len16-1)`, word1 = channel code (pool `0x0100_3A00`), word2 = src ptr, word3 = `[SCI+4]` (device/DMA id). |
| `0x4002_6704`–`0x4002_67F0` | App0UPD handshake `FUN_OVL_APP0UPD::6002298`: `FUN__60023326(&DAT_4002_67xx, mode)` for ~40 words | **pinmux**: per-pin function words, modes `0x800/0x801/0x802/0x806/0xC01` (AF-select encoding). This is the SCI/pin group setup used by Sub-CPU update mode. |

Consumers: App0 band `0x0100_0C68/0x0100_30F0/0x0100_35A8/0x0100_376C` (panel link +
protocol), App0UPD `0x6000_2616/0x6000_5574/0x6003_052C`, boot ROM `0x0120_3BF6`,
BOOTSTUB, idm1 `0x0112_286C`, sdram1 `Core1DevRegSetup 0x6103_238C` + `0x6100_138x` +
`6108_16DA/16F4` (`[ctx+0x248] |= /&= 0x8000` — core1-side SCI state flag).
**Core 0 App1 (this image's app band) has zero SCI xrefs** — the panel link is
App0/core1-owned; App1 talks to it via the kit/MsgSlot/Periph12 path documented in
`docs/core0_tkernel_boot_tree.md` (display-link trace section).

### 1.2 Protocol sketch (App0 packet core; partial)

- App0 packet handlers: `0x0100_3144` (status/ID check: compares two state words via
  pools `0x0100_3450/0x3454/0x3458` → state/dev pointers; mismatch → alert `0x0100_5FD8`),
  `0x0100_3174` (main handler: command byte bound `cmp #0xF0` → invalid-id error
  `~0x10`; per-command bit tests `[r6]>>1`), `0x0100_35F0` (frame gate:
  `[ctx+4] == 0x400` → update-block path; uses `[ctx+0x1EC]` bit31, `[ctx+0x38]`).
- Panel event facts (from `docs/core0_boot_and_memory_architecture.md` §5): 2 ms
  matrix scan in the G0; press/release packets streamed over the link; logical button
  IDs parsed from lookup table `0x602CABA4`; boot-mode bitmask SRAM cell inspected by
  App1 cold-boot (`0x4002_6040` strap is hardware-only, *not* G0-provided).
- Update mode (`= Upd Sub Cpu =`, `[EXIT]` at cold boot): App0UPD overlay owns the
  transfer: pinmux init (`0x4002_67xx` words above), SCI enable (`|=0x110`), chunk
  framing gate `0x400`-byte blocks.

### 1.3 App0 packet band decode + TX mechanism deep-dive (2026-11-22, extended trace)

**Band `0x0100_30F0-0x0100_39F0` is NOT a packet builder.** Full disassembly shows it is
T-Kernel core + the SCI `Dev*` device primitives: `DefineIrqVector 0x0100_3174` (the
`cmp #0xF0` bound is the **IRQ vector index bound**, not a command byte), `DispatchUnlockKernel
0x0100_3144` (scheduler unlock), `DevNotBusyProbe 0x0100_35F0` (device busy flags for device ids
`0x400`/`0x401`), `DevChannelInit 0x0100_3994`, `DevDmaStreamWrite/Read 0x0100_37A2/0x37F4`,
`DevData60Read 0x0100_3772`. This **corrects the §1.2 sketch** above (the "command byte bound"
reading was wrong).

**There is no ARM-side "write LCD text" packet with header/line/cursor/checksum.** Verified
data flow (three separate rails, none framed as text packets):

1. **Text grid rail (App1 -> DSP-side coprocessor, NOT the G0).** `DrawTextRow 0x60096BF8`
   blits ASCII into the 53-col grid `g_abDispBuf0 0x60962A34` (stride `0x35`), sets
   `g_bDispDirty 0x602CC114`. Shipped by `ArmPlayWorker 0x60096C84` ->
   `thunk_PostTimerCmd 0x0100_FC4E (grid base, 0x73f, 0x20)` (BMC timer-port post). The
   panel/LCD is owned by the **DSP-peripheral-side coprocessor**; the grid is a raw char cell
   buffer, no framing bytes.
2. **Command rail (App1 -> kernel relay -> Periph12 window).** `DisplayRecIssueSvc 0x6002198C`
   builds an 8-byte record `{0x01 lead, 0x48 cmd, 0x00 sub, 0x00 pad}` ->
   `DispCmdEncodeSvc = MsgSlotResetPostK5 0x0101_270C` (tagged msg-slot post, tag byte `0x05`,
   4 bytes) -> `DispCmdIssueSvc = KitWordRelayDispatch 0x0101_100A` (nibble remap: low nibble
   `4..8` -> `+0x10` class 1, `9` -> `+0x20`, `10` -> `+0x30`; so cmd `0x48` -> kit cmd `0x14`;
   gate on disp-ctl SRAM view `0x2000_0450` word != 0 for cmd `0x9`; `KitEnqArm` enqueue loop)
   -> completion via `Periph12ResultPoll2W 0x0101_137C` polling status word
   `0x4112_1028 >= 2`, result words `0x4112_202C/0x4112_2030`. Command block window:
   `Periph12CmdBlkLoad 0x0101_12EC` clears `0x4112_2000..2024` pairs + `0x4112_2028..2034`,
   stores a **6-word cmd block** to `0x4112_2000..2014`, `_d2f()` D2 barrier, aux word
   `[nA+0x20] -> 0x4112_201C`. No checksum/line/cursor fields exist at this layer either.
3. **G0 serial rail (SCI raw stream only).** The App0 `Dev*` layer moves **unframed 32-bit word
   streams** over the SCI; framing/interpretation lives in the G0 firmware (not extractable,
   §1.4). Press/release event packets are G0-originated.

**TX mechanism (question 2) = both: DMAC window inside the SCI block + a 16-bit data port:**

- `DevDmaStreamWrite 0x0100_37A2` (TX): doorbell `[SCI+0x200] |= 8` (at `0x0100_37B6`), then a
  7-word record: w0 = `0x10C0_0000 | ((len>>5)-1)` (32-bit word count field + config bits
  28/23/22), w1 = request vector literal (pool `0x0100_3A00` = `0x0918_0000` TX;
  `0x0100_3A04` = `0x0A18_0000` RX), w2 = src buffer, w3 = `[SCIdev+4]` device id (`0x400` =
  DMAC mode, `0x401` = DMA mode), w4-6 = 0. `DevDmaStreamRead 0x0100_37F4` is the mirror.
- **`0x0100_8C24` = `DmaCh0RecordApply`** (ctx `0x0100_8CE4`: byte0 = channel, word1 = SCI base):
  stores w0..w3 into the SCI-embedded DMA channel block at `SCI + 0x10 + 0x10*ch`
  (CHCTRL `+0x10`, count/status `+0x14`, periph/ptr `+0x18`, mem/ptr `+0x1C`) and w4..w6 into
  selector words `SCI + 0x90 + 0xC*ch`, `+4`, `+8`.
- Channel start `0x0100_8C60`: `CHCTRL |= 0x8000_0000` (EN). Stop `0x0100_8C74`:
  `&= ~0xA000_0000`. Completion dispatcher `0x0100_8C88`: reads status code
  `countReg bits 18:16` (codes 1..5) + `CHCTRL` bit25 (direction) -> calls
  `ctx[0x8]` (code-5 leg) / `ctx[0xC]` (codes 1-4 leg) handlers, clearing bits 18:16.
- **Direct data port**: `SCI+0x60` = 16-bit FIFO window. Bulk RX drain =
  `ItcmCmdExtDev6Bulk 0x0100_8784` loops 32x `DevData60Read 0x0100_3772` (`ldrh [SCI+0x60]`);
  the TX push twin is `0x0100_3A94` (called with the buffer at `0x0100_87A8`).
- Control legs: `0x0100_377E` `[SCI+0x10] |= 0x40_0000`; `0x0100_378A` `[SCI+0x10] = 0x10`
  (mode 1) / `0x20` (mode 2). `DevChannelInit 0x0100_3994`: `+0x1C0` reset-toggle pulse
  (`|1`, `~1`, `|1`, `|6`), `DevTimerSetPeriod 0x0100_3910(0xd7)`, `+0x50 &= ~0xF | 6`,
  `+0x10 |= 0x100`, `+0x40 &= ~5`, `+0x44 &= ~0x7F`; DMAC device (`id 0x400`): `+0x40 |= 0x18`;
  DMA device (`id 0x401`): `+0x1F8 = 0xFFFF`.
- **No TX-status-register polling loop exists in App0 for panel data**: TX = DMA record +
  doorbell (`+0x200` bit3); the `+0x60` halfword port is used by the kernel IPC bulk commands.

### 1.3b Payload-level trace (2026-11-22, caller follow-up)

- **`ArmPlayWorker` is a FILLER, not a sender**: `thunk_PostTimerCmd 0x0100_FC4E` is a
  byte/word **fill** primitive — `ArmPlayWorker 0x60096C84` calls it with
  `(g_abDispBuf0 0x60962A34, 0x73f, 0x20)` = word-fill of the grid with `0x20` SP
  (`0x20202020` pattern), then sets the dirty byte. **The grid is a shared-memory
  display buffer consumed by reading, not by sending**: the panel-side coprocessor reads
  `0x60962A34` directly (SDRAM-visible). **The 2x16 LCD payload = the raw ASCII cells**:
  line 0 = `base+0x00..0x0F`, line 1 = `base+0x35..0x6A` (grid 53 cols wide, stride
  `0x35`), each cell one byte masked `& 0x7F`, printable range `1..0x7D` else SP.
- **`DevDmaStreamWrite` callers = wave streaming, not display**: the only 2 callers are
  `McDevXferStreamWrite 0x0100_48B0` (legs at `0x493C/0x49A2`); its payload buffers come from
  the media descriptor at `0x0104_1E40` (5 x 32-bit data ptrs `0x61AB_89B0..` = sdram1 wave
  region, ops table `+0x14`: slot-engage/ready/cal/compat/collect/**xfer-write**/slot-write/
  tail), i.e. kit **sample chunk** transport.
- **`0x0100_3A94` = `DevLaddPairReadGuarded` (INBOUND)**: reads the device ladder state pair
  (packed `lo | hi<<16`) behind a busy guard; its 15 callers are the
  `ItcmCmd*` kernel IPC handlers (command ids 8/12/13/16/17/24/33/55/70/77/87/105/106) which
  service **device status/event requests** — `ItcmCmdExtDev6Bulk 0x0100_8784` drains the 32
  halfword event RAM at `SCI+0x60`. These carry G0 -> BMC events, not text.
- **`Periph12CmdBlkLoad` payload = 6-word command block + 1 aux** from the object:
  `[obj+0x00..0x14] -> 0x4112_2000..2014`, `[obj+0x20] -> 0x4112_201C`, `_d2f()` barrier;
  result words `[obj+4..8] <- 0x4112_202C/2030`, completion `0x4112_1028 >= 2`.
  Display-record trio payloads (all `{0x01, 0x48, sub}` records): `DisplayRecIssueSvc
  0x6002198C` (sub 0: commit + copy 10 wire words from `ScbXferObjC` fields `0x00..0x24` ->
  disp-rec table `0x6095B7C0` stride `0x50` at `rec+0x38/0x58`), `DisplayRecRetireSvc
  0x60021A38` (sub 1: retire, status word `>=2`), `DisplayRecConfirmSvc 0x60021B72`
  (sub 4: confirm). The 10 wire words = **kit row context** (row idx, flags, sample addr,
  len, loop pts via `TempoCurveDivCalcSvc`) — verified from sole caller
  `KitSampleMovePair 0x6007CBB8`. Not text.
- **Core-1 side**: `Core1BootModeSvc 0x6100_1380` writes the full SCI pinmux
  (`0x4002_6704..0x4002_67F0` AF words `0x800/0x801/0x802/0x806`), `Core1DevRegSetup
  0x6103_238C` = `DspDrvInit(); SCI |= 0x31;` then **parks in a dead loop** — Core 1 only
  initializes the SCI and hands it over. `idm1 0x0112_286C` is a tick-rate literal
  (`DspMicrosStampPair` reads `SCI+0x30` for microsecond stamps), not a consumer.
- No ARM image reads `0x4112_2000` (writer-only) and no ARM image contains an SCI TX
  framing loop with header/checksum for display data: **the G0-facing frame assembly that
  carries the grid lives in the DSP-peripheral-side firmware (not in the ARM vendor
  images).**

### 1.4 Not resolved (open work)

- Exact Sub-CPU packet framing/CRC-checksum bytes: the G0-side firmware is required.
  **The G0 image is not extractable from the shipped vendor files**: `RPG69_C0A_up.bin`
  payload is encrypted (C0 container), a Cortex-M0+ vector-table scan
  (SP ∈ `0x2000_xxxx`, reset ∈ `0x0800_xxxx` thumb) returned **zero candidates**, and
  no plaintext `Sub CPU`/`STM32` markers exist in the container.
- Suggested path to the exact protocol: extract the G0 image from a live device
  (SWD on the G0), or dump the Sub-CPU update traffic on a bench serial tap; the
  App0UPD band `0x6000_2298–0x6000_5FFF` (overlay `OVL_APP0UPD_60000000`) is the
  core0-side counterpart to decode framing (chunk=0x400, command words, acks).

## 2. Core 0 → Core 1 bring-up (Rust port notes)

### 2.1 What the vendor images actually do

1. **Loading is DMA-table driven** (documented `docs/core0_boot_and_memory_architecture.md`
   §4, §167): the loader services Core 1 with `idm1.bin → ITCM 0x0110_0000`
   (32-byte zero header + vector table at `0x0110_0020`), IPC block at
   `0x0114_0000`, `sdram1.bin → 0x6100_0000`. QSPI chain: Core 1/IDM1 continues on
   **flash chip 1** (`docs/toolchain_fingerprint_ac5.md` ~line 2870).
2. **Core 0 publishes the inter-core IPC mailbox** (verified bodies):
   - `KernelMainEntry 0x0100_8F28`: `g_pIpcCore1MboxHdr (0x60F0_0000) = 0x60F0_0008`
     (head self-link), `g_pIpcCore1MboxTail (0x60F0_0004) = 0x6100_0000`.
   - `InitIpcKernelPrep 0x0100_6044`: stores anchor `0x60F0_0008` into
     `g_pIpcCore1AnchorPtr (0x2000_03E0)`; clamps base ≥ `0x6000_0000`; registers the
     IPC mailbox handler entry `0x0100_5DAC` at ITCM word `0x0100_031C`.
   - `PoolZoneInitIpc 0x0100_544E`: carves the T-Kernel shared pool from
     `[0x60F0_0000 .. min(*0x60F0_0004, 0x6100_0000)]`.
   - `g_pC1IPCBase (0x0114_0000)` is consumed on the core1 side at `0x0110_0496`;
     App1 CRT0 (`0x6000_0052/0x6000_005C`) seeds it.
3. **Runtime core0↔core1 control** = KernelCtl block `0x4110_0000`:
   busy word `0x4110_0034`, DMA doorbell chan port `+0x40` / ack strobe `+0x50`
   (`Periph110DmaDoorbellLoop 0x0101_185E`), DSP IRQ-mask mirror `0x4110_0200`
   (idm1 `DspIrqNvicEnableAll 0x0112_DA8C` mirrors `g_dwDspIrqMaskWdBAC` there).
   `DspBootKick 0x0112_27E0` (kick word = 1 at core-A IPC base `0x4000_2030` /
   core-B `0x4003_1030`) targets the **DSP peripheral** IPC, not core 1's reset.

### 2.2 SYSCTRL conclusion

- `SYSCTRL 0x4000_0000..0x4000_0034`: **no verified writer exists in App1, C0 kernel,
  App0, App0UPD, BOOTSTUB, idm1, sdram1.** Every apparent xref resolves to
  soft-float constants (`0x4000_0000` = high word of double 2.0) or SDRAM-controller
  data values (`FUN_6005_03BC` writes it as a config word to `0x411c_E044`-style
  SDRAM block regs).
- Therefore Core 1's release from reset is performed by the **mask ROM / hardware**
  boot chain (it begins executing its QSPI-loaded ITCM image without an App1 write);
  the "Core 0 wakes Core 1 via SYSCTRL" line in
  `docs/core0_boot_and_memory_architecture.md` §6 describes the *custom patched-C1
  takeover scenario*, not a verified vendor instruction sequence.
- For mc-1r1: replicate (a) the loader DMA entries for idm1/sdram1/IPC, (b)
  `KernelMainEntry`-style mailbox publication at `0x60F0_0000/04/08` +
  `0x2000_03E0`, (c) KernelCtl busy/doorbell protocol at `0x4110_0000`. If a true
  software core1 re-reset is ever needed (warm restart), the SYSCTRL bitmask must be
  found by probing on hardware — it is **not recoverable from these images**.
- Clocking words seen on the SDRAM-controller path (not SYSCTRL):
  `0x4005_4000` (`g_dwUsbHwWinWd40054000`-named cell; boot ROM
  `0x0120_0170-0x0120_3AC6` + App0 `0x0100_342E-0x0100_3E5C` + idm1 `0x0112_2B74`
  heavy users) — the `0x4005_xxxx` window is used by boot ROM/App0/init; PLL offsets
  for the Rust port should be read from the boot ROM band `0x0120_0170` sequence.

## 3. Quick register card (for `mc-1r1/shared/src/memmap.rs`)

```
SCI_BASE          = 0x4002_6000   # control word (update-mode init sets bits 0x110)
SCI+0x10          = boot/tx enable bit0 (mask ROM serial bootloader)
SCI+0x30          = tick rate (units/sec) word
SCI+0x200 bit3    = TX-DMA kick
SCI+0x10          = control (mode set legs =0x10/0x20; |= 0x400000; |= 0x100 init)
SCI+0x10+0x10*ch  = SCI-embedded DMA channel block: CHCTRL (EN bit31, stop &= ~0xA0000000),
                    +0x14 count/status (code bits 18:16), +0x18/+0x1C ptr pair
SCI+0x40/0x44/0x50 = init-clear words (&= ~5 / ~0x7F / &= ~0xF then |6; id-0x400 |= 0x18)
SCI+0x60          = 16-bit data FIFO window (32x pop loop = ItcmCmdExtDev6Bulk; TX push 0x0100_3A94)
SCI+0x90+0xC*ch   = DMA selector words x3 (record w4..w6)
SCI+0x1C0         = reset-toggle (pulse |1, ~1, |1, |6 at DevChannelInit)
SCI+0x1F8         = id-0x401 run word (init 0xFFFF; bit0/bit1 run-stop legs)
PINMUX_BASE       = 0x4002_6704   # per-pin AF words up to 0x4002_67F0, modes 0x800/0x801/0x802/0x806/0xC01
DMA_SUBMIT_FN     = App0 0x0100_8C24 = DmaCh0RecordApply (7-word block; w0 = 0x10C0_0000 | ((len>>5)-1),
                    w1 = req vector 0x0918_0000 tx / 0x0A18_0000 rx, w2 = buffer, w3 = dev id)
DMA_START_STOP    = App0 0x0100_8C60 (CHCTRL |= 0x8000_0000) / 0x0100_8C74 (&= ~0xA000_0000)
DMA_IRQ_DISPATCH  = App0 0x0100_8C88 (status bits 18:16 codes 1..5, dir = CHCTRL bit25)
KIT_RELAY         = App0 0x0101_100A KitWordRelayDispatch (cmd nibble remap 0x48 -> 0x14 class 1)
PERIPH12_WIN      = 0x4112_2000 (6-word cmd block + _d2f barrier), status 0x4112_1028,
                    result 0x4112_202C/0x4112_2030
DISP_GRID         = g_abDispBuf0 0x60962A34 (53 col, stride 0x35) via ArmPlayWorker timer-port 0x73f/0x20
IPC_MBOX_HDR      = 0x60F0_0000   # head self-link @ +8
IPC_MBOX_TAIL     = 0x60F0_0004   # = 0x6100_0000
IPC_ANCHOR_PTR    = 0x2000_03E0   # = 0x60F0_0008
C1_IPC_BASE       = 0x0114_0000
KERNELCTL_BASE    = 0x4110_0000   # +0x34 busy, +0x40 chan, +0x50 ack, +0x200 irq-mask mirror
DSP_IPC_COREA     = 0x4000_2000   # kick word @ +0x30 (DSP peripheral, not core1)
DSP_IPC_COREB     = 0x4003_1000
SYSCTRL_BASE      = 0x4000_0000   # NO verified vendor writer; bitmask unknown (probe on hw)
```

## 4. Boot-trace answers for the hello-world (2026-11-22, opencode-ghidra, reply to IPC `1795309377-antigravity-1r1-2250751`)

### 4.1 Q1 — readers of the grid, and the boot splash path

- **No ARM-side readers.** Full-program (39 blocks, all overlays incl. Core-1 images)
  Ghidra xrefs + App1 raw-blob LE-dword scan + exhaustive PC-relative T1/T2/adr literal
  scan: every reference to `g_bDispDirty 0x602C_C114` (8 xrefs) and `g_abDispBuf0
  0x6096_2A34` (4 xrefs) is WRITE/DATA/PARAM — sources are only `DrawTextRow 0x6009_6BF8`,
  `ArmPlayWorker 0x6009_6C84` (space-filler), and pool cells `0x6002_FD30/34`.
  The grid consumer is the external coprocessor (SHARC → G0), not any ARM task.
  Your "write grid + set dirty" model is complete on the ARM side.
- **`0x6004_F1C4` splash is dead data in v182.** `"        Ver.%4s"` and `"Roland MC-101   "`
  (`0x6004_F1D4`) have **zero** xrefs, zero LE-dword hits, zero PC-relative literal hits in
  any loaded image. The live version line uses a second copy `"Ver.%4s"` at `0x6003_625C`,
  referenced only by `ModeNameCellGateSvc 0x6003_5F54` (a computed-call island, 0 static
  callers) — i.e. it is a normal grid draw, not a distinct boot banner renderer.
- **Minimal render call path (earliest/simplest):**
  `DrawTextRow 0x6009_6BF8(grid, row, str)` — writes 16 bytes per row into
  `0x6096_2A34+0x00` / `+0x35` (stride 0x35), masks bytes `&0x7F`, sets dirty byte →
  ← `EmitDisplayText 0x6009_6930` (snprintf via `SetFormatSlot 0x6009_6920`, then 2× DrawTextRow = rows 0/1) →
  ← display task at `0x6006_0B4C` (which also kicks `ArmPlayWorker` at `0x6006_0B14`).
- For `mc-1r1`: writing printable ASCII (`0x21..0x7D`, consumed as `&0x7F`, printable 1..0x7D)
  into `0x6096_2A34` and storing a nonzero dirty byte is sufficient. There is no ARM drain
  task to service and no one-time ARM init beyond the Core-1/SCI bring-up in §2.

### 4.2 Q2 — who loads idm1.bin / sdram1.bin (the CORE1 loader)

- The loader is a **previously-undecoded App1 code island at `0x600A_A458`** (no Ghidra
  function, zero static callers → entered via computed call/pointer table; its string band
  `0x600A_A5C8..0x600A_A658` has zero static xrefs, all refs are `adr`-relative).
- Decoded flow (boot mode gate `[ctx+0x40] & 0x3F == 0x28` selects a fast path):
  1. log `"%08d: CORE1 Boot"` (fmt callee `0x6004_076A`).
  2. build load descriptor `{ src 0x0C00_0000 (QSPI chip-1), dst 0x0100_0000 (ITCM), … }`
     + `"idm1.bin"` name; open/read via `0x6003_9434` (open, must return 2) / `0x6003_94D4`
     (read with 16-byte name, ASCII-clamped 0x20..0x7F); copy/verify via `0x6009_A64A`
     and compare byte count vs stat size.
  3. same descriptor leg for `"sdram1.bin"` → dst `0x6100_0000`; log
     `"%08d: CORE1 Load Image"`.
  4. size-mismatch on either leg → **infinite self-branch halt at `0x600A_A552`**.
  5. handshake: `EventFlagSet(0x0A)`-style post (`0x6001_9C84`) + spin pair
     `0x6001_A8FA` → logs `"CORE1 Boot:QueueIsReady"`, then `0x6001_A904` →
     `"CORE1 Boot:Core1IsReady"`; tail-jump `0x6001_E864`.
- The chip-1 image header entry at `0x0C00_0050` = `"sdram1.bin"` is the QSPI image
  directory the DMA-table loader (docs/core0_boot_and_memory_architecture.md §4) consumes;
  QSPI-view firmware logs `"PDMAC LLI Chain"` (`0x0C03_E8BF`) and `"aDSP%c VerifyProg()"`
  (`0x0C03_EE03`) — the SHARC program verify path. No mask-ROM pre-copy: Core-0 App1
  performs the payload copies via the PDMAC descriptor ring (§4.3).
- `CORE1 Version : %d.%02d` at `0x0110_1BCC` / `0x0C00_2BEC` is printed by the Core-1
  ITCM image itself after boot.

### 4.3 Q3 — complete one-time init write list (Core-1 side)

- `Core1DevRegSetup 0x6103_238C` (veneer `Core1DevRegInitVen 0x0113_11C6`) is exactly:
  `DspDrvInit(); SCI 0x4002_6000 |= 0x31; while(1);` (park loop — Core-1 device mode).
- `DspDrvInit 0x0112_C2A0` (alias `0x6107_DF1C`):
  `g_dwDspInitCntB98[0]` clamp-to-0; `g_dwDspFlagWordBA0[0] = 0x2000_0000`;
  `g_abDspListStateB94[0]` byte0 = 0; `SlotEventCfgExt` slots 8..13 → ISR `0x0112_C289`,
  slot 0x17 → `0x0112_C295`; `g_dwDbgHaltCtlE412 = 0x20`; `NVIC_ISER |= 0x4000_0` (IRQ 18);
  clear 32 PDMAC descriptors (32× word triplet at `0x200A_78A4` stride 0x10 +
  `g_dwDspDescDstA8/LinkAC/CtlWdB0` arrays, CtlWdB0 = 0x0C00_0000-family mask ops);
  `g_dwDspListCtlB90[0] = 0x8001`.
- `Core1BootModeSvc 0x6100_0C40` (veneer `0x0113_11B4`): `g_dwSCB_ACR |= 0xF0_0000`;
  `g_bSysDbgFlag20080094 = (g_dwDspCtl54028 << 31) sign-bit (bit6 leg)`; then **124
  `StoreWordLane(addr, af)` pinmux writes** — full list (deduped; `0xC01` block =
  QSPI/data-bus AF, `0x80x` = default/alt):
  - `0x4002_67C0=0x801, 67C4=0x801, 67C8=0x800, 67CC=0x800, 67D0=0x802, 67D4=0x802,
    67D8=0x802, 67DC=0x806, 67E0=0x801, 67E4=0x801, 67E8=0x801, 67EC=0x801, 67F0=0x801`
  - `0x4002_6704..6758` (all, skip 675C-6767) `=0x801`, `6768=0x801` (one leg `0xC01`)
  - `0x4002_6780..67B8 =0xC01`, `67BC=0xC01` (one leg 0x801)
  - `0x4002_6600..6638 =0x801`, `6640/6648/664C/6650/6654 =0x801` (6654 leg `0xC01`)
  - `0x4002_65B0..65EC (stride 4) =0xC01`
  (full 124-call list in Ghidra: literal pool `0x6100_10A8..`, callee `0x6107_D472`.)

## 5. App0UPD 16x2 LCD text + backlight trace (2026-11-22, reply to IPC `1795311644-antigravity-1r1-2305789`)

### 5.0 Sources used

- Real App0UPD image: `vendor/mc101_sys_v182/extracts/sector0/App0_UPD/App0_UPD_decrypted.bin`
  (0x30850 bytes, loads flat at `0x6000_0000`; file offset = addr − `0x6000_0000`).
- BMC boot ROM: `vendor/mc101_sys_v182/dumps/bootrom_full_16k.bin` — mapped at core-0
  `0x0100_0000` (verified: veneer targets `0x0100_1A3F..0x0100_7A39` land inside it).
- Bootloader SRAM agent: `part2_app0_bootloader.bin`; its RAM-0 image = SRAM `0x2000_0000`
  (Ghidra overlay `OVL_BOOTSTUB_00000000`; RAM 0 = file offset `0x14CC`). The agent is a
  copy of the ROM service set (byte-identical fragments vs ROM `0x0100_3AF8..`).
- App0 kernel band `0x0100_0000..` inside `App1_Main_decrypted.bin` (RAW_OFF `0x311C3C`).

### 5.1 Q1 — text output path (string literal → terminal MMIO)

Full chain, verified:

1. **Strings** live in App0UPD's own data band: `"@==== update ==== "` `0x6000_1EDB`,
   `" Up C0-LOD/UPD/APP ...  "` `0x6000_0F90/F C0/1000` (16-char fixed lines),
   `"Updating Loader.../Updater/App0/Data0/UserArea0/App1/Data1/Wave0/Wave1"`,
   `"update error   !!/update finished/no update file"` `0x6000_1BC4..1D50`.
   They carry **no in-image pointer**: consumed through the App0UPD printf family.
2. **printf core (in-image)**: vfprintf conversion dispatch `0x6000_04B0`
   (`%n/%p/%f/%e/%g→0x6002_1F14/2_1EE/2_3642`, `%x→0x6002_21C6`, `%s→0x6002_1F9A`,
   `%c→0x6002_1F8A`); conversions buffer text into the `tPrintfCtrl` output buffer at
   `[ctrl+0x24]` (epilogue `0x6002_1F76`). Format strings seen: `"@CRC_RESULT = %x"`,
   `"BMC head:%x size:%x "`, `"%d. FILE LIST %c\n\n"` — the `@`-prefixed lines are log
   commands on the same byte stream.
3. **Console sink = kernel service veneers**: App0UPD calls kernel/agent services through
   the 10-byte veneer table `0x6002_32C0..0x6002_3660`
   (`movw ip,#lo; movt ip,#hi; bx ip`): e.g. `0x6002_32CC→0x0100_2C52` (kernel boot:
   `strd (0x6000_058D, 0x14), [sp,#0x28]` registers App0UPD's 20-entry SVC handler blob
   into the kernel), `0x6002_3326→0x2000_0200` (SRAM-agent PinModeSet),
   `0x6002_33F8→0x2000_0374`, `0x6002_3416→0x2000_01F2`,
   `0x6002_3312→0x0100_1742` (Core-1/SDRAM bring-up, control block
   `{0x21, base=0x6100_0000, size=0x800_0000, ...}` — twin of the §4.1 loader).
4. **Kernel panel-link band** `0x0100_0C68..` (DISPLAY §1.1 "consumers"): verified as a
   64-byte-unrolled `ldrb/strb` text copy loop with `lsl #4` (×16) row indexing — i.e. the
   kernel assembles the **16-column × 2-row text image** (line stride 16) and ships it.
5. **Terminal MMIO = BMC SCI block `0x4002_6000`** (the §1.3 `Dev*` layer; no other
   peripheral touches text): TX = DMA record programmed into the SCI-embedded channel
   block `SCI+0x10 (CHCTRL) / +0x14 (count/status) / +0x18 (periph) / +0x1C (mem)`,
   selectors `SCI+0x90+0xC*ch`, start `CHCTRL |= 0x8000_0000`, completion = status
   bits 18:16, plus doorbell `[SCI+0x200] |= 8` (App0 `0x0100_37B6`); 16-bit halfword
   data port `SCI+0x60` (`ldrh/strh`).
6. **Byte/packet format**: raw 8-bit ASCII bytes, 16-char fixed panel lines, `@`-prefixed
   log commands, `\n\r` terminators in the console strings. **No ARM-side header/checksum
   framing exists** (§1.3/§1.3b confirmed); framing lives in the STM32G0 firmware, which
   is shipped only inside `RPG69_SUB_up.bin` update packages (file table entry
   `0x6000_0F60`) and is **not extractable** from the vendor dumps (§1.4).

Boot ROM console/SCI services (all in the 16 KiB dump, core-0 `0x0100_xxxx`):
`0x0100_3A7E` bit-RMW, `0x0100_3B18/3B20/3B28` set/clr/get, `0x0100_3B6E` SCI timeout
`SCI+0x18 = 0x0960_0011` (rx) / `0x2580_0011` (tx), ±`0x10` legs, poll `SCI+0x1C`;
`0x0100_2958` baud (udiv `[0x4010_B000]/1_000_000`, result×divisor → `[0x4001_015C]`);
`0x0100_39BC..0x3A78` SCI pin-group init (`0x4002_6478..`, `0x4002_6580..` word sweeps);
`0x0100_3B30` system reset (AIRCR key at `0xE000_E000+0x14`); `0x0100_3AF4` vector-set
via VTOR. ROM cold-boot `0x0100_00FC..0x0100_01C8`: SPI init `(0x4001_0000, 0x200)`,
`bl 0x0100_3A7E(0x4002_6030, -1, 0, 0x00B7_1B00)`, **boot-mode strap = bits 3:1 of
`[SCI+0x40]`** (decoded modes 4/6/8/0xA; 0xA = the update/loader path), sets
VTOR (`0xE000_ED08`) = `0x0105_0000`, then polls mailbox `[0x4001_0150]`.

### 5.2 Q2 — backlight enable

**No ARM-side backlight register exists.** Exhaustive MMIO-pool scans:
- App0UPD image: 460 MMIO pool words; **zero** GPIO words in `0x4002_2xxx` and zero PWM.
  The `0x4000_1000/0x4000_1020/24/28/2C` pool at `0x6000_03C8..` (consumers
  `0x6000_0198/0x6000_01BA/0x6000_0236`) is the **embedded boot-flash SFC driver**
  (cmd reg `[+0x24]=0x1800_0000/0x1A00_0000/0x0800_0000/0x0A00_0000`, len `[+0x20]`,
  busy poll `[+0x24]&0x7_0000==0x5_0000`) — *not* GPIO (correction of the naive reading).
- Bootloader (0x3F80) + boot ROM: GPIO-family words (`0x4000_1000/0x4000_1010`) appear
  only in pin/clock setup, never as panel/backlight writes.

Conclusion: the backlight is driven **inside the STM32G0 panel firmware** (it lights the
panel when the G0 enters its display/boot mode, e.g. via the SCI line state and the
`SCI+0x40` boot strap path the ROM reads at `0x0100_0128`). The only BMC-side lever
visible in stock code is driving that strap/link, not a GPIO. mc-1r1 cannot enable the
backlight by an ARM register write; the vendor mechanism = G0 boot-mode behavior + G0-side
SCI traffic.

### 5.3 Q3 — minimal init for panel text in a standalone (App0UPD-like) context

App0UPD's own update-mode panel init (beyond what the ROM/bootloader already did):
1. **Pinmux via agent** `bl 0x6002_3326 (pin_word, af)` (= SRAM agent `0x2000_0200`,
   ROM twin `0x0100_39BC..`): sweeps —
   `0x4002_65B0..0x4002_6654` (0x4002_6600 ± 0x50 group, two AF sets incl. `0xC01`),
   `0x4002_6660/666C/6678/6684` (`0x4002_6690` − 0x30/0x18/0xC/+4... AF `0x800/0xC01`),
   `0x4002_6668/6690..66A8`, `0x4002_66B4/66BC` (`0x4002_66B8 ±4`, AF `0/1/0x803`),
   `0x4002_6A48 = 1`, `0x4002_6558`, `0x4002_6228+0x20` (field 9, value 0/1),
   `0x4002_6220/6588/6688` group, and the §4.3 `0x4002_6704..67F0` table (same AF words
   Core-1 programs).
2. **SCI enable**: `SCI 0x4002_6000 |= 0x110` (bits 4+8) at the §1.1 init site
   `FUN_OVL_APP0UPD::6000_2616`, then local settle delay loop (~5000 iters; tick source
   `[SCI+0x30]` Hz-word, µs stamp = `[SCI+0x30]` / 1e6, helpers `0x6002_8C80/8C96`).
3. **Channel/timeout**: timeout words `SCI+0x18 = 0x0960_0011/0x2580_0011`, poll
   `SCI+0x1C` (ROM `0x0100_3B6E`); baud via ROM `0x0100_2958`; channel init per §1.3
   `DevChannelInit` (`+0x1C0` reset pulse, `+0x10 |= 0x100`, `+0x40 &= ~5 / |= 0x18`,
   `+0x44 &= ~0x7F`, `+0x50 &= ~0xF | 6`, `+0x1F8 = 0xFFFF`).
4. **TX**: DMA record + `CHCTRL |= 0x8000_0000` + doorbell `[SCI+0x200] |= 8` (§1.3) or
   halfword pushes to `SCI+0x60`.

App0UPD boot function body (`0x6000_1D60..0x6000_1E3E`) additionally: kernel boot
(`0x6002_32CC`), agent flash ops `bl 0x6002_32D6 (0x4002_6000, 0x21/0x11)` → SRAM agent
`0x2000_0218` (command post: `[p+0x1C] |= 2`, submit `0x226C`-family, poll-complete
`0x2282`), Core-1/SDRAM bring-up `0x6002_3312`, key-label/panel registrations
(`0x6000_6C7C` family, ioctl `0x52/0x53`), then `"@==== update ==== "` printf.

## 6. Exact G0 wake/backlight + banner TX sequence (2026-11-22, opencode-ghidra, reply to IPC `1795313427-antigravity-1r1-2362478`)

> **CORRECTION to §5**: the 16×2 text/backlight path is **NOT** SCI `0x4002_6000`.
> SCI is the flash-download / sub-CPU-update link (baud helper ROM `0x0100_2958`
> writes `[0x4010_B000]` clock + `SCI+0x30` divisor → `[0x4001_015C]`; timeout words
> `SCI+0x18 = 0x0960_0011/0x2580_0011` polled at `SCI+0x1C` via ROM `0x0100_3B6E` —
> all flash-link side, plus boot-time SCI command posts `0x6002_32D6(0x4002_6000, 0x21/0x11)`
> → SRAM agent `0x2000_0218`). The panel text path is **serial channel 7**:
> `0x4002_1000 + 7*0x400 = 0x4002_2C00` (the block §1.3 called "WriteDspMailbox").
> No DMA is used for panel text in `App0_UPD` — every byte is a polled single push.

### 6.1 Q1: what `0x6000_6C7C` does — it is a display **text write**, not an ioctl

- `0x6000_6C7C(fd_block, col, row, str)`: dereferences the device handle
  (`fd = *0x6002_8308`, opened by the boot call `bl 0x6000_1F78`); if open, tail-calls
  `0x6A5A → 0x6000_68F6(col, row, str)`:
  - clamps `col < 16`, `row < 2`; `off = col + row*16`;
  - `strlen` (veneer `0x6002_1C18`), length clamped to `0x20 − off`;
  - copies into the **character grid FB `0x6010_79EC`** (32 B = 2 rows × 16 cols,
    ends `0x6010_7A0B`) via kernel memcpy veneer `0x6002_343E → 0x0100_6F68`;
  - sets **dirty bit 0 (text)** in the shadow state word `0x6002_82F0[+2]`.
- **Zero MMIO writes.** The whole `0x6C7C` family is RAM-model only; the MMIO happens
  later in the flush worker (§6.2).
- The `0x52/0x53/0x54` and `0x59/0x5A/0x5E/0x5F` constants around this family
  (`0x6000_6CE6..0x6DF2`) are **keypad keycodes** registered into the key table
  `0x6010_7A4C` (lookup/set helpers `0x6AD4/0x6AEC/0x6B24/0x6B74`) — *not* display
  ioctls. Related display writers in the family: `0x6C88 → 0x68A0(col,row)` cursor
  pos → shadow `+5/+6` + dirty bit1; `0x6AA4/6AB2 → 0x68C0(0/1)` cursor flash →
  shadow `+4` + dirty bit1 — also pure RAM.

Data model (RAM):

| Address | Role |
|---|---|
| `0x6010_79EC` (32 B) | character grid FB, `grid[col + row*16]` |
| `0x6002_82F0` | shadow: `+0` contrast, `[+2]` dirty mask (bit0 text, bit1 cursor, bit2 contrast, bits 8..15 icon rows, bit3 = full-flush/led), `[+4..6]` cursor flash/col/row |
| `0x6002_82F8` | command template (built by `0x6000_710A`): `+0` contrast (boot `0x26`), `+1 = 0x80` DDRA row0, `+2 = 0xC0` DDRA row1, `+3..5 = 0x80,0x0E,0x0C`, `+6 = 0x39`, `+7 = 0x70\|(con&0xF)`, `+8 = 0x54\|((con>>4)&3)` (inline code `0x6002_3556`, not a veneer), `+9 = 0x38` |
| `0x6002_8308` | display device handle word |
| `0x6010_7A5C` | channel-7 hardware config block (desc table `0x6002_3698..0x6002_36AC`). `0x6E86` fills it (`[0]=0,[2]=0,[4]=1,[6]=0x167,[8]=0x2000,[0xA]=1,[0xC]=5`) then `0x6000_3A14(7, desc, r2)` maps: desc → `0x6010_78BC[7]`, magic `[ch+0]=0xFFFF_8000` then `0`, `[ch+4]=0`, `[ch+0xC]=0x167`, `[ch+0x14]=1`, `[ch+0x18]=0x2000` (FIFO threshold), `[ch+0xC0]=1`, `[ch+0xD0]=5` (all at `0x4002_2C00+…`) |
| `0x6010_7A84` | flush worker control (timer `0x1F40`, fn slot, armed `0x6000_6F46`) |

### 6.2 Byte writer → channel 7 (the only panel MMIO)

`0x6000_6E02(byte)` = command byte (RS=0), `0x6000_6E18(byte)` = data byte (RS=1).
Both tag the transfer in the busy word `DAT_424C_4374` ('tCLB', written via
`0x6000_6DF8`: 0 = command, 1 = data) and tail-call:

```
0x6000_39B8(ch=7, byte):
  base = (ch < 10) ? 0x4002_1000 + (ch<<10) : 0x4005_3800   ; → 0x4002_2C00
  spin: ([base+0x18] & 0xFF) > 0x3C  → retry                ; TX FIFO level < 60
  str  byte, [base+0x08]                                     ; one byte per write
  spin until [base+0x04] bit31 set, then bit30 set           ; handshake (lsls/bpl loop)
```

RX probe = `[0x4002_2C18] < 0x40` (as before). Panel link needs **no baud/clock
writes** in `App0_UPD` — the link is pre-configured (boot `bl 0x6000_6E86` pushes the
config block above to the channel; boot peripheral enables at `0x6000_7260/0x729E`:
`0x4002_5028` bits 5/1,5/2; `0x6000_651C(1,0x383,1)`; `0x4002_5300` bit ops, duty
`0x64−x`; `0x4002_5240` etc. — SPCS/DMA + timer enables for the link side).

### 6.3 Wake + backlight sequence `0x6000_6F60(contrast)` (boot calls it as `0x6FFA = 0x6F60(0x26)`)

Every byte goes through §6.2 (command class). Delay helper = veneer
`0x6002_32A4 → kernel 0x0100_2BB0` (tick-based; unit ≈ 1 ms). Exact wire bytes for
boot contrast `0x26`:

```
38  ; delay 1    function set: 8-bit, 2-line, basic set
39  ; delay 1    function set: 8-bit, 2-line, EXTENDED set
14  ; delay 1    extended: internal oscillator / power (pump) enable
76  ; delay 1    extended: 0x70 | (0x26 & 0xF) — contrast low nibble / pump setting
56  ; delay 1    extended: 0x54 | ((0x26>>4)&3) — extended function set, contrast high bits
6B  ; delay 200  extended: LED drive = HIGH  ==> *** BACKLIGHT ON *** (200 tick settle)
38  ; delay 1    back to basic function set
06  ; delay 1    entry mode: increment, no shift
0C  ; delay 1    display ON, cursor OFF, blink OFF
38  ; delay 1
```

There is **no ARM GPIO backlight**: the backlight is command **`0x6B`** ("LED drive
high" in the KS0064/LD-D403A-style extended instruction set, enabled by the
`0x39,0x14,0x7x,0x5x` preamble). Contrast byte pair at boot = `0x76`/`0x56` (`con=0x26`).

### 6.4 Boot panel flow (`0x6000_1D60..0x6000_1E3E`), panel-relevant steps in wire order

```
bl 0x6000_6E86   channel-7 hardware config block push (§6.1 table, via 0x6000_3A14(7,…))
bl 0x6000_7260   link/timer/DMA (RX side) enables: 0x4002_5028, 0x6000_651C(1,0x383,1),
                 0x4002_5240+0xC0 bit1, …
bl 0x6000_729E(0x50)  link duty/speed setup (0x4002_5300-family bit RMW)
kernel boot (0x6002_32CC), SCI flash-link posts 0x6002_32D6(0x4002_6000,0x21),(…,0x11)  [NOT panel]
bl 0x6000_1F78   open display device → handle word 0x6002_8308
bl 0x6000_7220 = 0x6FFA wake sequence (§6.3, contrast 0x26 — sends 38 39 14 76 56 6B 38 06 0C 38,
                 backlight ON mid-way)
               + 0x684E: kernel memset veneer 0x6002_3434 → 0x0100_7056(grid, ' ', 0x20),
                 dirty bit0; build cmd template (0x6FFE → 0x710A); clear 8 key slots
                 (0x6AC0(0x6010_7A4C,8) = 0xFFFF); spawn flush worker
                 (veneer 0x6002_3448 → 0x0100_7A40, fn 0x6000_6B80; handle → 0x6002_8304)
bl 0x6C7C(fd, 0, 0, 0x6000_1EDC "==== update ==== ")   ; NO '@' in the wire string
bl 0x6C7C(fd, 0, 1, 0x6000_1EF6 "              ")      ; 14 spaces (row pre-cleared)
bl 0x6000_724C → 0x69D4   kick flush (no-op if worker busy; full flush 0x6000_69E0 =
                          wake(0x26)+init+icons+text+contrast; queue dispatch 0x6794)
```

`"@==== update ==== "` as logged by mc-1r1 = the debug printf layer prefixing `@`
(log stream setup `0x6000_2B78..`); the grid/grid-flush text is **without `@`**.

### 6.5 Q2: exact banner TX wire bytes (PIO, no DMA)

Text flush worker `0x6000_7004(grid)`, via §6.2 one byte at a time:

```
cmd 0x80 | data: 3d 3d 3d 3d 20 75 70 64 61 74 65 20 3d 3d 3d 3d   ; row0 "==== update ====" (16)
cmd 0xC0 | data: 20 20 20 20 20 20 20 20 20 20 20 20 20 20 20 20   ; row1 all spaces
```

(17-char `"==== update ==== "` memcpy-clamped into grid at col 0; row 1 was memset
`0x20`, then the 14-space string; wire row 0 = first 16 chars.) Prepend the §6.3 wake
bytes once at power-up — that is what wakes the STM32G0 panel controller and lights
the backlight before the text appears.

Other wire command families (same channel, same writer): icons `0x40|(k<<3)` + 8 B
CG-RAM writes (`0x6000_703C/7060`, 8 glyphs); LED-matrix `0x80|(col + row*64)` + 8 B
per row (`0x6000_70A6`, 8×8 = 64 LEDs); contrast `0x70|lo, 0x54|hi2` (`0x6000_70D4`).

**Minimal replication recipe for `mc1r1-core0` (update-mode banner):**
1. Channel 7 gate: `([0x4002_2C18] & 0xFF) ≤ 0x3C` → write byte to `0x4002_2C08` →
   wait `[0x4002_2C04]` bit31 then bit30 set.
2. Wake (1 tick between bytes, 200 ticks after `0x6B`): `38 39 14 76 56 6B 38 06 0C 38`.
3. Text: `0x80` + 16 ASCII, `0xC0` + 16 ASCII (command bytes with busy tag 0, data tag 1).
4. Keep-alive: re-flush on dirty bits; worker timer period `0x1F40` ticks.

## 7. Core-1 release sequence + SDRAM1 `0x0C00_0000` window / QSPI1 bring-up (2026-11-22, opencode-ghidra, reply to IPC `1795318041-antigravity-1r1-2413928`)

### 7.0 Map corrections (important)

- **`0x0C00_0000` is NOT a QSPI AXI window.** It is the **SMC1 MSH window onto SDRAM1**:
  descriptor block `[0x4118_6040]=0x6100_0000` (SDRAM1 phys base) /
  `[0x4118_6060]=0x6000_0000, +4 = 0x6000_0000>>3 = 0x0C00_0000` (window address, /8
  encoding) / size fields `0xC_0000` (192 KiB), `+0x10=0x3FE00`; and
  `[0x411C_0400] = 0x0C00_0000 − 0x8000_0000 = −0x5800_0000` (window offset reg).
  Window = 32 MiB (`[0x411C_0000+0x0c]=0x4002_0000`, `+0x10 = 0x4002_0000<<11`)
  (`App1 0x6003_DE52..0x6003_DF1C`, the SDRAM1 audio/wave window path).
  So the `"QSPI"` magic at `0x0C00_0020` = the **image/filesystem header living inside
  SDRAM1** (the streamed chip-1 image), not an MMIO signature.
- SFC0 (boot flash) = `0x4000_4000` (+IDMA `0x4000_1000`) — matches the App0UPD
  "SFC pool family". **SFC1 = `0x4003_C000`** (the loader's `%08d` timestamp reads
  `[0x4003_C000+0x18]`, SFC1 clock counter). **QSPI1 = `0x4003_2000`**, its DMA =
  `0x4003_0000` (App1 driver object: `[+0x28]=0x4003_2000`, `[+0x2C]=0x4264_0000`
  size, `[+0x30]=0x4003_0000`, built at `0x6001_BE9A` for device `0x48`; boot-flash
  leg device `0x41`/`0x48→0x4000_4000`).
- Core-1 (SHARC1) control on the ARM side = **SMC1 core-control block `0x4110_0000`**
  (`+0x04` config, `+0x10` CMD/STATUS bit31, `+0x30` boot address, `+0x34`, `+0x310`
  zone ctrl). SMC1 ID = `0x3002` @`0x8C00_3002`, SMC2 ID = `0x3000` @`0x9C00_3000`
  (App0UPD boot probes). SDRAM ctrl block = `0x4120_0000` (`+0xE0` reg, `+0xE4`
  busy-poll bit31, `+0xEC=0x30`).

### 7.1 Q1 — who releases Core 1

- **The loader `0x600A_A458` itself never kicks the core.** It: reads boot strap
  `[0x4002_6000+0x40]&0x3F` (`==0x28` = fast path), copies `idm1.bin`
  (`src 0x0C00_0000 window` → `dst 0x0110_0000`) and `sdram1.bin` (→ `0x6100_0000`)
  with direct CPU copy+verify (`0x6009_A64A`), size-mismatch → halt `0x600A_A552`;
  then `EventFlagSet(0xA)` (veneer `0x6001_9C84` → kernel `0x0100_2A45`), spins
  `0x6001_A8FA` (→ kernel `0x0101_0E3F` queue) = "QueueIsReady", `0x6001_A904`
  (→ kernel `0x0101_0E8B`) = "Core1IsReady", tail `0x6001_E864`. On the `!fast` leg it
  first calls `0x6001_A8F0` → kernel `0x0101_0E11` (trampoline table → `0x0102_67E8`
  → App1 computed callbacks). Between the two spins it calls `0x6002_5AAC`:
  installs kernel IRQ handler (veneer `0x6001_9C52` → kernel `0x0100_3175`, args
  `{1, fn 0x0101_0E93}`, IRQ 6) and does `strb 0x70,[0xE000_E406]`,
  **`[0xE000_E100] = 0x40`** = NVIC_ISER0 IRQ6 (Core-1 mailbox IRQ). ARM has an NVIC ⇒
  Cortex-M7-class core; the kick target is SMC1, not SYSCON.
- **The release is the SMC1 command pair at `0x4110_0010`.** App1 core-setup island
  **`0x6005_03BC..0x6005_047C`** (zero static callers; reached via kernel trampoline
  /pointer table), exact ordered writes:
  1. `[0x411C_E000+0xC]=0`; sync `bl 0x6001_99DC(1)` (→ kernel `0x0100_2751`);
     `[0x411C_E000+0x00]=0`; `+0x04 = 0x411C_E000`; `+0x08 = 0x1047_8000`;
     `+0x0C = (0x411C_E000 & ~(0x3FF<<8)) + 0x20000`; `+0x10=0`; `+0x14=0`;
     `+0x18 = 0x1047_8000`; `+0x1C=0`
  2. `[0x4118_6040]=0x6100_0000`; `+4=0x01FF_F000`; `+8=+0xC=0x000C_0000`;
     `+0x10=0x000F_FE00`; `+0x14..0x1C=0`
  3. RAM pair `= −0x7C0_0000, +4=0`
  4. `[0x411C_E060+…]` = same pattern as E000 (second stream)
  5. sync(1); `[0x4118_6060]=0x6000_0000`; `+4=0x0C00_0000`; `+8=0`; `+0xC=0`;
     `+0x10=0x0003_FE00`; `+0x14..0x1C=0`
  6. sync(1); `[0x411C_E000…]/[0x411C_E060…]` second pass; sync(1)
  7. **`[0x4110_0010] = 5`; spin while `[0x4110_0010]` bit31 clear;
     `[0x4110_0010] = 0x16`**  ← Core-1 SMC boot/release command sequence
- App1 SDRAM1-window path (audio `CPlaySdCard` fn `0x6003_DDC4`): `[0x411C_0400]=−0x5800_0000`,
  `[0x411C_0000]=0`, `+0xC=0x4002_0000`, `+0x10=0x0000_0000`; audio DMA descriptor
  rings are written to `0x411A_0000` stride `0x40` (`0x6003_DD74`).
- App0UPD (update mode — Core-1 *not* booted) does only the window/SDRAM side, in its
  boot fn (`0x6000_20FC..0x6000_21C8`): `[0x4110_0004]=0x2000_4000`;
  `[0x4110_0030]=0x21000`; `[0x4110_0034]=0x800_0000`; `[0x4110_4000+0x20..0x3C]=0`;
  `[win+0x310]=0` for windows `0x4110_0000/2000/4000/5000/6000/7000/8000/9000`
  (`0x4110_0000+0x310` cleared); `[0x4120_00EC]=0x30`; `[0x4120_00E0]=0x400000`;
  poll `[0x4120_00E4]` bit31, write `0x800002` ×8 loop, `=0x2000`, `=0x800000`;
  `[0x4110_0004]`-style config via `0x6000_3A14`-pattern; DSP coefficient block
  written into `0x4110_1820+0x58..` (SHARC coeffs via the SMC1 window).
- **No SYSCON `0x4000_0000` and no `0x4010_0040` writes exist** in ROM/bootloader/
  App0UPD/App1 (scanned) — reset release is the SMC1 command pair, not a GPIO/SYSCON bit.
- Boot strap: `[SCI+0x40]&0x3F == 0x28` selects the loader fast path (boot mode
  `0x28`); the App0 stage-2 bootloader device table (stride `0x50`, RAM `~0x1C40`
  loop) already carries a `0x4003_2000` QSPI1 entry — QSPI1 boot mode exists in App0.

**Replication for `mc1r1-core0` (exit "Wait Core 1..."):**
1. Run the §7.1 write sequence (`0x6005_03BC` list) — SMC1 streams + window descriptors.
2. Ensure `idm1.bin → 0x0110_0000` and `sdram1.bin → 0x6100_0000` copied (loader does
   this via CPU copies from the `0x0C00_0000` window).
3. Issue `[0x4110_0010]=5` → poll bit31 → `[0x4110_0010]=0x16`.
4. `EventFlagSet(0xA)` and wait for the Core1IsReady queue; IRQ6 (NVIC ISER0 bit6)
   is the Core-1 mailbox IRQ, handler kernel `0x0101_0E93`.

### 7.2 Q2 — is the `0x0C00_0000` window readable at cold boot?

- **No.** At the App0→`App1_Main` jump, SMC1 (`0x4110_xxxx`/`0x411C_xxxx`) and QSPI1
  (`0x4003_2000`) are *not* initialized by the mask ROM (ROM inits SFC0 boot flash,
  SCI `0x4002_6030`, SPI `(0x4001_0000,0x200)`, boot strap `[SCI+0x40]` only).
- **App1 initializes both** before the loader's reads:
  - window/SMC1: core island `0x6005_03BC` (+ audio window `0x6003_DF0A/DF18`),
  - **QSPI1**: device open `0x6001_BE9A` (device `0x48`; alt leg
    `0x6001_BE66 → 0x6001_C0A8(0x41)`) builds the driver object (regs
    `0x4003_2000`, size `0x4264_0000`, DMA `0x4003_0000`) → init `0x6001_FEC0`:
    gate `[0x4002_6000+0x48]&1` (SPC signal-route status); if set, for QSPI1
    channels `+4/+8/+0xC/+0x10`: `(v &~ 0xFE00) + 0x400`, then `|= 0x100`
    (prescale/xfer-count + mode-enable); if clear, only `+8/+0xC` with `+0x200`.
- The loader's `0x6003_9434` (open; must return 2) / `0x6003_94D4` (read, 16-char
  ASCII name) = FAT-file access on the QSPI1 flash via DMA `0x4003_0000`; results are
  copied by CPU (`0x6009_A64A`) into `0x0110_0000` / the `0x0C00_0000`-window'd SDRAM1.
  **No separate "DMAC1" init is needed** — `0x4003_0000` is the DMA the QSPI driver
  drives; SFC1 (`0x4003_C000`) is only used for its timestamp counter in the loader.
- The SHARC-side chip-1 loader strings (`"PDMAC LLI Chain"` `0x0C03_E8BF`,
  `"aDSP%c VerifyProg()"` `0x0C03_EE03`) live in the SDRAM1 window image = the SHARC
  PDMAC descriptor-ring streaming of `sdram1.bin` (per §4.2), consistent with
  `DspDrvInit`'s PDMAC descriptor clear (§4.3).

## 8. Minimal raw QSPI1 (`0x4003_2000`) flash-read recipe for Chip 1 (2026-11-22, opencode-ghidra, reply to IPC `1795320007-antigravity-1r1-2448279`)

### 8.1 Chip 1 flash layout — you do NOT need a FAT

Chip 1 QSPI NOR holds a **"Roland QSPI Filesystem Container"** (see
`vendor/mc101_sys_v182/extracts/RPG69_C1A/manifest.json`). Header at flash offset `0x0000_0000`:
bytes 0..3 = `"QSPI"` (this exact header is what the loader probes — the
`0x0C00_0020` probe in §7 failed only because that window shows SDRAM1, which is empty
until you fill it). `0x6003_9434` open = read 16 B at flash 0, verify `"QSPI"` +
entry-size `0x20` at +8 + file count at +0xC; `0x6003_94A8` = TOC entry = `toc + idx*0x20`
(16-char name, u32 offset, u32 size, u16 checksum).

**v1.82 fixed offsets (hardcode-able):**

| file | flash offset | size | load address |
|---|---|---|---|
| `idm1.bin` | `0x0000_1000` | `0x3B084` (241 796) | `0x0110_0000` |
| `sdram1.bin` | `0x0003_D000` | `0x389A3C` (3 709 724) | `0x6100_0000` |

All addresses < 16 MiB → plain 24-bit-address `0x03` Read Data commands cover everything.
No XIP: QSPI1 has **no AXI window** and no XIP-enable write exists in any image; the
loader = command+DMA into RAM, then CPU copy (`0x6009_A64A`) to `0x0110_0000` / `0x6100_0000`.

### 8.2 Init / pinmux / clock (Q1)

Your S2 init (= `0x6001_FEC0` writes) is **all that exists**: all three images
(App1 `0x6001_FEC0`, App0UPD `0x6000_5130`-tail, App0 stage-2 bootloader RAM `0x1820..0x18B8`)
do exactly two things — read the SPC route gate (`[0x4002_6000+0x48]`; App1 tests **bit0**,
the bootloader tests **bit8** — same structure, use whichever is 1 on your board) and
program the 4 channel registers `[0x4003_2000+0x04/0x08/0x0C/0x10]`: `(v & ~0xFE00) + 0x400`
then `|= 0x100` (gate-clear branch: only `+0x08/+0x0C` with `+0x200`). **Zero SPC AF /
pinmux writes for QSPI1 exist anywhere** — routing is automatic once the gate is set.
No clock-enable writes exist (SFC1 clocked whenever gate set).

**Device-readiness probe** (identical in App1 `0x6002_03B4`, App0UPD `0x6000_5232`, boot
`0x18F2`): after init, read SFC status **`[0x4003_2000+0x00]`**; if it is `1`, `0x20`,
or `0xC2` (cold states) send single-byte opcodes **`0x66` then `0x99`**; if it is `1`
also send **`0xF0`**. Status bit **`+0x00 bit1` = SFC busy** (checked by `0x6002_053C`
/`0x6002_04C8` entry). The 0x66/0x99/0xF0 exchanges are only needed for the boot
status-register path; a plain single-line `0x03` read works after the channel regs.

### 8.3 Raw byte-command primitive (from `0x6001_FE18(dev,opcode)`)

```
send_cmd(op):                       # one opcode byte, no data
    while [0x4003_2000+0x00] & 2: pass        # SFC busy
    [0x4003_2000+0x3a] = ([0x4003_2000+0x3a] & ~3) | 0   # bits0-1 = SPI mode (single-line=0)
    [0x4003_2000+0x4c] &= ~0x300                          # DMA disarmed
    [0x4003_2000+0x4c] &= ~0x400
    [0x4003_2000+0x3b] = 8                                # cmd class 8 (0xA if wide table entry)
    [0x4003_2000+0x38] = 1 ; [0x4003_2000+0x39] = 0
    [0x4003_2000+0x50] = op                               # FIFO: opcode byte
    [0x4003_2000+0x1c] |= 2                               # KICK
    while [0x4003_2000+0x14] & 2: pass                    # busy poll (driver timeout 0x2DC6C0)
```

### 8.4 Raw read-bytes sequence (Q2) — distilled from `0x6001_F530` transfer class 8

SFC1 register map facts (all from `0x6001_F530`/`0x6001_FE18`/`0x6003_94xx`):
`+0x00` status (bit1 busy, cold = 1/0x20/0xC2), `+0x14` bit1/2 busy, `+0x1c` bit1 = kick,
`+0x3a` bits0-1 mode, `+0x3b` phase class byte, `+0x4c` bits 0xF0 = bus class
(**0xF0 single-line, 0xC0 dual (class 0xB), 0x80 quad (class 0xD)**), bit 0x300 = DMA arm,
`+0x50` = command/address/data **FIFO byte port** (bytes written MSB-first),
`+0x04/0x08/0x0C/0x10` = 4 channel timing regs.
DMA descriptor = `[0x4003_0000 + 0x10 + ch*16]` (QSPI1 uses DMA base `0x4003_0000`,
SFC0 uses `0x4000_1000`; `ch` = devblock+0x34, 0 for the loader objects → descriptor at
**`0x4003_0010`**): `+0x00` control = **`0x1200_0000 | (class<<20) | ((len>>2)-1)`**
(class 8 = single-line, count in 4-byte lines; SFC0 base is `0x1000_0000`),
**bit31 = transfer-in-progress (set = kick, auto-clears on done)**, `+0x04` = aux
(driver writes 0, clears 0x70000 pre-kick; ubfx(+0x04,0x10,3)==5 = channel idle),
`+0x08` = RAM destination (increments), `+0x0C` = peripheral = **`0x4003_2050`**.

```
read_raw(addr, dst, len):           # len multiple of 4; keep <= 64 KiB per command cycle
                                    # (driver chunks at 256 B; 16-bit count allows 256 KiB)
    # 1) descriptor (before phases, per 0x6001_F530 0x6001_f5b2..f5c2)
    d = 0x4003_0010
    [d+0x0C] = 0x4003_2050
    [d+0x08] = dst
    [d+0x04] = 0
    [d+0x00] = 0x1208_0000 | ((len>>2)-1)      # 0x1200_0000 | (8<<20) | count-1
    # 2) mode / bus class
    while [0x4003_2000+0x00] & 2: pass
    [0x4003_2000+0x3a] = ([0x4003_2000+0x3a] & ~3) | 0
    [0x4003_2000+0x4c] &= ~0x300
    [0x4003_2000+0x4c] |=  0xF0                # single-line DMA read
    [0x4003_2000+0x39] = 0
    # 3) opcode phase
    [0x4003_2000+0x3b] = 8
    [0x4003_2000+0x50] = 0x03                  # Read Data (single-line)
    [0x4003_2000+0x1c] |= 2
    while [0x4003_2000+0x14] & 2: pass
    # 4) address phase (3 bytes MSB-first, per 0x6001_f640..f666)
    [0x4003_2000+0x3b] = 8
    [0x4003_2000+0x50] = (addr>>16)&0xff
    [0x4003_2000+0x50] = (addr>>8)&0xff
    [0x4003_2000+0x50] =  addr&0xff
    [0x4003_2000+0x1c] |= 2
    while [0x4003_2000+0x14] & 2: pass
    # 5) arm DMA and start
    [0x4003_2000+0x4c] |= 0x300
    [d+0x04] &= ~0x70000                        # 0x6001_f6a2
    [d+0x00] |= 0x8000_0000                     # kick DMA
    while [d+0x00] & 0x8000_0000: pass          # completion = bit31 auto-clear
```

The stock loader loop is `0x6001_F6F4` (splits into 256-B chunks: chunk cycle =
`0x6001_FE18(dev,6)` + `0x6001_F24E(dev,2,5)` + the block above; completion also raises
kernel sync via flags at `[0x4264_0000+0x9b0]/[0x9ac]` set by the DMA IRQ — raw poll of
`desc+0x00` bit31 replaces that). `0x6002_053C`'s 8-byte call = RDSR-class status read
(class 0x16) on the SPM devblock path `0x4001_0000+dev*72`; the raw path above bypasses
the whole SPM/devblock/kernel stack.

### 8.5 End-to-end for `mc1r1-core0`

1. S2 channel init (done). Readiness opcodes §8.2 if `[0x4003_2000]` ∈ {1,0x20,0xC2}.
2. `read_raw(0, hdr, 16)` → verify `"QSPI"`.
3. `read_raw(0x1000, scratch, 0x3B084)` → **CPU copy** scratch → `0x0110_0000`
   (loader uses CPU copies for idm1; don't DMA into the SHARC1 IDM window).
4. `read_raw(0x3D000, 0x6100_0000, 0x389A3C)` — DMA destination may be SDRAM1 directly
   (loader itself CPU-copies into the `0x0C00_0000` window = same SDRAM1).
5. `release_core1()` per §7.1 (`[0x4110_0010]=5` → bit31 → `0x16`).


## 9. Hardware-Verified Dual-Core Bring-Up Summary (2026-11-22)

Confirmed on real Roland MC-101 hardware (`"Hello, MC-1R1!"` on row 0, `"Hello from C1!"` on row 1 — exercising all 4 execution sections: Core 0 SDRAM, Core 0 ITCM0, Core 1 ITCM1, Core 1 SDRAM1):

1. **16×2 LCD + Backlight (Core 0, Ch7 `0x4002_2C00`):**
   - Backlight: `*(0x4002_676C) = 0x806`, `0x4002_5028` bits 1 & 5 = 1, `0x4002_5300` bit 1 = 0 and bit 9 = 1 (steady ON).
   - Ch7 clock & pinmux: `*(0x4002_9504) = 3`, `0x4002_657C/6578 = 0x803`, `0x4002_6574/656C = 0x800`, `0x4002_6570 = 0x806`, `bitband(0x4002_6218, 0x1B) = 1`.
   - Ch7 config: `desc[0] = 0x0147` written last; status poll on `0x4002_2C04` **bits 8 & 9 (`0x100`/`0x200`)**; RS GPIO bit-band `0x424C_4374` (`0` = cmd, `1` = data).
2. **QSPI1 (`0x4003_2000`, bit-band `0x4264_0000`) Pure-PIO `0x0B` Fast Read:**
   - Must enable controller: `*(0x4264_0000) = 1`, wait `*(0x4264_0010) != 0` (`App1 0x6001_FFAE`).
   - Push `0x0B` + 24-bit addr + dummy byte to `0x4003_2050`, set `*(0x4264_09A8) = 1`, then push a kick byte (`0x00`) to `0x4003_2050` (`App1 0x6001_FABA`) to start shifting.
   - `RPG69_C1A_up.bin` TOC `file_off` values are **relative to `"QSPI"` at `+0x20`** (`App1 0x6009_A668` adds `+0x20`). `idm1.bin` starts directly with the Cortex-M4 vector table at `0x0110_0000`; `sdram1.bin` starts directly at `0x6100_0000`.
3. **Core 1 ARM Reset Release (CORRECTION to §7.1):**
   - `0x4110_0010` is the DSP/SMC1 engine, NOT Core 1 ARM reset.
   - Actual Core 1 ARM reset release is `App1` ITCM0 kernel `0x0101_0E10` (called via veneer `0x6001_A8F0`):
     - `*(0x4005_1054) = 1`
     - `*(0x4005_1058) &= ~1`
     - `*(0x4005_4028) &= ~1` (assert Core 1 reset)
     - `*(0x4005_4028) |= 1` (release Core 1 reset)
4. **`RPG69_C0A_up.bin` Packaging:**
   - Payload length must be **16-byte aligned** for `App0_UPD`.
