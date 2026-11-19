"""vmlab.py -- small, stdlib-only helpers shared by fc.py and ch.py.

* `UnixHTTP`: an HTTP/1.1 client over a Unix socket. Firecracker's API and
  Cloud Hypervisor's API are both REST over a Unix socket, with no TCP port,
  so the only thing that can reach them is a process that can open the path.
* `Vmm`: spawn a VMM, read its console (the guest's serial port, which both
  VMMs write to their stdout) on a thread, and timestamp every line against the
  moment the process was started, so boot timelines are measured from outside
  the guest with one clock.
"""

import http.client
import json
import os
import re
import socket
import subprocess
import threading
import time

ROOT = os.path.dirname(os.path.abspath(__file__))  # code/ch05/measure
GUEST = os.path.join(ROOT, "guest", "out")
VENDOR = os.path.join(ROOT, "vendor")


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path, timeout=10):
        super().__init__("localhost", timeout=timeout)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def api(sock_path, method, url, body=None, timeout=30, log_level="info"):
    """One request; returns (status, parsed JSON or text). Raises on 4xx/5xx so
    a misconfiguration stops the demo with the VMM's own error message."""
    c = UnixHTTP(sock_path, timeout=timeout)
    data = json.dumps(body) if body is not None else None
    headers = {"Content-Type": "application/json", "Accept": "application/json"} if data else {}
    if log_level == "debug":
        print(f"  {method} {url} {headers} -> {data}")
    c.request(method, url, body=data, headers=headers)
    r = c.getresponse()
    raw = r.read().decode()
    c.close()
    try:
        out = json.loads(raw) if raw else None
    except json.JSONDecodeError:
        out = raw
    if r.status >= 400:
        raise RuntimeError(f"{method} {url} -> {r.status}: {out}")
    return r.status, out


def wait_for_path(path, timeout=5.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if os.path.exists(path):
            return True
        time.sleep(0.0005)
    raise TimeoutError(f"{path} did not appear within {timeout}s")


class Vmm:
    """A VMM child process whose stdout is the guest console."""

    def __init__(self, argv, echo=False, env=None, stdin=subprocess.DEVNULL):
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(argv, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=env)
        self.lines = []  # (seconds since spawn, text)
        self.echo = echo
        self.cv = threading.Condition()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.argv = argv

    def _read(self):
        for raw in iter(self.proc.stdout.readline, b""):
            t = time.monotonic() - self.t0
            line = raw.decode(errors="replace").rstrip("\r\n")
            with self.cv:
                self.lines.append((t, line))
                self.cv.notify_all()
            if self.echo:
                print(f"  [{t * 1000:8.1f} ms] {line}")
        with self.cv:
            self.cv.notify_all()

    def wait_line(self, pattern, timeout=20.0, start=0):
        """Block until a console line matches `pattern`; return (t, line, index)."""
        rx = re.compile(pattern)
        end = time.monotonic() + timeout
        i = start
        with self.cv:
            while True:
                while i < len(self.lines):
                    t, line = self.lines[i]
                    if rx.search(line):
                        return t, line, i
                    i += 1
                left = end - time.monotonic()
                if left <= 0 or (self.proc.poll() is not None and i >= len(self.lines)):
                    tail = "\n".join(l for _, l in self.lines[-15:])
                    raise TimeoutError(f"no console line matching {pattern!r}; last lines:\n{tail}")
                self.cv.wait(min(left, 0.05))

    def now(self):
        return time.monotonic() - self.t0

    def wait(self, timeout=20.0):
        rc = self.proc.wait(timeout=timeout)
        self.reader.join(timeout=2)
        return rc

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        self.reader.join(timeout=2)

    def guest_lines(self):
        """Console lines that came from the guest, without the VMM's own log lines."""
        log = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d|^cloud-hypervisor: ")
        return [(t, l) for t, l in self.lines if not log.search(l)]


def rss_kib(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    return 0


def threads(pid):
    """[(tid, name, seccomp mode, number of filters)] for every thread."""
    out = []
    for tid in sorted(os.listdir(f"/proc/{pid}/task"), key=int):
        st = {}
        try:
            with open(f"/proc/{pid}/task/{tid}/status") as f:
                for line in f:
                    k, _, v = line.partition(":")
                    st[k] = v.strip()
        except FileNotFoundError:
            continue
        out.append((int(tid), st.get("Name", "?"), st.get("Seccomp", "?"), st.get("Seccomp_filters", "?")))
    return out


def median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2
