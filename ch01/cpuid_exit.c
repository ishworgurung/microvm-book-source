/*
 * cpuid_exit.c - am I in a VM, and what does one VM exit cost?
 *
 * CPUID is unconditionally intercepted under Intel VT-x: every execution in
 * VMX non-root mode causes a VM exit (Intel SDM Vol. 3C, "Instructions That
 * Cause VM Exits Unconditionally"). So timing CPUID from inside a guest times
 * a full exit-and-reentry round trip. On bare metal CPUID is just a slow,
 * serialising instruction.
 *
 * The program:
 *   1. reads CPUID.1:ECX bit 5 (VMX) and bit 31 (hypervisor present);
 *   2. if a hypervisor is present, reads its vendor signature at leaf 0x40000000;
 *   3. times CPUID against LFENCE+RDTSC-only loops, in TSC ticks and ns.
 *
 * Build and run:  cc -O2 -o cpuid_exit cpuid_exit.c && ./cpuid_exit
 */
#include <cpuid.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <x86intrin.h>

#define N 200000

static double now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1e9 + ts.tv_nsec;
}

/* Run fn N times, return mean ns per iteration (best of 5 runs). */
static double bench(int use_cpuid)
{
	double best = 1e30;
	for (int r = 0; r < 5; r++) {
		unsigned a, b, c, d;
		double t0 = now_ns();
		for (int i = 0; i < N; i++) {
			if (use_cpuid)
				__cpuid(0, a, b, c, d);
			else
				_mm_lfence();
		}
		double t = (now_ns() - t0) / N;
		if (t < best)
			best = t;
		(void)a; (void)b; (void)c; (void)d;
	}
	return best;
}

int main(void)
{
	unsigned a, b, c, d;
	__cpuid(1, a, b, c, d);
	int vmx = (c >> 5) & 1, hv = (c >> 31) & 1;
	printf("VT-x (CPUID.1:ECX bit 5)       : %d (%s)\n", vmx,
	       vmx ? "offered to this OS" : "not offered to this OS");
	printf("hypervisor (CPUID.1:ECX bit 31): %d (%s)\n", hv,
	       hv ? "running under one" : "bare metal, or it hides");

	if (hv) {
		char sig[13] = {0};
		__cpuid(0x40000000, a, b, c, d);
		memcpy(sig + 0, &b, 4);
		memcpy(sig + 4, &c, 4);
		memcpy(sig + 8, &d, 4);
		printf("hypervisor signature           : \"%s\" (max leaf 0x%x)\n",
		       sig, a);
	}

	double base = bench(0), cpuid = bench(1);
	printf("lfence                         : %7.1f ns per iteration\n", base);
	printf("cpuid                          : %7.1f ns per iteration\n", cpuid);
	printf("cpuid / lfence                 : ~%.0fx%s\n", cpuid / base,
	       hv ? " (each cpuid is a VM exit)" : "");
	return 0;
}
