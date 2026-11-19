# Build and run every chapter's code. Needs a C compiler, Rust (rustup),
# and /dev/kvm for the parts that boot a guest. See README.md.
.PHONY: all clean
all:
	$(MAKE) -C code/ch01 run
	$(MAKE) -C code/ch02 test
	$(MAKE) -C code/ch03 build test
	$(MAKE) -C code/ch04 test
	$(MAKE) -C code/ch05 run
clean:
	$(MAKE) -C code/ch01 clean
	$(MAKE) -C code/ch02 clean
	$(MAKE) -C code/ch03 clean
	$(MAKE) -C code/ch04 clean
	$(MAKE) -C code/ch05 clean
