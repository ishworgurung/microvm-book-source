#!/bin/sh
# Make an uncompressed vmlinux from the running kernel's bzImage, for
# the CI boot test. The bzImage holds the kernel ELF file compressed;
# extract-vmlinux (from the Linux source) finds and unpacks it.
# Usage: ci/get-vmlinux.sh OUT
set -eu
out=${1:?usage: get-vmlinux.sh OUT}
tmp=$(mktemp -d)
script=/usr/src/linux-headers-$(uname -r)/scripts/extract-vmlinux
if [ ! -x "$script" ]; then
	script=$tmp/extract-vmlinux
	curl -fsSL -o "$script" \
	  https://raw.githubusercontent.com/torvalds/linux/v6.8/scripts/extract-vmlinux
	chmod +x "$script"
fi
sudo cat "/boot/vmlinuz-$(uname -r)" > "$tmp/vmlinuz"
"$script" "$tmp/vmlinuz" > "$out"
rm -rf "$tmp"
file "$out"
