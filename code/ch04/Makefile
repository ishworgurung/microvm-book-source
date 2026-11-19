# Chapter 4: virtio-blk and virtio-net on the virtio-mmio transport.
#   make          build vmm4
#   make test     play the guest driver against the devices (no KVM)
#   make disk     a 64 MiB ext4 disk image, made without root
#   make run      boot to chapter 3's busybox shell, with the disk
#   make run-net  the same, plus a tap device (needs CAP_NET_ADMIN)
#
# KERNEL and INITRD are chapter 3's: a vmlinux with CONFIG_PVH=y and
# the initramfs that `make shell` in ../ch03 builds.
CARGO  ?= cargo
VMM    := target/release/vmm4
KERNEL ?= /usr/lib/modules/$(shell uname -r)/build/vmlinux
INITRD ?= ../ch03/guest/out/initramfs.cpio
EXTRA  := initcall_blacklist=i8042_init labexit=shell quiet
TAP    ?= tap0

all:
	$(CARGO) build --release

test:
	$(CARGO) test --quiet

disk.img:
	truncate -s 64M disk.img
	mkfs.ext4 -q -F disk.img
disk: disk.img

run: all disk.img
	$(VMM) --kernel $(KERNEL) --initrd $(INITRD) --disk disk.img \
	  --cmdline-extra "$(EXTRA)"

run-net: all disk.img
	$(VMM) --kernel $(KERNEL) --initrd $(INITRD) --disk disk.img \
	  --tap $(TAP) --cmdline-extra "$(EXTRA)"

clean:
	$(CARGO) clean
	rm -f disk.img

.PHONY: all test disk run run-net clean
