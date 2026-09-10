//! An in-memory duplex pipe.
//!
//! This is what makes the whole project testable without hardware or sockets: the simulator
//! runs on one end, a [`crate::session::Session`] on the other, and an entire capture happens
//! inside one `cargo test` with no ports to collide over on a CI runner.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{Transport, TransportInfo};
use crate::error::TransportError;

/// One direction of the pipe.
#[derive(Debug, Default)]
struct Channel {
    buf: VecDeque<u8>,
    closed: bool,
}

#[derive(Debug, Default)]
struct Shared {
    channel: Mutex<Channel>,
    signal: Condvar,
}

impl Shared {
    fn write(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let mut ch = self.channel.lock().expect("pipe mutex poisoned");
        if ch.closed {
            return Err(TransportError::Disconnected);
        }
        ch.buf.extend(bytes.iter().copied());
        drop(ch);
        self.signal.notify_all();
        Ok(())
    }

    fn read(&self, out: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let deadline = Instant::now() + timeout;
        let mut ch = self.channel.lock().expect("pipe mutex poisoned");
        loop {
            if !ch.buf.is_empty() {
                // Bulk-copy through the deque's contiguous halves. Popping byte by byte here
                // costs enough at 400 KB/s that the host falls behind a 50 ksps device and
                // silently loses the backlog at the end of a capture.
                let n = out.len().min(ch.buf.len());
                let (front, back) = ch.buf.as_slices();
                let take_front = n.min(front.len());
                out[..take_front].copy_from_slice(&front[..take_front]);
                if take_front < n {
                    let rest = n - take_front;
                    out[take_front..n].copy_from_slice(&back[..rest]);
                }
                ch.buf.drain(..n);
                return Ok(n);
            }
            if ch.closed {
                return Err(TransportError::Disconnected);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // A timeout is not an error: a configured-but-idle device is legitimately
                // silent, and must not look like a broken one.
                return Ok(0);
            }
            let (guard, _) = self
                .signal
                .wait_timeout(ch, remaining)
                .expect("pipe condvar poisoned");
            ch = guard;
        }
    }

    fn close(&self) {
        let mut ch = self.channel.lock().expect("pipe mutex poisoned");
        ch.closed = true;
        drop(ch);
        self.signal.notify_all();
    }

    fn pending(&self) -> usize {
        self.channel.lock().expect("pipe mutex poisoned").buf.len()
    }
}

/// One end of an in-memory duplex byte pipe.
pub struct PipeTransport {
    /// Bytes this end reads.
    inbox: Arc<Shared>,
    /// Bytes this end writes.
    outbox: Arc<Shared>,
    timeout: Duration,
    label: &'static str,
}

impl fmt::Debug for PipeTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipeTransport")
            .field("label", &self.label)
            .field("pending_in", &self.inbox.pending())
            .field("pending_out", &self.outbox.pending())
            .finish()
    }
}

impl PipeTransport {
    /// Create a connected pair. The first end is the host, the second is the device.
    pub fn pair() -> (PipeTransport, PipeTransport) {
        let host_to_device = Arc::new(Shared::default());
        let device_to_host = Arc::new(Shared::default());
        (
            PipeTransport {
                inbox: Arc::clone(&device_to_host),
                outbox: Arc::clone(&host_to_device),
                timeout: Duration::from_millis(100),
                label: "host",
            },
            PipeTransport {
                inbox: host_to_device,
                outbox: device_to_host,
                timeout: Duration::from_millis(100),
                label: "device",
            },
        )
    }

    /// Bytes waiting to be read at this end.
    pub fn pending(&self) -> usize {
        self.inbox.pending()
    }

    /// Close this end, so the peer's next read reports a disconnect.
    pub fn disconnect(&self) {
        self.outbox.close();
    }
}

impl Drop for PipeTransport {
    fn drop(&mut self) {
        // A dropped end must surface as a disconnect at the peer, or a test that forgets to
        // stop the simulator hangs until its read timeout instead of failing fast.
        self.outbox.close();
    }
}

impl Transport for PipeTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.inbox.read(buf, self.timeout)
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.outbox.write(buf)
    }

    fn flush(&mut self) -> Result<(), TransportError> {
        Ok(())
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.timeout = timeout;
        Ok(())
    }

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            kind: "pipe",
            target: self.label.to_string(),
        }
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.outbox.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn bytes_cross_in_both_directions() {
        let (mut host, mut device) = PipeTransport::pair();
        host.write_all(b"hello").unwrap();
        let mut buf = [0u8; 16];
        let n = device.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        device.write_all(b"world").unwrap();
        let n = host.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");
    }

    #[test]
    fn a_timeout_reads_zero_rather_than_failing() {
        let (mut host, _device) = PipeTransport::pair();
        host.set_read_timeout(Duration::from_millis(5)).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(host.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn closing_one_end_disconnects_the_other() {
        let (mut host, device) = PipeTransport::pair();
        drop(device);
        let mut buf = [0u8; 4];
        assert!(matches!(
            host.read(&mut buf),
            Err(TransportError::Disconnected)
        ));
    }

    #[test]
    fn buffered_bytes_are_readable_after_the_peer_goes_away() {
        let (mut host, mut device) = PipeTransport::pair();
        device.write_all(b"trailing").unwrap();
        drop(device);
        let mut buf = [0u8; 16];
        let n = host.read(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"trailing",
            "queued data must not be lost on disconnect"
        );
    }

    #[test]
    fn a_reader_blocked_on_an_empty_pipe_wakes_when_data_arrives() {
        let (mut host, mut device) = PipeTransport::pair();
        host.set_read_timeout(Duration::from_secs(5)).unwrap();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            device.write_all(b"late").unwrap();
            // Keep the device end alive until the reader has had its chance.
            thread::sleep(Duration::from_millis(50));
        });
        let mut buf = [0u8; 8];
        let n = host.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"late");
        writer.join().unwrap();
    }

    /// The transport hands over whatever it has; frame boundaries never align with reads.
    #[test]
    fn partial_reads_are_normal() {
        let (mut host, mut device) = PipeTransport::pair();
        device.write_all(&[1, 2, 3, 4, 5, 6]).unwrap();
        let mut small = [0u8; 2];
        let mut got = Vec::new();
        for _ in 0..3 {
            let n = host.read(&mut small).unwrap();
            got.extend_from_slice(&small[..n]);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6]);
    }

    /// The bulk-copy path must return the bytes in order across the deque's wrap point.
    #[test]
    fn bulk_reads_are_correct_across_the_ring_wrap() {
        let (mut host, mut device) = PipeTransport::pair();
        // Write, partially drain, then write again, so the deque's two halves both hold data.
        device.write_all(&(0u8..200).collect::<Vec<u8>>()).unwrap();
        let mut small = [0u8; 150];
        assert_eq!(host.read(&mut small).unwrap(), 150);
        device
            .write_all(&(200u8..=255).collect::<Vec<u8>>())
            .unwrap();

        let mut rest = [0u8; 256];
        let mut got = Vec::new();
        while got.len() < 106 {
            let n = host.read(&mut rest).unwrap();
            got.extend_from_slice(&rest[..n]);
        }
        let expected: Vec<u8> = (150u8..=255).collect();
        assert_eq!(got, expected, "bytes must survive the ring wrap in order");
    }

    #[test]
    fn moves_a_megabyte() {
        let (mut host, mut device) = PipeTransport::pair();
        host.set_read_timeout(Duration::from_secs(10)).unwrap();
        const TOTAL: usize = 1 << 20;
        let writer = thread::spawn(move || {
            let block = vec![0xABu8; 4096];
            let mut sent = 0;
            while sent < TOTAL {
                device.write_all(&block).unwrap();
                sent += block.len();
            }
            // Hold the end open until everything has been drained.
            thread::sleep(Duration::from_millis(200));
        });

        let mut buf = vec![0u8; 8192];
        let mut received = 0usize;
        while received < TOTAL {
            match host.read(&mut buf) {
                Ok(0) => panic!("timed out with {received} of {TOTAL} bytes"),
                Ok(n) => received += n,
                Err(e) => panic!("read failed after {received} bytes: {e}"),
            }
        }
        assert_eq!(received, TOTAL);
        writer.join().unwrap();
    }
}
