//! The virtio-mmio transport: the 4 KiB block of registers through
//! which a guest driver finds a virtio device, agrees on features,
//! tells the device where its queues are, and kicks it.
//!
//! The layout is the one in the virtio 1.2 specification, section
//! 4.2.2 ("MMIO Device Register Layout"). Only the non-legacy
//! layout (Version 2) is implemented.
//!
//! The transport does not move any data. When the driver sets
//! DRIVER_OK it hands the queues to the device, and from then on
//! the device's own thread serves them (see `blk.rs`, `net.rs`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;

/// Size of one device's register window.
pub const MMIO_SIZE: u64 = 0x1000;

// Register offsets from the start of the window (spec 4.2.2).
pub const MAGIC_VALUE: u64 = 0x000;
pub const VERSION: u64 = 0x004;
pub const DEVICE_ID: u64 = 0x008;
pub const VENDOR_ID: u64 = 0x00c;
pub const DEVICE_FEATURES: u64 = 0x010;
pub const DEVICE_FEATURES_SEL: u64 = 0x014;
pub const DRIVER_FEATURES: u64 = 0x020;
pub const DRIVER_FEATURES_SEL: u64 = 0x024;
pub const QUEUE_SEL: u64 = 0x030;
pub const QUEUE_NUM_MAX: u64 = 0x034;
pub const QUEUE_NUM: u64 = 0x038;
pub const QUEUE_READY: u64 = 0x044;
pub const QUEUE_NOTIFY: u64 = 0x050;
pub const INTERRUPT_STATUS: u64 = 0x060;
pub const INTERRUPT_ACK: u64 = 0x064;
pub const STATUS: u64 = 0x070;
pub const QUEUE_DESC_LOW: u64 = 0x080;
pub const QUEUE_DESC_HIGH: u64 = 0x084;
pub const QUEUE_DRIVER_LOW: u64 = 0x090;
pub const QUEUE_DRIVER_HIGH: u64 = 0x094;
pub const QUEUE_DEVICE_LOW: u64 = 0x0a0;
pub const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
pub const CONFIG_GENERATION: u64 = 0x0fc;
/// The device-specific configuration space starts here.
pub const CONFIG: u64 = 0x100;

/// "virt" in little-endian ASCII.
const MAGIC: u32 = 0x7472_6976;
/// Any value; the Linux driver only prints it.
const VENDOR: u32 = 0x4d56_4d34; // "4MVM"

// Device status bits (spec 2.1).
pub const ACKNOWLEDGE: u32 = 1;
pub const DRIVER: u32 = 2;
pub const DRIVER_OK: u32 = 4;
pub const FEATURES_OK: u32 = 8;
pub const FAILED: u32 = 128;

/// Feature bit 32: "this is a modern (virtio 1.x) device".
/// The Version 2 MMIO layout requires it.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// InterruptStatus bit 0: "I have put buffers in a used ring".
pub const INT_USED_RING: u32 = 1;

/// How a device tells the guest that it has used buffers.
///
/// `status` is the InterruptStatus register. The device thread
/// sets bit 0 and the guest's InterruptACK write clears it, so
/// it is shared and atomic. `irqfd` is registered with KVM: one
/// `write` raises the device's interrupt line in the guest.
pub struct Interrupt {
    pub status: AtomicU32,
    pub irqfd: EventFd,
}

impl Interrupt {
    /// Set the status bit *before* raising the line, so that the
    /// guest's handler finds it when it reads InterruptStatus.
    pub fn signal_used(&self) {
        self.status.fetch_or(INT_USED_RING, Ordering::AcqRel);
        let _ = self.irqfd.write(1);
    }
}

/// What a device needs to provide on top of the transport.
pub trait VirtioDevice: Send {
    /// 1 for a network card, 2 for a block device (spec 5).
    fn device_type(&self) -> u32;
    /// The feature bits the device offers, VERSION_1 included.
    fn features(&self) -> u64;
    /// One entry per queue: the largest size the device accepts.
    fn queue_max_sizes(&self) -> &[u16];
    /// Read from the device-specific configuration space.
    fn read_config(&self, offset: u64, data: &mut [u8]);
    /// The driver set DRIVER_OK: take the queues and start
    /// serving them. `kicks[i]` is signalled when the driver
    /// notifies queue `i`.
    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        queues: Vec<Queue>,
        kicks: Vec<EventFd>,
        irq: Arc<Interrupt>,
    );
}

/// One device behind a virtio-mmio register window.
pub struct MmioTransport {
    device: Box<dyn VirtioDevice>,
    mem: GuestMemoryMmap,
    queues: Vec<Queue>,
    /// One eventfd per queue. With an ioeventfd, KVM signals it
    /// on a QueueNotify write; without, `write` below does.
    kicks: Vec<EventFd>,
    irq: Arc<Interrupt>,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    active: bool,
}

impl MmioTransport {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        mem: GuestMemoryMmap,
        irqfd: EventFd,
    ) -> std::io::Result<Self> {
        let mut queues = Vec::new();
        let mut kicks = Vec::new();
        for &max in device.queue_max_sizes() {
            queues.push(Queue::new(max).expect("power of 2"));
            kicks.push(EventFd::new(0)?);
        }
        let irq = Arc::new(Interrupt {
            status: AtomicU32::new(0),
            irqfd,
        });
        Ok(MmioTransport {
            device,
            mem,
            queues,
            kicks,
            irq,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            active: false,
        })
    }

    /// The eventfd for queue `i`, to register as an ioeventfd.
    pub fn kick_fd(&self, i: usize) -> &EventFd {
        &self.kicks[i]
    }

    pub fn num_queues(&self) -> usize {
        self.kicks.len()
    }

    /// The queue that QueueSel points at, if it exists and the
    /// driver may still change it.
    fn selected(&mut self) -> Option<&mut Queue> {
        if self.active {
            return None;
        }
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// A guest read of `data.len()` bytes at `offset`.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        if offset >= CONFIG {
            self.device.read_config(offset - CONFIG, data);
            return;
        }
        let q = self.queues.get(self.queue_sel as usize);
        let feat = self.device.features();
        let v: u32 = match offset {
            MAGIC_VALUE => MAGIC,
            VERSION => 2,
            DEVICE_ID => self.device.device_type(),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => match self.device_features_sel {
                0 => feat as u32,
                1 => (feat >> 32) as u32,
                _ => 0,
            },
            QUEUE_NUM_MAX => q.map_or(0, |q| q.max_size() as u32),
            QUEUE_READY => match q {
                Some(q) => (self.active || q.ready()) as u32,
                None => 0,
            },
            INTERRUPT_STATUS => {
                self.irq.status.load(Ordering::Acquire)
            }
            STATUS => self.status,
            CONFIG_GENERATION => 0, // the config never changes
            _ => 0,
        };
        let b = v.to_le_bytes();
        let n = data.len().min(4);
        data[..n].copy_from_slice(&b[..n]);
    }

    /// A guest write of `data` at `offset`.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let mut b = [0u8; 4];
        let n = data.len().min(4);
        b[..n].copy_from_slice(&data[..n]);
        let v = u32::from_le_bytes(b);
        match offset {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            DRIVER_FEATURES => match self.driver_features_sel {
                0 => self.set_features(v as u64, 0xffff_ffff),
                1 => self.set_features(
                    (v as u64) << 32,
                    0xffff_ffff << 32,
                ),
                _ => {}
            },
            QUEUE_SEL => self.queue_sel = v,
            QUEUE_NUM => {
                if let Some(q) = self.selected() {
                    q.set_size(v as u16);
                }
            }
            QUEUE_READY => {
                if let Some(q) = self.selected() {
                    q.set_ready(v == 1);
                }
            }
            QUEUE_DESC_LOW | QUEUE_DESC_HIGH => {
                let (lo, hi) = halves(offset, QUEUE_DESC_LOW, v);
                if let Some(q) = self.selected() {
                    q.set_desc_table_address(lo, hi);
                }
            }
            QUEUE_DRIVER_LOW | QUEUE_DRIVER_HIGH => {
                let (lo, hi) =
                    halves(offset, QUEUE_DRIVER_LOW, v);
                if let Some(q) = self.selected() {
                    q.set_avail_ring_address(lo, hi);
                }
            }
            QUEUE_DEVICE_LOW | QUEUE_DEVICE_HIGH => {
                let (lo, hi) =
                    halves(offset, QUEUE_DEVICE_LOW, v);
                if let Some(q) = self.selected() {
                    q.set_used_ring_address(lo, hi);
                }
            }
            // Reached only when no ioeventfd is registered: pass
            // the kick on to the device thread ourselves.
            QUEUE_NOTIFY => {
                if let Some(k) = self.kicks.get(v as usize) {
                    let _ = k.write(1);
                }
            }
            INTERRUPT_ACK => {
                self.irq.status.fetch_and(!v, Ordering::AcqRel);
            }
            STATUS => self.set_status(v),
            _ => {}
        }
    }

    /// Record the driver's features, `mask` selecting which half.
    fn set_features(&mut self, v: u64, mask: u64) {
        self.driver_features = (self.driver_features & !mask) | v;
    }

    fn set_status(&mut self, v: u32) {
        if v == 0 {
            self.reset();
            return;
        }
        let mut v = v;
        // FEATURES_OK: the device checks the driver's choice.
        // Clearing the bit again tells the driver "refused".
        let new = v & !self.status;
        if new & FEATURES_OK != 0 {
            let offered = self.device.features();
            let ok = self.driver_features & !offered == 0
                && self.driver_features & VIRTIO_F_VERSION_1 != 0;
            if !ok {
                v &= !FEATURES_OK;
            }
        }
        self.status = v;
        if new & DRIVER_OK != 0 && !self.active {
            self.activate();
        }
    }

    /// DRIVER_OK: check the queues and hand them to the device.
    fn activate(&mut self) {
        let valid = self.status & FEATURES_OK != 0
            && self
                .queues
                .iter()
                .all(|q| q.ready() && q.is_valid(&self.mem));
        if !valid {
            self.status |= FAILED;
            return;
        }
        let queues = std::mem::take(&mut self.queues);
        // Keep empty stand-ins so that register reads still work.
        for q in &queues {
            self.queues.push(Queue::new(q.max_size()).unwrap());
        }
        let kicks = self
            .kicks
            .iter()
            .map(|k| k.try_clone().expect("dup eventfd"))
            .collect();
        self.device.activate(
            self.mem.clone(),
            queues,
            kicks,
            self.irq.clone(),
        );
        self.active = true;
    }

    /// Status 0: forget everything the driver set up. A device
    /// that is already running keeps running: this small VMM
    /// does not stop device threads.
    fn reset(&mut self) {
        if self.active {
            return;
        }
        for q in &mut self.queues {
            q.reset();
        }
        self.status = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.irq.status.store(0, Ordering::Release);
    }

    pub fn status(&self) -> u32 {
        self.status
    }

    pub fn driver_features(&self) -> u64 {
        self.driver_features
    }
}

/// A 64-bit address arrives as two 32-bit writes. Return the
/// value as the low or the high half, depending on the offset.
fn halves(
    off: u64,
    low: u64,
    v: u32,
) -> (Option<u32>, Option<u32>) {
    if off == low {
        (Some(v), None)
    } else {
        (None, Some(v))
    }
}
