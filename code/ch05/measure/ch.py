#!/usr/bin/env python3
"""ch.py -- drive a real Cloud Hypervisor through its REST API.

The guest is the same as fc.py's: the host's kernel, the shared /init,
steered by the command line.

  uv run ch.py boot       boot timeline (5 runs), the guest's view of a PCI machine
  uv run ch.py devices    virtio-block + vsock + rng, all virtio-pci; rng throughput
  uv run ch.py net        virtio-net through a tap in an unprivileged user+net namespace
  uv run ch.py hotplug    add vCPUs, RAM and a disk to a running guest
  uv run ch.py snapshot   pause, snapshot to a directory, restore in a new process
  uv run ch.py sandbox    seccomp per thread, and Landlock refusing a path
  uv run ch.py compare    Firecracker vs Cloud Hypervisor on the same guest
  uv run ch.py all

Nothing here needs root. /dev/kvm must be accessible.
"""

import importlib.util
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from typing import Any, cast

from vmlab import GUEST, VENDOR, Vmm, api, median, rss_kib, threads, wait_for_path

CH_VER = "v54.0"
CH = os.path.join(VENDOR, "cloud-hypervisor")
KERNEL = os.path.join(GUEST, "vmlinux")
INITRD = os.path.join(GUEST, "initramfs.cpio")
DISK = os.path.join(GUEST, "data.ext4")
# v54 refuses a disk without image_type ("Image type required for disk").
# Background, not verified as Cloud Hypervisor's stated reason: probing the
# format of a guest-writable raw image lets a guest that writes a qcow2 header
# choose a backing file on the host (QEMU's CVE-2008-2004 was this class).

# Cloud Hypervisor stops when the guest powers off through ACPI. A guest
# *reboot* restarts the VM instead, so every demo ends with labexit=poweroff.
BASE_ARGS = "console=ttyS0 panic=-1"

WORK = tempfile.mkdtemp(prefix="ch-lab-")


def hr(title):
    print(f"\n=== {title} " + "=" * max(0, 70 - len(title)))


class CloudHypervisor:
    """One cloud-hypervisor process: guest serial on stdout, its log in a file,
    its API on a Unix socket."""

    def __init__(self, name, labmode="info", labexit="poweroff", cpus="boot=2", memory="size=256M",
                 extra_args="", devices=(), boot=True, restore=None, wrap=()):
        self.dir = os.path.join(WORK, name)
        os.makedirs(self.dir, exist_ok=True)
        self.sock = os.path.join(self.dir, "api.sock")
        self.log = os.path.join(self.dir, "ch.log")
        argv = [*wrap, CH, "--api-socket", f"path={self.sock}", "--log-file", self.log, "-v"]
        if restore:
            argv += ["--restore", f"source_url=file://{restore}"]
        elif boot:
            argv += ["--kernel", KERNEL, "--initramfs", INITRD,
                     "--cmdline", f"{BASE_ARGS} labmode={labmode} labexit={labexit} {extra_args}".strip(),
                     "--cpus", cpus, "--memory", memory, "--serial", "tty", "--console", "off", *devices]
        self.vm = Vmm(argv)
        wait_for_path(self.sock)
        self.t_sock = self.vm.now()

    def call(self, method, endpoint, body=None) -> Any:
        return api(self.sock, method, f"/api/v1/{endpoint}", body)[1]

    def put(self, endpoint, body=None):
        return self.call("PUT", endpoint, body)

    def get(self, endpoint) -> Any:
        return self.call("GET", endpoint)

    def log_lines(self):
        with open(self.log) as f:
            return f.read().splitlines()


def show_guest(vm, prefixes=("  ", "[init] task")):
    for t, line in vm.guest_lines():
        if line.startswith(prefixes):
            print(f"  [{t * 1000:7.1f} ms] {line}")


# ---------------------------------------------------------------- boot -----


def one_boot(labmode="info,devices"):
    ch = CloudHypervisor(f"boot-{time.monotonic_ns()}", labmode=labmode)
    t_first = ch.vm.wait_line(r".", timeout=10)[0]
    t_init, init_line, _ = ch.vm.wait_line(r"^INIT-START")
    t_done = ch.vm.wait_line(r"^INIT-DONE")[0]
    rc = ch.vm.wait()
    r: dict[str, Any] = {"ch": ch, "sock": ch.t_sock, "first": t_first, "init": t_init, "done": t_done,
                         "exit": ch.vm.now(), "rc": rc, "guest_init": float(init_line.split()[2])}
    return r


def demo_boot():
    hr("1. Boot: one run, guest console")
    r = one_boot()
    ch: CloudHypervisor = r["ch"]
    for t, line in ch.vm.guest_lines():
        print(f"  [{t * 1000:7.1f} ms] {line}")

    hr("2. Timeline, 5 runs (ms since process spawn; --kernel on the command line, no API calls)")
    runs = [one_boot() for _ in range(5)]
    for k, label in [("sock", "API socket ready"), ("first", "first console line"), ("init", "/init prints INIT-START"),
                     ("done", "/init done"), ("exit", "cloud-hypervisor exited")]:
        xs = [x[k] * 1000 for x in runs]
        print(f"  {label:28s}" + "".join(f"{x:8.1f}" for x in xs) + f"{median(xs):8.1f}")
    xs = [x["guest_init"] * 1000 for x in runs]
    print(f"  {'(guest clock at /init)':28s}" + "".join(f"{x:8.1f}" for x in xs) + f"{median(xs):8.1f}")
    print("  exit codes:", [x["rc"] for x in runs])


# ------------------------------------------------------------- devices -----


def vsock_connect(uds, port, timeout=10):
    """Cloud Hypervisor adopted Firecracker's host-side vsock protocol:
    connect to the Unix socket, send `CONNECT <port>\\n`, read `OK <n>\\n`."""
    end = time.monotonic() + timeout
    while True:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(5)
        try:
            s.connect(uds)
            s.sendall(f"CONNECT {port}\n".encode())
            ack = b""
            while not ack.endswith(b"\n"):
                b = s.recv(1)
                if not b:
                    raise ConnectionError("closed during handshake")
                ack += b
            return s, ack.decode().strip()
        except (ConnectionError, OSError):
            s.close()
            if time.monotonic() > end:
                raise
            time.sleep(0.05)


def debugfs_cat(image, path):
    return subprocess.run(["debugfs", "-R", f"cat {path}", image], check=False, capture_output=True, text=True).stdout.strip()


def demo_devices():
    hr("3. Devices: virtio-block + vsock + rng, all virtio-pci")
    disk = os.path.join(WORK, "devices-data.ext4")
    shutil.copy(DISK, disk)
    uds = os.path.join(WORK, "vsock.sock")
    ch = CloudHypervisor("devices", labmode="devices,blk,rng,vsock",
                         devices=("--disk", f"path={disk},image_type=raw", "--vsock", f"cid=3,socket={uds}"))
    ch.vm.wait_line(r"\(READY\)")
    show_guest(ch.vm)
    info = ch.get("vm.info")
    print("  GET /api/v1/vm.info device_tree (id -> PCI BDF):",
          {k: v.get("pci_bdf") for k, v in sorted(info.get("device_tree", {}).items()) if v.get("pci_bdf")})
    s, ack = vsock_connect(uds, 5000)
    print(f"  host: CONNECT 5000 -> {ack!r}")
    for msg in (b"hello over vsock\n", b"quit\n"):
        s.sendall(msg)
        print(f"  host sent {msg!r}, guest answered {s.recv(256)!r}")
    s.close()
    counters = ch.get("vm.counters")
    ch.vm.wait()
    print("  vm.counters (selected):", {d: {k: v for k, v in c.items() if k in ("read_ops", "write_ops", "rx_bytes",
                                                                                  "tx_bytes")}
                                       for d, c in counters.items() if d.startswith(("_disk", "_vsock", "_rng"))})
    print("  after exit, on the host image:", repr(debugfs_cat(disk, "/from-guest.txt")))


# ----------------------------------------------------------------- net -----

NET_INNER = "--net-inner"


def demo_net():
    hr("4. Network: virtio-net -> tap0, inside an unprivileged user+net namespace")
    r = subprocess.run(["unshare", "--user", "--map-root-user", "--net", sys.executable, os.path.abspath(__file__),
                        NET_INNER], check=False)
    if r.returncode:
        print("  net demo failed, rc", r.returncode)


def net_inner():
    cast(Any, sys.stdout).reconfigure(line_buffering=True)
    for cmd in (["ip", "tuntap", "add", "tap0", "mode", "tap"], ["ip", "addr", "add", "172.16.0.1/24", "dev", "tap0"],
                ["ip", "link", "set", "tap0", "up"], ["ip", "link", "set", "lo", "up"]):
        subprocess.run(cmd, check=True)
    ch = CloudHypervisor("net", labmode="net", devices=("--net", "tap=tap0,mac=06:00:ac:10:00:02"))
    t, line, _ = ch.vm.wait_line(r"\(READY\)")
    print(f"  [{t * 1000:7.1f} ms] guest: {line.strip()}")
    ping = subprocess.run(["ping", "-c", "3", "-i", "0.2", "-q", "172.16.0.2"], check=False, capture_output=True, text=True)
    print("  host ping:", ping.stdout.strip().splitlines()[-1] if ping.stdout else ping.stderr.strip())
    for msg in (b"hello over virtio-net\n", b"quit\n"):
        c = socket.create_connection(("172.16.0.2", 8080), timeout=5)
        c.sendall(msg)
        print(f"  host -> 172.16.0.2:8080 {msg!r} -> {c.recv(512)!r}")
        c.close()
    ch.vm.wait()


# ------------------------------------------------------------- hotplug -----


def demo_hotplug():
    hr("5. Hot-plug: vCPUs and RAM through ACPI, a disk through PCI hot-plug")
    extra = os.path.join(WORK, "hot.ext4")
    shutil.copy(DISK, extra)
    ch = CloudHypervisor("hotplug", labmode="hotplug", cpus="boot=1,max=4",
                         memory="size=256M,hotplug_method=acpi,hotplug_size=1G",
                         # Online hot-added memory at once; some distribution kernels leave it offline.
                         extra_args="labticks=200 memhp_default_state=online")
    ch.vm.wait_line(r"cpus online")
    def show():
        info = ch.get("vm.info")
        print(f"  vm.info: vcpus={info['config']['cpus']['boot_vcpus']},"
              f" memory_actual_size={info['memory_actual_size'] >> 20} MiB")
    show()
    for label, body in (("4 vCPUs", {"desired_vcpus": 4}), ("768 MiB RAM", {"desired_ram": 768 << 20})):
        n = len(ch.vm.lines)
        t0 = ch.vm.now()
        ch.put("vm.resize", body)
        try:
            t, line, _ = ch.vm.wait_line(r"cpus online", start=n, timeout=3)
        except TimeoutError:
            # The guest kernel may lack ACPI memory hot-plug: say so, go on.
            print(f"  PUT vm.resize {json.dumps(body)} ({label}): guest reported no change within 3 s;"
                  " kernel lines since: " + " | ".join(l for _, l in ch.vm.lines[n:] if l.startswith("[ ")))
            continue
        print(f"  PUT vm.resize {json.dumps(body)} ({label}): guest within {(t - t0) * 1000:.0f} ms (polls every"
              f" 100 ms): {line}")
        time.sleep(0.3)
        n = len(ch.vm.lines)
        try:  # one more poll can follow if onlining finishes in two steps
            t, line, _ = ch.vm.wait_line(r"cpus online", start=n, timeout=0.5)
            print(f"  {'':10s}then: {line}")
        except TimeoutError:
            pass
    show()
    n = len(ch.vm.lines)
    t0 = ch.vm.now()
    r = ch.put("vm.add-disk", {"path": extra, "image_type": "Raw"})
    t, line, _ = ch.vm.wait_line(r"disks: vd", start=n, timeout=10)
    print(f"  PUT vm.add-disk -> {r}; guest within {(t - t0) * 1000:.0f} ms: {line}")
    ch.vm.wait(timeout=30)


# ------------------------------------------------------------ snapshot -----


def demo_snapshot():
    hr("6. Snapshot to a directory, restore in a new process")
    a = CloudHypervisor("snap-src", labmode="counter", memory="size=256M")
    a.vm.wait_line(r"^tick 10 ")
    t0 = a.vm.now()
    a.put("vm.pause")
    t1 = a.vm.now()
    snapdir = os.path.join(WORK, "snapshot")
    os.makedirs(snapdir)
    a.put("vm.snapshot", {"destination_url": f"file://{snapdir}"})
    t2 = a.vm.now()
    last = [l for _, l in a.vm.lines if l.startswith("tick ")][-1]
    print(f"  source: paused after {last!r}; pause {(t1 - t0) * 1000:.1f} ms, snapshot {(t2 - t1) * 1000:.1f} ms")
    for f in sorted(os.listdir(snapdir)):
        p = os.path.join(snapdir, f)
        print(f"    {f:20s} {os.path.getsize(p):>11d} bytes, {os.stat(p).st_blocks * 512 >> 10:>8d} KiB allocated")
    src_rss = rss_kib(a.vm.proc.pid)
    a.vm.kill()

    b = CloudHypervisor("snap-dst", restore=snapdir)
    b.put("vm.resume")  # the API answers only once --restore has loaded the snapshot
    t_resumed = b.vm.now()
    first = b.vm.wait_line(r"^tick ")
    print(f"  restore: spawn with --restore -> vm.resume returned {t_resumed * 1000:.1f} ms;"
          f" first tick {first[0] * 1000:.1f} ms (the guest finishes its 100 ms sleep)")
    print(f"  restored guest's first tick: {first[1]!r} (source stopped at {last!r})")
    b.vm.wait_line(r"^tick " + str(int(first[1].split()[1]) + 10) + " ")
    print(f"  RSS: source {src_rss >> 10} MiB at snapshot; restored {rss_kib(b.vm.proc.pid) >> 10} MiB after 1 s"
          " (memory-ranges is read into guest RAM at restore, not mapped lazily as in Firecracker)")
    b.vm.kill()


# ------------------------------------------------------------- sandbox -----


def demo_sandbox():
    hr("7. Sandboxing the VMM: seccomp per thread, Landlock on paths")
    ch = CloudHypervisor("seccomp", labmode="counter", devices=("--disk", f"path={DISK},readonly=on,image_type=raw"))
    ch.vm.wait_line(r"^tick 3 ")
    print(f"  threads of the running cloud-hypervisor (pid {ch.vm.proc.pid}):")
    for tid, name, mode, n in threads(ch.vm.proc.pid):
        print(f"    tid {tid:7d} {name:18s} Seccomp={mode} filters={n}")
    ch.vm.kill()

    probe = os.path.join(tempfile.mkdtemp(prefix="ch-outside-"), "outside.ext4")
    shutil.copy(DISK, probe)
    for landlock in (False, True):
        flags = ("--landlock",) if landlock else ()
        ch = CloudHypervisor(f"landlock-{landlock}", labmode="counter", devices=flags)
        ch.vm.wait_line(r"^tick 3 ")
        try:
            r = ch.put("vm.add-disk", {"path": probe, "image_type": "Raw"})
            res = f"accepted: {r}"
        except RuntimeError as e:
            msg = str(e)
            res = f"refused: 500 ... {msg[msg.find('Cannot open'):][:110]}"
        print(f"  {'--landlock' if landlock else 'no landlock':12s} add-disk {probe.replace(WORK, '$WORK')} -> {res}")
        ch.vm.kill()
    shutil.rmtree(os.path.dirname(probe), ignore_errors=True)


# ------------------------------------------------------------- compare -----


def demo_compare():
    hr("8. Firecracker and Cloud Hypervisor, same kernel and /init, 5 runs each")
    # fc.py is a script, not a library: load it from its path.
    fc_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fc.py")
    spec = importlib.util.spec_from_file_location("fc", fc_path)
    if spec is None or spec.loader is None:
        sys.exit(f"cannot load {fc_path}")
    fc = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fc)

    fc_exits = {}

    def fc_run():
        f = fc.Firecracker(f"cmp-{time.monotonic_ns()}")
        metrics = os.path.join(f.dir, "metrics.json")
        open(metrics, "w").close()
        f.put("/metrics", {"metrics_path": metrics})
        f.configure("info,rng", labexit="reboot", vcpus=2, mem=256)
        f.put("/entropy", {})
        f.start()
        t_init = f.vm.wait_line(r"^INIT-START")[0]
        f.vm.wait_line(r"rng .*hwrng=")
        rss = rss_kib(f.vm.proc.pid)
        nthreads = len(threads(f.vm.proc.pid))
        rng = float(f.vm.wait_line(r"read 1024 KiB")[1].split()[-2])
        f.vm.wait()
        m = fc.summed_metrics(metrics)
        fc_exits.update(mmio_read=m["vcpu"]["exit_mmio_read"], mmio_write=m["vcpu"]["exit_mmio_write"],
                        entropy_requests=m["entropy"]["entropy_event_count"])
        irq = [l.split("irq:")[1].strip() for _, l in f.vm.lines if "rng        irq:" in l]
        fc_exits["irq"] = irq[-1] if irq else "?"
        return t_init, f.vm.now(), rss, nthreads, rng

    ch_irq = {}

    def ch_run():
        c = CloudHypervisor(f"cmp-{time.monotonic_ns()}", labmode="info,rng")
        t_init = c.vm.wait_line(r"^INIT-START")[0]
        c.vm.wait_line(r"rng .*hwrng=")
        rss = rss_kib(c.vm.proc.pid)
        nthreads = len(threads(c.vm.proc.pid))
        rng = float(c.vm.wait_line(r"read 1024 KiB")[1].split()[-2])
        c.vm.wait()
        irq = [l.split("irq:")[1].strip() for _, l in c.vm.lines if "rng        irq:" in l and "input" in l]
        ch_irq["irq"] = irq[-1] if irq else "?"
        return t_init, c.vm.now(), rss, nthreads, rng

    try:
        rows = {f"Firecracker {fc.FC_VER}": [fc_run() for _ in range(5)], f"Cloud Hypervisor {CH_VER}": [ch_run() for _ in range(5)]}
    finally:
        shutil.rmtree(fc.WORK, ignore_errors=True)
    print(f"  {'':22s} {'spawn->/init':>13s} {'spawn->exit':>12s} {'RSS at /init':>13s} {'threads':>8s} {'rng 1 MiB':>10s}")
    for name, rs in rows.items():
        print(f"  {name:22s} {median([r[0] for r in rs]) * 1000:10.1f} ms {median([r[1] for r in rs]) * 1000:9.1f} ms"
              f" {median([r[2] for r in rs]) / 1024:9.1f} MiB {rs[0][3]:8d} {median([r[4] for r in rs]):7.1f} ms")
    for name, path in (("firecracker", fc.FC), ("cloud-hypervisor", CH)):
        print(f"  binary {name:17s} {os.path.getsize(path) / 2**20:5.1f} MiB (static)")
    print("  (Firecracker's exit is the guest's i8042 reset; Cloud Hypervisor's is an ACPI power-off)")
    print("\n  The interrupt path, from the last run of each (Cloud Hypervisor exposes no exit counters):")
    print(f"    Firecracker metrics: {fc_exits['entropy_requests']} entropy requests,"
          f" {fc_exits['mmio_read']} MMIO-read and {fc_exits['mmio_write']} MMIO-write exits to the VMM")
    print(f"    Firecracker guest:      {fc_exits['irq']}")
    print(f"    Cloud Hypervisor guest: {ch_irq['irq']}")


DEMOS = {"boot": demo_boot, "devices": demo_devices, "net": demo_net, "hotplug": demo_hotplug,
         "snapshot": demo_snapshot, "sandbox": demo_sandbox, "compare": demo_compare}


def main():
    if len(sys.argv) > 1 and sys.argv[1] == NET_INNER:
        net_inner()
        return
    cast(Any, sys.stdout).reconfigure(line_buffering=True)
    for p in (CH, KERNEL, INITRD, DISK):
        if not os.path.exists(p):
            sys.exit(f"missing {p}: run `make` first")
    which = sys.argv[1:] or ["all"]
    names = list(DEMOS) if which == ["all"] else which
    print(f"Cloud Hypervisor {CH_VER}, kernel {os.uname().release}, work dir $WORK={WORK}")
    try:
        for n in names:
            DEMOS[n]()
    finally:
        if not os.environ.get("KEEP"):
            shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    main()
