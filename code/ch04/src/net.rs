//! virtio-net: a network card whose cable is a host tap device.
//!
//! Two queues (spec 5.1.2): queue 0, the receiveq, holds empty
//! buffers the driver lends the device for incoming frames;
//! queue 1, the transmitq, holds frames the guest sends. Each
//! frame is preceded by a 12-byte `virtio_net_hdr`.

use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::thread;

use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};
use vmm_sys_util::epoll::{
    ControlOperation, Epoll, EpollEvent, EventSet,
};
use vmm_sys_util::eventfd::EventFd;

use crate::tap::Tap;
use crate::virtio::{
    Interrupt, VirtioDevice, VIRTIO_F_VERSION_1,
};

/// The header in front of every frame, with VERSION_1.
pub const HDR_LEN: usize = 12;
/// A full-size Ethernet frame plus the header. Without the
/// offload features, frames are never larger.
pub const MAX_FRAME: usize = HDR_LEN + 1514;

/// Feature bit 5: the config space holds a MAC address.
const F_MAC: u64 = 1 << 5;

pub const RX: usize = 0;
pub const TX: usize = 1;

/// Where frames go to and come from. The tap device in the
/// VMM, a pair of queues in the unit tests.
pub trait Wire: Send {
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn send(&mut self, frame: &[u8]) -> io::Result<()>;
}

impl Wire for Tap {
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_frame(buf)
    }
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        self.write_frame(frame)
    }
}

pub struct Net {
    mac: [u8; 6],
    tap: Option<Tap>,
}

impl Net {
    pub fn new(tap: Tap, mac: [u8; 6]) -> Self {
        Net {
            mac,
            tap: Some(tap),
        }
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        1
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | F_MAC
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[256, 256]
    }

    /// The config space starts with the 6-byte MAC address.
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            let at = offset as usize + i;
            *b = *self.mac.get(at).unwrap_or(&0);
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        queues: Vec<Queue>,
        kicks: Vec<EventFd>,
        irq: Arc<Interrupt>,
    ) {
        let tap = self.tap.take().expect("activated twice");
        let fd = tap.as_raw_fd();
        let mut dev = NetQueues::new(tap, queues);
        thread::Builder::new()
            .name("virtio-net".into())
            .spawn(move || {
                if let Err(e) =
                    serve(&mut dev, &mem, fd, &kicks, &irq)
                {
                    eprintln!("virtio-net thread: {e}");
                }
            })
            .expect("spawn virtio-net thread");
    }
}

// What woke the device thread up, as epoll's `data` field.
const EV_RX_KICK: u64 = 0;
const EV_TX_KICK: u64 = 1;
const EV_TAP: u64 = 2;

/// The device thread: wait on three file descriptors at once.
fn serve<W: Wire>(
    dev: &mut NetQueues<W>,
    mem: &GuestMemoryMmap,
    tap_fd: i32,
    kicks: &[EventFd],
    irq: &Interrupt,
) -> io::Result<()> {
    let ep = Epoll::new()?;
    let add = |fd, set, data| {
        let ev = EpollEvent::new(set, data);
        ep.ctl(ControlOperation::Add, fd, ev)
    };
    add(kicks[RX].as_raw_fd(), EventSet::IN, EV_RX_KICK)?;
    add(kicks[TX].as_raw_fd(), EventSet::IN, EV_TX_KICK)?;
    // Edge-triggered: one event when frames start to arrive.
    // We then read until the tap says WouldBlock.
    add(tap_fd, EventSet::IN | EventSet::EDGE_TRIGGERED, EV_TAP)?;
    let mut events = vec![EpollEvent::default(); 4];
    loop {
        let n = match ep.wait(-1, &mut events) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                continue
            }
            Err(e) => return Err(e),
        };
        let mut notify = false;
        for ev in &events[..n] {
            match ev.data() {
                EV_TX_KICK => {
                    let _ = kicks[TX].read();
                    notify |= dev.transmit(mem);
                }
                // New rx buffers: a frame may have been waiting.
                EV_RX_KICK => {
                    let _ = kicks[RX].read();
                    notify |= dev.receive(mem);
                }
                _ => notify |= dev.receive(mem),
            }
        }
        if notify {
            irq.signal_used();
        }
    }
}

/// The two queues and the wire, with no threads or fds, so that
/// the unit tests can drive them directly.
pub struct NetQueues<W> {
    pub wire: W,
    pub queues: Vec<Queue>,
    /// A frame read from the wire that found no rx buffer yet.
    pending: Option<Vec<u8>>,
}

impl<W: Wire> NetQueues<W> {
    pub fn new(wire: W, queues: Vec<Queue>) -> Self {
        NetQueues {
            wire,
            queues,
            pending: None,
        }
    }

    /// Send every frame on the transmitq. Returns true if the
    /// guest should be interrupted.
    pub fn transmit(&mut self, mem: &GuestMemoryMmap) -> bool {
        let q = &mut self.queues[TX];
        let mut used = false;
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            // Gather the header and frame from the chain's
            // readable buffers.
            let mut frame = Vec::with_capacity(MAX_FRAME);
            for d in chain {
                if d.is_write_only() {
                    continue;
                }
                let start = frame.len();
                frame.resize(start + d.len() as usize, 0);
                let dst = &mut frame[start..];
                if mem.read_slice(dst, d.addr()).is_err() {
                    frame.truncate(start);
                }
            }
            if frame.len() > HDR_LEN {
                let _ = self.wire.send(&frame);
            }
            // The device wrote nothing into this chain: len 0.
            if q.add_used(mem, head, 0).is_ok() {
                used = true;
            }
        }
        used && q.needs_notification(mem).unwrap_or(true)
    }

    /// Move frames from the wire into rx buffers until one of
    /// the two runs out. Returns true if the guest should be
    /// interrupted.
    pub fn receive(&mut self, mem: &GuestMemoryMmap) -> bool {
        let mut used = false;
        let mut buf = vec![0u8; MAX_FRAME];
        loop {
            let frame = match self.pending.take() {
                Some(f) => f,
                None => match self.wire.recv(&mut buf) {
                    Ok(n) if n > HDR_LEN => buf[..n].to_vec(),
                    Ok(_) => continue, // runt: drop it
                    Err(_) => break,   // WouldBlock: tap is empty
                },
            };
            let q = &mut self.queues[RX];
            let Some(chain) = q.pop_descriptor_chain(mem) else {
                // No buffer: keep the frame until the driver
                // adds some and kicks the receiveq.
                self.pending = Some(frame);
                break;
            };
            let head = chain.head_index();
            let mut frame = frame;
            // num_buffers: this frame fills exactly one chain.
            frame[10..12].copy_from_slice(&1u16.to_le_bytes());
            let mut off = 0;
            for d in chain {
                if !d.is_write_only() || off == frame.len() {
                    continue;
                }
                let n = (d.len() as usize).min(frame.len() - off);
                let src = &frame[off..off + n];
                if mem.write_slice(src, d.addr()).is_err() {
                    break;
                }
                off += n;
            }
            if q.add_used(mem, head, off as u32).is_ok() {
                used = true;
            }
        }
        let q = &mut self.queues[RX];
        used && q.needs_notification(mem).unwrap_or(true)
    }
}
