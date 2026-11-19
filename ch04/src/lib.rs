//! Chapter 4: virtio devices for the VMM of chapter 3.
//!
//! * [`virtio`]: the virtio-mmio transport (registers, feature
//!   negotiation, queue set-up, interrupt status).
//! * [`blk`]: virtio-blk, backed by a host file.
//! * [`net`]: virtio-net, backed by a host tap device ([`tap`]).
//! * [`boot`], [`machine`]: chapter 3's PVH boot and KVM set-up,
//!   copied so that this directory builds on its own.

pub mod blk;
pub mod boot;
pub mod machine;
pub mod net;
pub mod tap;
pub mod virtio;
