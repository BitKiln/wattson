//! Chunk payload layouts.
//!
//! Sample chunks are **structure-of-arrays**: all timestamps, then all currents, then all
//! voltages. Adjacent current readings differ by tens of microamps out of a 32-bit word, so
//! column-major layout puts long runs of identical high bytes together and compresses several
//! times better than interleaved records. `capture::tests` measures this rather than assuming
//! it.

use crate::error::CaptureError;

/// Fixed part of a SAMPLES payload.
pub const SAMPLES_HEADER: usize = 24;

/// Layout flags in a SAMPLES payload.
#[derive(Debug)]
pub struct SampleLayout;

impl SampleLayout {
    /// Samples are evenly spaced, so no per-sample delta array is stored.
    pub const UNIFORM: u32 = 1 << 0;
    /// A voltage column is present.
    pub const HAS_VOLTAGE: u32 = 1 << 1;
    /// Reserved for delta-encoded currents. Not written by v1.0.
    pub const CURRENT_DELTA: u32 = 1 << 2;
}

/// A decoded sample chunk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SampleChunk {
    /// Capture-relative nanoseconds of the first sample.
    pub t0_ns: u64,
    /// Nanoseconds between samples when uniform; 0 otherwise.
    pub period_ns: u32,
    /// Per-sample deltas from the previous sample, when not uniform.
    pub dt_ns: Vec<u32>,
    pub current_ua: Vec<i32>,
    /// Empty when the capture has no voltage channel.
    pub voltage_uv: Vec<u32>,
}

impl SampleChunk {
    pub fn len(&self) -> usize {
        self.current_ua.len()
    }

    pub fn is_empty(&self) -> bool {
        self.current_ua.is_empty()
    }

    pub fn is_uniform(&self) -> bool {
        self.dt_ns.is_empty()
    }

    pub fn has_voltage(&self) -> bool {
        !self.voltage_uv.is_empty()
    }

    /// Capture-relative timestamp of sample `i`.
    pub fn time_ns(&self, i: usize) -> u64 {
        if self.is_uniform() {
            self.t0_ns + i as u64 * self.period_ns as u64
        } else {
            self.t0_ns + self.dt_ns[..i].iter().map(|d| *d as u64).sum::<u64>()
        }
    }

    /// Timestamp of the last sample.
    pub fn last_time_ns(&self) -> u64 {
        if self.is_empty() {
            self.t0_ns
        } else {
            self.time_ns(self.len() - 1)
        }
    }

    /// Voltage of sample `i`, or `fallback` when the capture has no voltage channel.
    pub fn voltage_or(&self, i: usize, fallback: u32) -> u32 {
        self.voltage_uv.get(i).copied().unwrap_or(fallback)
    }

    pub fn encode(&self) -> Vec<u8> {
        let n = self.len();
        let mut flags = 0u32;
        if self.is_uniform() {
            flags |= SampleLayout::UNIFORM;
        }
        if self.has_voltage() {
            flags |= SampleLayout::HAS_VOLTAGE;
        }

        let mut out = Vec::with_capacity(SAMPLES_HEADER + n * 12);
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&self.t0_ns.to_le_bytes());
        out.extend_from_slice(&self.period_ns.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved

        if !self.is_uniform() {
            for d in &self.dt_ns {
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
        for c in &self.current_ua {
            out.extend_from_slice(&c.to_le_bytes());
        }
        for v in &self.voltage_uv {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<SampleChunk, CaptureError> {
        if b.len() < SAMPLES_HEADER {
            return Err(CaptureError::Metadata(
                "sample chunk shorter than its header".into(),
            ));
        }
        let count = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let flags = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let t0_ns = u64::from_le_bytes(b[8..16].try_into().expect("8 bytes"));
        let period_ns = u32::from_le_bytes([b[16], b[17], b[18], b[19]]);

        if flags & SampleLayout::CURRENT_DELTA != 0 {
            // Reserved for a future minor version; a v1.0 reader must not guess the layout.
            return Err(CaptureError::UnsupportedCompression(
                SampleLayout::CURRENT_DELTA,
            ));
        }

        let uniform = flags & SampleLayout::UNIFORM != 0;
        let has_voltage = flags & SampleLayout::HAS_VOLTAGE != 0;
        let columns = usize::from(!uniform) + 1 + usize::from(has_voltage);
        let need = SAMPLES_HEADER + count * 4 * columns;
        if b.len() < need {
            return Err(CaptureError::Metadata(format!(
                "sample chunk declares {count} samples but holds only {} bytes (needs {need})",
                b.len()
            )));
        }

        let mut off = SAMPLES_HEADER;
        let take_u32 = |n: usize, off: &mut usize| -> Vec<u32> {
            let v = b[*off..*off + n * 4]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            *off += n * 4;
            v
        };

        let dt_ns = if uniform {
            Vec::new()
        } else {
            take_u32(count, &mut off)
        };
        let current_ua: Vec<i32> = b[off..off + count * 4]
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        off += count * 4;
        let voltage_uv = if has_voltage {
            take_u32(count, &mut off)
        } else {
            Vec::new()
        };

        Ok(SampleChunk {
            t0_ns,
            period_ns,
            dt_ns,
            current_ua,
            voltage_uv,
        })
    }
}

/// One stored firmware event: 16 bytes on disk.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub t_ns: u64,
    pub id: u16,
    pub flags: u16,
    pub value: u32,
}

/// One stored GPIO edge: 12 bytes on disk.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GpioRow {
    pub t_ns: u64,
    pub state: u16,
}

/// One stored clock-correlation triple: 24 bytes on disk.
///
/// The raw triples are kept, not just the fitted line, so drift correction can be re-derived
/// offline with a better algorithm without recapturing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SyncRow {
    pub host_send_ns: u64,
    pub host_recv_ns: u64,
    pub device_ticks: u64,
}

/// Header shared by the EVENTS, GPIO, and SYNC payloads.
const ROWS_HEADER: usize = 8;

fn encode_rows<T>(rows: &[T], row_len: usize, mut write: impl FnMut(&T, &mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::with_capacity(ROWS_HEADER + rows.len() * row_len);
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flags, reserved
    for r in rows {
        write(r, &mut out);
    }
    out
}

fn decode_rows<T>(
    b: &[u8],
    row_len: usize,
    what: &str,
    read: impl Fn(&[u8]) -> T,
) -> Result<Vec<T>, CaptureError> {
    if b.len() < ROWS_HEADER {
        return Err(CaptureError::Metadata(format!(
            "{what} chunk shorter than its header"
        )));
    }
    let count = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let need = ROWS_HEADER + count * row_len;
    if b.len() < need {
        return Err(CaptureError::Metadata(format!(
            "{what} chunk declares {count} rows but holds only {} bytes (needs {need})",
            b.len()
        )));
    }
    Ok(b[ROWS_HEADER..need]
        .chunks_exact(row_len)
        .map(read)
        .collect())
}

pub fn encode_events(rows: &[EventRow]) -> Vec<u8> {
    encode_rows(rows, 16, |r, out| {
        out.extend_from_slice(&r.t_ns.to_le_bytes());
        out.extend_from_slice(&r.id.to_le_bytes());
        out.extend_from_slice(&r.flags.to_le_bytes());
        out.extend_from_slice(&r.value.to_le_bytes());
    })
}

pub fn decode_events(b: &[u8]) -> Result<Vec<EventRow>, CaptureError> {
    decode_rows(b, 16, "event", |c| EventRow {
        t_ns: u64::from_le_bytes(c[0..8].try_into().expect("8 bytes")),
        id: u16::from_le_bytes([c[8], c[9]]),
        flags: u16::from_le_bytes([c[10], c[11]]),
        value: u32::from_le_bytes([c[12], c[13], c[14], c[15]]),
    })
}

pub fn encode_gpio(rows: &[GpioRow]) -> Vec<u8> {
    encode_rows(rows, 12, |r, out| {
        out.extend_from_slice(&r.t_ns.to_le_bytes());
        out.extend_from_slice(&r.state.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
    })
}

pub fn decode_gpio(b: &[u8]) -> Result<Vec<GpioRow>, CaptureError> {
    decode_rows(b, 12, "gpio", |c| GpioRow {
        t_ns: u64::from_le_bytes(c[0..8].try_into().expect("8 bytes")),
        state: u16::from_le_bytes([c[8], c[9]]),
    })
}

pub fn encode_sync(rows: &[SyncRow]) -> Vec<u8> {
    encode_rows(rows, 24, |r, out| {
        out.extend_from_slice(&r.host_send_ns.to_le_bytes());
        out.extend_from_slice(&r.host_recv_ns.to_le_bytes());
        out.extend_from_slice(&r.device_ticks.to_le_bytes());
    })
}

pub fn decode_sync(b: &[u8]) -> Result<Vec<SyncRow>, CaptureError> {
    decode_rows(b, 24, "sync", |c| SyncRow {
        host_send_ns: u64::from_le_bytes(c[0..8].try_into().expect("8 bytes")),
        host_recv_ns: u64::from_le_bytes(c[8..16].try_into().expect("8 bytes")),
        device_ticks: u64::from_le_bytes(c[16..24].try_into().expect("8 bytes")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform_chunk(n: usize) -> SampleChunk {
        SampleChunk {
            t0_ns: 1_000_000,
            period_ns: 20_000,
            dt_ns: Vec::new(),
            current_ua: (0..n as i32).map(|i| 3_000 + i * 3).collect(),
            voltage_uv: (0..n as u32).map(|i| 3_300_000 - i).collect(),
        }
    }

    #[test]
    fn uniform_sample_chunk_round_trips() {
        let c = uniform_chunk(512);
        let back = SampleChunk::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert!(back.is_uniform());
        assert_eq!(back.time_ns(0), 1_000_000);
        assert_eq!(back.time_ns(3), 1_000_000 + 3 * 20_000);
        assert_eq!(back.last_time_ns(), 1_000_000 + 511 * 20_000);
    }

    #[test]
    fn non_uniform_sample_chunk_round_trips() {
        let c = SampleChunk {
            t0_ns: 500,
            period_ns: 0,
            dt_ns: vec![0, 10, 30, 5],
            current_ua: vec![1, 2, 3, 4],
            voltage_uv: vec![9, 9, 9, 9],
        };
        let back = SampleChunk::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert!(!back.is_uniform());
        // Deltas accumulate: sample 3 is at 500 + 0 + 10 + 30.
        assert_eq!(back.time_ns(3), 540);
    }

    #[test]
    fn a_chunk_without_voltage_round_trips() {
        let c = SampleChunk {
            t0_ns: 0,
            period_ns: 1_000,
            dt_ns: Vec::new(),
            current_ua: vec![10, 20, 30],
            voltage_uv: Vec::new(),
        };
        let back = SampleChunk::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert!(!back.has_voltage());
        assert_eq!(back.voltage_or(1, 3_300_000), 3_300_000);
    }

    #[test]
    fn an_empty_chunk_is_legal() {
        let c = SampleChunk::default();
        let back = SampleChunk::decode(&c.encode()).unwrap();
        assert!(back.is_empty());
        assert_eq!(back.last_time_ns(), 0);
    }

    /// A payload that claims more rows than it holds must be reported, never read past.
    #[test]
    fn a_truncated_payload_errors_rather_than_reading_past_the_end() {
        let full = uniform_chunk(100).encode();
        for cut in [SAMPLES_HEADER, SAMPLES_HEADER + 4, full.len() - 1] {
            assert!(
                SampleChunk::decode(&full[..cut]).is_err(),
                "a payload truncated to {cut} bytes must not decode"
            );
        }
        assert!(SampleChunk::decode(&[]).is_err());
        assert!(decode_events(&[0, 1, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(decode_gpio(&[]).is_err());
        assert!(decode_sync(&[]).is_err());
    }

    /// The reserved delta flag must be refused, not guessed at.
    #[test]
    fn the_reserved_delta_layout_is_refused() {
        let mut bytes = uniform_chunk(4).encode();
        let flags = SampleLayout::UNIFORM | SampleLayout::HAS_VOLTAGE | SampleLayout::CURRENT_DELTA;
        bytes[4..8].copy_from_slice(&flags.to_le_bytes());
        assert!(SampleChunk::decode(&bytes).is_err());
    }

    #[test]
    fn event_rows_round_trip() {
        let rows = vec![
            EventRow {
                t_ns: 1,
                id: 0x0101,
                flags: 0,
                value: 0,
            },
            EventRow {
                t_ns: 2_000_000_000,
                id: 0x0110,
                flags: 1,
                value: 64,
            },
        ];
        assert_eq!(decode_events(&encode_events(&rows)).unwrap(), rows);
        assert_eq!(decode_events(&encode_events(&[])).unwrap(), Vec::new());
    }

    #[test]
    fn gpio_rows_round_trip() {
        let rows = vec![
            GpioRow {
                t_ns: 5,
                state: 0b0001,
            },
            GpioRow {
                t_ns: 9,
                state: 0b1011,
            },
        ];
        assert_eq!(decode_gpio(&encode_gpio(&rows)).unwrap(), rows);
    }

    #[test]
    fn sync_rows_round_trip() {
        let rows = vec![SyncRow {
            host_send_ns: 100,
            host_recv_ns: 350,
            device_ticks: 1_234_567_890_123,
        }];
        assert_eq!(decode_sync(&encode_sync(&rows)).unwrap(), rows);
    }

    #[test]
    fn row_sizes_are_what_the_format_documents() {
        assert_eq!(
            encode_events(&[EventRow {
                t_ns: 0,
                id: 0,
                flags: 0,
                value: 0
            }])
            .len()
                - 8,
            16
        );
        assert_eq!(encode_gpio(&[GpioRow { t_ns: 0, state: 0 }]).len() - 8, 12);
        assert_eq!(
            encode_sync(&[SyncRow {
                host_send_ns: 0,
                host_recv_ns: 0,
                device_ticks: 0
            }])
            .len()
                - 8,
            24
        );
    }
}
