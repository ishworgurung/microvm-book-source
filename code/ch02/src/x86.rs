//! What a VMM must build in guest memory, and put in the
//! vCPU's registers, before a guest can run in 64-bit long
//! mode: a GDT, page tables, and a few control-register bits.

use kvm_bindings::{kvm_segment, kvm_sregs};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

/// Where each structure goes in guest-physical memory.
pub const GDT_ADDR: u64 = 0x500;
pub const STACK_TOP: u64 = 0x8000;
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xa000;
pub const PD_ADDR: u64 = 0xb000;

/// Control-register bits (Intel SDM vol. 3A, ch. 2.5).
pub const CR0_PE: u64 = 1 << 0; // protected mode on
pub const CR0_PG: u64 = 1 << 31; // paging on
pub const CR4_PAE: u64 = 1 << 5; // 8-byte page-table entries
/// Bits of the EFER register (an MSR).
pub const EFER_LME: u64 = 1 << 8; // long mode enabled
pub const EFER_LMA: u64 = 1 << 10; // long mode active

/// Page-table entry bits.
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE: u64 = 1 << 7; // in a PD: a 2 MiB page

/// Descriptor flags: access byte (low 8 bits) and the
/// G, D/B, L and AVL bits (bits 12 to 15).
pub const CODE64: u16 = 0xa09b; // G, L; present, code, r/x
pub const DATA: u16 = 0xc093; // G, D/B; present, data, r/w

/// Selectors are byte offsets into the GDT.
pub const CODE_SEL: u16 = 0x08;
pub const DATA_SEL: u16 = 0x10;

/// Pack one 8-byte GDT entry. The odd layout is history:
/// base and limit were split when the 286 grew into the 386.
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    // Widen to u64 so the shifts below cannot overflow. Each
    // line places one piece of the entry; the masks pick the
    // bits that belong there (see the SDM, "Segment Descriptor").
    let (b, l, f) = (base as u64, limit as u64, flags as u64);
    (b & 0xff00_0000) << 32
        | (f & 0xf0ff) << 40
        | (l & 0x000f_0000) << 32
        | (b & 0x00ff_ffff) << 16
        | (l & 0x0000_ffff)
}

/// The guest's GDT: a null entry, then code, then data.
pub fn gdt() -> [u64; 3] {
    [
        0,
        gdt_entry(CODE64, 0, 0xfffff),
        gdt_entry(DATA, 0, 0xfffff),
    ]
}

/// Unpack a GDT entry into the form KVM_SET_SREGS takes.
pub fn segment(entry: u64, selector: u16) -> kvm_segment {
    // bits(at, n): the n-bit field whose lowest bit is `at`.
    // This is gdt_entry in reverse.
    let bits = |at: u32, n: u32| (entry >> at) & ((1 << n) - 1);
    let g = bits(55, 1) as u8;
    let mut limit = (bits(48, 4) << 16 | bits(0, 16)) as u32;
    if g == 1 {
        limit = limit << 12 | 0xfff; // counted in 4 KiB pages
    }
    kvm_segment {
        base: bits(56, 8) << 24 | bits(16, 24),
        limit,
        selector,
        type_: bits(40, 4) as u8,
        s: bits(44, 1) as u8,
        dpl: bits(45, 2) as u8,
        present: bits(47, 1) as u8,
        avl: bits(52, 1) as u8,
        l: bits(53, 1) as u8,
        db: bits(54, 1) as u8,
        g,
        unusable: 0,
        padding: 0,
    }
}

/// Write the GDT and page tables that map the first 2 MiB
/// of guest memory to itself, with one 2 MiB page.
pub fn write_tables(
    mem: &GuestMemoryMmap,
) -> Result<(), vm_memory::GuestMemoryError> {
    for (i, entry) in gdt().iter().enumerate() {
        let at = GDT_ADDR + 8 * i as u64;
        mem.write_obj(*entry, GuestAddress(at))?;
    }
    // Three levels, each entry points at the next table. Table
    // addresses are 4 KiB aligned, so their low 12 bits are free
    // to hold the flag bits.
    let pml4 = PDPT_ADDR | PRESENT | WRITABLE;
    let pdpt = PD_ADDR | PRESENT | WRITABLE;
    // Address 0 (the frame), plus flags: maps 0 to 2 MiB.
    let pd = PRESENT | WRITABLE | HUGE;
    mem.write_obj(pml4, GuestAddress(PML4_ADDR))?;
    mem.write_obj(pdpt, GuestAddress(PDPT_ADDR))?;
    mem.write_obj(pd, GuestAddress(PD_ADDR))?;
    Ok(())
}

/// Switch the vCPU's special registers to 64-bit long mode.
pub fn set_long_mode(sregs: &mut kvm_sregs) {
    let table = gdt();
    sregs.gdt.base = GDT_ADDR;
    sregs.gdt.limit = (8 * table.len() - 1) as u16;
    sregs.cs = segment(table[1], CODE_SEL);
    let data = segment(table[2], DATA_SEL);
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    // Long mode needs paging; CR3 points at the top table.
    // Order of the flags below doesn't matter: KVM applies
    // them all at once when we call set_sregs.
    sregs.cr3 = PML4_ADDR;
    sregs.cr4 |= CR4_PAE;
    sregs.cr0 |= CR0_PE | CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_segment_is_64_bit_ring_0() {
        let cs = segment(gdt()[1], CODE_SEL);
        assert_eq!(
            (cs.l, cs.db, cs.dpl, cs.present),
            (1, 0, 0, 1)
        );
        assert_eq!((cs.type_, cs.s, cs.g), (0xb, 1, 1));
        assert_eq!((cs.base, cs.limit), (0, 0xffff_ffff));
    }

    #[test]
    fn data_segment_is_writable() {
        let ds = segment(gdt()[2], DATA_SEL);
        assert_eq!(
            (ds.l, ds.db, ds.type_, ds.present),
            (0, 1, 3, 1)
        );
    }

    #[test]
    fn gdt_entries_match_the_usual_constants() {
        // The values every x86-64 boot loader writes.
        assert_eq!(gdt()[1], 0x00af_9b00_0000_ffff);
        assert_eq!(gdt()[2], 0x00cf_9300_0000_ffff);
    }

    #[test]
    fn page_tables_map_the_first_2_mib() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(
            GuestAddress(0),
            crate::MEM_SIZE,
        )])
        .unwrap();
        write_tables(&mem).unwrap();
        let read =
            |a| mem.read_obj::<u64>(GuestAddress(a)).unwrap();
        assert_eq!(read(PML4_ADDR), PDPT_ADDR | 3);
        assert_eq!(read(PDPT_ADDR), PD_ADDR | 3);
        assert_eq!(read(PD_ADDR), 0x83);
        assert_eq!(read(GDT_ADDR + 8), gdt()[1]);
    }
}
