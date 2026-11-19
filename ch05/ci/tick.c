/* tick.c: a guest /init for the CI check of fc-snapshot.sh.
 * Prints "tick N mono S" every 100 ms and never exits, so the
 * VM is still running when the script snapshots it. Build it
 * static: cc -static -O2 -o init tick.c */
#include <stdio.h>
#include <time.h>

int main(void)
{
	struct timespec t, d = {0, 100 * 1000 * 1000};
	for (unsigned long n = 1;; n++) {
		clock_gettime(CLOCK_MONOTONIC, &t);
		printf("tick %lu mono %ld.%03ld\n", n, (long)t.tv_sec,
		       t.tv_nsec / 1000000);
		fflush(stdout);
		nanosleep(&d, NULL);
	}
}
