/* Linker script for mc-101-boot (ITCM0 + SRAM0 relocated SD card bootloader).
 *
 * The flat binary starts with a 64-byte position-independent `.entry` stub
 * (valid whether loaded at 0x6000_0000 standalone or appended at 0x6035_65B0
 * to stock App1_Main). On entry, the stub copies `.itcm` -> 0x0100_0000 and
 * `.data` -> 0x2000_0000, zeroes `.bss`, and jumps to `bootloader_main` in ITCM,
 * leaving all of SDRAM (0x6000_0000..0x61FF_FFFF) free for loading images.
 */

MEMORY
{
    STUB  (rx)  : ORIGIN = 0x00FFFF00, LENGTH = 0x100
    ITCM0 (rwx) : ORIGIN = 0x01000000, LENGTH = 0x00050000
    SRAM0 (rwx) : ORIGIN = 0x20000000, LENGTH = 0x00020000
}

ENTRY(Reset_Handler)

SECTIONS
{
    .entry 0x00FFFF00 :
    {
        KEEP(*(.text.entry))
        . = 0x100;
    } > STUB

    .itcm 0x01000000 :
    {
        __sitcm = .;
        KEEP(*(.vector_table))
        *(.itcm .itcm.*)
        *(.text .text.*)
        *(.rodata .rodata.*)
        . = ALIGN(4);
        __eitcm = .;
    } > ITCM0

    .data 0x20000000 : AT(LOADADDR(.itcm) + SIZEOF(.itcm))
    {
        __sdata = .;
        *(.data .data.*)
        . = ALIGN(4);
        __edata = .;
    } > SRAM0
    __sidata = LOADADDR(.data);

    .bss (NOLOAD) : ALIGN(4)
    {
        __sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4);
        __ebss = .;
    } > SRAM0

    /DISCARD/ :
    {
        *(.ARM.exidx*)
        *(.ARM.extab*)
    }
}

ASSERT(__eitcm <= 0x01050000, "Bootloader ITCM collides with initial MSP stack (0x0105_0800)");
ASSERT(__ebss <= 0x20020000, "Bootloader SRAM0 exceeds 128 KiB");
