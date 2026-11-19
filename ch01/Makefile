CFLAGS ?= -O2 -Wall -Wextra
PROGS  := popf cpuid_exit

all: $(PROGS)
run: all
	./popf
	./cpuid_exit
clean:
	rm -f $(PROGS)
.PHONY: all run clean
