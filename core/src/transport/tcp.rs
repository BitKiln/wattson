//! TCP transport.
//!
//! Used by the standalone simulator, and by anyone bridging a profiler over the network.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use super::{Transport, TransportInfo};
use crate::error::TransportError;

/// A framed byte pipe over TCP.
pub struct TcpTransport {
    stream: TcpStream,
    peer: SocketAddr,
}

impl fmt::Debug for TcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpTransport")
            .field("peer", &self.peer)
            .finish()
    }
}

impl TcpTransport {
    pub fn connect(addr: SocketAddr) -> Result<TcpTransport, TransportError> {
        let stream = TcpStream::connect(addr).map_err(|source| TransportError::Open {
            target: addr.to_string(),
            source,
        })?;
        Self::from_stream(stream)
    }

    /// Wrap an already-connected stream, as the simulator's server side does.
    pub fn from_stream(stream: TcpStream) -> Result<TcpTransport, TransportError> {
        // Sample blocks are small and latency-sensitive; Nagle would batch them into
        // multi-hundred-millisecond clumps and make a live plot stutter.
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        let peer = stream.peer_addr()?;
        Ok(TcpTransport { stream, peer })
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }
}

impl Transport for TcpTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.stream.read(buf) {
            Ok(0) => Err(TransportError::Disconnected),
            Ok(n) => Ok(n),
            // A read timeout is `WouldBlock` or `TimedOut` depending on the platform, and
            // neither means the device is gone.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(0)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                Err(TransportError::Disconnected)
            }
            Err(e) => Err(TransportError::Io(e)),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        match self.stream.write_all(buf) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                Err(TransportError::Disconnected)
            }
            Err(e) => Err(TransportError::Io(e)),
        }
    }

    fn flush(&mut self) -> Result<(), TransportError> {
        self.stream.flush()?;
        Ok(())
    }

    fn set_read_timeout(&mut self, timeout: Duration) -> Result<(), TransportError> {
        self.stream.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            kind: "tcp",
            target: self.peer.to_string(),
        }
    }

    fn close(&mut self) -> Result<(), TransportError> {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Port 0 lets the OS pick, so parallel CI jobs cannot collide.
    fn echo_server() -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 || stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        });
        (addr, handle)
    }

    #[test]
    fn round_trips_through_a_socket() {
        let (addr, server) = echo_server();
        let mut t = TcpTransport::connect(addr).unwrap();
        t.set_read_timeout(Duration::from_secs(5)).unwrap();
        t.write_all(b"ping").unwrap();

        let mut got = Vec::new();
        let mut buf = [0u8; 16];
        while got.len() < 4 {
            match t.read(&mut buf).unwrap() {
                0 => continue,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(&got, b"ping");
        t.close().unwrap();
        drop(t);
        server.join().unwrap();
    }

    #[test]
    fn a_read_timeout_is_zero_not_an_error() {
        let (addr, server) = echo_server();
        let mut t = TcpTransport::connect(addr).unwrap();
        t.set_read_timeout(Duration::from_millis(20)).unwrap();
        assert_eq!(t.read(&mut [0u8; 8]).unwrap(), 0);
        t.close().unwrap();
        drop(t);
        let _ = server.join();
    }

    #[test]
    fn connecting_to_a_closed_port_reports_the_target() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        match TcpTransport::connect(addr) {
            Err(TransportError::Open { target, .. }) => assert_eq!(target, addr.to_string()),
            other => panic!("expected an Open error naming the target, got {other:?}"),
        }
    }
}
