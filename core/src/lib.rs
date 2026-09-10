//! Wattson core: everything the CLI and the desktop app share.
//!
//! # The rule that shapes this crate
//!
//! **`core` never prints, never reads argv, and never exits.** Every operation returns typed
//! data or a typed error. That is what lets `cli/src/cmd/analyze.rs` and a future
//! `#[tauri::command]` call the same function and get the same answer, instead of the project
//! growing two subtly different analysis engines.
//!
//! There is also no async runtime here. A blocking read on a dedicated thread handles 400 KB/s
//! without effort, and a `tokio` dependency would propagate into the CLI and collide with
//! Tauri 2's own runtime.
//!
//! # Layout
//!
//! - [`units`] — physical quantities and the parser the CLI and GUI both use.
//! - [`time`] — device ticks, wrap unwrapping, host/device clock fitting. Read its module
//!   docs before touching anything that computes a duration.
//! - [`uri`] — one string names a serial device, a TCP simulator, or a stored capture.
//! - [`transport`] — the byte pipe, and its implementations.
//! - [`capture`] — the `.pprof` file format: writer, reader, and truncation recovery.
//! - [`metadata`] — what the opaque event ids on the wire actually mean.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod capture;
pub mod error;
pub mod metadata;
pub mod time;
pub mod transport;
pub mod units;
pub mod uri;

pub use capture::{
    CaptureHeader, CaptureReader, CaptureSummary, CaptureWriter, Compression, Gap, GapCause,
    GapPolicy, WriterOptions,
};
pub use error::{
    CaptureError, ExportError, SessionError, SinkError, StatsError, TimeError, TransportError,
    UnitError, UriError,
};
pub use metadata::{CaptureMetadata, EventDef, EventMap};
pub use time::{
    CaptureTime, ClockFit, DeviceTicks, DeviceTime, SyncSample, TickUnwrapper, TimeBase, TimeSpan,
    fit_clocks,
};
pub use transport::{DeviceDescriptor, PipeTransport, TcpTransport, Transport, TransportInfo};
pub use units::{Charge, Current, Duration, Energy, Power, Ratio, Resistance, SampleRate, Voltage};
pub use uri::{DeviceUri, SimProfile, SimSpec};

/// Re-exported so downstream crates need not depend on `wattson-protocol` directly.
pub use wattson_protocol as protocol;

/// This crate's version, recorded in every capture so a file can be traced to the build that
/// wrote it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
