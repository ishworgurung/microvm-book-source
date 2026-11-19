# Build the guest's initramfs: a cpio archive holding /init and,
# if BUSYBOX names a static busybox binary, a shell.
#   make                       out/initramfs.cpio
#   make BUSYBOX=/path/busybox  the same, plus /bin/sh
CC      ?= cc
BUSYBOX ?=

all: out/initramfs.cpio

out/init: init.c
	mkdir -p out
	$(CC) -O2 -Wall -Wextra -static -o $@ $<

out/initramfs.cpio: out/init
	rm -rf out/root
	mkdir -p out/root/proc out/root/dev out/root/sys out/root/bin
	cp out/init out/root/init
	if [ -n "$(BUSYBOX)" ]; then \
	  cp $(BUSYBOX) out/root/bin/busybox; \
	  for c in sh ls cat mount ps dmesg uname free; do \
	    ln -s busybox out/root/bin/$$c; done; \
	fi
	cd out/root && find . | cpio -o -H newc --quiet > ../initramfs.cpio

clean:
	rm -rf out

.PHONY: all clean
