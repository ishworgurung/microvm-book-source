/*
 * popf.c - the x86 instruction that broke Popek and Goldberg, run at ring 3.
 *
 * A trap-and-emulate VMM runs the guest kernel in user mode and relies on
 * every sensitive instruction trapping. This program shows, on whatever x86
 * machine you run it on, which of the classic offenders trap and which do not:
 *
 *   1. mov %cs  : reads the current privilege level. No trap.
 *   2. popf     : tries to clear IF (interrupt flag). No trap; silently ignored.
 *   3. cli      : tries to clear IF. Traps (#GP, delivered as SIGSEGV).
 *   4. smsw     : reads CR0's low bits. Unprivileged on the original x86.
 *   5. sgdt     : reads the GDT base address. Unprivileged on the original x86.
 *
 * On a CPU with UMIP (User-Mode Instruction Prevention) enabled, 4 and 5 fault
 * at CPL 3 and Linux emulates them, returning fixed dummy values (see
 * arch/x86/kernel/umip.c). A GDT base of 0xfffffffffffe0000 is that dummy.
 *
 * Build and run:  cc -O2 -o popf popf.c && ./popf
 */
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define IF_BIT (1UL << 9)

static sigjmp_buf env;
static volatile int last_sig;

static void on_fault(int sig) { last_sig = sig; siglongjmp(env, 1); }

static uint64_t read_flags(void)
{
	uint64_t f;
	__asm__ volatile("pushfq; popq %0" : "=r"(f));
	return f;
}

int main(void)
{
	struct sigaction sa;
	memset(&sa, 0, sizeof sa);
	sa.sa_handler = on_fault;
	sigaction(SIGSEGV, &sa, NULL);
	sigaction(SIGILL, &sa, NULL);

	/* 1. The low two bits of CS are the CPL. */
	uint16_t cs;
	__asm__ volatile("mov %%cs, %0" : "=r"(cs));
	printf("1. mov %%cs   -> CPL %u: no trap, any program sees its ring\n",
	       cs & 3);

	/* 2. POPF with IF cleared. At CPL 3 with IOPL 0 the CPU ignores the IF bit. */
	uint64_t before = read_flags();
	uint64_t want = before & ~IF_BIT;
	__asm__ volatile("pushq %0; popfq" : : "r"(want) : "cc", "memory");
	uint64_t after = read_flags();
	printf("2. popf IF=0 -> no trap; IF was %lu, asked 0, now %lu: %s\n",
	       (before & IF_BIT) >> 9, (after & IF_BIT) >> 9,
	       (after & IF_BIT) ? "ignored" : "cleared");

	/* 3. CLI is privileged (IOPL-sensitive): it faults at CPL 3. */
	if (sigsetjmp(env, 1) == 0) {
		__asm__ volatile("cli");
		printf("3. cli       -> no trap\n");
	} else {
		printf("3. cli       -> trapped: %s\n", strsignal(last_sig));
	}

	/* 4. SMSW: unprivileged on the original x86; UMIP makes it fault. */
	if (sigsetjmp(env, 1) == 0) {
		uint64_t msw = 0;
		__asm__ volatile("smsw %0" : "=r"(msw));
		printf("4. smsw      -> 0x%lx, no signal\n", msw);
	} else {
		printf("4. smsw      -> trapped: %s\n", strsignal(last_sig));
	}

	/* 5. SGDT: the "Red Pill" instruction. */
	struct __attribute__((packed)) { uint16_t limit; uint64_t base; } gdtr;
	if (sigsetjmp(env, 1) == 0) {
		__asm__ volatile("sgdt %0" : "=m"(gdtr));
		printf("5. sgdt      -> base 0x%lx%s\n", gdtr.base,
		       gdtr.base == 0xfffffffffffe0000UL ?
		       " (UMIP dummy: emulated)" : " (real: ran natively)");
	} else {
		printf("5. sgdt      -> trapped: %s\n", strsignal(last_sig));
	}
	return 0;
}
