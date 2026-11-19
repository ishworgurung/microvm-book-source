//! The guest programs, as machine code.
//!
//! The source of each is in guest/*.asm. `make guest`
//! reassembles them with nasm, and the tests below check
//! that the bytes here match. You do not need nasm to run
//! the chapter's programs.

/// 16-bit real-mode guest: write a string to port 0x3f8,
/// one byte per OUT, then HLT. Loaded at 0x1000.
#[rustfmt::skip]
pub const HELLO16: &[u8] = &[
    0xba, 0xf8, 0x03, // 1000: mov  dx, 0x3f8
    0xbe, 0x0f, 0x10, // 1003: mov  si, 0x100f (msg)
    0xac,             // 1006: lodsb       ; al = [si++]
    0x84, 0xc0,       // 1007: test al, al ; end of string?
    0x74, 0x03,       // 1009: jz   100e
    0xee,             // 100b: out  dx, al ; VM exit
    0xeb, 0xf8,       // 100c: jmp  1006
    0xf4,             // 100e: hlt         ; VM exit
    // 100f: msg
    b'H', b'e', b'l', b'l', b'o', b' ', b'f', b'r', b'o',
    b'm', b' ', b't', b'h', b'e', b' ', b'g', b'u', b'e',
    b's', b't', b'!', b'\n', 0,
];

/// The same program for 64-bit long mode. Loaded at 0x1000.
#[rustfmt::skip]
pub const HELLO64: &[u8] = &[
    0x66, 0xba, 0xf8, 0x03,  // 1000: mov dx, 0x3f8
    0x48, 0x8d, 0x35,        // 1004: lea rsi, [rip+9]
    0x09, 0x00, 0x00, 0x00,  //       (msg, at 1014)
    0xac,                    // 100b: lodsb
    0x84, 0xc0,              // 100c: test al, al
    0x74, 0x03,              // 100e: jz   1013
    0xee,                    // 1010: out  dx, al
    0xeb, 0xf8,              // 1011: jmp  100b
    0xf4,                    // 1013: hlt
    // 1014: msg
    b'H', b'e', b'l', b'l', b'o', b' ', b'f', b'r', b'o',
    b'm', b' ', b'6', b'4', b'-', b'b', b'i', b't', b' ',
    b'l', b'o', b'n', b'g', b' ', b'm', b'o', b'd', b'e',
    b'!', b'\n', 0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_the_assembled_sources() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/guest");
        for (bin, code) in
            [("hello16.bin", HELLO16), ("hello64.bin", HELLO64)]
        {
            let path = format!("{dir}/{bin}");
            let want = std::fs::read(&path).expect(&path);
            assert_eq!(code, &want[..], "{bin}");
        }
    }
}
