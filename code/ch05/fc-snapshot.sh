#!/usr/bin/env bash
# fc-snapshot.sh: boot a guest in Firecracker through its API,
# snapshot it, and restore the snapshot in a second process.
#
#   KERNEL=vmlinux INITRD=initrd.cpio ./fc-snapshot.sh
#
# Needs /dev/kvm, curl, and the Firecracker release that
# `make fetch` downloads. KERNEL is a vmlinux or, since
# Firecracker v1.17.0, a bzImage. INITRD is optional. The
# guest must keep running (a shell, say): one that reboots
# ends Firecracker before the snapshot. Every
# request is one curl call on the API's Unix socket; chapter 5
# explains each one. FC_RESTORE, if set, is a second Firecracker
# binary for the restore, to try a snapshot across releases.
set -euo pipefail

FC=${FC:-vendor/release-v1.18.0-x86_64/firecracker-v1.18.0-x86_64}
FC_RESTORE=${FC_RESTORE:-$FC}
KERNEL=${KERNEL:?set KERNEL to a vmlinux or bzImage file}
INITRD=${INITRD:-}
WORK=$(mktemp -d)
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; rm -rf "$WORK"' EXIT

# One API request: api SOCKET METHOD PATH [JSON]
api() {
	curl -sS --fail-with-body --unix-socket "$1" \
		-X "$2" "http://localhost$3" \
		${4:+-H 'Content-Type: application/json' -d "$4"}
	echo
}

# Start Firecracker with an API socket, console to a log file:
# start NAME [BINARY]. Its process ID goes into $pid.
start() {
	"${2:-$FC}" --api-sock "$WORK/$1.sock" >"$WORK/$1.log" 2>&1 &
	pid=$!
	pids+=("$pid")
	while [ ! -S "$WORK/$1.sock" ]; do sleep 0.01; done
}

initrd=${INITRD:+\"initrd_path\": \"$INITRD\",}
args="console=ttyS0 reboot=k panic=-1"
args="$args i8042.noaux i8042.nomux i8042.dumbkbd"

# 1. Boot: describe the machine, then start it.
start a
S=$WORK/a.sock
api "$S" PUT /boot-source "{
  \"kernel_image_path\": \"$KERNEL\", $initrd
  \"boot_args\": \"$args\" }"
api "$S" PUT /machine-config \
	'{"vcpu_count": 2, "mem_size_mib": 256}'
api "$S" PUT /actions '{"action_type": "InstanceStart"}'
sleep 1

# 2. Pause the vCPUs and write the two snapshot files.
api "$S" PATCH /vm '{"state": "Paused"}'
api "$S" PUT /snapshot/create "{
  \"snapshot_type\": \"Full\",
  \"snapshot_path\": \"$WORK/vm.snap\",
  \"mem_file_path\": \"$WORK/vm.mem\" }"
ls -l "$WORK/vm.snap" "$WORK/vm.mem"
kill "$pid"

# 3. Restore in a new, unconfigured Firecracker. The File
#    backend maps vm.mem; pages load when the guest uses them.
start b "$FC_RESTORE"
S2=$WORK/b.sock
api "$S2" PUT /snapshot/load "{
  \"snapshot_path\": \"$WORK/vm.snap\",
  \"mem_backend\": {\"backend_type\": \"File\",
                  \"backend_path\": \"$WORK/vm.mem\"},
  \"resume_vm\": true }"
sleep 1
grep VmRSS "/proc/$pid/status"
echo "console of the restored guest:"; tail -n 5 "$WORK/b.log"
