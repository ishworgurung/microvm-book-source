/*
 * init.c: the guest's /init, the first program the kernel runs.
 *
 * It prints how long the kernel took to boot, a few facts the
 * kernel learned from the VMM, and then either reboots (which
 * stops the VM) or, with labexit=shell on the kernel command
 * line, starts /bin/sh on the console and reboots when it exits.
 *
 * The kernel passes unknown key=value boot options to /init as
 * environment variables, which is how labexit arrives.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <time.h>
#include <unistd.h>
#include <termios.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/wait.h>

static double uptime(void) {
  struct timespec ts;
  clock_gettime(CLOCK_BOOTTIME, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* Print `label` and the first line of `path` that starts with
 * `key` (or the first line, if key is NULL). */
static void show(const char *label, const char *path,
                 const char *key) {
  char line[512];
  FILE *f = fopen(path, "r");
  if (!f) {
    printf("  %-10s (no %s)\n", label, path);
    return;
  }
  while (fgets(line, sizeof line, f)) {
    if (key && strncmp(line, key, strlen(key)))
      continue;
    line[strcspn(line, "\n")] = 0;
    printf("  %-10s %s\n", label, line);
    break;
  }
  fclose(f);
}

int main(void) {
  double t0 = uptime();
  mount("proc", "/proc", "proc", 0, NULL);
  mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
  mount("sysfs", "/sys", "sysfs", 0, NULL);
  setvbuf(stdout, NULL, _IOLBF, 0);
  printf("INIT-START at %.3f s (kernel boot time before PID 1)\n",
         t0);

  printf("[init] task info\n");
  show("cpus", "/sys/devices/system/cpu/online", NULL);
  show("cpuinfo", "/proc/cpuinfo", "model name");
  show("memory", "/proc/meminfo", "MemTotal");
  show("cmdline", "/proc/cmdline", NULL);
  show("clock", "/sys/devices/system/clocksource/"
                "clocksource0/current_clocksource", NULL);

  const char *ex = getenv("labexit");
  if (ex && !strcmp(ex, "shell")) {
    pid_t pid = fork();
    if (pid == 0) {
      setsid();                       /* a session of its own */
      int fd = open("/dev/console", O_RDWR);
      dup2(fd, 0), dup2(fd, 1), dup2(fd, 2);
      execl("/bin/sh", "sh", NULL);
      perror("[init] /bin/sh");
      _exit(127);
    }
    waitpid(pid, NULL, 0);
  }

  printf("INIT-DONE at %.3f s, exit=reboot\n", uptime());
  tcdrain(1); /* let the console finish before the reset */
  sync();
  reboot(RB_AUTOBOOT); /* reboot=k: the i8042 reset stops the VM */
  return 0;
}
