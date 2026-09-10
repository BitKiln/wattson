//! Error types.
//!
//! These are typed enums rather than `anyhow::Error` because they have to cross two
//! boundaries intact: the CLI's exit-code mapping, and a future Tauri IPC boundary where the
//! UI needs to distinguish "no device" from "device rejected the rate" from "capture file is
//! truncated". `anyhow` belongs in `main`, and nowhere else.

use std::path::PathBuf;

use thiserror::Error;

/// Failure parsing a human-written quantity.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum UnitError {
    #[error("expected a {what}, got an empty string")]
    Empty { what: &'static str },

    #[error("could not read a number from {what} {input:?}")]
    BadNumber { what: &'static str, input: String },

    #[error("{what} {input:?} has no unit; write something like 260uJ or 1.5mA")]
    MissingUnit { what: &'static str, input: String },

    #[error("{what} {input:?} has an unrecognised unit")]
    UnknownUnit { what: &'static str, input: String },

    #[error("{what} {input:?} is out of range")]
    OutOfRange { what: &'static str, input: String },
}

/// Failure resolving or parsing a device URI.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum UriError {
    #[error(
        "device {0:?} is not a recognised form; expected serial:PORT, tcp://HOST:PORT, sim://PROFILE, file://PATH, or auto"
    )]
    Unrecognised(String),

    #[error("device {uri:?} has a malformed {field}: {detail}")]
    BadField {
        uri: String,
        field: &'static str,
        detail: String,
    },

    #[error("device {0:?} names an unknown simulator profile")]
    UnknownProfile(String),
}

/// Something went wrong on the byte pipe to a device.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("no profiler device found")]
    NoDevice,

    #[error("more than one device is connected; name one explicitly with --device")]
    Ambiguous { candidates: Vec<String> },

    #[error("could not open {target}: {source}")]
    Open {
        target: String,
        #[source]
        source: std::io::Error,
    },

    #[error("the device disconnected")]
    Disconnected,

    #[error("transport I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("serial port error: {0}")]
    Serial(String),

    #[error(transparent)]
    Uri(#[from] UriError),
}

/// A device-time problem.
///
/// None of these are recoverable by guessing. A wrap the host got wrong is a 71-minute jump in
/// the data, and a jump in the data is a wrong energy figure that still looks plausible.
#[derive(Copy, Clone, Debug, Error, PartialEq, Eq)]
pub enum TimeError {
    #[error(
        "device timestamp jumped forward implausibly, from {from} to {to} ticks; \
         the wrap epoch can no longer be inferred"
    )]
    ImplausibleGap { from: u32, to: u32 },

    #[error("device timestamp went backwards, from {from} to {to} ticks")]
    BackwardsTime { from: u32, to: u32 },

    #[error("timer wrap disagreement: host inferred {inferred} wraps, device reported {reported}")]
    WrapMismatch { inferred: u16, reported: u16 },

    #[error("device reported a timer frequency of 0 Hz")]
    ZeroTimerRate,
}

/// A session-level failure: handshake, configuration, or streaming.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Transport(#[from] TransportError),

    #[error(transparent)]
    Time(#[from] TimeError),

    #[error("timed out waiting for {what} after {timeout_ms} ms")]
    Timeout { what: &'static str, timeout_ms: u64 },

    #[error("device speaks protocol 0x{device:04X}, this build speaks 0x{host:04X}")]
    ProtocolMismatch { device: u16, host: u16 },

    #[error(
        "device supports at most {max_hz} Hz, but {requested_hz} Hz was requested; \
         it would drop samples rather than meet it"
    )]
    RateTooHigh { requested_hz: u32, max_hz: u32 },

    #[error("device reported an error: code 0x{code:04X}, detail 0x{detail:04X}")]
    Device { code: u16, detail: u16 },

    #[error("the capture sink failed: {0}")]
    Sink(#[from] SinkError),

    #[error("no device info; call handshake() before configure()")]
    NotHandshaked,
}

/// A capture sink rejected a batch of records.
#[derive(Debug, Error)]
pub enum SinkError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error("sink I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Reading or writing a `.pprof` capture file.
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("{path} is not a wattson capture: bad magic")]
    NotACapture { path: PathBuf },

    #[error(
        "{path} was written by capture format v{major}.{minor}; this build reads v{supported_major}.x"
    )]
    IncompatibleVersion {
        path: PathBuf,
        major: u16,
        minor: u16,
        supported_major: u16,
    },

    #[error("{path} has a corrupt {what} (CRC mismatch)")]
    Corrupt { path: PathBuf, what: &'static str },

    #[error("{path} was truncated at byte {offset}; {recovered_chunks} chunks were recovered")]
    Truncated {
        path: PathBuf,
        offset: u64,
        recovered_chunks: usize,
    },

    #[error("capture I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("compression {0} is not supported by this build")]
    UnsupportedCompression(u32),

    #[error("failed to {op} a chunk: {detail}")]
    Compression { op: &'static str, detail: String },

    #[error("capture metadata is malformed: {0}")]
    Metadata(String),

    #[error(transparent)]
    Time(#[from] TimeError),
}

/// A statistics computation could not produce a trustworthy answer.
#[derive(Debug, Error)]
pub enum StatsError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error("the requested span contains no samples")]
    EmptySpan,

    #[error(
        "the span crosses {count} gap(s) totalling {lost_ns} ns of missing data; \
         integrating across them would under-report energy. Pass --on-gap=skip to \
         accept a partial result."
    )]
    GapInSpan { count: usize, lost_ns: u64 },

    #[error("no event named {0:?} appears in this capture's metadata")]
    UnknownEvent(String),

    #[error("event {0:?} has no matching stop id declared, so its duration is undefined")]
    UnpairedEvent(String),
}

/// Exporting to CSV, JSON, or a format not yet built.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    Stats(#[from] StatsError),

    #[error("export I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialisation failed: {0}")]
    Serialize(String),

    #[error("{0} export is planned but not implemented yet")]
    NotImplemented(&'static str),
}
