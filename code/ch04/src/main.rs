//! vmm4: chapter 3's VMM plus a virtio-blk disk and a
//! virtio-net card on the virtio-mmio transport.
//!
//! Usage:
//!   vmm4 --kernel vmlinux [--initrd initramfs.cpio]
//!        [--mem MiB] [--disk disk.img [--ro]] [--tap tap0]
//!        [--no-ioeventfd] [--cmdline-extra "..."]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};

use ch04::{blk, machine, net, tap, virtio};
use kvm_ioctls::{IoEventAddress, VcpuExit, VmFd};
use virtio::MmioTransport;
use vm_superio::{I8042Device, Serial, Trigger};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

const COM1: u16 = 0x3f8;
const COM1_IRQ: u32 = 4;
const I8042_DATA: u16 = 0x60;
const I8042_CMD: u16 = 0x64;

/// The first virtio device's registers, in the MMIO hole.
/// Each further device sits 4 KiB higher, one IRQ higher.
const VIRTIO_BASE: u64 = 0xd000_0000;
const VIRTIO_IRQ: u32 = 5;

/// A locally administered MAC address (bit 1 of byte 0 set).
const GUEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

struct EventFdTrigger(EventFd);
impl Trigger for EventFdTrigger {
    type E = io::Error;
    fn trigger(&self) -> io::Result<()> {
        self.0.write(1)
    }
}

struct Args {
    kernel: String,
    initrd: Option<String>,
    mem_mib: u64,
    disk: Option<String>,
    read_only: bool,
    tap: Option<String>,
    ioeventfd: bool,
    cmdline_extra: String,
}

fn parse_args() -> Args {
    let mut a = Args {
        kernel: String::new(),
        initrd: None,
        mem_mib: 256,
        disk: None,
        read_only: false,
        tap: None,
        ioeventfd: true,
        cmdline_extra: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().expect("missing value");
        match k.as_str() {
            "--kernel" => a.kernel = val(),
            "--initrd" => a.initrd = Some(val()),
            "--mem" => a.mem_mib = val().parse().expect("MiB"),
            "--disk" => a.disk = Some(val()),
            "--ro" => a.read_only = true,
            "--tap" => a.tap = Some(val()),
            "--no-ioeventfd" => a.ioeventfd = false,
            "--cmdline-extra" => a.cmdline_extra = val(),
            _ => panic!("unknown argument {k}"),
        }
    }
    assert!(!a.kernel.is_empty(), "--kernel is required");
    a
}

/// The MMIO bus: which register window belongs to which device.
struct Bus {
    devices: Vec<(u64, MmioTransport)>,
}

impl Bus {
    fn find(
        &mut self,
        addr: u64,
    ) -> Option<(u64, &mut MmioTransport)> {
        self.devices.iter_mut().find_map(|(base, d)| {
            let off = addr.checked_sub(*base)?;
            (off < virtio::MMIO_SIZE).then_some((off, d))
        })
    }
}

/// Where device `n` lives, and the command-line entry that
/// tells the guest: size@base:irq.
fn slot(n: u64) -> (u64, u32, String) {
    let base = VIRTIO_BASE + n * virtio::MMIO_SIZE;
    let gsi = VIRTIO_IRQ + n as u32;
    let arg = format!(" virtio_mmio.device=4K@{base:#x}:{gsi}");
    (base, gsi, arg)
}

/// Wire one device to KVM and put it on the bus.
fn attach(
    vm: &VmFd,
    bus: &mut Bus,
    dev: MmioTransport,
    irq: &EventFd,
    ioeventfd: bool,
) -> Result<(), String> {
    let (base, gsi, _) = slot(bus.devices.len() as u64);
    // irqfd: a write to `irq` raises line `gsi` in the guest.
    vm.register_irqfd(irq, gsi)
        .map_err(|e| format!("KVM_IRQFD: {e}"))?;
    // ioeventfd: a 4-byte write of value `i` to QueueNotify
    // signals queue i's eventfd inside KVM.
    if ioeventfd {
        let notify = base + virtio::QUEUE_NOTIFY;
        for i in 0..dev.num_queues() {
            let addr = IoEventAddress::Mmio(notify);
            vm.register_ioevent(dev.kick_fd(i), &addr, i as u32)
                .map_err(|e| format!("KVM_IOEVENTFD: {e}"))?;
        }
    }
    bus.devices.push((base, dev));
    Ok(())
}

fn main() {
    if let Err(e) = run(parse_args()) {
        eprintln!("vmm4: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    let mut cmdline = String::from(
        "console=ttyS0 reboot=k panic=-1 acpi=off pci=off \
         i8042.noaux i8042.nomux i8042.dumbkbd",
    );
    // The device list must be on the command line before the
    // kernel is loaded, so open the backends first.
    let disk = match &args.disk {
        Some(p) => Some(
            OpenOptions::new()
                .read(true)
                .write(!args.read_only)
                .open(p)
                .map_err(|e| format!("{p}: {e}"))?,
        ),
        None => None,
    };
    let tap = match &args.tap {
        Some(name) => Some(
            tap::Tap::open(name)
                .map_err(|e| format!("tap {name}: {e}"))?,
        ),
        None => None,
    };
    // One entry per device, in the order `attach` adds them.
    let count = disk.is_some() as u64 + tap.is_some() as u64;
    for n in 0..count {
        cmdline.push_str(&slot(n).2);
    }
    let full = format!("{cmdline} {}", args.cmdline_extra);
    let m = machine::build(&machine::Config {
        kernel: &args.kernel,
        initrd: args.initrd.as_deref(),
        mem_mib: args.mem_mib,
        cmdline: full.trim_end(),
    })?;
    let (vm, mut vcpu, mem) = (m.vm, m.vcpu, m.mem);
    eprintln!("[vmm4] cmdline: {}", m.cmdline);

    // Serial port and reset line, as in chapter 3.
    let serial_irq =
        EventFd::new(EFD_NONBLOCK).map_err(|e| e.to_string())?;
    vm.register_irqfd(&serial_irq, COM1_IRQ)
        .map_err(|e| format!("KVM_IRQFD serial: {e}"))?;
    let mut serial =
        Serial::new(EventFdTrigger(serial_irq), io::stdout());
    let reset =
        EventFd::new(EFD_NONBLOCK).map_err(|e| e.to_string())?;
    let reset2 = reset.try_clone().map_err(|e| e.to_string())?;
    let mut i8042 = I8042Device::new(EventFdTrigger(reset2));

    // The virtio devices, in the order of the command line.
    let mut bus = Bus {
        devices: Vec::new(),
    };
    let new_irq =
        || EventFd::new(EFD_NONBLOCK).map_err(|e| e.to_string());
    if let Some(file) = disk {
        let dev = blk::Block::new(file, args.read_only)
            .map_err(|e| e.to_string())?;
        let irq = new_irq()?;
        let t = MmioTransport::new(
            Box::new(dev),
            mem.clone(),
            irq.try_clone().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        attach(&vm, &mut bus, t, &irq, args.ioeventfd)?;
    }
    if let Some(tap) = tap {
        let dev = net::Net::new(tap, GUEST_MAC);
        let irq = new_irq()?;
        let t = MmioTransport::new(
            Box::new(dev),
            mem.clone(),
            irq.try_clone().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        attach(&vm, &mut bus, t, &irq, args.ioeventfd)?;
    }

    // The run loop. Only exits that KVM could not handle
    // itself arrive here; each is counted by kind.
    let mut exits: BTreeMap<&str, u64> = BTreeMap::new();
    let reason = loop {
        let exit =
            vcpu.run().map_err(|e| format!("KVM_RUN: {e}"))?;
        match exit {
            VcpuExit::IoOut(port, data) => {
                *exits.entry("io_out").or_default() += 1;
                match port {
                    COM1..=0x3ff => {
                        let off = (port - COM1) as u8;
                        let _ = serial.write(off, data[0]);
                    }
                    I8042_DATA | I8042_CMD => {
                        let off = (port - I8042_DATA) as u8;
                        let _ = i8042.write(off, data[0]);
                    }
                    _ => {}
                }
            }
            VcpuExit::IoIn(port, data) => {
                *exits.entry("io_in").or_default() += 1;
                data.fill(0);
                match port {
                    COM1..=0x3ff => {
                        data[0] = serial.read((port - COM1) as u8)
                    }
                    I8042_DATA | I8042_CMD => {
                        let off = (port - I8042_DATA) as u8;
                        data[0] = i8042.read(off)
                    }
                    _ => {}
                }
            }
            VcpuExit::MmioRead(addr, data) => {
                *exits.entry("mmio_read").or_default() += 1;
                match bus.find(addr) {
                    Some((off, dev)) => dev.read(off, data),
                    None => data.fill(0xff),
                }
            }
            VcpuExit::MmioWrite(addr, data) => {
                *exits.entry("mmio_write").or_default() += 1;
                if let Some((off, dev)) = bus.find(addr) {
                    dev.write(off, data);
                }
            }
            VcpuExit::Shutdown => break "triple fault",
            VcpuExit::Hlt => break "HLT",
            other => {
                eprintln!("[vmm4] unexpected exit {other:?}");
                *exits.entry("other").or_default() += 1;
            }
        }
        if reset.read().is_ok() {
            break "guest reset (reboot=k)";
        }
    };
    let _ = io::stdout().flush();
    eprintln!("\n[vmm4] stopped: {reason}");
    eprintln!("[vmm4] exits to user space: {exits:?}");
    for (base, d) in &bus.devices {
        eprintln!(
            "[vmm4] virtio-mmio at {base:#x}: status {:#x}, \
             features {:#x}",
            d.status(),
            d.driver_features()
        );
    }
    Ok(())
}
