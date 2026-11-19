//! Play the guest driver against the devices, without KVM.
//!
//! Each test lays out a virtqueue in a small guest memory with
//! virtio-queue's `MockSplitQueue` (which writes descriptors and
//! the available ring exactly as a driver would), lets the device
//! serve it, and checks the used ring and the guest's buffers.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use ch04::blk::{self, Disk};
use ch04::net::{NetQueues, Wire, HDR_LEN};
use ch04::virtio::*;
use virtio_bindings::bindings::virtio_ring::{
    VRING_DESC_F_NEXT, VRING_DESC_F_WRITE,
};
use virtio_queue::desc::{split::Descriptor, RawDescriptor};
use virtio_queue::mock::MockSplitQueue;
use virtio_queue::Queue;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

const NEXT: u16 = VRING_DESC_F_NEXT as u16;
const WRITE: u16 = VRING_DESC_F_WRITE as u16;

fn memory() -> GuestMemoryMmap {
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10_0000)])
        .unwrap()
}

fn desc(
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
) -> RawDescriptor {
    RawDescriptor::from(Descriptor::new(addr, len, flags, next))
}

/// The used ring's entry `i`: (chain head, bytes written).
fn used(
    q: &MockSplitQueue<GuestMemoryMmap>,
    i: usize,
) -> (u32, u32) {
    let e = q.used().ring().ref_at(i).unwrap().load();
    (e.id(), e.len())
}

// ---- virtio-blk ------------------------------------------------

/// A 16-sector disk in a temporary file. `name` keeps tests
/// that run in parallel apart.
fn disk(name: &str) -> Disk {
    let pid = std::process::id();
    let path = std::env::temp_dir()
        .join(format!("ch04-{name}-{pid}.img"));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    f.set_len(16 * blk::SECTOR).unwrap();
    std::fs::remove_file(&path).unwrap();
    Disk {
        file: Arc::new(f),
        sectors: 16,
        read_only: false,
    }
}

/// Write a request header {type, reserved, sector} at `at`.
fn header(
    mem: &GuestMemoryMmap,
    at: u64,
    kind: u32,
    sector: u64,
) {
    mem.write_obj(kind.to_le(), GuestAddress(at)).unwrap();
    mem.write_obj(0u32, GuestAddress(at + 4)).unwrap();
    mem.write_obj(sector.to_le(), GuestAddress(at + 8)).unwrap();
}

#[test]
fn blk_write_then_read() {
    let mem = memory();
    let mq = MockSplitQueue::new(&mem, 16);
    let mut q: Queue = mq.create_queue().unwrap();
    let mut d = disk("rw");

    // Request 1 (descriptors 0-2): write 512 bytes of 0xab
    // to sector 3. Request 2 (3-5): read sector 3 back.
    header(&mem, 0x8000, blk::T_OUT, 3);
    mem.write_slice(&[0xab; 512], GuestAddress(0x9000)).unwrap();
    header(&mem, 0xa000, blk::T_IN, 3);
    mq.add_desc_chains(
        &[
            desc(0x8000, 16, NEXT, 1),
            desc(0x9000, 512, NEXT, 2),
            desc(0x8100, 1, WRITE, 0),
            desc(0xa000, 16, NEXT, 4),
            desc(0xb000, 512, WRITE | NEXT, 5),
            desc(0xa100, 1, WRITE, 0),
        ],
        0,
    )
    .unwrap();

    assert!(d.process_queue(&mem, &mut q));
    assert_eq!(mq.used().idx().load(), 2);
    // The write: only the status byte was written.
    assert_eq!(used(&mq, 0), (0, 1));
    // The read: 512 data bytes plus the status byte.
    assert_eq!(used(&mq, 1), (3, 513));
    let st: u8 = mem.read_obj(GuestAddress(0x8100)).unwrap();
    assert_eq!(st, blk::S_OK);
    let st: u8 = mem.read_obj(GuestAddress(0xa100)).unwrap();
    assert_eq!(st, blk::S_OK);
    let mut back = [0u8; 512];
    mem.read_slice(&mut back, GuestAddress(0xb000)).unwrap();
    assert_eq!(back, [0xab; 512]);
}

#[test]
fn blk_rejects_bad_requests() {
    let mem = memory();
    let mq = MockSplitQueue::new(&mem, 16);
    let mut q: Queue = mq.create_queue().unwrap();
    let mut d = disk("bad");

    // Past the end: sector 16 of a 16-sector disk.
    header(&mem, 0x8000, blk::T_IN, 16);
    // A type the device does not know.
    header(&mem, 0xa000, 99, 0);
    // A data buffer that points outside guest memory.
    header(&mem, 0xc000, blk::T_IN, 0);
    mq.add_desc_chains(
        &[
            desc(0x8000, 16, NEXT, 1),
            desc(0x9000, 512, WRITE | NEXT, 2),
            desc(0x8100, 1, WRITE, 0),
            desc(0xa000, 16, NEXT, 4),
            desc(0xa100, 1, WRITE, 0),
            desc(0xc000, 16, NEXT, 6),
            desc(0x7fff_0000, 512, WRITE | NEXT, 7),
            desc(0xc100, 1, WRITE, 0),
        ],
        0,
    )
    .unwrap();

    d.process_queue(&mem, &mut q);
    assert_eq!(mq.used().idx().load(), 3);
    let st = |a| mem.read_obj::<u8>(GuestAddress(a)).unwrap();
    assert_eq!(st(0x8100), blk::S_IOERR);
    assert_eq!(st(0xa100), blk::S_UNSUPP);
    assert_eq!(st(0xc100), blk::S_IOERR);
}

// ---- virtio-net ------------------------------------------------

/// A wire that records sent frames and replays queued ones.
#[derive(Default, Clone)]
struct FakeWire {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl Wire for FakeWire {
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.incoming.lock().unwrap().pop_front() {
            Some(f) => {
                buf[..f.len()].copy_from_slice(&f);
                Ok(f.len())
            }
            None => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        self.sent.lock().unwrap().push(frame.to_vec());
        Ok(())
    }
}

/// An Ethernet frame with a 12-byte header in front.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; HDR_LEN];
    f.extend_from_slice(&[0xff; 6]); // destination: broadcast
    f.extend_from_slice(&[2, 0, 0, 0, 0, 1]); // source
    f.extend_from_slice(&[0x88, 0xb5]); // EtherType: local
    f.extend_from_slice(payload);
    f
}

#[test]
fn net_transmit_and_receive() {
    let mem = memory();
    let rxq = MockSplitQueue::create(&mem, GuestAddress(0), 16);
    let txq =
        MockSplitQueue::create(&mem, GuestAddress(0x4000), 16);
    let wire = FakeWire::default();
    let queues = vec![
        rxq.create_queue().unwrap(),
        txq.create_queue().unwrap(),
    ];
    let mut dev = NetQueues::new(wire.clone(), queues);

    // Transmit: header and frame in two readable buffers.
    let out = frame(b"hello");
    mem.write_slice(&out[..HDR_LEN], GuestAddress(0x8000))
        .unwrap();
    mem.write_slice(&out[HDR_LEN..], GuestAddress(0x9000))
        .unwrap();
    let rest = (out.len() - HDR_LEN) as u32;
    txq.add_desc_chains(
        &[
            desc(0x8000, HDR_LEN as u32, NEXT, 1),
            desc(0x9000, rest, 0, 0),
        ],
        0,
    )
    .unwrap();
    assert!(dev.transmit(&mem));
    assert_eq!(*wire.sent.lock().unwrap(), vec![out]);
    assert_eq!(used(&txq, 0), (0, 0));

    // Receive with no buffer posted: the frame waits.
    let inc = frame(b"world");
    wire.incoming.lock().unwrap().push_back(inc.clone());
    assert!(!dev.receive(&mem));
    assert_eq!(rxq.used().idx().load(), 0);

    // The driver posts one 2 KiB buffer and kicks the rx queue.
    rxq.add_desc_chains(&[desc(0xc000, 2048, WRITE, 0)], 0)
        .unwrap();
    assert!(dev.receive(&mem));
    assert_eq!(used(&rxq, 0), (0, inc.len() as u32));
    let mut got = vec![0u8; inc.len()];
    mem.read_slice(&mut got, GuestAddress(0xc000)).unwrap();
    // The device set num_buffers (bytes 10-11) to 1.
    assert_eq!(&got[10..12], &[1, 0]);
    assert_eq!(&got[12..], &inc[12..]);
}

// ---- the transport ---------------------------------------------

/// A device that only records that it was activated.
struct Probe {
    activated: Arc<Mutex<Option<usize>>>,
}

impl VirtioDevice for Probe {
    fn device_type(&self) -> u32 {
        2
    }
    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &[16]
    }
    fn read_config(&self, _: u64, data: &mut [u8]) {
        data.fill(0x5a);
    }
    fn activate(
        &mut self,
        _: GuestMemoryMmap,
        q: Vec<Queue>,
        _: Vec<EventFd>,
        _: Arc<Interrupt>,
    ) {
        *self.activated.lock().unwrap() = Some(q.len());
    }
}

fn rd(t: &MmioTransport, off: u64) -> u32 {
    let mut b = [0u8; 4];
    t.read(off, &mut b);
    u32::from_le_bytes(b)
}

fn wr(t: &mut MmioTransport, off: u64, v: u32) {
    t.write(off, &v.to_le_bytes());
}

/// The driver's set-up sequence, as Linux's virtio_mmio does it.
#[test]
fn transport_handshake() {
    let mem = memory();
    let mq = MockSplitQueue::new(&mem, 16);
    let seen = Arc::new(Mutex::new(None));
    let probe = Probe {
        activated: seen.clone(),
    };
    let irq = EventFd::new(0).unwrap();
    let mut t =
        MmioTransport::new(Box::new(probe), mem.clone(), irq)
            .unwrap();

    assert_eq!(rd(&t, MAGIC_VALUE), 0x7472_6976);
    assert_eq!(rd(&t, VERSION), 2);
    assert_eq!(rd(&t, DEVICE_ID), 2);
    wr(&mut t, STATUS, 0); // reset
    wr(&mut t, STATUS, ACKNOWLEDGE);
    wr(&mut t, STATUS, ACKNOWLEDGE | DRIVER);
    wr(&mut t, DEVICE_FEATURES_SEL, 1);
    assert_eq!(rd(&t, DEVICE_FEATURES), 1); // bit 32
    wr(&mut t, DRIVER_FEATURES_SEL, 1);
    wr(&mut t, DRIVER_FEATURES, 1);
    wr(&mut t, DRIVER_FEATURES_SEL, 0);
    wr(&mut t, DRIVER_FEATURES, 0);
    let s = ACKNOWLEDGE | DRIVER | FEATURES_OK;
    wr(&mut t, STATUS, s);
    assert_eq!(rd(&t, STATUS) & FEATURES_OK, FEATURES_OK);

    wr(&mut t, QUEUE_SEL, 0);
    assert_eq!(rd(&t, QUEUE_READY), 0);
    assert_eq!(rd(&t, QUEUE_NUM_MAX), 16);
    wr(&mut t, QUEUE_NUM, 16);
    let a = |g: GuestAddress| g.0 as u32;
    wr(&mut t, QUEUE_DESC_LOW, a(mq.desc_table_addr()));
    wr(&mut t, QUEUE_DESC_HIGH, 0);
    wr(&mut t, QUEUE_DRIVER_LOW, a(mq.avail_addr()));
    wr(&mut t, QUEUE_DRIVER_HIGH, 0);
    wr(&mut t, QUEUE_DEVICE_LOW, a(mq.used_addr()));
    wr(&mut t, QUEUE_DEVICE_HIGH, 0);
    wr(&mut t, QUEUE_READY, 1);
    assert_eq!(*seen.lock().unwrap(), None);
    wr(&mut t, STATUS, s | DRIVER_OK);
    assert_eq!(*seen.lock().unwrap(), Some(1));
    assert_eq!(rd(&t, STATUS) & FAILED, 0);
    assert_eq!(rd(&t, CONFIG), 0x5a5a_5a5a);
}

/// A driver that asks for a feature the device never offered
/// gets FEATURES_OK cleared: "refused".
#[test]
fn transport_refuses_unknown_features() {
    let mem = memory();
    let probe = Probe {
        activated: Arc::default(),
    };
    let irq = EventFd::new(0).unwrap();
    let mut t =
        MmioTransport::new(Box::new(probe), mem, irq).unwrap();
    wr(&mut t, STATUS, ACKNOWLEDGE | DRIVER);
    wr(&mut t, DRIVER_FEATURES_SEL, 1);
    wr(&mut t, DRIVER_FEATURES, 1);
    wr(&mut t, DRIVER_FEATURES_SEL, 0);
    wr(&mut t, DRIVER_FEATURES, 1 << 9); // never offered
    wr(&mut t, STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK);
    assert_eq!(rd(&t, STATUS) & FEATURES_OK, 0);
}

/// Without an ioeventfd, the VMM passes the kick on itself:
/// a QueueNotify write of `i` signals queue i's eventfd.
#[test]
fn notify_without_ioeventfd() {
    let mem = memory();
    let probe = Probe {
        activated: Arc::default(),
    };
    let irq = EventFd::new(0).unwrap();
    let mut t =
        MmioTransport::new(Box::new(probe), mem, irq).unwrap();
    wr(&mut t, QUEUE_NOTIFY, 0);
    wr(&mut t, QUEUE_NOTIFY, 0);
    assert_eq!(t.kick_fd(0).read().unwrap(), 2);
}
