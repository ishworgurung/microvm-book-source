//! rvmm, chapter 3: boot a real Linux kernel to its /init.
//!
//! Usage: rvmm --kernel vmlinux [--initrd initramfs.cpio]
//!             [--mem MiB] [--cmdline-extra "..."]
//!             [--absent-fill 0xNN]
//!
//! The VM gets the interrupt controllers and the timer inside
//! KVM, and two devices emulated here: a 16550A serial port at
//! I/O port 0x3f8 (the console) and an i8042 keyboard
//! controller at 0x60 and 0x64, used only for its reset line.
//! Every other I/O port reads as the --absent-fill byte.

mod boot;
mod layout;

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use boot::ctx;
use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region};
use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd};
use vm_memory::{Address, GuestAddress, GuestMemoryBackend};
use vm_memory::{GuestMemoryMmap, GuestMemoryRegion};
use vm_superio::serial::NoEvents;
use vm_superio::{I8042Device, Serial, Trigger};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

const COM1: u16 = 0x3f8; // ports 0x3f8 to 0x3ff
const COM1_IRQ: u32 = 4;
const I8042_DATA: u16 = 0x60;
const I8042_CMD: u16 = 0x64;

/// The kernel command line. Each option is explained in the
/// chapter's "The command line" section.
const CMDLINE: &str = "console=ttyS0 reboot=k panic=-1 \
    acpi=off pci=off i8042.noaux i8042.nomux i8042.dumbkbd";

/// vm-superio calls trigger() to raise an interrupt. Ours is
/// an eventfd: for the UART it is an irqfd, so one write(2)
/// makes KVM raise IRQ 4 in the guest.
struct EventFdTrigger(EventFd);
impl Trigger for EventFdTrigger {
    type E = io::Error;
    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
    }
}

/// Where the UART's output goes: stdout, plus the times of
/// the first byte and of /init's INIT-START and INIT-DONE.
struct Console {
    t0: Instant,
    line: Vec<u8>,
    first_byte: Option<f64>,
    marks: Vec<(f64, String)>,
    bytes: u64,
}
impl Write for Console {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let now = ms(self.t0, Instant::now());
        self.first_byte.get_or_insert(now);
        self.bytes += buf.len() as u64;
        for &b in buf {
            if b == b'\n' {
                let l = String::from_utf8_lossy(&self.line);
                if l.starts_with("INIT-") {
                    self.marks.push((now, l.into_owned()));
                }
                self.line.clear();
            } else if b != b'\r' {
                self.line.push(b);
            }
        }
        io::stdout().write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

type Uart = Serial<EventFdTrigger, NoEvents, Console>;

fn ms(t0: Instant, t: Instant) -> f64 {
    t.duration_since(t0).as_secs_f64() * 1e3
}

struct Args {
    kernel: String,
    initrd: Option<String>,
    mem_mib: u64,
    cmdline_extra: String,
    absent_fill: u8,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        kernel: String::new(),
        initrd: None,
        mem_mib: 256,
        cmdline_extra: String::new(),
        absent_fill: 0x00,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().ok_or(format!("{k}: value?"));
        match k.as_str() {
            "--kernel" => a.kernel = val()?,
            "--initrd" => a.initrd = Some(val()?),
            "--mem" => {
                a.mem_mib =
                    val()?.parse().map_err(ctx("--mem"))?
            }
            "--cmdline-extra" => a.cmdline_extra = val()?,
            "--absent-fill" => {
                let v = val()?;
                let hex = v.trim_start_matches("0x");
                a.absent_fill = u8::from_str_radix(hex, 16)
                    .map_err(ctx("--absent-fill"))?;
            }
            _ => return Err(format!("unknown argument {k}")),
        }
    }
    if a.kernel.is_empty() {
        return Err("--kernel PATH is required".into());
    }
    Ok(a)
}

fn main() {
    if let Err(e) = parse_args().and_then(run) {
        eprintln!("rvmm: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    let t0 = Instant::now();

    // ---- 1. The VM, with the PC's interrupt controllers and
    // timer emulated inside KVM.
    let kvm = Kvm::new().map_err(ctx("open /dev/kvm"))?;
    let vm = kvm.create_vm().map_err(ctx("KVM_CREATE_VM"))?;
    vm.set_tss_address(layout::KVM_TSS as usize)
        .map_err(ctx("KVM_SET_TSS_ADDR"))?;
    vm.create_irq_chip().map_err(ctx("KVM_CREATE_IRQCHIP"))?;
    let pit = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit).map_err(ctx("KVM_CREATE_PIT2"))?;

    // ---- 2. Guest RAM: one mmap and one memory slot per
    // region, with the MMIO hole between them.
    let regions = layout::ram_regions(args.mem_mib << 20);
    let ranges: Vec<(GuestAddress, usize)> = regions
        .iter()
        .map(|&(start, len)| (GuestAddress(start), len as usize))
        .collect();
    let mem = GuestMemoryMmap::<()>::from_ranges(&ranges)
        .map_err(ctx("guest memory"))?;
    for (slot, region) in mem.iter().enumerate() {
        let start = region.start_addr();
        let host = mem
            .get_host_address(start)
            .map_err(ctx("host address"))?;
        let r = kvm_userspace_memory_region {
            slot: slot as u32,
            guest_phys_addr: start.raw_value(),
            memory_size: region.len(),
            userspace_addr: host as u64,
            flags: 0,
        };
        // SAFETY: `mem` owns the mapping and outlives the VM.
        unsafe { vm.set_user_memory_region(r) }
            .map_err(ctx("KVM_SET_USER_MEMORY_REGION"))?;
    }
    let t_vm = Instant::now();

    // ---- 3. Be the bootloader. These are plain memory
    // writes: no KVM call is involved.
    let (entry, elf_entry, k_end) =
        boot::load_kernel(&mem, &args.kernel)?;
    let initrd = match &args.initrd {
        Some(p) => {
            Some(boot::load_initrd(&mem, p, regions[0].1)?)
        }
        None => None,
    };
    let mut cmdline = CMDLINE.to_string();
    if !args.cmdline_extra.is_empty() {
        cmdline.push(' ');
        cmdline.push_str(&args.cmdline_extra);
    }
    boot::write_cmdline(&mem, &cmdline)?;
    boot::write_start_info(&mem, &regions, initrd)?;
    let t_loaded = Instant::now();

    // ---- 4. The vCPU: CPUID first, then the registers.
    let mut vcpu =
        vm.create_vcpu(0).map_err(ctx("KVM_CREATE_VCPU"))?;
    let mut cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(ctx("KVM_GET_SUPPORTED_CPUID"))?;
    boot::fix_cpuid(&mut cpuid);
    vcpu.set_cpuid2(&cpuid).map_err(ctx("KVM_SET_CPUID2"))?;
    let mut sregs =
        vcpu.get_sregs().map_err(ctx("KVM_GET_SREGS"))?;
    let regs = boot::pvh_registers(&mem, entry, &mut sregs)?;
    vcpu.set_sregs(&sregs).map_err(ctx("KVM_SET_SREGS"))?;
    vcpu.set_regs(&regs).map_err(ctx("KVM_SET_REGS"))?;
    vcpu.set_fpu(&boot::fpu()).map_err(ctx("KVM_SET_FPU"))?;
    let t_vcpu = Instant::now();

    // ---- 5. Devices. The UART's interrupt is an irqfd.
    let irq =
        EventFd::new(EFD_NONBLOCK).map_err(ctx("eventfd"))?;
    vm.register_irqfd(&irq, COM1_IRQ)
        .map_err(ctx("KVM_IRQFD"))?;
    let console = Console {
        t0,
        line: Vec::new(),
        first_byte: None,
        marks: Vec::new(),
        bytes: 0,
    };
    let uart = Serial::new(EventFdTrigger(irq), console);
    let uart = Arc::new(Mutex::new(uart));
    spawn_stdin_reader(uart.clone());
    // The i8042 signals `reset` when the guest asks to reboot.
    let reset =
        EventFd::new(EFD_NONBLOCK).map_err(ctx("eventfd"))?;
    let reset2 = reset.try_clone().map_err(ctx("eventfd"))?;
    let mut i8042 = I8042Device::new(EventFdTrigger(reset2));

    eprintln!(
        "[rvmm] PVH entry {entry:#x}, kernel {elf_entry:#x}..\
         {k_end:#x}, initrd {:?}, {} MiB RAM",
        initrd.map(|(a, s)| format!("{s} B at {a:#x}")),
        args.mem_mib
    );
    eprintln!("[rvmm] cmdline: {cmdline}");
    for (slot, r) in mem.iter().enumerate() {
        let host = mem.get_host_address(r.start_addr()).unwrap();
        let (start, len) = (r.start_addr().raw_value(), r.len());
        eprintln!(
            "[rvmm] slot {slot}: guest {start:#011x}..{:#011x} \
             -> host {:#x}..{:#x} ({} MiB)",
            start + len,
            host as u64,
            host as u64 + len,
            len >> 20
        );
    }

    // ---- 6. Run.
    let mut stats = Stats::default();
    let stop = run_loop(
        &mut vcpu,
        &uart,
        &mut i8042,
        &reset,
        args.absent_fill,
        &mut stats,
    )?;
    let t_end = Instant::now();

    // ---- 7. Report, on stderr so stdout is only the console.
    let uart = uart.lock().unwrap();
    let con = uart.writer();
    eprintln!("\n[rvmm] stopped: {stop}");
    eprintln!("[rvmm] timeline (ms since process start):");
    eprintln!("    {:8.1}  KVM VM + guest RAM", ms(t0, t_vm));
    eprintln!(
        "    {:8.1}  kernel ELF + initrd + PVH start info \
             written",
        ms(t0, t_loaded)
    );
    eprintln!(
        "    {:8.1}  vCPU configured: first guest instruction",
        ms(t0, t_vcpu)
    );
    if let Some(t) = con.first_byte {
        eprintln!("    {t:8.1}  first console byte");
    }
    for (t, l) in &con.marks {
        let l: String = l.chars().take(60).collect();
        eprintln!("    {t:8.1}  {l}");
    }
    eprintln!("    {:8.1}  VM stopped", ms(t0, t_end));
    eprintln!("[rvmm] exits to user space: {:?}", stats.exits);
    let mut busy: Vec<_> = stats.absent.iter().collect();
    busy.sort_by(|a, b| b.1.cmp(a.1));
    let busy: Vec<String> = busy
        .iter()
        .take(6)
        .map(|(p, n)| format!("{p:#x}:{n}"))
        .collect();
    eprintln!(
        "[rvmm] unemulated port accesses (fill {:#04x}): {}",
        args.absent_fill,
        busy.join(" ")
    );
    eprintln!("[rvmm] console bytes: {}", con.bytes);
    Ok(())
}

/// Exit counts by kind, and accesses to ports with no device.
#[derive(Default)]
struct Stats {
    exits: BTreeMap<&'static str, u64>,
    absent: BTreeMap<u16, u64>,
}

/// Run the vCPU until the guest resets or the CPU shuts down.
/// Returns why it stopped.
fn run_loop(
    vcpu: &mut VcpuFd,
    uart: &Mutex<Uart>,
    i8042: &mut I8042Device<EventFdTrigger>,
    reset: &EventFd,
    fill: u8,
    st: &mut Stats,
) -> Result<&'static str, String> {
    loop {
        match vcpu.run().map_err(ctx("KVM_RUN"))? {
            VcpuExit::IoOut(port, data) => {
                *st.exits.entry("io_out").or_default() += 1;
                match port {
                    COM1..=0x3ff => {
                        let off = (port - COM1) as u8;
                        let mut u = uart.lock().unwrap();
                        u.write(off, data[0])
                            .map_err(|e| format!("{e:?}"))?;
                    }
                    I8042_DATA | I8042_CMD => {
                        let off = (port - I8042_DATA) as u8;
                        let _ = i8042.write(off, data[0]);
                    }
                    // No device: drop the write.
                    _ => *st.absent.entry(port).or_default() += 1,
                }
            }
            VcpuExit::IoIn(port, data) => {
                *st.exits.entry("io_in").or_default() += 1;
                match port {
                    COM1..=0x3ff => {
                        let off = (port - COM1) as u8;
                        data[0] = uart.lock().unwrap().read(off);
                    }
                    I8042_DATA | I8042_CMD => {
                        let off = (port - I8042_DATA) as u8;
                        data[0] = i8042.read(off);
                    }
                    // No device: the VMM picks the value.
                    _ => {
                        data.fill(fill);
                        *st.absent.entry(port).or_default() += 1;
                    }
                }
            }
            // Nothing is mapped in the MMIO hole yet (chapter 4
            // puts virtio devices there): reads float to 0xff.
            VcpuExit::MmioRead(_, data) => {
                *st.exits.entry("mmio_read").or_default() += 1;
                data.fill(0xff);
            }
            VcpuExit::MmioWrite(..) => {
                *st.exits.entry("mmio_write").or_default() += 1;
            }
            // With the in-kernel irqchip, KVM handles HLT
            // itself, so this should never happen.
            VcpuExit::Hlt => {
                return Ok("HLT without an irqchip?")
            }
            VcpuExit::Shutdown => return Ok("triple fault"),
            other => {
                *st.exits.entry("other").or_default() += 1;
                eprintln!("[rvmm] unexpected exit {other:?}");
            }
        }
        if reset.read().is_ok() {
            return Ok("guest reset via i8042 (reboot=k)");
        }
    }
}

/// Feed what the user types into the UART's receive buffer.
/// enqueue_raw_bytes raises the receive interrupt, through the
/// irqfd, so the guest wakes even if its vCPU is halted.
fn spawn_stdin_reader(uart: Arc<Mutex<Uart>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            let n = match io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => return, // no more input
                Ok(n) => n,
            };
            let mut u = uart.lock().unwrap();
            let _ = u.enqueue_raw_bytes(&buf[..n]);
        }
    });
}
