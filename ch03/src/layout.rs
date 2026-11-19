//! Where everything goes in guest-physical memory.
//!
//! The VMM chooses this layout. The kernel learns it from the
//! memory map in `hvm_start_info` (see boot.rs), and the only
//! address it finds by itself is START_INFO, through %ebx.
//!
//!   0x0000_0500  GDT: 4 entries of 8 bytes
//!   0x0000_6000  hvm_start_info
//!   0x0000_6100  module list: one entry, the initrd
//!   0x0000_7000  memory map: 2 or 3 entries
//!   0x0002_0000  kernel command line
//!   0x0009_fc00  end of the RAM reported below 1 MiB
//!   0x0010_0000  the kernel's ELF segments load above this
//!   top of low RAM  the initrd
//!   0xc000_0000  MMIO hole: no RAM from here to 4 GiB
//!   0xfee0_0000    local APIC page, mapped by KVM
//!   0xfffb_d000    three pages for KVM's own TSS
//!   0x1_0000_0000  high RAM: whatever did not fit below

pub const GDT: u64 = 0x500;
pub const START_INFO: u64 = 0x6000;
pub const MODLIST: u64 = 0x6100;
pub const MEMMAP: u64 = 0x7000;
pub const CMDLINE: u64 = 0x2_0000;
/// The longest command line, including the final NUL.
pub const CMDLINE_MAX: usize = 4096;
/// Conventional memory ends here on a PC; the BIOS data
/// area and the video memory follow.
pub const EBDA_START: u64 = 0x9_fc00;
/// 1 MiB. The kernel image must load at or above this.
pub const HIMEM_START: u64 = 0x10_0000;
pub const MMIO_HOLE_START: u64 = 0xc000_0000;
pub const KVM_TSS: u64 = 0xfffb_d000;
pub const HIGH_RAM_START: u64 = 0x1_0000_0000;

/// Split `size` bytes of RAM around the MMIO hole, as
/// (guest-physical start, length) pairs.
pub fn ram_regions(size: u64) -> Vec<(u64, u64)> {
    let low = size.min(MMIO_HOLE_START);
    let mut regions = vec![(0, low)];
    if size > low {
        regions.push((HIGH_RAM_START, size - low));
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIB: u64 = 1 << 30;

    #[test]
    fn small_guests_get_one_region() {
        assert_eq!(ram_regions(256 << 20), vec![(0, 256 << 20)]);
        assert_eq!(ram_regions(3 * GIB), vec![(0, 3 * GIB)]);
    }

    #[test]
    fn no_region_covers_the_hole() {
        let r = ram_regions(4 * GIB);
        assert_eq!(r, vec![(0, 3 * GIB), (4 * GIB, GIB)]);
        for (start, len) in ram_regions(64 * GIB) {
            let end = start + len;
            assert!(
                end <= MMIO_HOLE_START || start >= HIGH_RAM_START
            );
        }
    }
}
