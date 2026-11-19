//! virtio-blk: a disk backed by an ordinary host file.
//!
//! One queue (the "requestq"). Each request is a descriptor
//! chain: a 16-byte header the device reads, zero or more data
//! buffers, and a 1-byte status the device writes (spec 5.2.6).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::thread;

use virtio_queue::desc::split::Descriptor;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

use crate::virtio::{
    Interrupt, VirtioDevice, VIRTIO_F_VERSION_1,
};

pub const SECTOR: u64 = 512;

// Request types (spec 5.2.6).
pub const T_IN: u32 = 0; // read from the disk
pub const T_OUT: u32 = 1; // write to the disk
pub const T_FLUSH: u32 = 4;
pub const T_GET_ID: u32 = 8;

// Status values the device writes in the last byte.
pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

// Feature bits (spec 5.2.3).
const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;

/// Bytes in the serial number a GET_ID request returns.
const ID_LEN: usize = 20;

pub struct Block {
    /// The backing file, shared with the device thread.
    file: Arc<File>,
    /// Disk size in 512-byte sectors.
    sectors: u64,
    read_only: bool,
}

impl Block {
    pub fn new(
        file: File,
        read_only: bool,
    ) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Block {
            file: Arc::new(file),
            sectors: len / SECTOR,
            read_only,
        })
    }
}

impl VirtioDevice for Block {
    fn device_type(&self) -> u32 {
        2
    }

    fn features(&self) -> u64 {
        let ro = if self.read_only { F_RO } else { 0 };
        VIRTIO_F_VERSION_1 | F_FLUSH | ro
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[256]
    }

    /// The config space starts with `capacity`: a 64-bit,
    /// little-endian count of 512-byte sectors.
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.sectors.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let at = offset as usize + i;
            *b = *cfg.get(at).unwrap_or(&0);
        }
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        mut queues: Vec<Queue>,
        kicks: Vec<EventFd>,
        irq: Arc<Interrupt>,
    ) {
        let mut q = queues.remove(0);
        let kick = kicks.into_iter().next().unwrap();
        let mut disk = Disk {
            file: self.file.clone(),
            sectors: self.sectors,
            read_only: self.read_only,
        };
        thread::Builder::new()
            .name("virtio-blk".into())
            .spawn(move || loop {
                // Sleep until the guest kicks. Several kicks
                // while we were busy add up to one wake-up.
                if kick.read().is_err() {
                    return;
                }
                if disk.process_queue(&mem, &mut q) {
                    irq.signal_used();
                }
            })
            .expect("spawn virtio-blk thread");
    }
}

/// What the device thread owns: the file and its geometry.
pub struct Disk {
    pub file: Arc<File>,
    pub sectors: u64,
    pub read_only: bool,
}

impl Disk {
    /// Serve every request the driver has made available.
    /// Returns true if any went into the used ring, in which
    /// case the caller should interrupt the guest.
    pub fn process_queue(
        &mut self,
        mem: &GuestMemoryMmap,
        q: &mut Queue,
    ) -> bool {
        let mut used = false;
        while let Some(chain) = q.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<_> = chain.collect();
            let (status, len) = self.request(mem, &descs);
            // The status byte is always the last descriptor.
            if let Some(last) = descs.last() {
                let _ = mem.write_obj(status, last.addr());
            }
            // `len` counts the bytes the device wrote into
            // the guest's buffers, status byte included.
            if q.add_used(mem, head, len + 1).is_ok() {
                used = true;
            }
        }
        used && q.needs_notification(mem).unwrap_or(true)
    }

    /// Carry out one request. Returns the status and the number
    /// of data bytes written into guest memory.
    fn request(
        &mut self,
        mem: &GuestMemoryMmap,
        descs: &[Descriptor],
    ) -> (u8, u32) {
        // At least a header and a status, header readable
        // and 16 bytes long, status writable.
        let (hdr, rest) = match descs.split_first() {
            Some(x) if descs.len() >= 2 => x,
            _ => return (S_IOERR, 0),
        };
        let (st, data) = rest.split_last().unwrap();
        if hdr.is_write_only() || hdr.len() < 16 {
            return (S_IOERR, 0);
        }
        if !st.is_write_only() || st.len() < 1 {
            return (S_IOERR, 0);
        }
        let Ok(kind) = mem.read_obj::<u32>(hdr.addr()) else {
            return (S_IOERR, 0);
        };
        let at = hdr.addr().0 + 8;
        let Ok(sector) = mem.read_obj::<u64>(GuestAddress(at))
        else {
            return (S_IOERR, 0);
        };
        match u32::from_le(kind) {
            T_IN => self.read(mem, u64::from_le(sector), data),
            T_OUT if self.read_only => (S_IOERR, 0),
            T_OUT => self.write(mem, u64::from_le(sector), data),
            T_FLUSH => match self.file.sync_all() {
                Ok(()) => (S_OK, 0),
                Err(_) => (S_IOERR, 0),
            },
            T_GET_ID => self.get_id(mem, data),
            _ => (S_UNSUPP, 0),
        }
    }

    /// Is [sector, sector + bytes) inside the disk?
    fn in_range(&self, sector: u64, bytes: u64) -> bool {
        let end = sector
            .checked_mul(SECTOR)
            .and_then(|s| s.checked_add(bytes));
        matches!(end, Some(e) if e <= self.sectors * SECTOR)
    }

    /// IN: disk -> guest. Every data buffer must be writable.
    fn read(
        &mut self,
        mem: &GuestMemoryMmap,
        sector: u64,
        data: &[Descriptor],
    ) -> (u8, u32) {
        let total: u64 =
            data.iter().map(|d| d.len() as u64).sum();
        if data.iter().any(|d| !d.is_write_only())
            || !self.in_range(sector, total)
        {
            return (S_IOERR, 0);
        }
        let mut pos = sector * SECTOR;
        let mut done = 0u32;
        for d in data {
            let mut buf = vec![0u8; d.len() as usize];
            if self.file.read_exact_at(&mut buf, pos).is_err()
                || mem.write_slice(&buf, d.addr()).is_err()
            {
                return (S_IOERR, done);
            }
            pos += d.len() as u64;
            done += d.len();
        }
        (S_OK, done)
    }

    /// OUT: guest -> disk. Every data buffer must be readable.
    fn write(
        &mut self,
        mem: &GuestMemoryMmap,
        sector: u64,
        data: &[Descriptor],
    ) -> (u8, u32) {
        let total: u64 =
            data.iter().map(|d| d.len() as u64).sum();
        if data.iter().any(|d| d.is_write_only())
            || !self.in_range(sector, total)
        {
            return (S_IOERR, 0);
        }
        let mut pos = sector * SECTOR;
        for d in data {
            let mut buf = vec![0u8; d.len() as usize];
            if mem.read_slice(&mut buf, d.addr()).is_err()
                || self.file.write_all_at(&buf, pos).is_err()
            {
                return (S_IOERR, 0);
            }
            pos += d.len() as u64;
        }
        (S_OK, 0)
    }

    /// GET_ID: a serial number of up to 20 bytes.
    fn get_id(
        &mut self,
        mem: &GuestMemoryMmap,
        data: &[Descriptor],
    ) -> (u8, u32) {
        let Some(d) = data.first() else {
            return (S_IOERR, 0);
        };
        let mut id = [0u8; ID_LEN];
        id[..8].copy_from_slice(b"vmm4-blk");
        let n = (d.len() as usize).min(ID_LEN);
        if !d.is_write_only()
            || mem.write_slice(&id[..n], d.addr()).is_err()
        {
            return (S_IOERR, 0);
        }
        (S_OK, n as u32)
    }
}
