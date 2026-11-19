; The chapter 2 guest, in 16-bit real mode. Assemble with:
;   nasm -f bin -o hello16.bin hello16.asm
; The VMM loads these bytes at guest-physical address 0x1000.
bits 16
org 0x1000
        mov dx, 0x3f8       ; the port of the first serial port
        mov si, msg         ; si = address of the first byte
next:   lodsb               ; al = byte at [si], then si += 1
        test al, al         ; is it the 0 that ends the string?
        jz done
        out dx, al          ; write al to port dx: a VM exit
        jmp next
done:   hlt                 ; stop the CPU: another VM exit
msg:    db "Hello from the guest!", 10, 0
