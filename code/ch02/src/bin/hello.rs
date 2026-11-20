//! Step 2: the same VM, built with the rust-vmm crates that
//! Firecracker and Cloud Hypervisor use.

use std::error::Error;
use std::io::{self, Write};

use ch02::{COM1, LOAD_ADDR, MEM_SIZE, guest::HELLO16};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit, VmFd};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend,
    GuestMemoryMmap, GuestMemoryRegion,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Register every region of `mem` with KVM, one slot each.
pub fn add_memory(
    vm: &VmFd,
    mem: &GuestMemoryMmap,
) -> Result<()> {
    for (slot, region) in mem.iter().enumerate() {
        let host = mem.get_host_address(region.start_addr())?;
        let r = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: host as u64,
        };
        // SAFETY: `mem` owns the mapping, and outlives the VM.
        unsafe { vm.set_user_memory_region(r)? };
    }
    Ok(())
}

fn main() -> Result<()> {
    let kvm = Kvm::new()?; // opens /dev/kvm, checks version 12
    let vm = kvm.create_vm()?;

    let ranges = [(GuestAddress(0), MEM_SIZE)];
    let mem = GuestMemoryMmap::<()>::from_ranges(&ranges)?;
    mem.write_slice(HELLO16, GuestAddress(LOAD_ADDR))?;
    add_memory(&vm, &mem)?;

    let mut vcpu = vm.create_vcpu(0)?; // also maps kvm_run
    // A fresh vCPU starts at the real-mode reset address
    // (cs base 0xffff0000); zero it so rip is a plain address.
    let mut sregs = vcpu.get_sregs()?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs)?;
    let mut regs = vcpu.get_regs()?;
    regs.rip = LOAD_ADDR;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs)?;

    let mut out = io::stdout();
    // Each exit is one trip from guest to VMM and back; we
    // count them to show how costly byte-at-a-time I/O is.
    let mut exits = 0;
    loop {
        let exit = vcpu.run()?;
        exits += 1;
        match exit {
            VcpuExit::IoOut(COM1, data) => out.write_all(data)?,
            VcpuExit::Hlt => break,
            other => return Err(format!("{other:?}").into()),
        }
    }
    println!("[hello] guest halted after {exits} exits");
    Ok(())
}
