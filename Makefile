# Build and run every chapter's code. Needs a C compiler, Rust (rustup),
# and /dev/kvm for the parts that boot a guest. See README.md.
.PHONY: all clean
all:
	$(MAKE) -C ch01 run
	$(MAKE) -C ch02 test
	$(MAKE) -C ch03 build test
	$(MAKE) -C ch04 test
	$(MAKE) -C ch05 run
clean:
	$(MAKE) -C ch01 clean
	$(MAKE) -C ch02 clean
	$(MAKE) -C ch03 clean
	$(MAKE) -C ch04 clean
	$(MAKE) -C ch05 clean
