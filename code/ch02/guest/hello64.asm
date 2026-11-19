; The same guest for 64-bit long mode. Assemble with:
;   nasm -f bin -o hello64.bin hello64.asm
; The VMM loads these bytes at guest-physical address 0x1000.
bits 64
org 0x1000
        mov dx, 0x3f8
        lea rsi, [rel msg]  ; rip-relative: works at any address
next:   lodsb
        test al, al
        jz done
        out dx, al
        jmp next
done:   hlt
msg:    db "Hello from 64-bit long mode!", 10, 0
