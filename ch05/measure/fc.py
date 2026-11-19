#!/usr/bin/env python3
"""fc.py -- drive a real Firecracker through its REST API.

Every demo boots the host's own kernel (guest/out/vmlinux) and the shared
static /init (guest/init.c), steered by the kernel command line.

  uv run fc.py boot       API-driven boot, timeline from outside, 5 runs; config-file boot
  uv run fc.py devices    virtio-block (ext4 image) + vsock + entropy, all virtio-mmio
  uv run fc.py net        virtio-net through a tap in an unprivileged user+net namespace
  uv run fc.py snapshot   pause, full snapshot, restore in a new process; diff snapshot
  uv run fc.py hotplug    virtio-mem: grow the guest's RAM while it runs
  uv run fc.py seccomp    the per-thread filters, and what happens when one fires
  uv run fc.py jailer     what the jailer needs, and how far an unprivileged user gets
  uv run fc.py all

Nothing here needs root. /dev/kvm must be accessible.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from vmlab import GUEST, VENDOR, Vmm, api, median, rss_kib, threads, wait_for_path

FC_VER = os.environ.get("FC_VER", "v1.18.0")
REL = os.path.join(VENDOR, f"release-{FC_VER}-x86_64")
FC = os.path.join(REL, f"firecracker-{FC_VER}-x86_64")
JAILER = os.path.join(REL, f"jailer-{FC_VER}-x86_64")
SECCOMPILER = os.path.join(REL, f"seccompiler-bin-{FC_VER}-x86_64")
FILTER_JSON = os.path.join(REL, f"seccomp-filter-{FC_VER}-x86_64.json")

KERNEL = os.path.join(GUEST, "vmlinux")
INITRD = os.path.join(GUEST, "initramfs.cpio")
DISK = os.path.join(GUEST, "data.ext4")

# Serial console, reset through the i8042 (the only way a guest can stop an
# x86 Firecracker), no reboot on panic.
BASE_ARGS = "console=ttyS0 reboot=k panic=-1"
# Firecracker's i8042 is just enough to carry the reset line. Without these the
# guest's atkbd driver spends ~0.5 s probing a keyboard that is not there, and
# reboot waits for that probe (section 2 measures it). reboot=k still works: it
# writes to the controller, not to the keyboard.
I8042_ARGS = "i8042.noaux i8042.nomux i8042.dumbkbd"
ARGS = f"{BASE_ARGS} {I8042_ARGS}"

WORK = tempfile.mkdtemp(prefix="fc-lab-")

LOG_LEVEL = "info"  # Application's log level
FC_LOG_LEVEL = "Info"  # Firecracker's log level for its own log file, not the guest's


def hr(title):
    print(f"\n=== {title} " + "=" * max(0, 70 - len(title)))


class Firecracker:
    """One Firecracker process with its API socket and a separate log file, so
    stdout carries only the guest's serial console."""

    def __init__(self, name, extra=(), echo=False, wrap=()):
        self.dir = os.path.join(WORK, name)
        os.makedirs(self.dir, exist_ok=True)
        self.sock = os.path.join(self.dir, "api.sock")
        self.log = os.path.join(self.dir, "fc.log")
        open(self.log, "w").close()  # Firecracker opens, it does not create
        argv = [*wrap, FC, "--api-sock", self.sock, "--log-path", self.log, "--level", FC_LOG_LEVEL, *extra]
        self.vm = Vmm(argv, echo=echo)
        self.t_sock = None
        if "--no-api" not in extra:
            wait_for_path(self.sock)
            self.t_sock = self.vm.now()

    def put(self, url, body):
        return api(self.sock, "PUT", url, body)

    def patch(self, url, body):
        return api(self.sock, "PATCH", url, body)

    def get(self, url):
        return api(self.sock, "GET", url)[1]

    def configure(
        self, labmode, labexit="reboot", vcpus=2, mem=256, extra_args="", base=ARGS, CUSTOM_INITRD=None, **mc
    ):
        self.put(
            "/boot-source",
            {
                "kernel_image_path": KERNEL,
                "initrd_path": CUSTOM_INITRD if CUSTOM_INITRD else INITRD,
                "boot_args": f"{base} labmode={labmode} labexit={labexit} {extra_args}".strip(),
            },
        )
        self.put("/machine-config", {"vcpu_count": vcpus, "mem_size_mib": mem, **mc})

    def start(self):
        self.put("/actions", {"action_type": "InstanceStart"})
        return self.vm.now()

    def log_lines(self):
        with open(self.log) as f:
            return f.read().splitlines()


# ---------------------------------------------------------------- boot -----


def one_boot(base=ARGS):
    fc = Firecracker(f"boot-{time.monotonic_ns()}")
    metrics = os.path.join(fc.dir, "metrics.json")
    open(metrics, "w").close()
    fc.put("/metrics", {"metrics_path": metrics})
    t_cfg0 = fc.vm.now()
    fc.configure("info,devices", base=base)
    t_cfg1 = fc.vm.now()
    t_start = fc.start()
    t_first = fc.vm.wait_line(r".", timeout=10)[0]
    t_init, init_line, _ = fc.vm.wait_line(r"^INIT-START")
    t_done, done_line, _ = fc.vm.wait_line(r"^INIT-DONE")
    rc = fc.vm.wait()
    t_exit = fc.vm.now()
    guest_init = float(init_line.split()[2])
    return dict(
        fc=fc,
        sock=fc.t_sock,
        cfg=t_cfg1 - t_cfg0,
        start=t_start,
        first=t_first,
        init=t_init,
        done=t_done,
        exit=t_exit,
        rc=rc,
        guest_init=guest_init,
        metrics=metrics,
    )


def summed_metrics(path):
    """Firecracker's metrics are JSON lines of *deltas* since the previous
    flush (one at InstanceStart-ish, one at exit), so totals are sums."""
    tot = {}
    with open(path) as f:
        for line in f:
            if not line.strip():
                continue
            for group, vals in json.loads(line).items():
                if isinstance(vals, dict):
                    g = tot.setdefault(group, {})
                    for k, v in vals.items():
                        if isinstance(v, (int, float)):
                            g[k] = g.get(k, 0) + v
    return tot


def timeline(runs):
    keys = [
        ("sock", "API socket ready"),
        ("start", "InstanceStart returned"),
        ("first", "first console line"),
        ("init", "/init prints INIT-START"),
        ("done", "/init done"),
        ("exit", "Firecracker exited"),
    ]
    print(f"  {'event':28s}" + "".join(f"{i + 1:>8d}" for i in range(len(runs))) + "  median")
    for k, label in keys:
        xs = [x[k] * 1000 for x in runs]
        print(f"  {label:28s}" + "".join(f"{x:8.1f}" for x in xs) + f"{median(xs):8.1f}")
    for k, label in (("cfg", "(config API calls took)"), ("guest_init", "(guest clock at /init)")):
        xs = [x[k] * 1000 for x in runs]
        print(f"  {label:28s}" + "".join(f"{x:8.1f}" for x in xs) + f"{median(xs):8.1f}")
    print("  exit codes:", [x["rc"] for x in runs])


def demo_boot():
    hr("1. API-driven boot: one run, guest console")
    r = one_boot()
    for t, line in r["fc"].vm.guest_lines():
        print(f"  [{t * 1000:7.1f} ms] {line}")

    # Port I/O nobody emulates: Firecracker logs each one and lets it read as
    # all-ones. 0xcf8/0xcfc is PCI config space, 0x70/0x71 the CMOS clock.
    miss = {}
    for l in r["fc"].log_lines():
        if "failed: Failed to find address range" in l:
            port = l.split("@ ")[1].split(":")[0]
            miss[port] = miss.get(port, 0) + 1
    print("\n  unemulated port I/O in Firecracker's log:", miss)
    m = summed_metrics(r["metrics"])
    print(
        "  metrics, vcpu:",
        {k: m["vcpu"][k] for k in ("exit_io_in", "exit_io_out", "exit_mmio_read", "exit_mmio_write", "failures")},
    )
    print(
        "  metrics, uart:",
        {k: m["uart"][k] for k in ("read_count", "write_count")},
        " i8042:",
        {k: m["i8042"][k] for k in ("read_count", "write_count", "reset_count")},
    )
    print("  metrics, PUT requests:", {k: v for k, v in m["put_api_requests"].items() if v and k.endswith("_count")})

    # Nothing is built before InstanceStart, so a start without a kernel is
    # refused with a 400 and no VM exists.
    fc = Firecracker("no-kernel")
    try:
        fc.start()
        print("\n  InstanceStart without /boot-source: unexpectedly accepted")
    except RuntimeError as e:
        print(f"\n  InstanceStart without /boot-source: {e}")
    fc.vm.kill()

    hr("2. Timeline, 5 runs each (ms since process spawn)")
    print(f'  a) boot_args = "{BASE_ARGS}"')
    timeline([one_boot(BASE_ARGS) for _ in range(5)])
    print(f'\n  b) boot_args = "{ARGS}"')
    timeline([one_boot(ARGS) for _ in range(5)])

    hr("3. The same VM from one JSON file (--no-api --config-file)")
    cfg = {
        "boot-source": {
            "kernel_image_path": KERNEL,
            "initrd_path": INITRD,
            "boot_args": f"{ARGS} labmode=info labexit=reboot",
        },
        "machine-config": {"vcpu_count": 2, "mem_size_mib": 256},
        "drives": [],
    }
    path = os.path.join(WORK, "vm.json")
    with open(path, "w") as f:
        json.dump(cfg, f)
    ts = []
    for _ in range(5):
        fc = Firecracker(f"cfg-{time.monotonic_ns()}", extra=("--no-api", "--config-file", path))
        t_init = fc.vm.wait_line(r"^INIT-START")[0]
        fc.vm.wait()
        ts.append((t_init, fc.vm.now()))
    print(
        "  spawn -> INIT-START (ms):",
        " ".join(f"{a * 1000:.1f}" for a, _ in ts),
        f" median {median([a * 1000 for a, _ in ts]):.1f}",
    )
    print(
        "  spawn -> exit (ms):      ",
        " ".join(f"{b * 1000:.1f}" for _, b in ts),
        f" median {median([b * 1000 for _, b in ts]):.1f}",
    )


# ------------------------------------------------------------- devices -----


def vsock_connect(uds, port, timeout=10):
    """Firecracker's host side of vsock: connect to the Unix socket and ask for
    the guest port with a text line, `CONNECT <port>\\n`; it answers `OK <n>\\n`."""
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
    r = subprocess.run(["debugfs", "-R", f"cat {path}", image], capture_output=True, text=True)
    return r.stdout.strip()


def demo_devices():
    hr("4. Devices: virtio-block + vsock + entropy (all virtio-mmio)")
    fc = Firecracker("devices")
    disk = os.path.join(fc.dir, "data.ext4")
    shutil.copy(DISK, disk)  # the guest writes to it: never touch the pristine image
    uds = os.path.join(fc.dir, "vsock.sock")
    fc.configure("info,devices,blk,vsock", labexit="reboot")
    fc.put("/drives/data", {"drive_id": "data", "path_on_host": disk, "is_root_device": False, "is_read_only": False})
    fc.put("/vsock", {"guest_cid": 3, "uds_path": uds})
    fc.put("/entropy", {})
    print("  GET /vm/config (as Firecracker will build it):")
    vc = fc.get("/vm/config")
    print("   ", json.dumps({k: vc[k] for k in ("drives", "vsock", "entropy") if k in vc})[:300])
    fc.start()
    fc.vm.wait_line(r"\(READY\)")
    for t, line in fc.vm.guest_lines():
        if line.startswith("  ") or line.startswith("[init] task"):
            print(f"  [{t * 1000:7.1f} ms] {line}")
    s, ack = vsock_connect(uds, 5000)
    print(f"  host -> {uds.replace(WORK, '$WORK')}: CONNECT 5000 -> {ack!r}")
    for msg in (b"hello over vsock\n", b"quit\n"):
        s.sendall(msg)
        print(f"  host sent {msg!r}, guest answered {s.recv(256)!r}")
    s.close()
    fc.vm.wait()
    print("  after exit, the guest's file on the host image:", repr(debugfs_cat(disk, "/from-guest.txt")))
    # The device addresses Firecracker chose are on the guest command line it built.
    for _, line in fc.vm.lines:
        if "cmdline" in line:
            print("  guest cmdline:", line.split("cmdline", 1)[1].strip())


# ----------------------------------------------------------------- net -----

NET_INNER = "--net-inner"


def demo_net():
    hr("5. Network: virtio-net -> tap0, inside an unprivileged user+net namespace")
    # A tap device needs CAP_NET_ADMIN. An unprivileged user gets it inside a
    # new user namespace, over a new network namespace that it owns: the tap and
    # the VM's whole network exist only there, the way the jailer's --netns does.
    hr("  a) unshare --user --map-root-user --net <python fc.py --net-inner>")
    print(["unshare", "--user", "--map-root-user", "--net", sys.executable, os.path.abspath(__file__), NET_INNER])

    r = subprocess.run(
        ["unshare", "--user", "--map-root-user", "--net", sys.executable, os.path.abspath(__file__), NET_INNER],
        text=True,
    )
    if r.returncode:
        print("  net demo failed, rc", r.returncode)


def net_inner():
    for cmd in (
        ["ip", "tuntap", "add", "tap0", "mode", "tap"],
        ["ip", "addr", "add", "172.16.0.1/24", "dev", "tap0"],
        ["ip", "link", "set", "tap0", "up"],
        ["ip", "link", "set", "lo", "up"],
    ):
        subprocess.run(cmd, check=True)

    fc = Firecracker("net")
    fc.configure("net", labexit="reboot")
    fc.put("/network-interfaces/eth0", {"iface_id": "eth0", "host_dev_name": "tap0", "guest_mac": "06:00:AC:10:00:02"})
    fc.start()
    t, line, _ = fc.vm.wait_line(r"\(READY\)")
    print(f"  [{t * 1000:7.1f} ms] guest: {line.strip()}")
    print(
        "  host side, in the namespace:",
        subprocess.run(
            ["ip", "-br", "addr", "show", "tap0"], capture_output=True, text=True, check=True
        ).stdout.strip(),
    )
    # Quick ping to the guest's tap0 address, then a TCP connection to the guest's 8080 echo server.
    ping = subprocess.run(["ping", "-c", "3", "-i", "0.2", "172.16.0.2"], capture_output=True, text=True, check=True)
    print("  host ping:", ping.stdout.strip().splitlines()[-1] if ping.stdout else ping.stderr.strip())

    # Show the routing table
    route = subprocess.run(["ip", "route"], capture_output=True, text=True, check=True)
    print("  host routing table:\n", route.stdout.strip() if route.stdout else route.stderr.strip())

    # Show the network interfaces' addresses in the netns
    addrs = subprocess.run(["ip", "addr", "show"], capture_output=True, text=True, check=True)
    print("  host network interfaces:\n", addrs.stdout.strip() if addrs.stdout else addrs.stderr.strip())

    for msg in (b"hello over virtio-net\n", b"quit\n"):
        c = socket.create_connection(("172.16.0.2", 8080), timeout=5)
        c.sendall(msg)
        print(f"  host -> 172.16.0.2:8080 {msg!r} -> {c.recv(512)!r}")
        c.close()
    fc.vm.wait()
    print("  outside this namespace no tap0 exists; the host's real interfaces were never visible to the VM")


# ------------------------------------------------------------ snapshot -----


def ticks(vm, start=0):
    out = []
    for t, line in vm.lines[start:]:
        if line.startswith("tick "):
            p = line.split()
            out.append((t, int(p[1]), float(p[3])))
    return out


def metrics_file(fc):
    path = os.path.join(fc.dir, "metrics.json")
    open(path, "w").close()
    fc.put("/metrics", {"metrics_path": path})
    return path


def latencies(path):
    """latencies_us are gauges, unlike the counters: each flush repeats the
    last value measured, so read the final flush rather than summing."""
    with open(path) as f:
        lines = [l for l in f if l.strip()]
    return json.loads(lines[-1]).get("latencies_us", {})


def data_runs(path):
    """[(offset, length)] of the regions a sparse file really holds, found
    with lseek(SEEK_DATA) and lseek(SEEK_HOLE); the rest are holes."""
    runs, off = [], 0
    with open(path, "rb") as f:
        fd, end = f.fileno(), os.fstat(f.fileno()).st_size
        while off < end:
            try:
                start = os.lseek(fd, off, os.SEEK_DATA)
            except OSError:  # ENXIO: no data after off
                break
            off = os.lseek(fd, start, os.SEEK_HOLE)
            runs.append((start, off - start))
    return runs


def changed_pages(a, b, page=4096):
    """Indexes of the 4 KiB pages that differ between two equal-sized files."""
    out = []
    with open(a, "rb") as fa, open(b, "rb") as fb:
        i = 0
        while True:
            x, y = fa.read(page * 256), fb.read(page * 256)
            if not x:
                return out
            if x != y:
                out += [
                    i + j
                    for j in range(0, len(x) // page)
                    if x[j * page : (j + 1) * page] != y[j * page : (j + 1) * page]
                ]
            i += len(x) // page


def faults(pid):
    """Minor and major page faults so far: fields 10 and 12 of /proc/PID/stat."""
    with open(f"/proc/{pid}/stat") as f:
        st = f.read().rsplit(")", 1)[1].split()
    return int(st[7]), int(st[9])


def demo_snapshot():
    hr("6. Snapshot and restore")
    a = Firecracker("snap-src")
    a_metrics = metrics_file(a)
    a.configure("counter", labexit="reboot", mem=256, track_dirty_pages=True)
    a.start()
    a.vm.wait_line(r"^tick 10 ")
    t = a.vm.now()
    a.patch("/vm", {"state": "Paused"})
    t_paused = a.vm.now()
    snap, mem = os.path.join(a.dir, "vm.snap"), os.path.join(a.dir, "vm.mem")
    a.put("/snapshot/create", {"snapshot_type": "Full", "snapshot_path": snap, "mem_file_path": mem})
    t_snap = a.vm.now()
    last = ticks(a.vm)[-1]
    print(
        f"  source: paused after tick {last[1]} (guest mono {last[2]:.3f}); pause took {(t_paused - t) * 1000:.1f} ms,"
        f" full snapshot {(t_snap - t_paused) * 1000:.1f} ms"
    )
    print(
        f"  files: vm.snap {os.path.getsize(snap)} bytes (device + vCPU state),"
        f" vm.mem {os.path.getsize(mem) >> 20} MiB on disk ({os.stat(mem).st_blocks * 512 >> 20} MiB allocated)"
    )
    src_rss = rss_kib(a.vm.proc.pid)

    # A second change, then a diff snapshot: only pages dirtied since the full one.
    a.patch("/vm", {"state": "Resumed"})
    a.vm.wait_line(rf"^tick {last[1] + 5} ")
    a.patch("/vm", {"state": "Paused"})
    dsnap, dmem = os.path.join(a.dir, "diff.snap"), os.path.join(a.dir, "diff.mem")
    t0 = a.vm.now()
    a.put("/snapshot/create", {"snapshot_type": "Diff", "snapshot_path": dsnap, "mem_file_path": dmem})
    print(
        f"  diff snapshot after 5 more ticks: {(a.vm.now() - t0) * 1000:.1f} ms, vm.mem apparent"
        f" {os.path.getsize(dmem) >> 20} MiB, allocated {os.stat(dmem).st_blocks * 512 >> 10} KiB (sparse: dirty pages only)"
    )
    a.put("/actions", {"action_type": "FlushMetrics"})
    t_init = next(t for t, l in a.vm.guest_lines() if l.startswith("INIT-START"))
    a.vm.kill()
    lat = latencies(a_metrics)
    print(
        "  Firecracker's own latencies_us (source): "
        + ", ".join(f"{k} {lat[k]}" for k in ("pause_vm", "full_create_snapshot", "diff_create_snapshot", "resume_vm"))
    )

    # Restore the *full* snapshot in a brand-new process. Loading needs an
    # unconfigured Firecracker: no /boot-source, no /machine-config, no start.
    b = Firecracker("snap-dst")
    b_metrics = metrics_file(b)
    t0 = b.vm.now()
    b.put(
        "/snapshot/load",
        {"snapshot_path": snap, "mem_backend": {"backend_type": "File", "backend_path": mem}, "resume_vm": True},
    )
    t_loaded = b.vm.now()
    first = b.vm.wait_line(r"^tick ")
    tk = ticks(b.vm)
    # The guest was paused inside a 100 ms usleep, so its first line comes when
    # that sleep ends: it measures the counter, not the restore.
    print(
        f"  restore: /snapshot/load (resume_vm) returned in {(t_loaded - t0) * 1000:.1f} ms;"
        f" first tick {(first[0] - t0) * 1000:.1f} ms after the request (the guest finishes its 100 ms sleep)"
    )
    print(f"  restored guest's first tick: {first[1]!r}  (source stopped after tick {last[1]})")
    print(
        f"  spawn to a running guest, one clock: cold boot reaches /init at {t_init * 1000:.1f} ms;"
        f" restore is running at {t_loaded * 1000:.1f} ms"
    )
    b.vm.wait_line(rf"^tick {tk[0][1] + 10} ")
    minflt, majflt = faults(b.vm.proc.pid)
    print(
        f"  RSS: source {src_rss >> 10} MiB at snapshot; restored {rss_kib(b.vm.proc.pid) >> 10} MiB after 1 s"
        " (guest memory is mmap'd from vm.mem and faulted in on touch)"
    )
    print(f"  restored process page faults after 1 s: {minflt} minor, {majflt} major (vm.mem is in the page cache)")
    b.put("/actions", {"action_type": "FlushMetrics"})
    b.vm.kill()
    lat = latencies(b_metrics)
    print("  Firecracker's own latencies_us (restore): " + ", ".join(f"{k} {v}" for k, v in lat.items() if v))

    # One snapshot, two machines: both resume as the same guest.
    clones = [Firecracker(f"snap-clone{i}") for i in (1, 2)]
    for c in clones:
        c.put(
            "/snapshot/load",
            {"snapshot_path": snap, "mem_backend": {"backend_type": "File", "backend_path": mem}, "resume_vm": True},
        )
    print(
        "  two clones from one snapshot, first ticks: " + ", ".join(repr(c.vm.wait_line(r"^tick ")[1]) for c in clones)
    )
    for c in clones:
        c.vm.kill()

    # The diff alone is not a VM: rebase it onto the full memory file first.
    editor = os.path.join(REL, f"snapshot-editor-{FC_VER}-x86_64")
    merged = os.path.join(a.dir, "merged.mem")
    shutil.copy(mem, merged)
    subprocess.run([editor, "edit-memory", "rebase", "--memory-path", merged, "--diff-path", dmem], check=True)
    runs = data_runs(dmem)
    pages = sum(n for _, n in runs) // 4096
    print(
        f"  diff.mem holds data in {len(runs)} runs, {pages} pages; first runs (offset, bytes): "
        + ", ".join(f"({o:#x}, {n})" for o, n in runs[:4])
    )
    ch = changed_pages(mem, merged)
    inside = sum(1 for p in ch if any(o <= p * 4096 < o + n for o, n in runs))
    print(
        f"  rebase: merged.mem differs from vm.mem in {len(ch)} pages, {inside} of them inside the diff's data runs;"
        f" the other {pages - len(ch)} diff pages were rewritten with the bytes they already had"
    )
    c = Firecracker("snap-diff")
    c.put(
        "/snapshot/load",
        {"snapshot_path": dsnap, "mem_backend": {"backend_type": "File", "backend_path": merged}, "resume_vm": True},
    )
    print(
        f"  diff restored on top of the full image (snapshot-editor rebase): first tick {c.vm.wait_line(r'^tick ')[1]!r}"
    )
    c.vm.kill()


# ------------------------------------------------------------- hotplug -----


def demo_hotplug():
    hr("7. Memory hot-plug with virtio-mem")
    fc = Firecracker("hotplug")
    boot = 256
    # Boot RAM is fixed at 256 MiB. memhp_default_state=online_movable makes the
    # guest kernel online plugged memory by itself, in ZONE_MOVABLE, so that its
    # pages can be migrated away and the blocks unplugged later. labfill=384
    # tells /init to write and free 384 MiB once the plugged memory appears.
    fc.configure(
        "hotplug", labexit="reboot", mem=boot, extra_args="memhp_default_state=online_movable labticks=80 labfill=384"
    )
    # Before boot: create the virtio-mem device. total_size_mib is the most that
    # can ever be plugged (address space reserved above boot RAM), slot_size_mib
    # the unit Firecracker hands to KVM, block_size_mib the unit the guest
    # driver plugs and unplugs. Nothing is plugged yet: the guest sees a
    # virtio-mem device, not the memory.
    fc.put("/hotplug/memory", {"total_size_mib": 1024, "slot_size_mib": 128, "block_size_mib": 2})
    fc.start()
    pid = fc.vm.proc.pid

    def guest_mem(start):
        t, line, _ = fc.vm.wait_line(r"MemTotal", start=start, timeout=10)
        return int(line.split("MemTotal:")[1].split()[0])

    def plugged_after(target, t0):
        """Poll the API until the device reports the target plugged: the time
        the guest driver took to plug or unplug, seen from the host."""
        # plugged_size_mib is what the guest driver has actually plugged, as the
        # device reports it. It trails requested_size_mib until the driver is done.
        while fc.get("/hotplug/memory")["plugged_size_mib"] != target:
            time.sleep(0.002)
        return (fc.vm.now() - t0) * 1000

    def row(what, plugged, mem, took=""):
        time.sleep(0.3)  # let RSS settle
        # VM total = the fixed boot RAM + whatever virtio-mem has plugged.
        print(
            f"  {what:44s} {plugged:>5d} MiB {boot + plugged:>6d} MiB {mem >> 10:>7d} MiB {took:>12s}"
            f" {rss_kib(pid) >> 10:>5d} MiB"
        )

    print(f"  {'step':44s} {'plugged':>9s} {'VM total':>10s} {'MemTotal':>11s} {'device took':>12s} {'FC RSS':>9s}")
    mem = guest_mem(0)
    row(f"boot: {boot} MiB of fixed RAM", 0, mem)

    n, t0 = len(fc.vm.lines), fc.vm.now()
    # Plug: raise the target to 512 MiB. Firecracker updates the device config
    # and interrupts the guest; its virtio_mem driver plugs 256 blocks of 2 MiB.
    fc.patch("/hotplug/memory", {"requested_size_mib": 512})
    took = plugged_after(512, t0)  # the guest driver's round trip, seen from the host
    mem = guest_mem(n)
    row("set plugged target to 512 MiB", 512, mem, f"{took:.1f} ms")

    # The guest writes 384 MiB, more than its free boot RAM: now the host
    # must back those pages, and RSS grows.
    fc.vm.wait_line(r"filled \d+ MiB", start=n)
    row("guest writes 384 MiB", 512, mem)
    # The guest frees it. The pages go to the guest's free list, not back to
    # the host, so RSS stays where it was.
    fc.vm.wait_line(r"freed \d+ MiB", start=n)
    row("guest frees the 384 MiB", 512, mem)

    n, t0 = len(fc.vm.lines), fc.vm.now()
    # Unplug: lower the target to 128 MiB. The driver must empty 192 blocks
    # (migrating any pages in use), then reports them unplugged, and Firecracker
    # releases the host memory behind them: RSS falls.
    fc.patch("/hotplug/memory", {"requested_size_mib": 128})
    took = plugged_after(128, t0)
    mem = guest_mem(n)
    row("set plugged target to 128 MiB (unplug 384)", 128, mem, f"{took:.1f} ms")
    # The device's final view: requested and plugged agree at 128 MiB.
    print("  GET /hotplug/memory:", fc.get("/hotplug/memory"))
    fc.vm.wait(timeout=30)


# ------------------------------------------------------------- seccomp -----


def demo_seccomp():
    hr("8. seccomp: one filter per thread")
    with open(FILTER_JSON) as f:
        filt = json.load(f)
    for name, v in filt.items():
        print(
            f"  {name:5s} thread: default_action={v['default_action']:5s} {len(v['filter']):3d} rules,"
            f" filter_action={v['filter_action']}"
        )
    # Firecracker never opens that JSON: its build compiles the same file with
    # seccompiler and embeds the result. Compile it here and look for the bytes.
    default_bpf = os.path.join(WORK, "default.bpf")
    subprocess.run(
        [SECCOMPILER, "--input-file", FILTER_JSON, "--target-arch", "x86_64", "--output-file", default_bpf],
        check=True,
        capture_output=True,
    )
    with open(default_bpf, "rb") as f, open(FC, "rb") as g:
        blob, binary = f.read(), g.read()
    at = binary.find(blob)
    print(
        f"  the JSON compiled with seccompiler-bin: {len(blob)} bytes, found in the firecracker binary"
        + (f" at offset {at:#x} (the embedded default filter)" if at >= 0 else ": NOT found")
    )
    fc = Firecracker("seccomp")
    fc.configure("counter", labexit="reboot")
    fc.start()
    fc.vm.wait_line(r"^tick 3 ")
    open_files(fc.vm.proc.pid)
    print(f"\n  threads of the running Firecracker (pid {fc.vm.proc.pid}):")
    for tid, name, mode, n in threads(fc.vm.proc.pid):
        print(f"    tid {tid:7d} {name:12s} Seccomp={mode} (2 = filter) filters={n}")
    fc.vm.kill()

    # One attack per check: compile a filter whose API thread may not call
    # accept4, then make an API request. The kernel stops the thread on the
    # syscall itself; Firecracker's SIGSYS handler logs it and exits.
    bad = json.loads(json.dumps(filt))
    rules = bad["api"]["filter"]
    bad["api"]["filter"] = [r for r in rules if r.get("syscall") != "accept4"]
    removed = len(rules) - len(bad["api"]["filter"])
    src, bpf = os.path.join(WORK, "bad.json"), os.path.join(WORK, "bad.bpf")
    with open(src, "w") as f:
        json.dump(bad, f)
    subprocess.run(
        [SECCOMPILER, "--input-file", src, "--target-arch", "x86_64", "--output-file", bpf],
        check=True,
        text=True,
        capture_output=True,
    )
    print(f"\n  custom filter: api thread without accept4 ({removed} rule removed), compiled with seccompiler-bin")
    fc = Firecracker("seccomp-bad", extra=("--seccomp-filter", bpf))
    try:
        api(fc.sock, "GET", "/version", timeout=3)
        print("  GET /version unexpectedly succeeded")
    except Exception as e:  # noqa: BLE001 -- any failure is the point here
        print(f"  GET /version -> {type(e).__name__}: {e}")
    rc = fc.vm.wait(timeout=5)
    print(f"  firecracker exit code {rc}")
    for l in fc.log_lines():
        if "syscall" in l.lower() or "SIGSYS" in l or "exit_code" in l:
            print("   log:", l.split("] ", 1)[-1][:120])


def open_files(pid):
    """What one running Firecracker holds: its open files, grouped by what
    each descriptor points at, and its thread names."""
    seen = {}
    for fd in os.listdir(f"/proc/{pid}/fd"):
        try:
            t = os.readlink(f"/proc/{pid}/fd/{fd}")
        except OSError:
            continue
        if t.startswith("socket:"):
            t = "socket"
        elif t.startswith("pipe:"):
            t = "pipe"
        seen[t] = seen.get(t, 0) + 1
    print(f"\n  firecracker pid {pid}, open files:")
    for t in sorted(seen):
        print(f"    {seen[t]:3d} {t}")
    print("  threads:", " ".join(n for _, n, _, _ in threads(pid)))
    ps = subprocess.run(["ps", "-eo", "comm"], capture_output=True, text=True, check=False).stdout
    print("  qemu processes:", sum(1 for l in ps.splitlines() if "qemu" in l))


def demo_kbdprobe():
    hr("2b. Where the slow stop goes: initcall_debug, no i8042 options")
    fc = Firecracker(f"kbd-{time.monotonic_ns()}")
    fc.configure("info", base=f"{BASE_ARGS} initcall_debug")
    fc.start()
    fc.vm.wait(timeout=30)
    for t, line in fc.vm.guest_lines():
        if "INIT-DONE" in line or "serio" in line or "reboot:" in line or "AT Raw" in line or "atkbd" in line:
            print(f"  [{t * 1000:7.1f} ms] {line[:110]}")


# -------------------------------------------------------------- jailer -----


def demo_jailer():
    hr("9. The jailer, run as an unprivileged user")
    print("  jailer --help (first lines of the required arguments):")
    r = subprocess.run([JAILER, "--help"], capture_output=True, text=True)
    for l in r.stdout.splitlines():
        if l.strip().startswith(
            (
                "--id",
                "--exec-file",
                "--uid",
                "--gid",
                "--chroot-base-dir",
                "--netns",
                "--cgroup ",
                "--daemonize",
                "--new-pid-ns",
                "--resource-limit",
            )
        ):
            print("   ", l.strip()[:110])
    # The flags say "chroot", but the jailer's own error messages name the
    # steps it really takes: a mount namespace, pivot_root, unmount the old root.
    with open(JAILER, "rb") as f:
        binary = f.read()
    print("  error messages inside the jailer binary, in the order of the steps they guard:")
    for s in (
        "Failed to unshare into new mount namespace",
        "Failed to pivot root",
        "Failed to unmount the old jail root",
        "Failed to chdir into chroot directory",
    ):
        print(f"    {s!r}: {'present' if s.encode() in binary else 'NOT present'}")
    base = os.path.join(WORK, "jail")
    os.makedirs(base, exist_ok=True)
    argv = [
        JAILER,
        "--id",
        "lab1",
        "--exec-file",
        FC,
        "--uid",
        str(os.getuid()),
        "--gid",
        str(os.getgid()),
        "--chroot-base-dir",
        base,
        "--",
        "--no-api",
        "--config-file",
        "/dev/null",
    ]
    print(f"\n  unprivileged user {os.getuid()} runs: {' '.join(argv)}")

    r = subprocess.run(argv, capture_output=True, text=True)
    print(f"\n  as uid {os.getuid()}: rc={r.returncode}: {(r.stderr or r.stdout).strip()[:300]}")
    r = subprocess.run(
        ["unshare", "--user", "--map-root-user", "--mount", *argv[:5], "--uid", "0", "--gid", "0", *argv[9:]],
        capture_output=True,
        text=True,
    )
    print(f"  as root inside a user namespace: rc={r.returncode}: {(r.stderr or r.stdout).strip()[:300]}")
    print("  jail directory left behind:")
    for dp, dn, fn in os.walk(base):
        for x in sorted(dn + fn):
            p = os.path.join(dp, x)
            st = os.lstat(p)
            print(f"    {p.replace(base, '<chroot-base>')}  mode {oct(st.st_mode)}")

    userns_limits()
    jail_by_hand()


def userns_limits():
    """Why the jailer needs root to give a VM its own uid: a user namespace
    relabels the creator's own uid, and nothing else. Three checks."""
    me, other = os.getuid(), os.getuid() + 1
    print(f"\n  Can a user namespace make uid {me} into another host user?")
    f = os.path.join(WORK, "userns-file")
    # 1. Claim another uid inside. uid_map says "other inside = me outside",
    #    so the file is stored, and every permission is checked, as me.
    r = subprocess.run(
        ["unshare", "--user", f"--map-user={other}", f"--map-group={other}", "sh", "-c",
         f'echo "uid=$(id -u) gid=$(id -g)"; touch {f}; stat -c "%u:%g" {f}'],
        capture_output=True, text=True, check=True,
    )
    inside_id, inside_owner = r.stdout.split()[0:2], r.stdout.split()[2]
    st = os.stat(f)
    print(f"    mapped as {other} inside: {' '.join(inside_id)}; a new file is {inside_owner} inside,"
          f" {st.st_uid}:{st.st_gid} on the host")
    # 2. Map a host uid that is not ours. Without CAP_SETUID in the parent
    #    namespace, the kernel accepts only a one-line map onto our own uid.
    r = subprocess.run(
        ["unshare", "--user", "sh", "-c", f'echo "0 {other} 1" > /proc/self/uid_map'],
        capture_output=True, text=True,
    )
    print(f"    write '0 {other} 1' to uid_map (host uid {other} as root inside): "
          f"{r.stderr.strip().split(': ')[-1] if r.returncode else 'accepted'}")
    # 3. Host uids that are not mapped show as the overflow uid, with "other"
    #    permissions, even to "root" inside.
    r = subprocess.run(
        ["unshare", "--user", "--map-root-user", "sh", "-c",
         "stat -c '%n %u:%g %A' /etc/shadow /root; cat /proc/sys/kernel/overflowuid"],
        capture_output=True, text=True, check=True,
    )
    *files, overflow = r.stdout.split("\n")[:3]
    print(f"    as 'root' with --map-root-user: {'; '.join(files)} (overflow uid {overflow.strip()})")


def descendants(pid):
    kids = {}
    for d in os.listdir("/proc"):
        if d.isdigit():
            try:
                with open(f"/proc/{d}/stat") as f:
                    ppid = int(f.read().rsplit(")", 1)[1].split()[1])
                kids.setdefault(ppid, []).append(int(d))
            except (FileNotFoundError, ProcessLookupError, IndexError):
                pass
    out, todo = [], [pid]
    while todo:
        p = todo.pop()
        for c in kids.get(p, []):
            out.append(c)
            todo.append(c)
    return out


JAIL_INNER = "--jail-inner"
MS_BIND, MNT_DETACH = 0x1000, 2


def jail_inner(how, jail):
    """Runs inside `unshare --user --map-root-user --mount --pid --fork --net`:
    root in its own user namespace, owner of its own mount namespace. Builds
    the jail with chroot or with pivot_root, then execs Firecracker in it."""
    import ctypes

    shutil.rmtree(WORK, ignore_errors=True)  # this process's own, unused, temp dir
    libc = ctypes.CDLL(None, use_errno=True)

    def mount(src, dst):
        print(f".........................................  mount --bind {src} {dst}", flush=True)
        if libc.mount(src.encode(), dst.encode(), None, MS_BIND, None):
            raise OSError(ctypes.get_errno(), f"mount --bind {src} {dst}")

    if how == "pivot_root":
        # pivot_root needs the new root to be a mount point: bind the jail
        # onto itself first, then put /dev/kvm inside that mount.
        mount(jail, jail)
        print(f".........................................  mount --bind {jail} {jail}", flush=True)
        mount("/dev/kvm", os.path.join(jail, "dev", "kvm"))
        print(
            f".........................................  mount --bind /dev/kvm {os.path.join(jail, 'dev', 'kvm')}",
            flush=True,
        )
        os.chdir(jail)
        print(f".........................................  chdir {jail}", flush=True)
        # pivot_root(".", "."): the jail becomes / and the old root is stacked
        # on top of it; unmounting "." detaches the old root, so the host's
        # filesystem is no longer mounted anywhere in this namespace.
        if libc.pivot_root(b".", b"."):
            raise OSError(ctypes.get_errno(), "pivot_root")
        if libc.umount2(b".", MNT_DETACH):
            raise OSError(ctypes.get_errno(), "umount2")
        os.chdir("/")
    else:
        # chroot only changes where path lookups start. Every host mount is
        # still mounted in this namespace, merely unreachable by name.
        mount("/dev/kvm", os.path.join(jail, "dev", "kvm"))
        os.chroot(jail)
        os.chdir("/")
    print(open("/vm.json", "r").readlines(), flush=True)
    os.execv("/firecracker", ["/firecracker", "--no-api", "--config-file", "/vm.json", "--log-path", "/dev/null"])


def jail_by_hand():
    """What the jailer does, minus the parts that need real root: a private
    mount namespace whose root holds only the files the VM needs, /dev/kvm
    bind-mounted in (a user namespace may not mknod a device, but it may
    bind-mount one that exists), new PID and network namespaces, then exec.
    Built twice, with chroot and with pivot_root as the jailer does, to show
    what each leaves mounted."""
    for how in ("chroot", "pivot_root"):
        one_hand_jail(how)
    print("  still missing versus the real jailer: setuid to a dedicated uid, cgroups, rlimits, and a /dev/net/tun")


def one_hand_jail(how):
    print(f"\n  By hand, with {how}: unshare --user --map-root-user --mount --pid --net, bind /dev/kvm, {how}")
    jail = os.path.join(WORK, f"handjail-{how}")
    os.makedirs(os.path.join(jail, "dev"))
    open(os.path.join(jail, "dev", "kvm"), "w").close()
    for src, dst in ((FC, "firecracker"), (KERNEL, "vmlinux"), (INITRD, "initramfs.cpio")):
        shutil.copy(src, os.path.join(jail, dst))
    with open(os.path.join(jail, "vm.json"), "w") as f:
        jailer_conf = {
            "boot-source": {
                "kernel_image_path": "/vmlinux",
                "initrd_path": "/initramfs.cpio",
                "boot_args": f"{ARGS} labmode=counter labticks=15 labexit=reboot",
            },
            "machine-config": {"vcpu_count": 1, "mem_size_mib": 128},
            "drives": [],
        }
        # print(f"  /vm.json for the jail: {jailer_conf}")
        json.dump(jailer_conf, f)
    vm = Vmm(
        [
            "unshare",
            "--user",
            "--map-root-user",
            "--mount",
            "--pid",
            "--fork",
            "--net",
            sys.executable,
            os.path.abspath(__file__),
            JAIL_INNER,
            how,
            jail,
        ],
        echo=True,
    )
    print(f"full unshare command ==> {' '.join(vm.argv)}")
    # time.sleep(2)

    vm.wait_line(r"^tick 5 ")
    fcpid = next(p for p in descendants(vm.proc.pid) if open(f"/proc/{p}/comm").read().strip() == "firecracker")
    root = sorted(os.listdir(f"/proc/{fcpid}/root"))
    print(f"  firecracker pid {fcpid} (host view); its / contains only: {root}")
    with open(f"/proc/{fcpid}/status") as f:
        st = dict(l.rstrip("\n").split(":\t", 1) for l in f if ":\t" in l)
    print(f"  host uid {st['Uid'].split()[0]}, NSpid {st['NSpid']} (host pid, then pid inside its namespace)")
    for ns in ("mnt", "pid", "net", "user"):
        mine, its = os.readlink(f"/proc/self/ns/{ns}"), os.readlink(f"/proc/{fcpid}/ns/{ns}")
        print(f"    ns/{ns:5s} {'separate' if mine != its else 'SHARED'}  ({its})")
    # /proc/PID/mountinfo lists only the mounts reachable from PID's own root,
    # so the chrooted VMM cannot show what lies outside its jail. The unshare
    # parent shares its mount namespace but was not chrooted: its mountinfo
    # lists everything the namespace still holds.
    assert os.readlink(f"/proc/{vm.proc.pid}/ns/mnt") == os.readlink(f"/proc/{fcpid}/ns/mnt")
    with open(f"/proc/{fcpid}/mountinfo") as f:
        visible = len(f.readlines())

    def fs(line):
        # (filesystem type, source, path within that filesystem): the jail's
        # bind mount may share the host root's device, but not its path "/".
        return (*line.split(" - ", 1)[1].split()[:2], line.split()[3])

    with open("/proc/self/mountinfo") as f:
        host_root = next(fs(l) for l in f if l.split()[4] == "/")
    with open(f"/proc/{vm.proc.pid}/mountinfo") as f:
        held = [fs(l) for l in f]

    [print(h, flush=True) for h in held]
    print("  host root filesystem:", host_root, flush=True)
    print(
        f"  mounts the VMM can see from its root: {visible}; mounts its namespace still holds: {len(held)};"
        f" the host's root filesystem ({host_root[0]}) among them: {'yes' if host_root in held else 'no'}"
    )
    vm.wait(timeout=10)
    done = [l for _, l in vm.lines if "INIT-DONE" in l]
    print("  guest ran to completion:", "yes, INIT-DONE seen, rc", vm.proc.returncode if done else "NO")


DEMOS = {
    "boot": demo_boot,
    "kbdprobe": demo_kbdprobe,
    "devices": demo_devices,
    "snapshot": demo_snapshot,
    "seccomp": demo_seccomp,
    "jailer": demo_jailer,
}


def main():
    sys.stdout.reconfigure(line_buffering=True)  # interleave correctly with the namespace child
    if len(sys.argv) > 1 and sys.argv[1] == NET_INNER:
        net_inner()
        return
    if len(sys.argv) > 1 and sys.argv[1] == JAIL_INNER:
        jail_inner(sys.argv[2], sys.argv[3])
        return
    for p in (FC, KERNEL, INITRD, DISK):
        if not os.path.exists(p):
            sys.exit(f"missing {p}: run `make` first")
    which = sys.argv[1:] or ["all"]
    names = list(DEMOS) if which == ["all"] else which
    print(f"Firecracker {FC_VER}, kernel {os.uname().release}, work dir $WORK={WORK}")
    try:
        for n in names:
            DEMOS[n]()
    finally:
        if not os.environ.get("KEEP"):
            shutil.rmtree(WORK, ignore_errors=True)


if __name__ == "__main__":
    main()
