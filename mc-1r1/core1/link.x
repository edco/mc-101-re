/* Unified linker script for Core 1 (produces one ELF split by xtask into
 * idm1.bin @ 0x0110_0000 and sdram1.bin @ 0x6100_0000).
 *
 * Drop-in compatibility constraints enforced here:
 * - ITCM1 (0x0110_0000, 256 KiB): 32-byte zero header at +0x00, Cortex-M4
 *   vector table at +0x20 (MSP = 0x200A_91C8).
 * - SRAM1 (0x2002_0000..0x200A_9000): strictly above Core 0's SRAM
 *   (0x2000_0000..0x2001_FFFF) and below Core 1's initial stack (0x200A_91C8).
 * - SDRAM1 (0x6100_0000..0x6170_0000): 32-byte zero header at +0x00, entry at
 *   +0x20, strictly below wave RAM (0x6180_0000).
 */

MEMORY
{
    ITCM1  (rx)  : ORIGIN = 0x01100000, LENGTH = 0x00040000
    SRAM1  (rwx) : ORIGIN = 0x20020000, LENGTH = 0x00089000
    SDRAM1 (rwx) : ORIGIN = 0x61000000, LENGTH = 0x00700000
}

ENTRY(Reset_Handler)

SECTIONS
{
    .vector_table 0x01100000 :
    {
        KEEP(*(.vector_table))
    } > ITCM1

    .text : ALIGN(4)
    {
        *(.itcm .itcm.*)
        *(.text .text.*)
        *(.rodata .rodata.*)
        . = ALIGN(4);
    } > ITCM1

    .data : ALIGN(4)
    {
        __sdata = .;
        *(.data .data.*)
        . = ALIGN(4);
        __edata = .;
    } > SRAM1 AT > ITCM1
    __sidata = LOADADDR(.data);
    __eitcm = LOADADDR(.data) + SIZEOF(.data);

    .bss (NOLOAD) : ALIGN(4)
    {
        __sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4);
        __ebss = .;
    } > SRAM1

    /* SDRAM1 image (extracted to sdram1.bin): entry @ 0x6100_0000. */
    .sdram 0x61000000 :
    {
        KEEP(*(.sdram.entry))
        *(.sdram .sdram.*)
        . = ALIGN(4);
        __esdram = .;
    } > SDRAM1

    /DISCARD/ :
    {
        *(.ARM.exidx*)
        *(.ARM.extab*)
    }
}

ASSERT(__eitcm <= 0x01140000, "Core 1 ITCM (idm1.bin) overflows 256 KiB");
ASSERT(__sdata >= 0x20020000 && __ebss <= 0x200A9000, "Core 1 SRAM collides with Core 0 or stack");
ASSERT(__esdram <= 0x61700000, "Core 1 SDRAM (sdram1.bin) collides with upper BSS / wave RAM");
