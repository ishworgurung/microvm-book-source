//! The machine without its devices: a KVM VM with in-kernel interrupt
//! controllers and timer, guest RAM, a PVH-booted kernel and one configured
//! vCPU. This is chapter 3's machine, unchanged; src/main.rs adds
//! the virtio devices.
//!
//! [`build`] makes these KVM calls, in this order:
//!
//! 1. `open("/dev/kvm")`, `KVM_CREATE_VM`, `KVM_SET_TSS_ADDR`,
//!    `KVM_CREATE_IRQCHIP`, `KVM_CREATE_PIT2`: the VM and the devices that
//!    run inside KVM.
//! 2. `KVM_SET_USER_MEMORY_REGION`: guest RAM.
//! 3. No KVM calls: the VMM writes the kernel, initrd, command line and PVH
//!    structures into guest RAM through its own mapping of it.
//! 4. `KVM_CREATE_VCPU`, `KVM_GET_SUPPORTED_CPUID`, `KVM_SET_CPUID2`,
//!    `KVM_SET_SREGS`, `KVM_SET_REGS`, `KVM_SET_FPU`: the vCPU.
//!
//! After step 4 the vCPU is ready. The caller adds devices and calls `KVM_RUN`.

use std::fs::File;
use std::time::Instant;

use kvm_bindings::{
    kvm_pit_config, kvm_userspace_memory_region,
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use linux_loader::cmdline::Cmdline;
use linux_loader::loader::{
    elf::Elf, elf::PvhBootCapability, load_cmdline, KernelLoader,
};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend,
    GuestMemoryMmap, GuestMemoryRegion,
};

use crate::boot;

/// What the caller asks for. Paths are host paths.
pub struct Config<'a> {
    /// An uncompressed ELF `vmlinux` with the PVH entry note (`CONFIG_PVH=y`).
    /// A `bzImage` does not work: it has no ELF headers to load.
    pub kernel: &'a str,
    /// An optional initramfs (cpio archive), copied to the top of RAM.
    pub initrd: Option<&'a str>,
    /// Guest RAM in MiB (1 MiB = 1,048,576 bytes).
    pub mem_mib: u64,
    /// The whole kernel command line; the caller decides what its devices need.
    pub cmdline: &'a str,
}

/// A VM that is ready for its first `KVM_RUN`, plus what the caller needs to
/// add devices and print a report.
pub struct Machine {
    /// The `/dev/kvm` handle. Kept so the caller can make system-level
    /// queries; dropping it does not close the VM.
    pub kvm: Kvm,
    /// The VM file descriptor: memory slots, irqfds and ioeventfds are
    /// registered on it.
    pub vm: VmFd,
    /// vCPU 0's file descriptor. `KVM_RUN` is called on it.
    pub vcpu: VcpuFd,
    /// Guest RAM as the VMM sees it. Device code reads and writes guest memory
    /// through this mapping; KVM maps the same host pages into the guest.
    pub mem: GuestMemoryMmap,
    /// The guest-physical address of the PVH entry point (the vCPU's first RIP).
    pub entry: u64,
    /// Guest-physical start and end of the loaded kernel image.
    pub kernel_range: (u64, u64),
    /// Guest-physical address and size of the initrd, if one was loaded.
    pub initrd: Option<(u64, u64)>,
    /// The command line as it was written into guest memory.
    pub cmdline: String,
    /// When guest RAM was mapped, the kernel loaded, and the vCPU configured.
    pub t_vm: Instant,
    pub t_loaded: Instant,
    pub t_vcpu: Instant,
}

/// Turn a KVM ioctl error into a message that names the ioctl, for `map_err`.
fn kvm_err(
    what: &'static str,
) -> impl Fn(kvm_ioctls::Error) -> String {
    move |err| format!("{what}: {err}")
}

/// Build the machine described by `cfg`. Errors are strings that name the
/// failing step, because the binary only prints them and exits.
pub fn build(cfg: &Config) -> Result<Machine, String> {
    let e = kvm_err;

    // ---- 1. The VM: in-kernel interrupt controllers and timer, as Firecracker.
    let kvm_object = Kvm::new().map_err(e("open /dev/kvm"))?;
    let vm =
        kvm_object.create_vm().map_err(e("KVM_CREATE_VM"))?;
    // Intel VT-x needs three pages for a real-mode TSS even though we never run real mode.
    // KVM uses them to emulate real mode on CPUs that cannot run it directly.
    // The address only has to be unused by the guest: it is above RAM, just
    // below 4 GiB, and is the address Firecracker uses.
    vm.set_tss_address(0xfffb_d000)
        .map_err(e("KVM_SET_TSS_ADDR"))?;
    // KVM_CREATE_IRQCHIP creates, inside KVM, the interrupt controllers a PC
    // has: two 8259 PICs (programmable interrupt controllers), one IOAPIC
    // (I/O advanced programmable interrupt controller, which routes device
    // interrupt lines to CPUs), and a local APIC in each vCPU created later.
    // Guest accesses to them are handled by KVM in the host kernel and never
    // reach this process. The irqfds in src/main.rs need this: they raise
    // lines on these controllers.
    vm.create_irq_chip().map_err(e("KVM_CREATE_IRQCHIP"))?;

    // KVM_CREATE_PIT2 creates the 8254 PIT (programmable interval timer) in
    // KVM. The guest kernel uses it to calibrate its clocks at boot.
    // KVM_PIT_SPEAKER_DUMMY makes KVM also answer port 0x61 (the PC speaker
    // control port, which shares bits with the PIT) instead of exiting to us.
    let pit = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit).map_err(e("KVM_CREATE_PIT2"))?;

    // ---- 2. Guest RAM: vm-memory mmaps it; we tell KVM where it lives.
    // `<< 20` converts MiB to bytes.
    let mem_size_bytes = cfg.mem_mib << 20;
    // RAM below the MMIO hole, and the rest from 4 GiB (boot::ram_regions).
    // One region of 4079 MiB or more would cover the local APIC page that KVM
    // maps at 0xfee0_0000, and KVM_CREATE_VCPU would fail with EEXIST.
    let regions = boot::ram_regions(mem_size_bytes);
    let low_end = regions[0].1;
    // The highest guest-physical address is limited by the CPU's physical
    // address width: CPUID leaf 0x8000_0008, EAX bits 7..0. Past it, KVM
    // rejects the memory slot with a bare EINVAL, so say why here instead.
    let phys_bits = kvm_object
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(e("KVM_GET_SUPPORTED_CPUID"))?
        .as_slice()
        .iter()
        .find(|ent| ent.function == 0x8000_0008)
        .map_or(36, |ent| ent.eax & 0xff); // 36 bits: the oldest x86-64 CPUs
    let (last_start, last_size) = regions[regions.len() - 1];
    if last_start + last_size > 1u64 << phys_bits {
        return Err(format!(
            "--mem {} MiB needs guest-physical addresses up to {:#x}, but this CPU has {phys_bits} physical address bits",
            cfg.mem_mib,
            last_start + last_size
        ));
    }
    // vm-memory allocates each region with an anonymous mmap: pages are
    // allocated on first touch, so an idle guest does not use its whole RAM
    // size on the host.
    //
    // `<()>` is vm-memory's dirty-page bitmap type: `()` tracks nothing, and
    // its empty `mark_dirty` compiles away. A VMM that migrates or takes diff
    // snapshots would use `AtomicBitmap` here, and would also set
    // KVM_MEM_LOG_DIRTY_PAGES on the slot below to track the vCPU's own
    // writes, which never pass through vm-memory.
    let ranges: Vec<(GuestAddress, usize)> = regions
        .iter()
        .map(|&(start, size)| {
            (GuestAddress(start), size as usize)
        })
        .collect();
    let mem = GuestMemoryMmap::<()>::from_ranges(&ranges)
        .map_err(|e| format!("guest memory: {e}"))?;
    // Each region becomes one KVM memory slot. A slot tells KVM: guest-physical
    // addresses [guest_phys_addr, +memory_size) are backed by this process's
    // virtual addresses [userspace_addr, +memory_size). KVM builds its EPT/NPT
    // (the second-level page tables that translate guest-physical to
    // host-physical addresses) from this on demand.
    for (slot, region) in mem.iter().enumerate() {
        let host = mem
            .get_host_address(region.start_addr())
            .map_err(|e| e.to_string())?;
        let r = kvm_userspace_memory_region {
            slot: slot as u32,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: host as u64,
            flags: 0, // no dirty-page logging, not read-only
        };
        // SAFETY: the mapping is owned by `mem`, which outlives the VM.
        unsafe { vm.set_user_memory_region(r) }
            .map_err(e("KVM_SET_USER_MEMORY_REGION"))?;
    }
    let t_vm = Instant::now();

    // ---- 3. Be the bootloader: ELF segments, initrd, cmdline, PVH start info.
    // Everything in this step is a plain memory write into `mem`. KVM is not
    // involved: the guest sees the bytes because the slot above maps the same
    // pages.
    let mut kernel_file = File::open(cfg.kernel)
        .map_err(|e| format!("{}: {e}", cfg.kernel))?;
    // Elf::load copies each PT_LOAD segment (a program-header entry that says
    // "load these file bytes at this physical address") into guest memory, and
    // looks for the PVH note. `None` as the offset: load at the addresses the
    // ELF file gives. The last argument is the lowest address allowed.
    let loaded = Elf::load(
        &mem,
        None,
        &mut kernel_file,
        Some(GuestAddress(boot::HIMEM_START)),
    )
    .map_err(|e| format!("ELF load: {e}"))?;
    let entry = match loaded.pvh_boot_cap {
        PvhBootCapability::PvhEntryPresent(a) => a.raw_value(),
        other => return Err(format!("kernel has no PVH entry note ({other:?}): build with CONFIG_PVH=y")),
    };
    let initrd = match cfg.initrd {
        Some(p) => {
            let data = std::fs::read(p)
                .map_err(|e| format!("{p}: {e}"))?;
            // Top of low RAM, page aligned, like every x86 bootloader.
            // `& !0xfff` rounds down to a 4 KiB boundary. Placing it high keeps
            // it away from the kernel image, which loads just above 16 MiB.
            // Low RAM, not high: it then stays below 4 GiB, so 32-bit early
            // boot code can reach it.
            let addr = low_end
                .checked_sub(data.len() as u64)
                .ok_or_else(|| {
                    format!(
                        "{p}: does not fit in {} MiB of RAM",
                        cfg.mem_mib
                    )
                })?
                & !0xfff;
            mem.write_slice(&data, GuestAddress(addr))
                .map_err(|e| e.to_string())?;
            Some((addr, data.len() as u64))
        }
        None => None,
    };
    // Cmdline checks the length against CMDLINE_MAX and rejects characters
    // that are not printable ASCII; load_cmdline writes it NUL-terminated.
    let mut cmdline = Cmdline::new(boot::CMDLINE_MAX)
        .map_err(|e| e.to_string())?;
    cmdline.insert_str(cfg.cmdline).map_err(|e| e.to_string())?;
    load_cmdline(
        &mem,
        GuestAddress(boot::CMDLINE_ADDR),
        &cmdline,
    )
    .map_err(|e| e.to_string())?;
    boot::write_pvh_params(&mem, &regions, initrd)?;
    let t_loaded = Instant::now();

    // ---- 4. The vCPU.
    // vCPU id 0 is also its APIC ID, and the boot processor.
    let vcpu = vm.create_vcpu(0).map_err(e("KVM_CREATE_VCPU"))?;
    // KVM_GET_SUPPORTED_CPUID returns every CPUID (the CPU-identification
    // instruction) leaf that KVM can present to a guest on this host. A new
    // vCPU has no CPUID table at all: without KVM_SET_CPUID2 the guest would
    // see no features.
    let mut cpuid = kvm_object
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(e("KVM_GET_SUPPORTED_CPUID"))?;
    // "Supported" CPUID still describes the host's topology: leaf 1 and the
    // x2APIC leaves report the host CPU's APIC ID. Ours is vCPU 0 of 1.
    for ent in cpuid.as_mut_slice() {
        match ent.function {
            // Leaf 1 EBX: bits 31..24 = initial APIC ID (set to 0), bits 23..16
            // = logical processors per package (set to 1). Bits 15..0 (brand
            // index and CLFLUSH line size) are kept.
            0x1 => ent.ebx = (ent.ebx & 0x0000_ffff) | (1 << 16),
            // Leaves 0xb and 0x1f (extended topology): EDX = the x2APIC ID.
            0xb | 0x1f => ent.edx = 0,
            _ => {}
        }
    }
    vcpu.set_cpuid2(&cpuid).map_err(e("KVM_SET_CPUID2"))?;
    // Read the reset state first, so that boot::pvh_registers changes only
    // the fields PVH cares about.
    let mut sregs =
        vcpu.get_sregs().map_err(e("KVM_GET_SREGS"))?;
    let regs = boot::pvh_registers(&mem, entry, &mut sregs)?;
    vcpu.set_sregs(&sregs).map_err(e("KVM_SET_SREGS"))?;
    vcpu.set_regs(&regs).map_err(e("KVM_SET_REGS"))?;
    vcpu.set_fpu(&boot::fpu()).map_err(e("KVM_SET_FPU"))?;
    let t_vcpu = Instant::now();

    // Return the command line as written (from the Cmdline, not cfg) so the
    // report shows what the guest actually received.
    let cmdline = cmdline
        .as_cstring()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    Ok(Machine {
        kvm: kvm_object,
        vm,
        vcpu,
        mem,
        entry,
        kernel_range: (
            loaded.kernel_load.raw_value(),
            loaded.kernel_end,
        ),
        initrd,
        cmdline,
        t_vm,
        t_loaded,
        t_vcpu,
    })
}
