# The guest for the chapter 5 measurements: the host's own kernel, a static
# /init, the virtio modules the distribution kernel builds as modules, and a
# small ext4 data disk.
#   make          build everything below into guest/out/
KVER    ?= $(shell uname -r)
# Load order matters: dependencies first. A module that the kernel has
# built in is skipped.
MODULES := virtio_mmio failover net_failover virtio_net vsock \
           vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport \
           virtio_rng virtio_balloon virtio_mem

all: out/vmlinux out/initramfs.cpio out/data.ext4

# The uncompressed ELF kernel (with the PVH entry note), unpacked from
# the bzImage that boots this host.
out/vmlinux:
	mkdir -p out && ../../../ch03/ci/get-vmlinux.sh $@

out/init: init.c
	mkdir -p out && $(CC) -O2 -Wall -Wextra -static -o $@ $<

# Each module is stored uncompressed, in load order.
out/initramfs.cpio: out/init
	rm -rf out/root
	mkdir -p out/root/proc out/root/dev out/root/sys out/root/lib/modules
	cp out/init out/root/init
	for m in $(MODULES); do \
	  p=$$(modinfo -k $(KVER) -n $$m 2>/dev/null || true); \
	  case "$$p" in \
	  ""|"(builtin)") echo "module $$m: built in or absent";; \
	  *.zst) b=$$(basename $$p .zst); zstd -q -d -c $$p > out/root/lib/modules/$$b; \
	     echo $$b >> out/root/lib/modules/order; echo "module $$m: $$p";; \
	  *) b=$$(basename $$p); cp $$p out/root/lib/modules/$$b; \
	     echo $$b >> out/root/lib/modules/order; echo "module $$m: $$p";; \
	  esac; \
	done
	cd out/root && find . | cpio -o -H newc --quiet > ../initramfs.cpio

# 16 MiB ext4 built from a directory, no root or loop device needed.
out/data.ext4:
	mkdir -p out/disk
	echo "hello from the host's disk image" > out/disk/hello.txt
	mkfs.ext4 -q -F -d out/disk $@ 16M

clean:
	rm -rf out

.PHONY: all clean
