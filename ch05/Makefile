# Chapter 5: production VMMs.
#   make run     build and run the two programs (no /dev/kvm needed)
#   make fetch   download and checksum Firecracker v1.18.0 into vendor/
#   make fc      run fc-snapshot.sh (needs /dev/kvm and KERNEL=...)
FC_VER := v1.18.0
TGZ    := firecracker-$(FC_VER)-x86_64.tgz
URL    := https://github.com/firecracker-microvm/firecracker/releases/download/$(FC_VER)
BIN    := target/release

all:
	cargo build --release --locked

# `kill` ends the whole process with SIGSYS, so its exit status
# (159 = 128 + 31) is printed instead of stopping make.
run: all
	$(BIN)/seccomp_thread
	-$(BIN)/seccomp_thread kill; echo "exit status $$?"
	$(BIN)/lazy_restore

fetch:
	mkdir -p vendor
	cd vendor && curl -sSLO $(URL)/$(TGZ) \
	  && curl -sSLO $(URL)/$(TGZ).sha256.txt \
	  && sha256sum -c $(TGZ).sha256.txt && tar xzf $(TGZ)

fc: fetch
	./fc-snapshot.sh

clean:
	cargo clean
	rm -rf vendor
.PHONY: all run fetch fc clean
