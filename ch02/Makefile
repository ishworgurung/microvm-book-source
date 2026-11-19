# Chapter 2: a VM from /dev/kvm. Needs Rust (rustup) and
# read/write access to /dev/kvm; `make check` tests for it.
CARGO ?= cargo
BIN   := target/release

all:
	$(CARGO) build --release
run: all
	$(BIN)/raw
	$(BIN)/hello
	$(BIN)/long
test:
	$(CARGO) test --release
check:
	./check-kvm.sh
# Reassemble the guests from guest/*.asm (needs nasm). The
# test in src/guest.rs checks that src/guest.rs matches.
guest:
	nasm -f bin -o guest/hello16.bin guest/hello16.asm
	nasm -f bin -o guest/hello64.bin guest/hello64.asm
clean:
	$(CARGO) clean
.PHONY: all run test check guest clean
