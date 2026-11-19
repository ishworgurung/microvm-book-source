# Re-measure chapter 5 on a host with /dev/kvm: every number the chapter
# quotes for Firecracker and Cloud Hypervisor, on the pinned releases.
#   make run      fetch, build the guest, run fc.py, ch.py and uffd_demo
FC_VER := v1.18.0
FC_TGZ := firecracker-$(FC_VER)-x86_64.tgz
FC_URL := https://github.com/firecracker-microvm/firecracker/releases/download/$(FC_VER)
FC     := vendor/release-$(FC_VER)-x86_64/firecracker-$(FC_VER)-x86_64
CH_VER := v54.0
CH_URL := https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$(CH_VER)
CH     := vendor/cloud-hypervisor
# Cloud Hypervisor publishes no checksum file: these are the SHA-256 of
# the v54.0 static binaries, so a changed download is caught.
CH_SHA := 4f4f98a3d661a40f3e5272627cc895a1b2e525f2f6bedf5f895f9cd21ba5c65b
CR_SHA := acd5be240d5c73373effbe818908fc03e313ec8486237904bff11d93b71b1c83

all: $(FC) $(CH) guest uffd_demo

$(FC):
	mkdir -p vendor
	cd vendor && curl -sSLO $(FC_URL)/$(FC_TGZ) \
	  && curl -sSLO $(FC_URL)/$(FC_TGZ).sha256.txt \
	  && sha256sum -c $(FC_TGZ).sha256.txt && tar xzf $(FC_TGZ)

$(CH):
	mkdir -p vendor
	curl -sSL -o vendor/cloud-hypervisor $(CH_URL)/cloud-hypervisor-static
	curl -sSL -o vendor/ch-remote $(CH_URL)/ch-remote-static
	echo "$(CH_SHA)  vendor/cloud-hypervisor" | sha256sum -c
	echo "$(CR_SHA)  vendor/ch-remote" | sha256sum -c
	chmod +x vendor/cloud-hypervisor vendor/ch-remote

guest:
	$(MAKE) -C guest

uffd_demo: uffd_demo.c
	$(CC) -O2 -Wall -std=gnu11 -pthread -o $@ $<

run: all
	FC_VER=$(FC_VER) python3 fc.py all
	FC_VER=$(FC_VER) python3 ch.py all
	./uffd_demo 64

clean:
	rm -rf vendor uffd_demo
	$(MAKE) -C guest clean

.PHONY: all guest run clean
