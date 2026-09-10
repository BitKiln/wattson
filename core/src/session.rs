//! A live conversation with a device.
//!
//! ```text
//! host                          device
//!  |  HELLO                  ->  |
//!  |  <-              DEVICE_INFO |
//!  |  CONFIG                 ->  |
//!  |  START_CAPTURE          ->  |
//!  |  <- CURRENT_SAMPLES *       |
//!  |  <- EVENT *  GPIO_EVENT *   |
//!  |  SYNC  <->  SYNC            |   at least 1 Hz throughout
//!  |  STOP_CAPTURE           ->  |
//! ```
//!
//! The session owns the one place device timestamps are unwrapped. Nothing above this layer
//! ever sees a `u32` tick value, which removes an entire class of wraparound bugs by
//! construction.

use std::time::{Duration, Instant};

use wattson_protocol::{
    Config, DeviceInfo, Frame, FrameType, Hello, MAX_ENCODED, MAX_PAYLOAD, PROTOCOL_VERSION,
    Sample, SampleBlock, SyncFrame, encode_frame,
};

use crate::capture::{Gap, GapCause};
use crate::error::{SessionError, SinkError, TransportError};
use crate::time::{DeviceTime, SyncSample, TickUnwrapper, TimeBase};
use crate::transport::Transport;

/// What to ask a device for.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub sample_rate_hz: u32,
    pub averaging: u16,
    pub conv_time_code: u16,
    pub gpio_mask: u32,
    pub shunt_micro_ohm: i32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            sample_rate_hz: 50_000,
            averaging: 1,
            conv_time_code: 0,
            gpio_mask: 0,
            shunt_micro_ohm: 100_000,
        }
    }
}

/// Where decoded records go.
///
/// Implemented by [`crate::capture::CaptureWriter`], and by counting sinks in tests.
pub trait CaptureSink {
    fn on_samples(&mut self, samples: &[(u64, i32, Option<u32>)]) -> Result<(), SinkError>;
    fn on_event(&mut self, t_ns: u64, id: u16, value: Option<u32>) -> Result<(), SinkError>;
    fn on_gpio(&mut self, t_ns: u64, state: u16) -> Result<(), SinkError>;
    fn on_sync(&mut self, sample: SyncSample) -> Result<(), SinkError>;
    /// Called when data is known to be missing. Never inferred later.
    fn on_gap(&mut self, gap: Gap) -> Result<(), SinkError>;
    fn on_device_error(&mut self, code: u16, dropped_samples: u32) -> Result<(), SinkError>;
}

/// A sink that counts and discards, for throughput tests.
#[derive(Debug, Default)]
pub struct CountingSink {
    pub samples: u64,
    pub events: u64,
    pub gpio: u64,
    pub gaps: Vec<Gap>,
    pub device_errors: u64,
    pub first_ns: Option<u64>,
    pub last_ns: u64,
}

impl CaptureSink for CountingSink {
    fn on_samples(&mut self, samples: &[(u64, i32, Option<u32>)]) -> Result<(), SinkError> {
        self.samples += samples.len() as u64;
        if let Some(&(t, _, _)) = samples.first() {
            self.first_ns.get_or_insert(t);
        }
        if let Some(&(t, _, _)) = samples.last() {
            self.last_ns = self.last_ns.max(t);
        }
        Ok(())
    }
    fn on_event(&mut self, _t_ns: u64, _id: u16, _value: Option<u32>) -> Result<(), SinkError> {
        self.events += 1;
        Ok(())
    }
    fn on_gpio(&mut self, _t_ns: u64, _state: u16) -> Result<(), SinkError> {
        self.gpio += 1;
        Ok(())
    }
    fn on_sync(&mut self, _sample: SyncSample) -> Result<(), SinkError> {
        Ok(())
    }
    fn on_gap(&mut self, gap: Gap) -> Result<(), SinkError> {
        self.gaps.push(gap);
        Ok(())
    }
    fn on_device_error(&mut self, _code: u16, _dropped: u32) -> Result<(), SinkError> {
        self.device_errors += 1;
        Ok(())
    }
}

/// What one [`Session::poll`] did.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PollStats {
    pub bytes: usize,
    pub frames: u64,
    pub samples: u64,
    pub events: u64,
}

/// A live device session.
#[derive(Debug)]
pub struct Session {
    transport: Box<dyn Transport>,
    decoder: wattson_protocol::Decoder,
    unwrapper: TickUnwrapper,
    timebase: TimeBase,
    info: Option<DeviceInfo>,
    seq: u8,
    /// Set once the first sample fixes the capture origin.
    origin_set: bool,
    /// Last sample timestamp seen, for detecting timing discontinuities.
    last_sample_ns: Option<u64>,
    nominal_period_ns: u64,
    read_buf: Vec<u8>,
}

impl Session {
    /// Wrap an already-open transport.
    pub fn new(transport: Box<dyn Transport>) -> Session {
        Session {
            transport,
            decoder: wattson_protocol::Decoder::new(),
            unwrapper: TickUnwrapper::new(),
            timebase: TimeBase::start_live(wattson_protocol::DEFAULT_TIMER_HZ, 0),
            info: None,
            seq: 0,
            origin_set: false,
            last_sample_ns: None,
            nominal_period_ns: 0,
            read_buf: vec![0u8; 64 * 1024],
        }
    }

    pub fn device_info(&self) -> Option<&DeviceInfo> {
        self.info.as_ref()
    }

    pub fn timebase(&self) -> &TimeBase {
        &self.timebase
    }

    pub fn decoder_stats(&self) -> wattson_protocol::DecoderStats {
        self.decoder.stats()
    }

    pub fn transport_info(&self) -> crate::transport::TransportInfo {
        self.transport.describe()
    }

    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn send(&mut self, ty: FrameType, payload: &[u8]) -> Result<(), TransportError> {
        let seq = self.next_seq();
        let mut buf = [0u8; MAX_ENCODED];
        let n = encode_frame(ty, seq, payload, &mut buf)
            .map_err(|e| TransportError::Serial(format!("frame encoding failed: {e:?}")))?;
        self.transport.write_all(&buf[..n])?;
        self.transport.flush()
    }

    /// Exchange HELLO for DEVICE_INFO.
    pub fn handshake(&mut self, timeout: Duration) -> Result<DeviceInfo, SessionError> {
        let hello = Hello {
            proto_version: PROTOCOL_VERSION,
            nonce: 0x5741,
            reserved: 0,
        };
        let mut payload = [0u8; Hello::LEN];
        hello
            .encode(&mut payload)
            .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
        self.send(FrameType::Hello, &payload)?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let n = self.transport.read(&mut self.read_buf)?;
            if n == 0 {
                continue;
            }
            let mut found: Option<DeviceInfo> = None;
            let bytes = self.read_buf[..n].to_vec();
            self.decoder.feed(&bytes, &mut |_, frame| {
                if let Frame::DeviceInfo(info) = frame {
                    found = Some(info);
                }
            });
            if let Some(info) = found {
                // A major-version difference means the frame layouts may differ, and guessing
                // produces plausible garbage rather than an error.
                if info.proto_version >> 8 != PROTOCOL_VERSION >> 8 {
                    return Err(SessionError::ProtocolMismatch {
                        device: info.proto_version,
                        host: PROTOCOL_VERSION,
                    });
                }
                let timer_hz = if info.timer_hz == 0 {
                    return Err(SessionError::Time(crate::error::TimeError::ZeroTimerRate));
                } else {
                    info.timer_hz
                };
                self.timebase = TimeBase::start_live(timer_hz, 0);
                self.info = Some(info);
                return Ok(info);
            }
        }
        Err(SessionError::Timeout {
            what: "DEVICE_INFO",
            timeout_ms: timeout.as_millis() as u64,
        })
    }

    /// Send CONFIG, refusing rates the device has said it cannot meet.
    pub fn configure(&mut self, cfg: &CaptureConfig) -> Result<(), SessionError> {
        let info = self.info.ok_or(SessionError::NotHandshaked)?;
        // Refusing here beats letting the device drop samples silently: a capture that is
        // quietly missing a third of its data still produces confident-looking numbers.
        if info.max_sample_rate_hz > 0 && cfg.sample_rate_hz > info.max_sample_rate_hz {
            return Err(SessionError::RateTooHigh {
                requested_hz: cfg.sample_rate_hz,
                max_hz: info.max_sample_rate_hz,
            });
        }

        self.nominal_period_ns = if cfg.sample_rate_hz > 0 {
            1_000_000_000 / cfg.sample_rate_hz as u64
        } else {
            0
        };

        let config = Config {
            sample_rate_hz: cfg.sample_rate_hz,
            averaging: cfg.averaging,
            conv_time_code: cfg.conv_time_code,
            gpio_mask: cfg.gpio_mask,
            shunt_micro_ohm: cfg.shunt_micro_ohm,
            flags: 0,
            reserved: 0,
        };
        let mut payload = [0u8; Config::LEN];
        config
            .encode(&mut payload)
            .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
        self.send(FrameType::Config, &payload)?;
        Ok(())
    }

    pub fn start(&mut self) -> Result<(), SessionError> {
        self.send(FrameType::StartCapture, &[])?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), SessionError> {
        self.send(FrameType::StopCapture, &[])?;
        Ok(())
    }

    /// Send a host annotation. The device timestamps it on receipt, so it lands in the same
    /// time base as everything else.
    pub fn send_marker(&mut self, marker_id: u32, value: u32) -> Result<(), SessionError> {
        let marker = wattson_protocol::Marker {
            device_ticks: 0,
            marker_id,
            value,
        };
        let mut payload = [0u8; wattson_protocol::Marker::LEN];
        marker
            .encode(&mut payload)
            .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
        self.send(FrameType::Marker, &payload)?;
        Ok(())
    }

    /// Send a SYNC and remember when, so the round trip can be timed.
    pub fn send_sync(&mut self) -> Result<(), SessionError> {
        let send_ns = self.timebase.host_elapsed_ns().unwrap_or(0);
        let sync = SyncFrame {
            device_ticks: 0,
            wrap_count: 0,
            flags: 0,
            host_time_ns: send_ns,
        };
        let mut payload = [0u8; SyncFrame::LEN];
        sync.encode(&mut payload)
            .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
        self.send(FrameType::Sync, &payload)?;
        Ok(())
    }

    /// Read once from the transport and dispatch whatever decodes.
    ///
    /// Never blocks longer than the transport's read timeout, so a caller can interleave
    /// progress rendering and Ctrl-C handling.
    pub fn poll(&mut self, sink: &mut dyn CaptureSink) -> Result<PollStats, SessionError> {
        let n = self.transport.read(&mut self.read_buf)?;
        let mut stats = PollStats {
            bytes: n,
            ..Default::default()
        };
        if n == 0 {
            return Ok(stats);
        }

        // The decoder borrows its own buffer for the frame's lifetime, so collect what each
        // frame yields and dispatch after the borrow ends.
        let bytes = std::mem::take(&mut self.read_buf);
        let mut samples: Vec<(u32, i32, u32)> = Vec::new();
        let mut events: Vec<(u32, u16, Option<u32>)> = Vec::new();
        let mut gpio: Vec<(u32, u16)> = Vec::new();
        let mut syncs: Vec<SyncFrame> = Vec::new();
        let mut device_errors: Vec<(u16, u32)> = Vec::new();
        let mut overflow_before = false;
        let mut frames = 0u64;
        let mut seq_gap = false;

        let before = self.decoder.stats().seq_gaps;
        self.decoder.feed(&bytes[..n], &mut |_seq, frame| {
            frames += 1;
            match frame {
                Frame::CurrentSamples(block) => {
                    overflow_before |= block.overflow_before();
                    samples.extend(
                        block
                            .iter()
                            .map(|s: Sample| (s.timestamp, s.current_ua, s.voltage_uv)),
                    );
                }
                Frame::Event(block) => {
                    events.extend(block.iter().map(|e| (e.timestamp, e.id.0, e.value)));
                }
                Frame::GpioEvent(block) => {
                    gpio.extend(block.iter().map(|g| (g.timestamp, g.state)));
                }
                Frame::Sync(s) => syncs.push(s),
                Frame::Error(e) => device_errors.push((e.code, e.dropped_samples)),
                Frame::DeviceInfo(_)
                | Frame::Hello(_)
                | Frame::Config(_)
                | Frame::StartCapture
                | Frame::StopCapture
                | Frame::Marker(_)
                | Frame::Unknown { .. } => {}
            }
        });
        seq_gap |= self.decoder.stats().seq_gaps > before;
        self.read_buf = bytes;

        stats.frames = frames;

        // Unwrap timestamps exactly once, here, before anything else sees them.
        let mut out: Vec<(u64, i32, Option<u32>)> = Vec::with_capacity(samples.len());
        for (raw, current_ua, voltage_uv) in samples {
            let t = self.unwrap_to_capture_ns(raw)?;
            out.push((t, current_ua, Some(voltage_uv)));
        }

        if let Some(&(first_ns, _, _)) = out.first() {
            // A jump larger than a few sample periods is missing data, not jitter, and must
            // be recorded where it was observed rather than inferred from the file later.
            if let Some(last) = self.last_sample_ns
                && self.nominal_period_ns > 0
                && first_ns > last + self.nominal_period_ns * 3
            {
                let lost = (first_ns - last) / self.nominal_period_ns;
                sink.on_gap(Gap {
                    start_ns: last,
                    end_ns: first_ns,
                    cause: if overflow_before {
                        GapCause::DeviceOverflow
                    } else if seq_gap {
                        GapCause::SequenceGap
                    } else {
                        GapCause::TimestampJump
                    },
                    lost_estimate: lost.saturating_sub(1),
                })?;
            }
        }
        if let Some(&(t, _, _)) = out.last() {
            self.last_sample_ns = Some(t);
        }

        stats.samples = out.len() as u64;
        if !out.is_empty() {
            sink.on_samples(&out)?;
        }

        // Events, edges and SYNC echoes share the sample stream's timer but arrive
        // interleaved with it, so they resolve against the epoch the samples established
        // rather than advancing it. See `TickUnwrapper::resolve`.
        for (raw, id, value) in events {
            let t = self.resolve_to_capture_ns(raw);
            sink.on_event(t, id, value)?;
            stats.events += 1;
        }
        for (raw, state) in gpio {
            let t = self.resolve_to_capture_ns(raw);
            sink.on_gpio(t, state)?;
        }
        for s in syncs {
            let recv_ns = self.timebase.host_elapsed_ns().unwrap_or(0);
            // Verify the inferred wrap epoch rather than trusting it. A disagreement means a
            // 71-minute jump is about to appear in the data.
            if let Err(e) = self.unwrapper.check_wrap_count(s.wrap_count) {
                tracing::warn!(error = %e, "resynchronising the timer wrap epoch from the device");
                self.unwrapper.resync(s.device_ticks, s.wrap_count);
            }
            let ticks = self.unwrapper.resolve(s.device_ticks);
            sink.on_sync(SyncSample {
                host_send_ns: s.host_time_ns,
                host_recv_ns: recv_ns,
                device_ticks: ticks.0,
            })?;
        }
        for (code, dropped) in device_errors {
            sink.on_device_error(code, dropped)?;
        }

        Ok(stats)
    }

    /// Resolve a non-sample timestamp against the epoch the sample stream established.
    fn resolve_to_capture_ns(&mut self, raw: u32) -> u64 {
        let t = self.unwrapper.resolve(raw);
        if !self.origin_set {
            self.timebase.set_origin(t.0);
            self.origin_set = true;
        }
        self.timebase.to_capture_ns(t)
    }

    fn unwrap_to_capture_ns(&mut self, raw: u32) -> Result<u64, SessionError> {
        let t: DeviceTime = self.unwrapper.unwrap_ticks(raw)?;
        if !self.origin_set {
            self.timebase.set_origin(t.0);
            self.origin_set = true;
        }
        Ok(self.timebase.to_capture_ns(t))
    }

    /// Capture for a fixed duration, polling until it elapses.
    pub fn run_for(
        &mut self,
        duration: Duration,
        sink: &mut dyn CaptureSink,
    ) -> Result<PollStats, SessionError> {
        let deadline = Instant::now() + duration;
        let mut total = PollStats::default();
        let mut last_sync = Instant::now();

        while Instant::now() < deadline {
            let s = self.poll(sink)?;
            total.bytes += s.bytes;
            total.frames += s.frames;
            total.samples += s.samples;
            total.events += s.events;

            // At least 1 Hz, so the wrap-count cross-check and the clock fit both stay fed.
            if last_sync.elapsed() >= Duration::from_millis(500) {
                let _ = self.send_sync();
                last_sync = Instant::now();
            }
        }
        Ok(total)
    }

    /// Drain whatever the device has already queued, after a stop.
    pub fn drain(
        &mut self,
        grace: Duration,
        sink: &mut dyn CaptureSink,
    ) -> Result<PollStats, SessionError> {
        let deadline = Instant::now() + grace;
        let mut total = PollStats::default();
        while Instant::now() < deadline {
            match self.poll(sink) {
                Ok(s) if s.bytes == 0 => break,
                Ok(s) => {
                    total.bytes += s.bytes;
                    total.frames += s.frames;
                    total.samples += s.samples;
                    total.events += s.events;
                }
                // A device that hangs up after a stop is normal, not a failure.
                Err(SessionError::Transport(TransportError::Disconnected)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }
}

/// Largest payload a session will ever build. Exposed so callers can size buffers.
pub const MAX_SESSION_PAYLOAD: usize = MAX_PAYLOAD;

/// Number of samples per transmitted block, matching the protocol's recommendation.
pub const SAMPLES_PER_BLOCK: usize = 64;

/// Encode one uniform sample block, for anything acting as a device.
pub fn encode_sample_block(
    t0_ticks: u32,
    period_ticks: u32,
    samples: &[(i32, u32)],
    seq: u8,
    out: &mut [u8],
) -> Result<usize, SessionError> {
    let mut payload = [0u8; MAX_PAYLOAD];
    let plen = SampleBlock::encode_uniform(&mut payload, t0_ticks, period_ticks, samples)
        .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
    let n = encode_frame(FrameType::CurrentSamples, seq, &payload[..plen], out)
        .map_err(|e| TransportError::Serial(format!("{e:?}")))?;
    Ok(n)
}
