//! Shared pieces of the chapter 2 programs: the guest code
//! (`guest`) and the x86 setup for 64-bit long mode (`x86`).
//!
//! The three programs are in src/bin:
//!   raw.rs    a VM made with nothing but ioctl() and mmap()
//!   hello.rs  the same VM with kvm-ioctls and vm-memory
//!   long.rs   the guest in 64-bit long mode

pub mod guest;
pub mod x86;

/// Guest RAM: 2 MiB, starting at guest-physical address 0.
pub const MEM_SIZE: usize = 2 << 20;

/// Where the VMM copies the guest code, and where it starts.
pub const LOAD_ADDR: u64 = 0x1000;

/// The I/O port of the first PC serial port, COM1.
pub const COM1: u16 = 0x3f8;
