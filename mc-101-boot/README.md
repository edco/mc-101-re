# mc-1r1 — Rust Firmware Skeleton for Roland MC-101

Self-contained `#![no_std]` Rust workspace targeting both ARM Cortex-M4F cores of the Roland BMC (ESC2) SoC (`thumbv7em-none-eabihf`), including a pure-Rust container packager and constraint verifier (`xtask`) that produces flashable `RPG69_C0A_up.bin` and `RPG69_C1A_up.bin` update images with zero external script or vendor-binary dependencies.

See [PLAN.md](PLAN.md) for the full architecture, drop-in compatibility constraints, phased roadmap, and remaining open items, and [DISPLAY.md](DISPLAY.md) for verified display/panel/Core 1 bring-up findings.

## Crates

- `shared` (`mc1r1-shared`): Verified memory map constants (`memmap.rs`), FPU bring-up (`enable_fpu()`), and bring-up inter-core mailbox (`ipc.rs` at `0x0114_0800`).
- `core0` (`mc1r1-core0`): Core 0 application (`App1_Main` replacement, linked at `0x6000_0000` with ITCM vector table relocated to `0x0100_0000` and shared-SDRAM 2×16 display driver at `0x6096_2A34`).
- `core1` (`mc1r1-core1`): Unified Core 1 ELF linking both `ITCM1` (`0x0110_0000`, 32-byte zero header + vector table at `0x0110_0020`) and `SDRAM1` (`0x6100_0000`, 32-byte zero header + entry at `0x6100_0020`), with `.data`/`.bss` isolated in Core 1 SRAM (`0x2002_0000+`). Split by `xtask` into `idm1.bin` and `sdram1.bin`.
- `xtask` (`mc1r1-xtask`): Self-contained host build, ELF/binary constraint checker, and container packager.

## Building & Packaging

From inside `mc-1r1/`:

```bash
make package
```

This builds both embedded ELFs, verifies all linker and binary drop-in constraints (memory-map non-overlap, entry points, 32-byte zero headers, vector table bounds, size limits, checksums), and writes verified update containers to:
- `target/dist/RPG69_C0A_up.bin` (unencrypted Core 0 `App1_Main` container, control word `0x00000000`)
- `target/dist/RPG69_C1A_up.bin` (4 MiB Core 1 QSPI container containing `idm1.bin` and `sdram1.bin`)

To run the packager & constraint-checker unit tests:

```bash
make test
```
