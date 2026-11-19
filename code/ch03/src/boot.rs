//! Be the bootloader: put the kernel, the initrd, the command
//! line and the PVH start info into guest memory, then give the
//! vCPU the registers the PVH entry point expects.
//!
//! PVH is the boot protocol Firecracker and Cloud Hypervisor
//! use for an uncompressed `vmlinux`. The VMM starts the vCPU
//! in 32-bit protected mode with paging off, at the address an
//! ELF note in the kernel names, with %ebx pointing at a
//! `struct hvm_start_info`. The kernel does everything else.

use kvm_bindings::CpuId;
use kvm_bindings::{kvm_fpu, kvm_regs, kvm_segment, kvm_sregs};
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::pvh::PvhBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::elf::start_info::{
    hvm_memmap_table_entry, hvm_modlist_entry, hvm_start_info,
};
use linux_loader::loader::elf::{Elf, PvhBootCapability};
use linux_loader::loader::{load_cmdline, KernelLoader};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

use crate::layout;

/// Turn any error into a message that says what failed.
pub fn ctx<E: std::fmt::Display>(
    what: &'static str,
) -> impl Fn(E) -> String {
    move |e| format!("{what}: {e}")
}

/// Copy the kernel's ELF segments into guest memory. Returns
/// the PVH entry point, the ELF header's own (64-bit) entry
/// point, and the end of the loaded image.
pub fn load_kernel(
    mem: &GuestMemoryMmap,
    path: &str,
) -> Result<(u64, u64, u64), String> {
    let mut file =
        std::fs::File::open(path).map_err(ctx("kernel"))?;
    let himem = Some(GuestAddress(layout::HIMEM_START));
    let k = Elf::load(mem, None, &mut file, himem)
        .map_err(ctx("ELF load"))?;
    match k.pvh_boot_cap {
        PvhBootCapability::PvhEntryPresent(entry) => {
            // linux-loader reports the ELF entry point as
            // `kernel_load`; the image itself starts lower.
            let elf_entry = k.kernel_load.0;
            Ok((entry.0, elf_entry, k.kernel_end))
        }
        _ => {
            Err("kernel has no PVH note: need CONFIG_PVH=y"
                .into())
        }
    }
}

/// Copy the initrd to the top of low RAM, page aligned.
pub fn load_initrd(
    mem: &GuestMemoryMmap,
    path: &str,
    low_ram_end: u64,
) -> Result<(u64, u64), String> {
    let data = std::fs::read(path).map_err(ctx("initrd"))?;
    let size = data.len() as u64;
    let addr = low_ram_end
        .checked_sub(size)
        .ok_or("initrd does not fit in RAM")?
        & !0xfff; // round down to a 4 KiB page
    mem.write_slice(&data, GuestAddress(addr))
        .map_err(ctx("initrd"))?;
    Ok((addr, size))
}

/// Write the NUL-terminated command line at layout::CMDLINE.
pub fn write_cmdline(
    mem: &GuestMemoryMmap,
    text: &str,
) -> Result<(), String> {
    let mut cmdline = Cmdline::new(layout::CMDLINE_MAX)
        .map_err(ctx("cmdline"))?;
    cmdline.insert_str(text).map_err(ctx("cmdline"))?;
    let at = GuestAddress(layout::CMDLINE);
    load_cmdline(mem, at, &cmdline).map_err(ctx("cmdline"))
}

/// "xEn3" with bit 7 of the E set: the kernel checks this.
const XEN_HVM_START_MAGIC: u32 = 0x336e_c578;
/// Memory-map entry type 1 is usable RAM, as in an e820 map.
const E820_RAM: u32 = 1;

/// Write hvm_start_info, the memory map and the module list.
pub fn write_start_info(
    mem: &GuestMemoryMmap,
    regions: &[(u64, u64)],
    initrd: Option<(u64, u64)>,
) -> Result<(), String> {
    let mut info = hvm_start_info {
        magic: XEN_HVM_START_MAGIC,
        version: 1, // version 1 has the memory-map fields
        cmdline_paddr: layout::CMDLINE,
        memmap_paddr: layout::MEMMAP,
        ..Default::default()
    };
    // The memory map: low memory up to the EBDA, RAM from
    // 1 MiB to the end of low RAM, then any high RAM.
    let ram = |addr, size| hvm_memmap_table_entry {
        addr,
        size,
        type_: E820_RAM,
        reserved: 0,
    };
    let low_end = regions[0].1;
    let mut map = vec![
        ram(0, layout::EBDA_START),
        ram(layout::HIMEM_START, low_end - layout::HIMEM_START),
    ];
    for &(addr, size) in &regions[1..] {
        map.push(ram(addr, size));
    }
    info.memmap_entries = map.len() as u32;

    let mut module = hvm_modlist_entry::default();
    if let Some((paddr, size)) = initrd {
        info.nr_modules = 1;
        info.modlist_paddr = layout::MODLIST;
        module.paddr = paddr;
        module.size = size;
    }

    let at = GuestAddress(layout::START_INFO);
    let mut params = BootParams::new(&info, at);
    // Some(addr) puts an entry AT addr; None appends it after
    // the previous one. Passing the address every time would
    // write every entry over the first.
    for (i, entry) in map.iter().enumerate() {
        let at = (i == 0).then_some(GuestAddress(layout::MEMMAP));
        params.add_section(entry, at).map_err(ctx("memmap"))?;
    }
    if initrd.is_some() {
        let at = GuestAddress(layout::MODLIST);
        params.set_modules(&[module], at);
    }
    PvhBootConfigurator::write_bootparams(&params, mem)
        .map_err(ctx("PVH start info"))
}

/// Pack one GDT entry (a segment descriptor) into the 8-byte
/// hardware format. `flags` holds the access byte in bits 7..0
/// and the G, D/B, L and AVL bits in bits 15..12.
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let (b, l, f) = (base as u64, limit as u64, flags as u64);
    ((b & 0xff00_0000) << 32)
        | ((f & 0xf0ff) << 40)
        | ((l & 0x000f_0000) << 32)
        | ((b & 0x00ff_ffff) << 16)
        | (l & 0xffff)
}

/// The same descriptor unpacked into the fields KVM wants.
fn segment(entry: u64, selector: u16) -> kvm_segment {
    let bit = |n: u32| ((entry >> n) & 1) as u8;
    let g = bit(55);
    let raw = ((entry >> 32) & 0xf_0000) | (entry & 0xffff);
    let raw = raw as u32;
    kvm_segment {
        base: ((entry >> 16) & 0xff_ffff)
            | ((entry >> 32) & 0xff00_0000),
        // With G=1 the limit counts 4 KiB pages.
        limit: if g == 1 { (raw << 12) | 0xfff } else { raw },
        selector,
        type_: ((entry >> 40) & 0xf) as u8,
        present: bit(47),
        dpl: ((entry >> 45) & 3) as u8,
        db: bit(54),
        s: bit(44),
        l: bit(53),
        g,
        avl: bit(52),
        unusable: 1 - bit(47),
        padding: 0,
    }
}

/// Registers for the PVH entry point: flat 32-bit protected
/// mode, paging off, %ebx = hvm_start_info, interrupts off.
pub fn pvh_registers(
    mem: &GuestMemoryMmap,
    entry: u64,
    sregs: &mut kvm_sregs,
) -> Result<kvm_regs, String> {
    let gdt = [
        gdt_entry(0, 0, 0),            // 0x00: null
        gdt_entry(0xc09b, 0, 0xfffff), // 0x08: code, 4 GiB
        gdt_entry(0xc093, 0, 0xfffff), // 0x10: data, 4 GiB
        gdt_entry(0x008b, 0, 0x67),    // 0x18: TSS, 104 B
    ];
    for (i, e) in gdt.iter().enumerate() {
        let at = GuestAddress(layout::GDT + 8 * i as u64);
        mem.write_obj(*e, at).map_err(ctx("GDT"))?;
    }
    sregs.gdt.base = layout::GDT;
    sregs.gdt.limit = (gdt.len() * 8 - 1) as u16;
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    sregs.cs = segment(gdt[1], 0x08);
    let data = segment(gdt[2], 0x10);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = segment(gdt[3], 0x18);
    sregs.cr0 = 0x1 | 0x10; // PE (protected mode) and ET
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0; // not in long mode yet
    Ok(kvm_regs {
        rip: entry,
        rbx: layout::START_INFO,
        rflags: 0x2, // bit 1 is always 1; IF (bit 9) is 0
        ..Default::default()
    })
}

/// The x87 and SSE control registers at their reset values.
pub fn fpu() -> kvm_fpu {
    kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    }
}

/// Make KVM's supported CPUID describe this VM, not the host:
/// one CPU, whose APIC ID is 0.
pub fn fix_cpuid(cpuid: &mut CpuId) {
    for e in cpuid.as_mut_slice() {
        match e.function {
            // EBX bits 31..24: APIC ID. 23..16: CPU count.
            0x1 => e.ebx = (e.ebx & 0xffff) | (1 << 16),
            // The x2APIC ID, in the topology leaves.
            0xb | 0x1f => e.edx = 0,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_code_descriptor() {
        let code = gdt_entry(0xc09b, 0, 0xfffff);
        assert_eq!(code, 0x00cf_9b00_0000_ffff);
        let s = segment(code, 0x08);
        assert_eq!((s.base, s.limit), (0, 0xffff_ffff));
        assert_eq!((s.type_, s.db, s.l, s.g), (0xb, 1, 0, 1));
        assert_eq!((s.present, s.unusable), (1, 0));
    }

    #[test]
    fn busy_tss_descriptor() {
        let tss = segment(gdt_entry(0x008b, 0, 0x67), 0x18);
        assert_eq!((tss.limit, tss.type_), (0x67, 0xb));
        assert_eq!((tss.s, tss.g), (0, 0));
    }
}
