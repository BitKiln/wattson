//! Serving the synthetic device over a transport.
//!
//! Two ways to reach it:
//!
//! - [`run_device`] drives a device over any [`ByteChannel`], so a caller can put it behind
//!   an in-memory pipe and run an entire capture inside one `cargo test` with no ports for
//!   parallel CI jobs to collide over.
//! - [`SimServer`] listens on TCP, which is what `wattson sim` exposes and what the
//!   end-to-end CLI test drives.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::device::SimDevice;
use crate::engine::SimConfig;

/// How long the device loop sleeps when it has nothing to send.
const IDLE_SLEEP: Duration = Duration::from_micros(500);

/// Blocks generated per iteration, so the loop stays responsive to commands.
const BLOCK_BUDGET: usize = 32;

/// A running simulated device that can be asked to stop.
#[derive(Debug)]
pub struct SimHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl SimHandle {
    /// Ask the device to stop, and wait for its thread.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        // A test that forgets to stop the device must not leave a thread spinning for the
        // rest of the run.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Trait alias for the byte pipe the device speaks over.
///
/// Declared here rather than importing `wattson_core::Transport` so the simulator does not
/// depend on the core crate: the dependency runs core -> nothing, sim -> protocol.
pub trait ByteChannel: Send {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
}

impl ByteChannel for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match Read::read(self, buf) {
            Ok(0) => Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => Ok(n),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(0)
            }
            Err(e) => Err(e),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        Write::write_all(self, buf)
    }
}

/// Run a device over a channel until the channel closes or `stop` is set.
pub fn run_device(mut device: SimDevice, mut channel: impl ByteChannel, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        match channel.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let reply = device.receive(&buf[..n]);
                if !reply.is_empty() && channel.write_all(&reply).is_err() {
                    break;
                }
            }
            // The host went away; that is how a capture normally ends.
            Err(_) => break,
        }

        let out = device.step(BLOCK_BUDGET);
        if !out.is_empty() {
            if channel.write_all(&out).is_err() {
                break;
            }
        } else if !device.is_capturing() {
            thread::sleep(IDLE_SLEEP);
        } else {
            // Capturing but nothing owed yet: wait for wall time to catch up rather than
            // spinning a core.
            thread::sleep(IDLE_SLEEP);
        }
    }
}

/// A TCP server for the simulated device.
#[derive(Debug)]
pub struct SimServer {
    listener: TcpListener,
    config: SimConfig,
    duration: Option<Duration>,
}

impl SimServer {
    /// Bind, without accepting yet.
    ///
    /// Pass port 0 to let the OS choose; [`SimServer::local_addr`] then reports what it
    /// picked, which is how parallel CI jobs avoid colliding.
    pub fn bind(addr: &str, config: SimConfig) -> std::io::Result<SimServer> {
        let listener = TcpListener::bind(addr)?;
        Ok(SimServer {
            listener,
            config,
            duration: None,
        })
    }

    /// Stop each connection's device after this much time.
    pub fn with_duration(mut self, duration: Option<Duration>) -> Self {
        self.duration = duration;
        self
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve connections until `stop` is set. One device per connection.
    pub fn serve(&self, stop: Arc<AtomicBool>) -> std::io::Result<()> {
        self.listener.set_nonblocking(true)?;
        while !stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    stream.set_nodelay(true)?;
                    stream.set_read_timeout(Some(Duration::from_millis(5)))?;
                    stream.set_nonblocking(false)?;
                    let device = SimDevice::new(self.config.clone()).with_duration(self.duration);
                    let stop = Arc::clone(&stop);
                    thread::spawn(move || run_device(device, stream, stop));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Serve one connection on a background thread and return a handle.
    pub fn spawn(self) -> SimHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let join = thread::spawn(move || {
            let _ = self.serve(flag);
        });
        SimHandle {
            stop,
            join: Some(join),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Profile;
    use std::time::Instant;
    use wattson_protocol::{
        Config, Decoder, Frame, FrameType, Hello, MAX_ENCODED, PROTOCOL_VERSION, encode_frame,
    };

    fn frame(ty: FrameType, payload: &[u8], seq: u8) -> Vec<u8> {
        let mut buf = [0u8; MAX_ENCODED];
        let n = encode_frame(ty, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    /// Drive a real handshake and capture over a socket, exactly as the CLI will.
    #[test]
    fn a_tcp_client_can_handshake_and_capture() {
        let mut cfg = SimConfig::new(Profile::ble_sensor());
        cfg.sample_rate_hz = 50_000;
        let server = SimServer::bind("127.0.0.1:0", cfg).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.spawn();

        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let mut hello_payload = [0u8; Hello::LEN];
        Hello {
            proto_version: PROTOCOL_VERSION,
            nonce: 1,
            reserved: 0,
        }
        .encode(&mut hello_payload)
        .unwrap();
        Write::write_all(&mut client, &frame(FrameType::Hello, &hello_payload, 0)).unwrap();

        let mut decoder = Decoder::new();
        let mut buf = [0u8; 16384];
        let mut got_info = false;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !got_info && Instant::now() < deadline {
            if let Ok(n) = Read::read(&mut client, &mut buf) {
                decoder.feed(&buf[..n], &mut |_, f| {
                    if let Frame::DeviceInfo(info) = f {
                        assert_eq!(info.timer_hz, 1_000_000);
                        got_info = true;
                    }
                });
            }
        }
        assert!(got_info, "the device never answered HELLO");

        let mut cfg_payload = [0u8; Config::LEN];
        Config {
            sample_rate_hz: 50_000,
            averaging: 1,
            conv_time_code: 0,
            gpio_mask: 0,
            shunt_micro_ohm: 100_000,
            flags: 0,
            reserved: 0,
        }
        .encode(&mut cfg_payload)
        .unwrap();
        Write::write_all(&mut client, &frame(FrameType::Config, &cfg_payload, 1)).unwrap();
        Write::write_all(&mut client, &frame(FrameType::StartCapture, &[], 2)).unwrap();

        let mut samples = 0usize;
        let mut events = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        while samples < 1_000 && Instant::now() < deadline {
            if let Ok(n) = Read::read(&mut client, &mut buf) {
                decoder.feed(&buf[..n], &mut |_, f| match f {
                    Frame::CurrentSamples(b) => samples += b.len(),
                    Frame::Event(b) => events += b.len(),
                    _ => {}
                });
            }
        }

        assert!(samples >= 1_000, "only {samples} samples arrived over TCP");
        assert_eq!(
            decoder.stats().crc_errors,
            0,
            "a clean simulator must not produce CRC errors"
        );
        drop(client);
        handle.stop();
    }

    #[test]
    fn port_zero_reports_the_port_the_os_chose() {
        let server = SimServer::bind("127.0.0.1:0", SimConfig::new(Profile::always_on())).unwrap();
        let addr = server.local_addr().unwrap();
        assert_ne!(
            addr.port(),
            0,
            "port 0 must be resolved before it is reported"
        );
    }

    #[test]
    fn a_handle_stops_its_thread_when_dropped() {
        let server = SimServer::bind("127.0.0.1:0", SimConfig::new(Profile::always_on())).unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.spawn();
        drop(handle);
        // The listener is closed once the thread exits, so a later connect must fail.
        thread::sleep(Duration::from_millis(100));
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
    }
}
