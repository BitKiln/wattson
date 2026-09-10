//! The byte pipe to a device.
//!
//! Everything above this layer sees bytes and nothing else — no serial port, no socket, no
//! file. That is deliberate: the throughput escape hatch for phase 2 is to abandon USB
//! CDC-ACM for a WinUSB/vendor bulk interface, which is typically 2-3x faster on Windows.
//! Keeping this trait narrow means that change is a new implementation, not a refactor.
//!
//! Reads are **blocking with a timeout**, and a timeout returns `Ok(0)` rather than an error.
//! A profiler that is configured but not yet capturing is legitimately silent, and a silent
//! device must not look like a broken one.

use std::fmt;
use std::time::Duration;

use crate::error::TransportError;
use crate::uri::DeviceUri;

pub mod pipe;
pub mod serial;
pub mod tcp;

pub use pipe::PipeTransport;
pub use serial::SerialTransport;
pub use tcp::TcpTransport;

/// What a transport is connected to, for logging and capture metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportInfo {
    /// Short kind: `serial`, `tcp`, `pipe`, `file`.
    pub kind: &'static str,
    /// Human-readable target, e.g. `COM7@921600` or `127.0.0.1:9000`.
    pub target: String,
}

impl fmt::Display for TransportInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.target)
    }
}

/// A bidirectional byte pipe.
///
/// `Send` because capture runs the read loop on its own thread.
pub trait Transport: Send + fmt::Debug {
    /// Read available bytes.
    ///
    /// Returns `Ok(0)` on timeout — that is normal, not an error. Returns
    /// [`TransportError::Disconnected`] when the device is genuinely gone.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    /// Write every byte or fail.
    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError>;

    fn flush(&mut self) -> Result<(), TransportError>;

    /// Set how long [`Transport::read`] blocks before returning `Ok(0)`.
    fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), TransportError>;

    fn describe(&self) -> TransportInfo;

    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

/// A device found by enumeration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDescriptor {
    /// The URI that would open this device.
    pub uri: DeviceUri,
    /// Port name or address.
    pub name: String,
    /// USB vendor id, when the OS reports one.
    pub vid: Option<u16>,
    /// USB product id, when the OS reports one.
    pub pid: Option<u16>,
    /// USB serial string, when the OS reports one.
    pub serial: Option<String>,
    /// Free-form description from the OS.
    pub description: Option<String>,
    /// Whether this looks like a profiler rather than some other serial device.
    ///
    /// Enumeration cannot be certain without opening the port and shaking hands, so this is
    /// a hint for ordering and for `--device auto`, not a guarantee.
    pub likely_profiler: bool,
}

impl fmt::Display for DeviceDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(desc) = &self.description {
            write!(f, "  {desc}")?;
        }
        if let (Some(vid), Some(pid)) = (self.vid, self.pid) {
            write!(f, "  [{vid:04x}:{pid:04x}]")?;
        }
        if let Some(serial) = &self.serial {
            write!(f, "  serial={serial}")?;
        }
        Ok(())
    }
}

/// List candidate devices.
///
/// Blocking, and on Windows `SetupAPI` enumeration can take hundreds of milliseconds. Never
/// call this from a UI thread; the desktop app must wrap it in a task.
pub fn enumerate_devices() -> Result<Vec<DeviceDescriptor>, TransportError> {
    serial::enumerate()
}

/// Open a transport for a URI.
///
/// [`DeviceUri::Sim`] is not handled here — the simulator lives in `wattson-sim`, which
/// depends on this crate rather than the other way round. The CLI resolves `sim://` before
/// calling this.
pub fn open(uri: &DeviceUri) -> Result<Box<dyn Transport>, TransportError> {
    match uri {
        DeviceUri::Auto => {
            let mut found = enumerate_devices()?;
            found.retain(|d| d.likely_profiler);
            match found.len() {
                0 => Err(TransportError::NoDevice),
                1 => open(&found[0].uri.clone()),
                _ => Err(TransportError::Ambiguous {
                    candidates: found.into_iter().map(|d| d.name).collect(),
                }),
            }
        }
        DeviceUri::Serial { port, baud } => {
            Ok(Box::new(SerialTransport::open(port, *baud)?) as Box<dyn Transport>)
        }
        DeviceUri::Tcp(addr) => Ok(Box::new(TcpTransport::connect(*addr)?) as Box<dyn Transport>),
        DeviceUri::Sim(_) => Err(TransportError::Uri(crate::error::UriError::BadField {
            uri: uri.label(),
            field: "scheme",
            detail: "sim:// must be opened through wattson-sim, not core::transport".into(),
        })),
        DeviceUri::File(path) => Err(TransportError::Uri(crate::error::UriError::BadField {
            uri: path.display().to_string(),
            field: "scheme",
            detail: "capture replay is not implemented yet".into(),
        })),
    }
}
