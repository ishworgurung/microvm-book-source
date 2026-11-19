//! Step 3: start the vCPU directly in 64-bit long mode, the
//! mode a Linux kernel runs in (chapter 3).

use std::error::Error;
use std::io::{self, Write};

use ch02::x86::{STACK_TOP, set_long_mode, write_tables};
use ch02::{COM1, LOAD_ADDR, MEM_SIZE, guest::HELLO64};
use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VcpuExit};
use vm_memory::{
    Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;

    // The same 2 MiB of RAM, now also holding a GDT and page
    // tables, next to the guest code.
    let ranges = [(GuestAddress(0), MEM_SIZE)];
    let mem = GuestMemoryMmap::<()>::from_ranges(&ranges)?;
    mem.write_slice(HELLO64, GuestAddress(LOAD_ADDR))?;
    write_tables(&mem)?;
    let host = mem.get_host_address(GuestAddress(0))?;
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: MEM_SIZE as u64,
        userspace_addr: host as u64,
    };
    // SAFETY: `mem` owns the mapping, and outlives the VM.
    unsafe { vm.set_user_memory_region(region)? };

    let mut vcpu = vm.create_vcpu(0)?;
    // Tell the guest which CPU features it has: everything KVM
    // supports, unfiltered. 64-bit mode is one of them.
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    vcpu.set_cpuid2(&cpuid)?;

    // Registers use read-modify-write: fetch what KVM set up,
    // change only our fields, write it back. set_long_mode
    // fills in the GDT, segments, CR0/CR3/CR4 and EFER so the
    // vCPU starts already in 64-bit mode. rip is the first
    // instruction, rsp the stack top, and rflags bit 1 is
    // reserved and must be 1.
    let mut sregs = vcpu.get_sregs()?;
    set_long_mode(&mut sregs);
    vcpu.set_sregs(&sregs)?;
    let mut regs = vcpu.get_regs()?;
    regs.rip = LOAD_ADDR;
    regs.rsp = STACK_TOP;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)?;

    let mut out = io::stdout();
    loop {
        // Same loop as before: the match arm on COM1 only fires
        // for OUT to that port; anything unexpected is an error.
        match vcpu.run()? {
            VcpuExit::IoOut(COM1, data) => out.write_all(data)?,
            VcpuExit::Hlt => break,
            other => return Err(format!("{other:?}").into()),
        }
    }
    let sregs = vcpu.get_sregs()?;
    println!(
        "[long] guest halted; cs.l={} efer={:#x}",
        sregs.cs.l, sregs.efer
    );
    Ok(())
}
