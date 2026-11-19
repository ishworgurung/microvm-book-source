//! A tap device: a network interface on the host whose "wire"
//! is a file descriptor. Every Ethernet frame the host sends to
//! the interface can be read from the fd, and every frame
//! written to the fd appears on the interface.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};

// From <linux/if_tun.h>.
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const IFF_VNET_HDR: libc::c_short = 0x4000;
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

/// `struct ifreq`, cut down to the two fields TUNSETIFF uses.
#[repr(C)]
struct IfReq {
    name: [u8; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

pub struct Tap {
    file: File,
}

impl Tap {
    /// Attach to the tap interface `name`, which must already
    /// exist (`ip tuntap add NAME mode tap`).
    ///
    /// IFF_VNET_HDR: every frame starts with the same 12-byte
    /// header that virtio-net uses, so frames pass between the
    /// virtqueue and the tap without being rewritten.
    pub fn open(name: &str) -> io::Result<Tap> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/net/tun")?;
        let mut req = IfReq {
            name: [0; 16],
            flags: IFF_TAP | IFF_NO_PI | IFF_VNET_HDR,
            _pad: [0; 22],
        };
        let n = name.len().min(15);
        req.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        let fd = file.as_raw_fd();
        // SAFETY: both ioctls read a struct we own and that
        // outlives the call.
        unsafe {
            if libc::ioctl(fd, TUNSETIFF, &mut req) < 0 {
                return Err(io::Error::last_os_error());
            }
            let size: libc::c_int = 12;
            if libc::ioctl(fd, TUNSETVNETHDRSZ, &size) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Tap { file })
    }

    /// One frame, header included. WouldBlock when none waits.
    pub fn read_frame(
        &mut self,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        self.file.read(buf)
    }

    /// One frame, header included.
    pub fn write_frame(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file.write(buf).map(|_| ())
    }
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
