#!/bin/sh
# Can this machine run the chapter 2 programs? Each check
# prints ok or the reason it failed, and a fix.
ok=1
if grep -qwE 'vmx|svm' /proc/cpuinfo; then
    echo "ok   the CPU has VT-x or AMD-V (vmx/svm in cpuinfo)"
else
    echo "FAIL no vmx or svm flag: enable virtualisation in the"
    echo "     firmware, or ask your cloud for nested virt"
    ok=0
fi
if [ -e /dev/kvm ]; then
    echo "ok   /dev/kvm exists"
else
    echo "FAIL no /dev/kvm: try 'sudo modprobe kvm_intel'"
    echo "     (or kvm_amd)"
    ok=0
fi
if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    echo "ok   you can read and write /dev/kvm"
elif [ -e /dev/kvm ]; then
    echo "FAIL no access: 'sudo usermod -aG kvm $USER', then"
    echo "     log out and in again"
    ok=0
fi
for f in /sys/module/kvm_intel/parameters/nested \
         /sys/module/kvm_amd/parameters/nested; do
    [ -r "$f" ] && echo "info nested virtualisation: $(cat "$f")"
done
[ "$ok" = 1 ]
