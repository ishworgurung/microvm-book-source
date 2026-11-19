/*
 * init.c -- the guest's /init, shared by fc.py and ch.py.
 *
 * One static binary, steered by the kernel command line, so that Firecracker,
 * the rust-vmm VMM and Cloud Hypervisor all boot the *same* kernel and the
 * *same* user space, and differences in what we observe are the VMM's.
 *
 * The kernel hands unknown `key=value` parameters without a dot to /init as
 * environment variables (init/main.c, unknown_bootoption), so:
 *
 *   labmode=info,blk,vsock   tasks to run, in order
 *   labexit=reboot|poweroff|hang
 *
 * The same binary can run as PID 1 of a gVisor sandbox. There is no
 * kernel command line; runsc passes labmode and labexit in the environment
 * from the OCI config.json instead, and /init reads them the same way.
 *
 * Firecracker on x86 stops when the guest resets through the i8042 (reboot=k);
 * Cloud Hypervisor stops on an ACPI power-off but *restarts* the VM on reboot.
 *
 * A distribution kernel builds virtio-mmio, virtio-net and vsock as modules,
 * so /init loads the ones the build script copied into /lib/modules (in the
 * order of /lib/modules/order) with finit_module(MODULE_INIT_COMPRESSED_FILE):
 * the kernel decompresses the .ko.zst itself (CONFIG_MODULE_DECOMPRESS).
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <linux/vm_sockets.h>
#include <net/if.h>
#include <net/route.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#ifndef MODULE_INIT_COMPRESSED_FILE
#define MODULE_INIT_COMPRESSED_FILE 4
#endif

static double uptime(void) {
  struct timespec ts;
  clock_gettime(CLOCK_BOOTTIME, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}

static int slurp(const char *path, char *buf, size_t len) {
  int fd = open(path, O_RDONLY);
  ssize_t n = fd < 0 ? -1 : read(fd, buf, len - 1);
  if (fd >= 0)
    close(fd);
  if (n < 0)
    return -1;
  buf[n] = 0;
  if (n && buf[n - 1] == '\n')
    buf[n - 1] = 0;
  return 0;
}

static void spit(const char *path, const char *s) {
  int fd = open(path, O_WRONLY);
  if (fd >= 0) {
    if (write(fd, s, strlen(s)) < 0) { /* offline-able but busy: ignore */
    }
    close(fd);
  }
}

/* First line of `path` that starts with `prefix`. */
static void show(const char *label, const char *path, const char *prefix) {
  char buf[16384];
  if (slurp(path, buf, sizeof buf) < 0)
    return;
  for (char *l = strtok(buf, "\n"); l; l = strtok(NULL, "\n"))
    if (!prefix || strncmp(l, prefix, strlen(prefix)) == 0) {
      printf("  %-10s %s\n", label, l);
      if (prefix)
        break;
    }
}

/* modprobe's job, done by hand: a `module.param=value` on the kernel command
 * line is *not* applied by the kernel to a module loaded later with
 * finit_module(); the loader must pass it. virtio-mmio devices declared as
 * `virtio_mmio.device=4K@0xd0000000:5` depend on this. */
static void module_params(const char *file, char *out, size_t len) {
  char name[64], cmd[4096];
  size_t n = strcspn(file, ".");
  snprintf(name, sizeof name, "%.*s", (int)n, file);
  for (char *c = name; *c; c++)
    if (*c == '-')
      *c = '_';
  out[0] = 0;
  if (slurp("/proc/cmdline", cmd, sizeof cmd) < 0)
    return;
  size_t nl = strlen(name), used = 0;
  for (char *save = NULL, *tok = strtok_r(cmd, " ", &save); tok;
       tok = strtok_r(NULL, " ", &save))
    if (!strncmp(tok, name, nl) && tok[nl] == '.')
      used += snprintf(out + used, used < len ? len - used : 0, "%s%s",
                       used ? " " : "", tok + nl + 1);
}

static void load_modules(void) {
  char order[4096], path[256], params[512];
  if (slurp("/lib/modules/order", order, sizeof order) < 0)
    return;
  for (char *save = NULL, *m = strtok_r(order, "\n", &save); m;
       m = strtok_r(NULL, "\n", &save)) {
    snprintf(path, sizeof path, "/lib/modules/%s", m);
    module_params(m, params, sizeof params);
    if (params[0])
      printf("[init] module %s params \"%s\"\n", m, params);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    long rc = fd < 0 ? -1
                     : syscall(SYS_finit_module, fd, params,
                               strstr(m, ".ko.") ? MODULE_INIT_COMPRESSED_FILE
                                                 : 0);
    if (rc < 0 && errno != EEXIST)
      printf("[init] module %s: %s\n", m, strerror(errno));
    if (fd >= 0)
      close(fd);
  }
  printf("[init] modules loaded at %.3f s\n", uptime());
}

/* ---- tasks ------------------------------------------------------------- */

static void task_info(void) {
  show("cpus", "/sys/devices/system/cpu/online", NULL);
  show("cpuinfo", "/proc/cpuinfo", "model name");
  show("memory", "/proc/meminfo", "MemTotal");
  show("cmdline", "/proc/cmdline", NULL);
  show("clock",
       "/sys/devices/system/clocksource/clocksource0/current_clocksource",
       NULL);
  show("dmi", "/sys/class/dmi/id/sys_vendor", NULL);
}

/* Which bus each virtio device hangs off: virtio-mmio (Firecracker) or PCI
 * (Cloud Hypervisor). The device id is the virtio spec's (1 net, 2 block,
 * 4 entropy, 5 balloon, 19 vsock, 24 memory). */
static void task_devices(void) {
  DIR *d = opendir("/sys/bus/virtio/devices");
  struct dirent *e;
  char p[512], id[32], link[512];
  if (!d) {
    printf("  virtio     (no virtio bus)\n");
    return;
  }
  while ((e = readdir(d))) {
    if (e->d_name[0] == '.')
      continue;
    snprintf(p, sizeof p, "/sys/bus/virtio/devices/%s/device", e->d_name);
    slurp(p, id, sizeof id);
    snprintf(p, sizeof p, "/sys/bus/virtio/devices/%s", e->d_name);
    ssize_t n = readlink(p, link, sizeof link - 1);
    link[n > 0 ? n : 0] = 0;
    /* link looks like ../../../devices/platform/d0000000.virtio_mmio/virtio0 */
    char *bus = strstr(link, "devices/");
    printf("  virtio     %-8s id=%-6s on %s\n", e->d_name, id,
           bus ? bus + 8 : link);
  }
  closedir(d);
  d = opendir("/sys/bus/pci/devices");
  int npci = 0;
  if (d) {
    while ((e = readdir(d)))
      if (e->d_name[0] != '.')
        npci++;
    closedir(d);
  }
  printf("  pci        %d device(s)\n", npci);
}

static void list_dir(const char *path) {
  DIR *d = opendir(path);
  struct dirent *e;
  if (!d) {
    printf("  blk        no %s: %s\n", path, strerror(errno));
    return;
  }
  printf("  blk        %s:\n", path);
  while ((e = readdir(d)))
    if (e->d_name[0] != '.') {
      // No need to traverse `/proc` and `/sys` directories as they are virtual
      // filesystems
      if (strcmp(e->d_name, "proc") == 0 || strcmp(e->d_name, "sys") == 0) {
        continue;
      }
      printf("  blk          %s\n", e->d_name);
      if (e->d_type == DT_DIR) {
        char sub[256];
        snprintf(sub, sizeof sub, "%s/%s", path, e->d_name);
        list_dir(sub);
      }
    }
  closedir(d);
}

/* Read the first sector of the virtio block device, then mount it and read a
 * file the build put there, and write one back (the host checks it later). */
static void task_blk(void) {
  char buf[512];
  int fd = open("/dev/vda", O_RDONLY);
  if (fd < 0) {
    printf("  blk        no /dev/vda: %s\n", strerror(errno));
    return;
  }
  close(fd);
  mkdir("/mnt", 0755);
  if (mount("/dev/vda", "/mnt", "ext4", 0, NULL) < 0) {
    printf("  blk        mount: %s\n", strerror(errno));
    return;
  }
  // List the dirs and files recursively in /
  // list_dir("/");

  if (slurp("/mnt/hello.txt", buf, sizeof buf) == 0)
    printf("  blk        /dev/vda:/hello.txt = \"%s\"\n", buf);
  fd = open("/mnt/from-guest.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
  snprintf(buf, sizeof buf, "written by the guest at uptime %.3f s\n",
           uptime());
  if (fd >= 0) {
    if (write(fd, buf, strlen(buf)) < 0)
      perror("write");
    fsync(fd);
    close(fd);
  }
  umount("/mnt");
  printf("  blk        wrote /from-guest.txt, unmounted\n");
}

/* AF_VSOCK echo server on port 5000: answers each line upper-cased, `quit`
 * ends it. Both Firecracker and Cloud Hypervisor expose the guest's vsock to
 * the host as a Unix socket, so the host side needs no networking at all. */
static void task_vsock(void) {
  int s = socket(AF_VSOCK, SOCK_STREAM, 0);
  struct sockaddr_vm a = {
      .svm_family = AF_VSOCK, .svm_port = 5000, .svm_cid = VMADDR_CID_ANY};
  if (s < 0 || bind(s, (void *)&a, sizeof a) < 0 || listen(s, 1) < 0) {
    int err = errno; /* close() may change errno */
    if (s >= 0)
      close(s);
    printf("  vsock      %s\n", strerror(err));
    return;
  }
  printf("  vsock      listening on port 5000 (READY)\n");
  fflush(stdout);
  for (;;) {
    int c = accept(s, NULL, NULL);
    if (c < 0)
      continue;
    char buf[256];
    ssize_t n;
    int quit = 0;
    while ((n = read(c, buf, sizeof buf)) > 0) {
      if (n >= 4 && !strncmp(buf, "quit", 4))
        quit = 1;
      for (ssize_t i = 0; i < n; i++)
        if (buf[i] >= 'a' && buf[i] <= 'z')
          buf[i] -= 32; // toupper
      if (write(c, buf, n) < 0)
        break;
      if (quit)
        break;
    }
    close(c);
    if (quit)
      break;
  }
  close(s);
}

static int ifup(int s, const char *ifn, const char *ip) {
  struct ifreq r = {0};
  strncpy(r.ifr_name, ifn, IFNAMSIZ - 1);
  struct sockaddr_in *sin = (void *)&r.ifr_addr;
  sin->sin_family = AF_INET;
  inet_pton(AF_INET, ip, &sin->sin_addr);
  if (ioctl(s, SIOCSIFADDR, &r) < 0)
    return -1;
  inet_pton(AF_INET, "255.255.255.0", &sin->sin_addr);
  if (ioctl(s, SIOCSIFNETMASK, &r) < 0)
    return -1;
  if (ioctl(s, SIOCGIFFLAGS, &r) < 0)
    return -1;
  r.ifr_flags |= IFF_UP | IFF_RUNNING;
  return ioctl(s, SIOCSIFFLAGS, &r);
}

/* Bring up lo and eth0 (172.16.0.2/24, no DHCP), then serve TCP port 8080:
 * one line in, one line out, `quit` ends it. */
static void task_net(void) {
  int s = socket(AF_INET, SOCK_DGRAM, 0);
  if (ifup(s, "lo", "127.0.0.1") < 0 || ifup(s, "eth0", "172.16.0.2") < 0) {
    printf("  net        ifup: %s\n", strerror(errno));
    close(s);
    return;
  }
  close(s);
  s = socket(AF_INET, SOCK_STREAM, 0);
  int one = 1;
  setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(8080)};
  if (bind(s, (void *)&a, sizeof a) < 0 || listen(s, 1) < 0) {
    printf("  net        %s\n", strerror(errno));
    close(s);
    return;
  }
  printf("  net        eth0 172.16.0.2/24 up, TCP 8080 listening (READY)\n");
  fflush(stdout);
  for (;;) {
    int c = accept(s, NULL, NULL);
    if (c < 0)
      continue;
    char buf[256], out[320];
    ssize_t n = read(c, buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    int len = snprintf(out, sizeof out,
                       "guest got %zd bytes over virtio-net: %s", n, buf);
    if (write(c, out, len) < 0)
      perror("write");
    close(c);
    if (!strncmp(buf, "quit", 4))
      break;
  }
  close(s);
}

/* A visible clock for snapshot/restore: a counter and the guest's monotonic
 * time, every 100 ms, for `labticks` ticks (default forever). A restored VM
 * continues the count; the gap in CLOCK_MONOTONIC shows whether guest time
 * stood still while it was paused. */
static void task_counter(void) {
  const char *t = getenv("labticks");
  long max = t ? atol(t) : -1;
  for (long i = 0; max < 0 || i < max; i++) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    printf("tick %ld mono %.3f\n", i, ts.tv_sec + ts.tv_nsec / 1e9);
    fflush(stdout);
    usleep(100000);
  }
}

/* Read the hardware RNG the kernel picked: with a virtio entropy device,
 * every read becomes a request on its virtqueue. */
static void task_rng(void) {
  char cur[64] = "";
  slurp("/sys/class/misc/hw_random/rng_current", cur, sizeof cur);
  int fd = open("/dev/hwrng", O_RDONLY);
  unsigned char b[32];
  if (fd < 0 || read(fd, b, sizeof b) != (ssize_t)sizeof b) {
    printf("  rng        /dev/hwrng (%s): %s\n", cur, strerror(errno));
    if (fd >= 0)
      close(fd);
    return;
  }
  printf("  rng        hwrng=%s, 32 bytes: ", cur);
  for (size_t i = 0; i < sizeof b; i++)
    printf("%02x", b[i]);
  printf("\n");
  /* Throughput: 1 MiB through the device. */
  double t = uptime();
  size_t total = 0;
  char big[4096];
  while (total < (1 << 20)) {
    ssize_t n = read(fd, big, sizeof big);
    if (n <= 0)
      break;
    total += n;
  }
  printf("  rng        read %zu KiB in %.1f ms\n", total >> 10,
         (uptime() - t) * 1e3);
  close(fd);
  /* How the device's interrupts arrive: IO-APIC (a pin, then the driver must
   * read and acknowledge the device's interrupt status) or PCI-MSIX (a
   * message per queue, nothing to read back). Squeeze the column padding. */
  char irq[16384];
  if (slurp("/proc/interrupts", irq, sizeof irq) == 0)
    for (char *sv = NULL, *l = strtok_r(irq, "\n", &sv); l;
         l = strtok_r(NULL, "\n", &sv))
      if (strstr(l, "virtio")) {
        char sq[256];
        size_t j = 0;
        for (char *c = l; *c && j < sizeof sq - 1; c++)
          if (!(*c == ' ' && j && sq[j - 1] == ' '))
            sq[j++] = *c;
        sq[j] = 0;
        printf("  rng        irq:%s\n", sq);
      }
}

/* Keyboard: find the evdev node whose device name says
 * "keyboard", then turn EV_KEY presses back into characters, one line at a
 * time, until a line reads "quit". This is the far end of the chain that
 * starts with a scancode in the VMM: i8042 -> serio -> atkbd -> input core ->
 * evdev -> here. */
static void task_kbd(void) {
  static const char map[KEY_MAX] = {
      [KEY_A] = 'a',     [KEY_B] = 'b',     [KEY_C] = 'c',  [KEY_D] = 'd',
      [KEY_E] = 'e',     [KEY_F] = 'f',     [KEY_G] = 'g',  [KEY_H] = 'h',
      [KEY_I] = 'i',     [KEY_J] = 'j',     [KEY_K] = 'k',  [KEY_L] = 'l',
      [KEY_M] = 'm',     [KEY_N] = 'n',     [KEY_O] = 'o',  [KEY_P] = 'p',
      [KEY_Q] = 'q',     [KEY_R] = 'r',     [KEY_S] = 's',  [KEY_T] = 't',
      [KEY_U] = 'u',     [KEY_V] = 'v',     [KEY_W] = 'w',  [KEY_X] = 'x',
      [KEY_Y] = 'y',     [KEY_Z] = 'z',     [KEY_0] = '0',  [KEY_1] = '1',
      [KEY_2] = '2',     [KEY_3] = '3',     [KEY_4] = '4',  [KEY_5] = '5',
      [KEY_6] = '6',     [KEY_7] = '7',     [KEY_8] = '8',  [KEY_9] = '9',
      [KEY_SPACE] = ' ', [KEY_MINUS] = '-', [KEY_DOT] = '.'};
  char path[64] = "", name[128];
  /* atkbd probes asynchronously, so the keyboard may appear after PID 1
   * starts: wait up to 3 s for it, and say how long that took. */
  double t0 = uptime();
  while (!path[0] && uptime() - t0 < 3.0) {
    for (int i = 0; i < 8 && !path[0]; i++) {
      char p[96];
      snprintf(p, sizeof p, "/sys/class/input/event%d/device/name", i);
      if (slurp(p, name, sizeof name) == 0 && strstr(name, "keyboard"))
        snprintf(path, sizeof path, "/dev/input/event%d", i);
    }
    if (!path[0])
      usleep(1000);
  }
  printf("  kbd        waited %.0f ms for the keyboard to appear (uptime %.3f "
         "s)\n",
         (uptime() - t0) * 1e3, uptime());
  int fd = path[0] ? open(path, O_RDONLY) : -1;
  if (fd < 0) {
    printf("  kbd        no keyboard input device\n");
    return;
  }
  printf("  kbd        %s is \"%s\" (READY)\n", path, name);
  fflush(stdout);
  char line[128];
  size_t n = 0;
  int shift = 0;
  struct input_event ev;
  while (read(fd, &ev, sizeof ev) == (ssize_t)sizeof ev) {
    if (ev.type != EV_KEY)
      continue;
    if (ev.code == KEY_LEFTSHIFT) {
      shift = ev.value != 0;
      continue;
    }
    if (ev.value != 1) /* 1 press, 0 release, 2 autorepeat */
      continue;
    if (ev.code == KEY_ENTER) {
      line[n] = 0;
      printf("  kbd        guest read a line: \"%s\"\n", line);
      fflush(stdout);
      if (!strcmp(line, "quit"))
        break;
      n = 0;
    } else if (ev.code < KEY_MAX && map[ev.code] && n < sizeof line - 1) {
      char c = map[ev.code];
      line[n++] = shift && c >= 'a' && c <= 'z' ? c - 32 : c;
    }
  }
  close(fd);
  /* A sniffer module logs with pr_info, below this kernel's
   * console level, so fetch its lines from the log buffer as dmesg does. */
  static char log[1 << 17];
  int len = klogctl(3 /* SYSLOG_ACTION_READ_ALL */, log, sizeof log - 1);
  if (len > 0) {
    log[len] = 0;
    for (char *sv = NULL, *l = strtok_r(log, "\n", &sv); l;
         l = strtok_r(NULL, "\n", &sv))
      if (strstr(l, "i8042-sniff") || strstr(l, "input: AT") ||
          strstr(l, "probe of serio0") || strstr(l, "atkbd"))
        printf("  dmesg      %s\n", l);
  }
}

/* Hot-plug watcher: bring every offline CPU and memory block online as it
 * appears, and print the totals whenever they change. Linux onlines hot-added
 * memory by itself only if memhp_default_state / auto_online_blocks says so. */
static void task_hotplug(void) {
  const char *t = getenv("labticks");
  long max = t ? atol(t) : 600;
  /* labfill=N: once MemTotal has grown by N MiB, touch N MiB and free it
   * again, so the host can see what plugged memory costs once it is used. */
  const char *f = getenv("labfill");
  long fill = f ? atol(f) : 0, mem0 = 0;
  char cpus[64] = "", mem[128] = "", last[320] = "";
  for (long i = 0; i < max; i++) {
    char p[320], v[16];
    DIR *d = opendir("/sys/devices/system/cpu");
    struct dirent *e;
    while (d && (e = readdir(d))) {
      if (strncmp(e->d_name, "cpu", 3) || e->d_name[3] < '0' ||
          e->d_name[3] > '9')
        continue;
      snprintf(p, sizeof p, "/sys/devices/system/cpu/%s/online", e->d_name);
      if (slurp(p, v, sizeof v) == 0 && v[0] == '0')
        spit(p, "1");
    }
    if (d)
      closedir(d);
    d = opendir("/sys/devices/system/memory");
    while (d && (e = readdir(d))) {
      if (strncmp(e->d_name, "memory", 6))
        continue;
      snprintf(p, sizeof p, "/sys/devices/system/memory/%s/online", e->d_name);
      if (slurp(p, v, sizeof v) == 0 && v[0] == '0')
        spit(p, "1");
    }
    if (d)
      closedir(d);
    slurp("/sys/devices/system/cpu/online", cpus, sizeof cpus);
    char mi[16384];
    if (slurp("/proc/meminfo", mi, sizeof mi) == 0) {
      char *l = strstr(mi, "MemTotal:");
      if (l) {
        char *nl = strchr(l, '\n');
        if (nl)
          *nl = 0;
        snprintf(mem, sizeof mem, "%s", l);
      }
    }
    char blk[96] = "";
    d = opendir("/sys/block");
    while (d && (e = readdir(d)))
      if (!strncmp(e->d_name, "vd", 2) &&
          strlen(blk) + strlen(e->d_name) + 2 < sizeof blk) {
        strcat(blk, " ");
        strcat(blk, e->d_name);
      }
    if (d)
      closedir(d);
    char now[320];
    snprintf(now, sizeof now, "cpus online %s, %s, disks:%s", cpus, mem,
             blk[0] ? blk : " none");
    if (strcmp(now, last)) {
      printf("[%.3f] %s\n", uptime(), now);
      fflush(stdout);
      strcpy(last, now);
    }
    long kb = mem[0] ? atol(mem + strlen("MemTotal:")) : 0;
    if (!mem0)
      mem0 = kb;
    if (fill && kb - mem0 >= fill << 10) {
      sleep(1); /* give the host time to measure the plug on its own */
      size_t n = (size_t)fill << 20;
      char *m = mmap(NULL, n, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      if (m != MAP_FAILED) {
        memset(m, 0x5a, n); /* every page written: the host must back it */
        printf("[%.3f] filled %ld MiB\n", uptime(), fill);
        fflush(stdout);
        sleep(1);
        munmap(m, n); /* free in the guest: the host still holds the pages */
        printf("[%.3f] freed %ld MiB\n", uptime(), fill);
      } else {
        printf("[%.3f] fill: %s\n", uptime(), strerror(errno));
      }
      fflush(stdout);
      fill = 0;
    }
    usleep(100000);
  }
}

/* One counter from /proc/vmstat, or -1 if the file or the line is missing. */
static long vmstat(const char *name) {
  char buf[16384];
  if (slurp("/proc/vmstat", buf, sizeof buf) < 0)
    return -1;
  size_t n = strlen(name);
  for (char *sv = NULL, *l = strtok_r(buf, "\n", &sv); l;
       l = strtok_r(NULL, "\n", &sv))
    if (!strncmp(l, name, n) && l[n] == ' ')
      return atol(l + n + 1);
  return -1;
}

/* Nanoseconds on CLOCK_MONOTONIC. The vDSO answers this call in user space,
 * so the measurement itself makes no syscall (bench checks that claim). */
static double now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1e9 + ts.tv_nsec;
}

/* what one kernel service costs, measured from inside. Each line
 * is the mean of many calls, so one slow call does not decide the result.
 *
 *  0. Wait for the vDSO clock. gVisor's vDSO answers clock_gettime in user
 *     space only once the Sentry has calibrated the TSC against the host
 *     clock and set `ready` in the vDSO's parameter page (gVisor
 *     vdso/vdso_time.cc). Until then, every call is a real syscall. Calls are
 *     timed in 100 ms windows until one window averages under 100 ns (or 5 s
 *     pass). The time that took is printed; a Linux kernel is ready at once.
 *  1. getppid: the smallest real syscall. The kernel (or gVisor's Sentry)
 *     enters, reads one field and returns. This is the cost of crossing into
 *     the kernel and back. getpid is not used because a libc may cache it.
 *  2. clock_gettime: served by the vDSO, a page of kernel code mapped into the
 *     process. Once the clock is ready, no syscall happens.
 *  3. rdtsc: the instruction the vDSO reads the clock with. If the platform
 *     made it fault, step 2 would be slow without any syscall.
 *  4. stat("/"): a path lookup. The kernel walks its dentry cache; gVisor
 *     walks its own, and asks its gofer (or the host) only on a miss.
 *  5. page fault: first touch of fresh anonymous memory. The CPU faults, the
 *     kernel allocates a zeroed page and maps it. In a VM, EPT maps it a
 *     second time on the host side; in gVisor, the Sentry handles the fault
 *     and maps host memory into the process. */
static void task_bench(void) {
  enum { CALLS = 200000, STATS = 50000, MIB = 64 };
  double start = now_ns(), window = start, per = 0;
  while (window - start < 5e9) {
    long n = 0;
    while (now_ns() - window < 1e8)
      n++;
    per = 1e8 / n;
    if (per < 100)
      break;
    window = now_ns();
  }
  /* `window` is the start of the first fast window, or of the last one. */
  printf("  bench      vDSO clock fast after %.1f s (%.1f ns/call)\n",
         (window - start) / 1e9, per);

  double t = now_ns();
  for (int i = 0; i < CALLS; i++)
    syscall(SYS_getppid);
  printf("  bench      getppid        %8.1f ns/call\n",
         (now_ns() - t) / CALLS);

  t = now_ns();
  for (int i = 0; i < CALLS; i++)
    now_ns();
  printf("  bench      clock_gettime  %8.1f ns/call\n",
         (now_ns() - t) / CALLS);

  /* The vDSO reads the time-stamp counter with RDTSC. If the platform makes
   * RDTSC fault (PR_SET_TSC on Linux, or a VM that exits on it), the vDSO
   * call is slow although no syscall happens. This line times RDTSC alone. */
  t = now_ns();
  for (int i = 0; i < CALLS; i++)
    __builtin_ia32_rdtsc();
  printf("  bench      rdtsc          %8.1f ns/call\n",
         (now_ns() - t) / CALLS);

  struct stat st;
  t = now_ns();
  for (int i = 0; i < STATS; i++)
    stat("/", &st);
  printf("  bench      stat /         %8.1f ns/call\n",
         (now_ns() - t) / STATS);

  size_t len = (size_t)MIB << 20;
  long page = sysconf(_SC_PAGESIZE);
  char *m = mmap(NULL, len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (m == MAP_FAILED) {
    printf("  bench      mmap: %s\n", strerror(errno));
    return;
  }
  struct rusage before, after;
  long thp_before = vmstat("thp_fault_alloc");
  getrusage(RUSAGE_SELF, &before);
  t = now_ns();
  for (size_t off = 0; off < len; off += page)
    m[off] = 1;
  double took = now_ns() - t;
  getrusage(RUSAGE_SELF, &after);
  long thp_after = vmstat("thp_fault_alloc");
  printf("  bench      page fault     %8.1f ns/page (%d MiB)\n",
         took / ((double)len / (double)page), MIB);
  /* How many faults this kernel took for those pages. With transparent huge
   * pages, one fault maps 2 MiB, so far fewer faults than pages. -1 means
   * this kernel has no /proc/vmstat. */
  printf("  bench      faults         %ld minor, %ld THP, for %ld pages\n",
         after.ru_minflt - before.ru_minflt,
         thp_before < 0 ? -1 : thp_after - thp_before, (long)(len / page));
  munmap(m, len);
}

/* write one file to the root file system and one to /data, then
 * read each back. Where each write goes depends on who serves the path:
 *
 *  - in a VM, / is the initramfs (a tmpfs in guest RAM) and there is no
 *    /data, so the first write succeeds and is lost at power-off;
 *  - in gVisor, / is the container's root file system. By default the
 *    Sentry puts a memory-backed overlay on top of it, so the write stays in
 *    the sandbox. With --overlay2=none it reaches the host directory.
 *    /data is a bind mount of a host directory: the write reaches the host. */
static void task_write(void) {
  const char *paths[] = {"/from-init.txt", "/data/from-init.txt"};
  for (size_t i = 0; i < sizeof paths / sizeof paths[0]; i++) {
    char back[64] = "";
    int fd = open(paths[i], O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "written by /init\n", 17) != 17) {
      printf("  write      %-20s %s\n", paths[i], strerrorname_np(errno));
      if (fd >= 0)
        close(fd);
      continue;
    }
    close(fd);
    slurp(paths[i], back, sizeof back);
    printf("  write      %-20s ok, reads back \"%s\"\n", paths[i], back);
  }
}

/* does this kernel implement these syscalls? Each one is called
 * with arguments that do no harm (a null pointer, a zero size, an fd of -1),
 * so the answer is only the errno:
 *
 *  - ENOSYS: the syscall is not implemented here at all;
 *  - EPERM, EBADF, EINVAL, EFAULT and the rest: it exists and rejected the
 *    arguments or the caller;
 *  - 0: it exists and succeeded (the fd it returned is closed).
 *
 * A Linux kernel implements all of them. gVisor's Sentry implements the
 * syscalls its authors chose; the rest return ENOSYS without reaching the
 * host kernel. */
static void task_syscalls(void) {
  const struct {
    const char *name;
    long nr;
    long a[3];
  } calls[] = {
      {"io_uring_setup", SYS_io_uring_setup, {1, 0, 0}},
      {"bpf", SYS_bpf, {0, 0, 0}},
      {"perf_event_open", SYS_perf_event_open, {0, 0, -1}},
      {"userfaultfd", SYS_userfaultfd, {0, 0, 0}},
      {"finit_module", SYS_finit_module, {-1, 0, 0}},
      {"kexec_load", SYS_kexec_load, {0, 0, 0}},
      {"add_key", SYS_add_key, {0, 0, 0}},
      {"open_by_handle_at", SYS_open_by_handle_at, {-1, 0, 0}},
      {"memfd_create", SYS_memfd_create, {(long)"x", 0, 0}},
      {"pidfd_open", SYS_pidfd_open, {1, 0, 0}},
  };
  for (size_t i = 0; i < sizeof calls / sizeof calls[0]; i++) {
    errno = 0;
    long rc = syscall(calls[i].nr, calls[i].a[0], calls[i].a[1],
                      calls[i].a[2], 0, 0, 0);
    int err = errno;
    if (rc >= 0 && calls[i].nr != SYS_kexec_load)
      close((int)rc);
    printf("  syscall    %-18s %s\n", calls[i].name,
           rc >= 0 ? "ok" : strerrorname_np(err));
  }
}

int main(void) {
  double t0 = uptime();
  mount("proc", "/proc", "proc", 0, NULL);
  mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
  mount("sysfs", "/sys", "sysfs", 0, NULL);
  int con = open("/dev/console", O_WRONLY);
  if (con >= 0) {
    dup2(con, 1);
    dup2(con, 2);
  }
  setvbuf(stdout, NULL, _IOLBF, 0);
  printf("INIT-START at %.3f s (kernel boot time before PID 1)\n", t0);
  load_modules();

  const char *mode = getenv("labmode");
  char modes[256];
  snprintf(modes, sizeof modes, "%s", mode ? mode : "info");
  char *save = NULL;
  for (char *m = strtok_r(modes, ",", &save); m;
       m = strtok_r(NULL, ",", &save)) {
    printf("[init] task %s\n", m);
    if (!strcmp(m, "info"))
      task_info();
    else if (!strcmp(m, "devices"))
      task_devices();
    else if (!strcmp(m, "blk"))
      task_blk();
    else if (!strcmp(m, "vsock"))
      task_vsock();
    else if (!strcmp(m, "net"))
      task_net();
    else if (!strcmp(m, "counter"))
      task_counter();
    else if (!strcmp(m, "rng"))
      task_rng();
    else if (!strcmp(m, "kbd"))
      task_kbd();
    else if (!strcmp(m, "hotplug"))
      task_hotplug();
    else if (!strcmp(m, "bench"))
      task_bench();
    else if (!strcmp(m, "syscalls"))
      task_syscalls();
    else if (!strcmp(m, "write"))
      task_write();
    else
      printf("[init] unknown task %s\n", m);
  }

  const char *ex = getenv("labexit");
  printf("INIT-DONE at %.3f s, exit=%s\n", uptime(), ex ? ex : "reboot");
  /* Our output sits in the tty's queue and drains asynchronously; the
   * kernel's "reboot:" printk goes straight to the UART and would cut into
   * it. Wait until the queue is empty. */
  tcdrain(1);
  sync();
  if (ex && !strcmp(ex, "hang"))
    for (;;)
      pause();
  reboot(ex && !strcmp(ex, "poweroff") ? RB_POWER_OFF : RB_AUTOBOOT);
  return 0;
}
