# microVMs from Scratch: source code

Code for the book *microVMs from Scratch: KVM, virtio and Firecracker in Rust*
by Ishwor Gurung. Each directory holds the programs for one chapter.

| Dir   | Chapter                  | What it is                                              | Needs `/dev/kvm` |
|-------|--------------------------|---------------------------------------------------------|------------------|
| `code/ch01` | Trap and emulate        | Two small C programs: `popf` and `cpuid_exit`           | no               |
| `code/ch02` | KVM from userspace      | A VM from `/dev/kvm`: real, protected and long mode     | to run           |
| `code/ch03` | Booting Linux           | `rvmm`, a VMM that boots a Linux kernel                 | to run           |
| `code/ch04` | virtio                  | virtio-blk and virtio-net on virtio-mmio                | to run           |
| `code/ch05` | Production VMMs         | seccomp and lazy restore; a Firecracker snapshot script | to run           |

## Requirements

- Linux on x86-64
- a C compiler and `make`
- Rust via [rustup](https://rustup.rs) (each chapter pins its toolchain in
  `rust-toolchain.toml`)
- `/dev/kvm` that you can read and write, for the parts that boot a guest.
  `make -C code/ch02 check` tells you what is missing.
- chapter 3 also needs an uncompressed `vmlinux` built with `CONFIG_PVH=y`
  (set `KERNEL=...`) and `cpio`; see `code/ch03/Makefile`

## Build and run

```sh
git clone git@github.com:ishworgurung/microvm-book-source.git
cd microvm-book-source

make -C code/ch01 run          # one chapter
make -C code/ch03 test         # unit tests, no /dev/kvm needed
make                      # everything that needs no kernel
```

Every `Makefile` starts with a comment listing its targets. Chapter 4 builds
on chapter 3's initramfs, so build `code/ch03` first.

## CI

`.github/workflows/ch03-boot.yml` boots a real kernel under chapter 3's VMM on
GitHub's KVM-enabled runners, as a check that the code works.

## License

MIT. See [LICENSE](LICENSE).
