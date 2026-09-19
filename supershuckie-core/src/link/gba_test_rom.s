@ The Game Boy Advance link test ROM: exchanges 256 halfwords with the other player over the
@ cable in MULTI mode (see test_rom.rs, which embeds the assembled bytes). Assemble with
@ devkitARM:
@
@   arm-none-eabi-as -mcpu=arm7tdmi -o gba_test_rom.o gba_test_rom.s
@   arm-none-eabi-objcopy -O binary gba_test_rom.o gba_test_rom.bin
@
@ Layout: ROM entry (a branch, so byte 3 is 0xEA), the header's fixed byte at 0xB2, then the
@ program at 0xC0. RAM use (EWRAM): 0x02000000 role (1 = master, else slave), 0x02000001 done
@ (0xAA when finished), 0x02000002..3 the magic word A5 5A the ROM waits for, 0x02000100.. the
@ 256 bytes received. The master sends i and receives i ^ 0xFF; the slave the other way round.

    .arm
    .global _start
_start:
    b main
    .space 0xB2 - 4
    .byte 0x96
    .space 0xC0 - 0xB3
main:
    ldr r4, =0x04000120         @ I/O: the serial registers (SIOMULTI0 = 0, SIOMULTI1 = 2, SIOCNT = 8, SIOMLT_SEND = 0xA, RCNT = 0x14)
    ldr r5, =0x02000000         @ EWRAM
    mov r0, #0
    strb r0, [r5, #1]           @ done = 0
wait_role:
    ldrb r0, [r5, #2]
    cmp r0, #0xA5
    bne wait_role
    ldrb r0, [r5, #3]
    cmp r0, #0x5A
    bne wait_role
    mov r0, #0
    strh r0, [r4, #0x14]       @ RCNT = 0: serial modes
    ldr r0, =0x2003
    strh r0, [r4, #0x8]       @ SIOCNT: MULTI, 115200 baud
    ldrb r0, [r5, #0]
    cmp r0, #1
    beq master
slave:
    add r6, r5, #0x100
    mov r7, #0
slave_loop:
    eor r0, r7, #0xFF
    strh r0, [r4, #0xA]       @ SIOMLT_SEND = i ^ 0xFF
slave_wait_busy:
    ldrh r0, [r4, #0x8]
    tst r0, #0x80
    beq slave_wait_busy         @ until the master starts the transfer
slave_wait_done:
    ldrh r0, [r4, #0x8]
    tst r0, #0x80
    bne slave_wait_done         @ until it completes
    ldrh r0, [r4, #0x0]       @ SIOMULTI0: the master's halfword
    strb r0, [r6, r7]
    add r7, r7, #1
    cmp r7, #256
    bne slave_loop
    b done
master:
    ldr r0, =0x40000            @ a few frames: the slave is surely set up
mdelay:
    subs r0, r0, #1
    bne mdelay
    add r6, r5, #0x100
    mov r7, #0
master_loop:
    strh r7, [r4, #0xA]       @ SIOMLT_SEND = i
master_wait_ready:
    ldrh r0, [r4, #0x8]
    tst r0, #0x08
    beq master_wait_ready       @ until every player is in MULTI mode (SD)
    orr r0, r0, #0x80
    strh r0, [r4, #0x8]       @ start
master_wait_done:
    ldrh r0, [r4, #0x8]
    tst r0, #0x80
    bne master_wait_done
    ldrh r0, [r4, #0x2]         @ SIOMULTI1: the slave's halfword
    strb r0, [r6, r7]
    mov r0, #0x800              @ a pause, so the slave preloads its next halfword first
mpause:
    subs r0, r0, #1
    bne mpause
    add r7, r7, #1
    cmp r7, #256
    bne master_loop
done:
    mov r0, #0xAA
    strb r0, [r5, #1]
hang:
    b hang
    .ltorg
