//! PVH direct boot, as Cloud Hypervisor does it.
//!
//! The kernel's ELF image carries a note
//! (`XEN_ELFNOTE_PHYS32_ENTRY`) naming a 32-bit entry point; the VMM loads the
//! ELF segments, writes an `hvm_start_info` (cmdline, initrd, memory map) and
//! starts the vCPU in flat 32-bit protected mode with paging *off* and `%ebx`
//! pointing at that struct. The kernel builds its own page tables.
//!
//! `linux-loader` does the ELF parsing and writes the PVH structures; this file
//! decides where things go and sets the vCPU's registers.
//!
//! What the VMM does, in order (the caller, src/machine.rs, runs these steps):
//!
//! 1. Load the ELF segments at their physical addresses (`linux-loader`).
//! 2. Write the command line at `CMDLINE_ADDR`.
//! 3. Write `hvm_start_info`, the memory map and the module list
//!    ([`write_pvh_params`]).
//! 4. Write a GDT and set the segment, control and general registers
//!    ([`pvh_registers`]), then set the FPU state ([`fpu`]).
//! 5. Call `KVM_RUN`. The first instruction the vCPU executes is the kernel's
//!    PVH entry point. No firmware and no bootloader code runs before it.
//!
//! The ABI is Xen's "x86/HVM direct boot ABI" (xenbits misc/pvh.html); the
//! struct layouts are in Xen's public header `arch-x86/hvm/start_info.h`.

use kvm_bindings::{kvm_fpu, kvm_regs, kvm_segment, kvm_sregs};
use linux_loader::configurator::pvh::PvhBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::elf::start_info::{
    hvm_memmap_table_entry, hvm_modlist_entry, hvm_start_info,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

// Guest-physical layout. RAM starts at 0; everything below 1 MiB is ours.
//
//   0x0000_0500  GDT, 4 entries of 8 bytes (0x500..0x520)
//   0x0000_6000  hvm_start_info (56 bytes in version 1)
//   0x0000_6100  module list: one hvm_modlist_entry for the initrd
//   0x0000_7000  memory map: two or three hvm_memmap_table_entry
//   0x0002_0000  kernel command line, up to CMDLINE_MAX bytes
//   0x0009_fc00  end of the RAM the memory map reports below 1 MiB
//   0x0010_0000  HIMEM_START: the kernel's ELF segments load at or above this
//   top of low RAM   the initrd, page aligned (placed by src/machine.rs)
//   0xc000_0000  MMIO_HOLE_START: low RAM ends here at the latest
//   0xd000_0000    virtio-blk registers (src/main.rs), in the hole
//   0xd000_1000    virtio-net registers (src/main.rs), in the hole
//   0xfee0_0000    the local APIC page that KVM maps, in the hole
//   0xfffb_d000    KVM's TSS pages (src/machine.rs), in the hole
//   0x1_0000_0000  HIGH_RAM_START: the RAM that does not fit below the hole
//
// The low addresses are not special to KVM. They only have to be inside RAM,
// not overlap each other, and stay below the kernel. The kernel reads them once
// at boot and then reuses the memory, because the memory map reports it as RAM.

/// Where the GDT (global descriptor table) is written. The kernel may reload
/// segment registers from it before it installs its own GDT.
pub const GDT_ADDR: u64 = 0x500;
/// Where `hvm_start_info` is written. `%ebx` holds this address at entry.
pub const START_INFO_ADDR: u64 = 0x6000;
/// Where the module list (the initrd's address and size) is written.
pub const MODLIST_ADDR: u64 = 0x6100;
/// Where the memory map table is written.
pub const MEMMAP_ADDR: u64 = 0x7000;
/// Where the NUL-terminated kernel command line is written.
pub const CMDLINE_ADDR: u64 = 0x20000;
/// The largest command line accepted, including the terminating NUL.
pub const CMDLINE_MAX: usize = 4096;
/// The ELF segments must load at or above this (the kernel links at 16 MiB anyway).
pub const HIMEM_START: u64 = 0x10_0000;
/// End of the conventional memory a PC reports below 1 MiB (the EBDA follows).
const EBDA_START: u64 = 0x9_fc00;

/// Guest RAM stops here and starts again at 4 GiB. The hole between holds
/// addresses that must not be RAM: device registers, the local APIC page at
/// 0xfee0_0000 that KVM maps for each vCPU, and KVM's TSS pages. With RAM
/// over 0xfee0_0000, KVM_CREATE_VCPU fails with EEXIST. PCs leave the same hole, the 32-bit
/// MMIO (memory-mapped I/O) hole, below 4 GiB.
pub const MMIO_HOLE_START: u64 = 0xc000_0000;
/// Where the RAM above the hole starts: 4 GiB.
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

/// Split `mem_size` bytes of guest RAM around the MMIO hole, as
/// `(guest-physical start, size)` pairs: low RAM from 0, then high RAM from
/// 4 GiB if there is more than fits below the hole.
pub fn ram_regions(mem_size: u64) -> Vec<(u64, u64)> {
    let low = mem_size.min(MMIO_HOLE_START);
    let mut regions = vec![(0, low)];
    if mem_size > low {
        regions.push((HIGH_RAM_START, mem_size - low));
    }
    regions
}

/// The value the kernel checks in `hvm_start_info.magic`. It is the ASCII
/// string "xEn3" with bit 7 of the "E" set, stored little-endian.
const XEN_HVM_START_MAGIC_VALUE: u32 = 0x336e_c578;
/// Memory map entry type 1: usable RAM. The type numbers are the e820 ones.
const E820_RAM: u32 = 1;

/// Write `hvm_start_info`, its memory map and the initrd's module entry.
///
/// `regions` is guest RAM as [`ram_regions`] returns it. `initrd` is
/// `(guest-physical address, size in bytes)`, if there is one.
/// The command line must already be at `CMDLINE_ADDR`: this function only
/// records its address.
pub fn write_pvh_params(
    mem: &GuestMemoryMmap,
    regions: &[(u64, u64)],
    initrd: Option<(u64, u64)>,
) -> Result<(), String> {
    // The start info is the only thing the kernel finds by itself (through
    // %ebx). Every other structure is found through an address in it.
    let mut start_info = hvm_start_info {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1, // version 1 has the memory map fields
        cmdline_paddr: CMDLINE_ADDR,
        memmap_paddr: MEMMAP_ADDR,
        ..Default::default()
    };
    // The e820-style map the kernel will use as its RAM layout: low memory up
    // to the EBDA, then everything from 1 MiB to the end of low RAM, then the
    // high RAM above 4 GiB if there is any. The legacy VGA/BIOS hole below
    // 1 MiB stays out, as on a real PC. The MMIO hole below 4 GiB is not in
    // the map either, so the kernel never uses those addresses as RAM.
    //
    // The two holes differ. The legacy hole (0xa0000 to 0xfffff) is inside
    // slot 0, so to KVM it is plain RAM; no device is emulated there. It is
    // left out of the map only because the kernel expects a PC's layout, and
    // the kernel does not use memory that the map does not report. The MMIO
    // hole (MMIO_HOLE_START to 4 GiB) has no slot at all: an access there
    // exits to the VMM, which is where the virtio registers live.
    let ram = |addr, size| hvm_memmap_table_entry {
        addr,
        size,
        type_: E820_RAM,
        reserved: 0,
    };
    let (_, low_size) = regions[0];
    let mut memmap = vec![
        ram(0, EBDA_START),
        ram(HIMEM_START, low_size - HIMEM_START),
    ];
    memmap.extend(
        regions[1..].iter().map(|&(addr, size)| ram(addr, size)),
    );
    start_info.memmap_entries = memmap.len() as u32;
    // The module list holds the initrd. `mods` must exist in both branches
    // because `set_modules` below borrows it; with no initrd it is unused and
    // `nr_modules` stays 0, so the kernel does not read it.
    let mods;
    if let Some((addr, size)) = initrd {
        start_info.nr_modules = 1;
        start_info.modlist_paddr = MODLIST_ADDR;
        // cmdline_paddr 0: the module has no command line of its own.
        mods = [hvm_modlist_entry {
            paddr: addr,
            size,
            cmdline_paddr: 0,
            reserved: 0,
        }];
    } else {
        mods = [hvm_modlist_entry::default()];
    }
    // `BootParams` collects the structures; `write_bootparams` writes them all
    // into guest memory at the addresses given here.
    let mut params = BootParams::new::<hvm_start_info>(
        &start_info,
        GuestAddress(START_INFO_ADDR),
    );
    // `add_section(e, Some(addr))` places `e` *at* addr; `None` appends after
    // the previous one. Passing the table's address for every entry silently
    // overwrites entry 0, and a kernel with no RAM below 1 MiB panics with
    // "Real mode trampoline was not allocated".
    for (i, e) in memmap.iter().enumerate() {
        let at = if i == 0 {
            Some(GuestAddress(MEMMAP_ADDR))
        } else {
            None
        };
        params
            .add_section::<hvm_memmap_table_entry>(e, at)
            .map_err(|e| e.to_string())?;
    }
    if initrd.is_some() {
        params.set_modules::<hvm_modlist_entry>(
            &mods,
            GuestAddress(MODLIST_ADDR),
        );
    }
    PvhBootConfigurator::write_bootparams::<GuestMemoryMmap>(
        &params, mem,
    )
    .map_err(|e| format!("PVH params: {e}"))
}

/// One GDT entry in the packed hardware format (Intel SDM Vol. 3A, 3.4.5).
///
/// A segment descriptor is 8 bytes, and the hardware splits the base and the
/// limit into pieces for historical (80286) reasons:
///
/// ```text
///  bits 63..56  base 31..24
///  bits 55..52  flags: G (granularity), D/B (32-bit), L (64-bit code), AVL
///  bits 51..48  limit 19..16
///  bits 47..40  access byte: P (present), DPL (ring, 2 bits), S, type (4 bits)
///  bits 39..16  base 23..0
///  bits 15..0   limit 15..0
/// ```
///
/// `flags` packs the access byte in its bits 7..0 and the four flag bits in
/// its bits 15..12, so shifting it left by 40 puts both in place. The mask
/// `0xf0ff` drops bits 11..8, which would otherwise land on limit 19..16.
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let (base, limit, flags) =
        (base as u64, limit as u64, flags as u64);
    ((base & 0xff00_0000) << 32)
        | ((flags & 0xf0ff) << 40)
        | ((limit & 0x000f_0000) << 32)
        | ((base & 0x00ff_ffff) << 16)
        | (limit & 0xffff)
}

/// The same descriptor as KVM wants it: unpacked into `kvm_segment` fields.
///
/// `KVM_SET_SREGS` does not read the GDT in guest memory. It loads each
/// segment register's hidden part (base, limit, attributes) directly from a
/// `kvm_segment`. That hidden part is what a real CPU caches when software
/// loads a selector. This function produces the values the CPU would cache if
/// the guest loaded `selector` from a GDT holding `entry`.
fn segment(entry: u64, selector: u16) -> kvm_segment {
    let g = ((entry >> 55) & 1) as u8;
    let raw_limit =
        (((entry >> 32) & 0x000f_0000) | (entry & 0xffff)) as u32;
    kvm_segment {
        base: ((entry >> 16) & 0x00ff_ffff)
            | ((entry >> 32) & 0xff00_0000),
        // With G=1 the 20-bit limit counts 4 KiB pages; KVM wants it in bytes.
        // 0xfffff pages becomes 0xffff_ffff bytes: the whole 4 GiB.
        limit: if g == 1 {
            (raw_limit << 12) | 0xfff
        } else {
            raw_limit
        },
        selector,
        type_: ((entry >> 40) & 0xf) as u8,
        present: ((entry >> 47) & 1) as u8,
        dpl: ((entry >> 45) & 3) as u8,
        db: ((entry >> 54) & 1) as u8,
        s: ((entry >> 44) & 1) as u8,
        l: ((entry >> 53) & 1) as u8,
        g,
        avl: ((entry >> 52) & 1) as u8,
        // KVM's own flag (not a hardware bit): a segment that is not present
        // is marked unusable. VT-x has a matching "segment unusable" bit.
        unusable: if (entry >> 47) & 1 == 1 { 0 } else { 1 },
        padding: 0,
    }
}

/// Registers for the PVH entry: flat 32-bit protected mode, paging off,
/// `%ebx` = `hvm_start_info`. The PVH ABI (xenbits misc/pvh.html) asks for a
/// 4 GiB code and data segment and a valid TR; the GDT itself goes into guest
/// memory so the selectors mean something if the kernel reloads them.
///
/// `sregs` comes in holding KVM's reset values (from `KVM_GET_SREGS`), so the
/// fields not set here (for example CR2, CR8 and the APIC base) keep them.
pub fn pvh_registers(
    mem: &GuestMemoryMmap,
    entry: u64,
    sregs: &mut kvm_sregs,
) -> Result<kvm_regs, String> {
    // Access bytes: 0x9b = present, ring 0, code segment, execute/read,
    // accessed. 0x93 = present, ring 0, data segment, read/write, accessed.
    // 0x8b = present, ring 0, system segment of type 0xb (busy 32-bit TSS).
    // Flags 0xc = G (limit in 4 KiB pages) and D/B (32-bit operands).
    let gdt = [
        gdt_entry(0, 0, 0),            // 0x00 null
        gdt_entry(0xc09b, 0, 0xfffff), // 0x08 code: present, ring 0, exec/read, 32-bit, 4 KiB granular
        gdt_entry(0xc093, 0, 0xfffff), // 0x10 data: present, ring 0, read/write
        gdt_entry(0x008b, 0, 0x67), // 0x18 TSS (busy), 104 bytes
    ];
    for (i, e) in gdt.iter().enumerate() {
        mem.write_obj(*e, GuestAddress(GDT_ADDR + 8 * i as u64))
            .map_err(|e| e.to_string())?;
    }
    // GDTR: the GDT's base address and its limit (size in bytes, minus 1).
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = (gdt.len() * 8 - 1) as u16;
    // No IDT (interrupt descriptor table). Interrupts are disabled (RFLAGS.IF
    // is 0 below), and the kernel installs its own IDT before it enables them.
    sregs.idt.base = 0;
    sregs.idt.limit = 0;
    // A selector is the descriptor's byte offset in the GDT (index * 8), with
    // RPL (requested privilege level) 0 in its low two bits.
    sregs.cs = segment(gdt[1], 0x08);
    let data = segment(gdt[2], 0x10);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    // TR (task register): VT-x refuses to enter a guest whose TR is not a
    // usable, busy TSS (Intel SDM Vol. 3C, checks on guest segment registers), even though nothing here
    // uses hardware task switching.
    sregs.tr = segment(gdt[3], 0x18);
    sregs.cr0 = 0x1 | 0x10; // PE (protected mode) + ET; PG stays clear
                            // CR3 (page table base) and CR4 (paging extensions) mean nothing while
                            // CR0.PG is 0. EFER (extended feature enable register) 0 means long mode
                            // is off. The kernel sets all three when it builds its page tables.
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.efer = 0;
    // RIP = the PVH entry point from the ELF note. RBX = the start info, as the
    // ABI requires. RFLAGS bit 1 is reserved and must be 1; IF (bit 9) is 0,
    // so the guest starts with interrupts disabled. All other registers are 0.
    Ok(kvm_regs {
        rip: entry,
        rbx: START_INFO_ADDR,
        rflags: 0x2,
        ..Default::default()
    })
}

/// x87 control word and MXCSR at their reset values, as Firecracker sets them.
///
/// FCW (floating-point control word) 0x37f masks every x87 exception and
/// selects 64-bit precision with round-to-nearest. MXCSR (the SSE control and
/// status register) 0x1f80 masks every SSE exception. 0x37f is the value
/// `FNINIT` sets, and 0x1f80 is the MXCSR value after reset (Intel SDM Vol. 1).
pub fn fpu() -> kvm_fpu {
    kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packing a descriptor and unpacking it again gives back the fields the
    /// flags were meant to set. The packed value is checked against the
    /// well-known flat code descriptor, so a wrong shift cannot pass both ways.
    #[test]
    fn gdt_round_trip() {
        let code = gdt_entry(0xc09b, 0, 0xfffff);
        assert_eq!(code, 0x00cf_9b00_0000_ffff); // the textbook flat 32-bit code descriptor
        let s = segment(code, 0x08);
        assert_eq!(
            (s.base, s.limit, s.type_, s.db, s.l, s.g, s.present),
            (0, 0xffff_ffff, 0xb, 1, 0, 1, 1)
        );
        let tss = segment(gdt_entry(0x008b, 0, 0x67), 0x18);
        assert_eq!(
            (tss.limit, tss.type_, tss.s, tss.g),
            (0x67, 0xb, 0, 0)
        );
    }

    /// RAM that fits below the hole is one region. Anything more continues at
    /// 4 GiB, so no region ever covers the hole (and the local APIC page in it).
    #[test]
    fn ram_regions_skip_the_hole() {
        const GIB: u64 = 1 << 30;
        assert_eq!(ram_regions(256 << 20), vec![(0, 256 << 20)]);
        assert_eq!(ram_regions(3 * GIB), vec![(0, 3 * GIB)]);
        assert_eq!(
            ram_regions(4 * GIB),
            vec![(0, 3 * GIB), (4 * GIB, GIB)]
        );
        for (start, size) in ram_regions(64 * GIB) {
            assert!(
                start + size <= MMIO_HOLE_START
                    || start >= HIGH_RAM_START
            );
        }
    }
}
