//! A synthetic profiler device.
//!
//! This crate is why the rest of the project could be built before any measurement hardware
//! existed. It generates a realistic current and voltage stream with embedded firmware events
//! and speaks the real wire protocol, so the protocol, the capture format, the analysis
//! engine, and the CLI are all developed and tested end to end against it.
//!
//! It is deliberately *not* a stub. Constant voltage, square edges, and zero jitter would let
//! every one of those layers pass its tests while being wrong. See [`engine`] for the list of
//! bugs each realism feature exists to catch.

#![forbid(unsafe_code)]

pub mod device;
pub mod engine;
pub mod profiles;
pub mod rng;
pub mod server;

pub use device::{DEVICE_TYPE_SIMULATOR, SAMPLES_PER_BLOCK, SimDevice};
pub use engine::{FaultInjection, SimBlock, SimConfig, SimEngine};
pub use profiles::{PowerState, Profile};
pub use rng::SimRng;
pub use server::{ByteChannel, SimHandle, SimServer, run_device};
