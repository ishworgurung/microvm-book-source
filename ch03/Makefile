# Chapter 3: boot a Linux kernel in rvmm.
#   make build       build rvmm (no /dev/kvm needed)
#   make test        unit tests (no /dev/kvm needed)
#   make run         boot KERNEL with the guest initramfs
#   make run-i8042   the same, with the guest's i8042 driver on
#   make run-absent  the same, absent I/O ports reading 0xff
#   make run-memmap  a 4096 MiB guest's memory map
#   make shell       boot to a busybox shell (BUSYBOX=...)
#
# KERNEL must be an uncompressed vmlinux built with CONFIG_PVH=y.
# Arch and Manjaro ship one in the kernel's build directory.
KERNEL  ?= /usr/lib/modules/$(shell uname -r)/build/vmlinux
INITRD  := guest/out/initramfs.cpio
RVMM    := target/release/rvmm
ARGS    := --kernel $(KERNEL) --initrd $(INITRD)
# Skip the guest's i8042 driver: rvmm's i8042 only resets.
NOKBD   := initcall_blacklist=i8042_init

build:
	cargo build --release --locked

test:
	cargo test --release --locked

guest:
	$(MAKE) -C guest

run: build guest
	$(RVMM) $(ARGS) --cmdline-extra "$(NOKBD)"

run-i8042: build guest
	$(RVMM) $(ARGS)

run-absent: build guest
	$(RVMM) $(ARGS) --cmdline-extra "$(NOKBD)" --absent-fill 0xff

# Lines the kernel prints about the memory map the VMM gave it.
run-memmap: build guest
	$(RVMM) $(ARGS) --mem 4096 --cmdline-extra "$(NOKBD) loglevel=8" 2>&1 \
	  | grep -aE "BIOS-e820|RAMDISK|Memory: |\[rvmm\] (slot|stopped)"

shell: build
	$(MAKE) -C guest BUSYBOX=$(BUSYBOX)
	$(RVMM) $(ARGS) --cmdline-extra "$(NOKBD) labexit=shell quiet"

clean:
	cargo clean
	$(MAKE) -C guest clean

.PHONY: build test guest run run-i8042 run-absent run-memmap \
	shell clean
