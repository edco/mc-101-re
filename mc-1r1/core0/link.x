/* Linker script for Core 0 (App1_Main replacement).
 *
 * Drop-in compatibility constraints enforced here:
 * - Loaded into SDRAM at 0x6000_0000, jumped to at 0x6000_0001 (Thumb).
 * - SDRAM code/data/BSS strictly < 0x602C_C000 (below g_bDispDirty 0x602C_C114,
 *   g_abDispBuf0 0x6096_2A34, and shared IPC pool 0x60F0_0000).
 * - ITCM0 code/vectors strictly < 0x0105_0000 (below initial MSP 0x0105_0800).
 * - SRAM0 strictly within 0x2000_0000..0x2001_FFFF (never overlapping Core 1 SRAM).
 */

MEMORY
{
    SDRAM (rwx) : ORIGIN = 0x60000000, LENGTH = 0x002CC000
    ITCM0 (rwx) : ORIGIN = 0x01000000, LENGTH = 0x00050000
    SRAM0 (rwx) : ORIGIN = 0x20000000, LENGTH = 0x00020000
}

ENTRY(Reset_Handler)

SECTIONS
{
    /* Stock App0 jumps directly to 0x6000_0001 (Thumb Reset_Handler). */
    .text 0x60000000 :
    {
        KEEP(*(.text.entry))
        *(.text .text.*)
        *(.rodata .rodata.*)
        . = ALIGN(4);
    } > SDRAM

    /* Vector table + ITCM code, relocated from SDRAM load image to 0x0100_0000. */
    .itcm : ALIGN(4)
    {
        __sitcm = .;
        KEEP(*(.vector_table))
        *(.itcm .itcm.*)
        . = ALIGN(4);
        __eitcm = .;
    } > ITCM0 AT > SDRAM
    __siitcm = LOADADDR(.itcm);

    /* Initialized writable data in SDRAM (in-place). */
    .data : ALIGN(4)
    {
        __sdata = .;
        *(.data .data.*)
        . = ALIGN(4);
        __edata = .;
    } > SDRAM

    /* Zero-initialized BSS in SDRAM. */
    .bss (NOLOAD) : ALIGN(4)
    {
        __sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4);
        __ebss = .;
    } > SDRAM

    /DISCARD/ :
    {
        *(.ARM.exidx*)
        *(.ARM.extab*)
    }
}

ASSERT(__eitcm <= 0x01050000, "Core 0 ITCM collides with initial MSP stack (0x0105_0800)");
ASSERT(__ebss <= 0x602CC000, "Core 0 SDRAM collides with g_bDispDirty / g_abDispBuf0 / IPC pool");
