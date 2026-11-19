#!/usr/bin/env bash
# check-firecracker.sh: the CI check of chapter 5's Firecracker
# material, on a runner with /dev/kvm. Run from code/ch05 after
# `make fetch`. The output is for checking the book's claims,
# not for printing in it.
set -euo pipefail
V=${FC_VER:-v1.18.0}
R=vendor/release-$V-x86_64
FC=$R/firecracker-$V-x86_64
tmp=$(mktemp -d)

echo "== guest: static tick /init, runner kernel as a bzImage"
cc -static -O2 -o "$tmp/init" ci/tick.c
(cd "$tmp" && echo init | cpio -o -H newc --quiet > initrd.cpio)
sudo cat "/boot/vmlinuz-$(uname -r)" > "$tmp/bzImage"
file "$tmp/bzImage"

echo "== boot, snapshot and restore with $V"
KERNEL=$tmp/bzImage INITRD=$tmp/initrd.cpio FC=$FC \
	./fc-snapshot.sh | tee "$tmp/run.log"
grep -q "^tick" "$tmp/run.log"

if [ -n "${FC_OLD:-}" ]; then
	echo "== snapshot with $FC_OLD, restore with $V"
	KERNEL=$tmp/bzImage INITRD=$tmp/initrd.cpio FC=$FC_OLD \
		FC_RESTORE=$FC ./fc-snapshot.sh | tee "$tmp/cross.log"
	grep -q "^tick" "$tmp/cross.log"
fi

echo "== seccomp: rules per thread, compiled size"
python3 - "$R/seccomp-filter-$V-x86_64.json" <<'PY'
import json, sys
for t, f in json.load(open(sys.argv[1])).items():
    print(t, len(f["filter"]), "rules")
PY
"$R/seccompiler-bin-$V-x86_64" --target-arch x86_64 \
	--input-file "$R/seccomp-filter-$V-x86_64.json" \
	--output-file "$tmp/bpf"
echo "compiled filters: $(stat -c %s "$tmp/bpf") bytes"
python3 - "$tmp/bpf" "$FC" <<'PY'
import sys
b, e = (open(p, "rb").read() for p in sys.argv[1:])
print("same bytes inside firecracker:", b in e)
PY
echo "firecracker binary: $(stat -c %s "$FC") bytes"

echo "== 33 vCPUs: refused at machine-config or InstanceStart?"
sock=$tmp/v.sock
"$FC" --api-sock "$sock" >"$tmp/v.log" 2>&1 &
pid=$!
while [ ! -S "$sock" ]; do sleep 0.01; done
req() {
	curl -sS --unix-socket "$sock" -X "$1" "http://localhost$2" \
		-H 'Content-Type: application/json' -d "$3" \
		-w ' -> HTTP %{http_code}\n'
}
req PUT /boot-source "{\"kernel_image_path\": \"$tmp/bzImage\"}"
req PUT /machine-config '{"vcpu_count": 33, "mem_size_mib": 256}'
req PUT /actions '{"action_type": "InstanceStart"}'
kill "$pid" 2>/dev/null || true
rm -rf "$tmp"
