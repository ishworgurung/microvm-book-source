//! Step 1: a virtual machine from nothing but open(), ioctl()
//! and mmap(). Only the struct layouts come from a crate
//! (kvm-bindings, generated from <linux/kvm.h>).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::ptr::null_mut;

use ch02::{COM1, LOAD_ADDR, MEM_SIZE, guest::HELLO16};
use kvm_bindings::{
    KVM_EXIT_HLT, KVM_EXIT_IO, KVM_EXIT_IO_OUT, kvm_regs,
    kvm_run, kvm_sregs, kvm_userspace_memory_region,
};

// An ioctl request number packs four fields (see
// <asm-generic/ioctl.h>): direction, argument size, a
// "type" byte (0xAE for KVM) and a command number.
const KVMIO: u64 = 0xae;
const fn io(nr: u64) -> u64 {
    KVMIO << 8 | nr
}
const fn iow<T>(nr: u64) -> u64 {
    1 << 30 | (size_of::<T>() as u64) << 16 | io(nr)
}
const fn ior<T>(nr: u64) -> u64 {
    2 << 30 | (size_of::<T>() as u64) << 16 | io(nr)
}

// On the /dev/kvm file descriptor:
const KVM_GET_API_VERSION: u64 = io(0x00);
const KVM_CREATE_VM: u64 = io(0x01);
const KVM_GET_VCPU_MMAP_SIZE: u64 = io(0x04);
// On a VM file descriptor:
const KVM_CREATE_VCPU: u64 = io(0x41);
const KVM_SET_USER_MEMORY_REGION: u64 =
    iow::<kvm_userspace_memory_region>(0x46);
// On a vCPU file descriptor:
const KVM_RUN: u64 = io(0x80);
const KVM_SET_REGS: u64 = iow::<kvm_regs>(0x82);
const KVM_GET_SREGS: u64 = ior::<kvm_sregs>(0x83);
const KVM_SET_SREGS: u64 = iow::<kvm_sregs>(0x84);

/// ioctl(2), with errors turned into io::Error.
fn ioctl(fd: RawFd, req: u64, arg: u64) -> io::Result<i32> {
    // SAFETY: each caller passes a request and an argument
    // that match, as <linux/kvm.h> defines them.
    // `req as _`: the request type differs by libc (c_ulong on
    // glibc, c_int on musl), so let the compiler pick.
    let ret = unsafe { libc::ioctl(fd, req as _, arg) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ret)
}

/// Turn a new file descriptor number into an owned File,
/// so that it is closed when it goes out of scope.
fn owned(fd: i32) -> File {
    // SAFETY: fd was just returned by KVM and is ours.
    unsafe { File::from_raw_fd(fd) }
}

/// mmap(2): `fd` -1 means anonymous memory.
fn mmap(
    len: usize,
    flags: i32,
    fd: RawFd,
) -> io::Result<*mut u8> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // SAFETY: a fresh mapping; we touch only its `len` bytes.
    let p = unsafe {
        libc::mmap(null_mut(), len, prot, flags, fd, 0)
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

fn main() -> io::Result<()> {
    // 1. The system: /dev/kvm.
    let kvm = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|e| {
            io::Error::new(e.kind(), format!("/dev/kvm: {e}"))
        })?;
    let version = ioctl(kvm.as_raw_fd(), KVM_GET_API_VERSION, 0)?;
    assert_eq!(version, 12, "KVM API version");

    // 2. A VM: a new file descriptor.
    let vm = owned(ioctl(kvm.as_raw_fd(), KVM_CREATE_VM, 0)?);

    // 3. Guest memory: ordinary anonymous memory in our own
    //    process, then registered with KVM as memory slot 0.
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    let vm_ram = mmap(MEM_SIZE, flags, -1)?;
    // SAFETY: HELLO16 fits well inside the MEM_SIZE mapping.
    unsafe {
        let dst = vm_ram.add(LOAD_ADDR as usize);
        dst.copy_from_nonoverlapping(
            HELLO16.as_ptr(),
            HELLO16.len(),
        );
    }
    // Tell KVM: guest-physical address 0 is backed by this
    // host range. The guest never sees our virtual addresses.
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: MEM_SIZE as u64,
        userspace_addr: vm_ram as u64,
    };
    let arg = &region as *const _ as u64;
    ioctl(vm.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, arg)?;

    // 4. A vCPU: another file descriptor, plus the kvm_run
    //    page that KVM shares with us to report each exit.
    let vcpu = owned(ioctl(vm.as_raw_fd(), KVM_CREATE_VCPU, 0)?);
    let size = ioctl(kvm.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0)?;
    let run =
        mmap(size as usize, libc::MAP_SHARED, vcpu.as_raw_fd())?
            as *const kvm_run;

    // 5. Registers. A new vCPU is in 16-bit real mode, as a PC
    //    is at power-on; point its code segment at address 0.
    let mut sregs = kvm_sregs::default();
    // ioctl() takes its argument as a plain integer, so we pass
    // the struct's address. KVM_GET_SREGS writes the vCPU's
    // current segment and control registers into it. Read first,
    // then change only what we need: the rest stays valid.
    let arg = &mut sregs as *mut _ as u64;
    ioctl(vcpu.as_raw_fd(), KVM_GET_SREGS, arg)?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    // Same trick in the other direction: KVM_SET_SREGS only
    // reads the struct, so a const pointer is enough. `sregs`
    // must stay alive until the call returns (it does).
    let arg = &sregs as *const _ as u64;
    ioctl(vcpu.as_raw_fd(), KVM_SET_SREGS, arg)?;
    let regs = kvm_regs {
        rip: LOAD_ADDR, // the first instruction
        rflags: 0x2,    // bit 1 is reserved and must be 1
        ..Default::default()
    };
    let arg = &regs as *const _ as u64;
    ioctl(vcpu.as_raw_fd(), KVM_SET_REGS, arg)?;

    // 6. The run loop.
    let mut out = io::stdout();
    loop {
        // KVM_RUN blocks while the guest executes, and returns
        // when the guest does something KVM can't handle itself
        // (port I/O, HLT). EINTR just means a signal woke us.
        match ioctl(vcpu.as_raw_fd(), KVM_RUN, 0) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                continue; // a signal arrived: just run again
            }
            r => r?,
        };
        // SAFETY: KVM filled the shared page before returning.
        // Turn the raw pointer into a shared reference so the
        // fields can be read normally. We rebuild it after every
        // KVM_RUN, because KVM rewrites this page on each exit;
        // a reference kept across the ioctl would be stale.
        let run = unsafe { &*run };
        match run.exit_reason {
            KVM_EXIT_IO => {
                // SAFETY: for this exit, `io` is the live field.
                let io = unsafe { run.__bindgen_anon_1.io };
                // The value written by OUT is not in `io` itself;
                // `data_offset` says where in the kvm_run page it
                // sits, counted from the start of the page.
                let off = io.data_offset as usize;
                // SAFETY: data lies inside the kvm_run mapping.
                let byte = unsafe {
                    *(run as *const _ as *const u8).add(off)
                };
                // Only OUT to COM1 is a "serial write"; forward
                // that byte to our stdout, ignore anything else.
                if io.direction == KVM_EXIT_IO_OUT as u8
                    && io.port == COM1
                {
                    out.write_all(&[byte])?;
                }
            }
            KVM_EXIT_HLT => {
                println!("[raw] guest halted");
                return Ok(());
            }
            reason => panic!("unexpected exit reason {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The values <linux/kvm.h> gives on x86-64.
    #[test]
    fn request_numbers_match_the_kernel_header() {
        assert_eq!(KVM_GET_API_VERSION, 0xae00);
        assert_eq!(KVM_CREATE_VM, 0xae01);
        assert_eq!(KVM_GET_VCPU_MMAP_SIZE, 0xae04);
        assert_eq!(KVM_CREATE_VCPU, 0xae41);
        assert_eq!(KVM_SET_USER_MEMORY_REGION, 0x4020_ae46);
        assert_eq!(KVM_RUN, 0xae80);
        assert_eq!(KVM_SET_REGS, 0x4090_ae82);
        assert_eq!(KVM_GET_SREGS, 0x8138_ae83);
        assert_eq!(KVM_SET_SREGS, 0x4138_ae84);
    }
}
