//! Serial transport, over USB CDC-ACM in practice.
//!
//! # Windows
//!
//! Two Windows-specific facts shape this file, and both cost real throughput if ignored:
//!
//! 1. The default `COMMTIMEOUTS` and driver FIFO trigger levels add 10-16 ms of latency and
//!    can cap throughput below the line rate. The timeout is therefore set explicitly rather
//!    than left to the driver.
//! 2. `SetupAPI` enumeration can take hundreds of milliseconds. [`enumerate`] is a plain
//!    blocking function; the desktop app must wrap it in a task rather than call it from a
//!    UI thread.

use std::fmt;
use std::io::{Read, Write};
use std::time::Duration;

use serialport::{SerialPort, SerialPortType};

use super::{DeviceDescriptor, Transport, TransportInfo};
use crate::error::TransportError;
use crate::uri::DeviceUri;

/// USB VID/PID pairs that identify a profiler.
///
/// The RP2040 entries are Raspberry Pi's generic CDC ids, which any Pico running TinyUSB
/// reports — hence a *hint*, not proof. Certainty requires a handshake.
const KNOWN_IDS: &[(u16, u16)] = &[
    (0x2E8A, 0x000A), // Raspberry Pi, Pico SDK CDC
    (0x2E8A, 0x0003), // Raspberry Pi, RP2 boot
    (0x1209, 0x5741), // pid.codes, allocated for wattson
];

/// A byte pipe over a serial port.
pub struct SerialTransport {
    port: Box<dyn SerialPort>,
    name: String,
    baud: u32,
}

impl fmt::Debug for SerialTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerialTransport")
            .field("name", &self.name)
            .field("baud", &self.baud)
            .finish()
    }
}

impl SerialTransport {
    pub fn open(port_name: &str, baud: u32) -> Result<SerialTransport, TransportError> {
        let port = serialport::new(port_name, baud)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| TransportError::Open {
                target: format!("{port_name}@{baud}"),
                source: std::io::Error::other(e.to_string()),
            })?;

        let mut t = SerialTransport {
            port,
            name: port_name.to_string(),
            baud,
        };
        // Best-effort: not every backend supports these, and none of them is fatal.
        let _ = t.port.set_data_bits(serialport::DataBits::Eight);
        let _ = t.port.set_parity(serialport::Parity::None);
        let _ = t.port.set_stop_bits(serialport::StopBits::One);
        let _ = t.port.set_flow_control(serialport::FlowControl::None);
        let _ = t.port.clear(serialport::ClearBuffer::All);
        // The driver's receive buffer size is not settable through `serialport`. At 400 KB/s
        // a 4 KB default drains every 10 ms, so a scheduling hiccup longer than that costs
        // samples. If that shows up in measurement, raising it needs a platform-specific
        // call here (`SetupComm` on Windows) rather than a portable one.
        Ok(t)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Transport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                // Normal: the device is configured but not streaming.
                Ok(0)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::NotConnected
                        | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                // A yanked USB cable shows up as one of these, platform depending.
                Err(TransportError::Disconnected)
            }
            Err(e) => Err(TransportError::Io(e)),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.port.write_all(buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected => {
                TransportError::Disconnected
            }
            _ => TransportError::Io(e),
        })
    }

    fn flush(&mut self) -> Result<(), TransportError> {
        self.port.flush()?;
        Ok(())
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.port
            .set_timeout(timeout)
            .map_err(|e| TransportError::Serial(e.to_string()))
    }

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            kind: "serial",
            target: format!("{}@{}", self.name, self.baud),
        }
    }
}

/// Enumerate serial ports, flagging the ones that look like a profiler.
///
/// Blocking, and slow on Windows. Never call from a UI thread.
pub fn enumerate() -> Result<Vec<DeviceDescriptor>, TransportError> {
    let ports = serialport::available_ports().map_err(|e| TransportError::Serial(e.to_string()))?;

    let mut out = Vec::with_capacity(ports.len());
    for p in ports {
        let (vid, pid, serial, description) = match &p.port_type {
            SerialPortType::UsbPort(info) => (
                Some(info.vid),
                Some(info.pid),
                info.serial_number.clone(),
                info.product.clone().or_else(|| info.manufacturer.clone()),
            ),
            SerialPortType::BluetoothPort => (None, None, None, Some("Bluetooth".to_string())),
            SerialPortType::PciPort => (None, None, None, Some("PCI".to_string())),
            SerialPortType::Unknown => (None, None, None, None),
        };

        let likely_profiler = match (vid, pid) {
            (Some(v), Some(p)) => KNOWN_IDS.contains(&(v, p)),
            _ => false,
        } || description
            .as_deref()
            .is_some_and(|d| d.to_ascii_lowercase().contains("wattson"));

        out.push(DeviceDescriptor {
            uri: DeviceUri::Serial {
                port: p.port_name.clone(),
                baud: crate::uri::DEFAULT_BAUD,
            },
            name: p.port_name,
            vid,
            pid,
            serial,
            description,
            likely_profiler,
        });
    }

    // Likely profilers first, then by name, so `wattson devices` output is stable and
    // `--device auto` picks predictably.
    out.sort_by(|a, b| {
        b.likely_profiler
            .cmp(&a.likely_profiler)
            .then(a.name.cmp(&b.name))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enumeration must succeed on a machine with no serial ports at all, which is the
    /// normal state of a CI runner.
    #[test]
    fn enumeration_succeeds_even_with_no_devices() {
        let found =
            enumerate().expect("enumeration must not fail merely because nothing is attached");
        // Ordering contract: likely profilers first.
        let first_unlikely = found.iter().position(|d| !d.likely_profiler);
        if let Some(idx) = first_unlikely {
            assert!(
                found[idx..].iter().all(|d| !d.likely_profiler),
                "likely profilers must sort before everything else"
            );
        }
    }

    #[test]
    fn opening_a_nonexistent_port_names_the_target() {
        match SerialTransport::open("COM_DOES_NOT_EXIST_9999", 921_600) {
            Err(TransportError::Open { target, .. }) => {
                assert!(target.contains("COM_DOES_NOT_EXIST_9999"));
                assert!(target.contains("921600"));
            }
            Ok(_) => panic!("a port named COM_DOES_NOT_EXIST_9999 should not exist"),
            Err(other) => panic!("expected an Open error, got {other:?}"),
        }
    }
}
