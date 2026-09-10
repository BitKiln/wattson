//! The synthetic device: a state machine that speaks the real wire protocol.
//!
//! It is a *device*, not a mock. It performs the handshake, refuses rates it cannot meet,
//! batches samples into 64-sample blocks exactly as the protocol recommends, emits SYNC with
//! a real wrap counter, and — when asked — drops blocks and corrupts bytes so the host's
//! recovery paths are actually exercised rather than merely written.

use std::time::{Duration, Instant};

use wattson_protocol::{
    Caps, Config, DEVICE_MAGIC, Decoder, DeviceError, DeviceInfo, ErrorCode, EventBlock, Frame,
    FrameType, MAX_ENCODED, MAX_PAYLOAD, Marker, PROTOCOL_VERSION, SampleBlock, SyncFrame,
    encode_frame,
};

use crate::engine::{FaultInjection, SimConfig, SimEngine};

/// Samples per transmitted block. 64 samples is a 524-byte payload, giving roughly 780
/// blocks/s at 50 ksps — small enough to keep latency low, large enough that per-frame
/// overhead stays under 3%.
pub const SAMPLES_PER_BLOCK: usize = 64;

/// Device type reported in DEVICE_INFO by a simulator.
///
/// Deliberately in the reserved high range so a capture from the simulator can never be
/// mistaken for one from real measurement hardware.
pub const DEVICE_TYPE_SIMULATOR: u16 = 0xFFFF;

/// Where the device is in its lifecycle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    /// Waiting for HELLO.
    Idle,
    /// Handshaken, waiting for CONFIG.
    Greeted,
    /// Configured, waiting for START_CAPTURE.
    Configured,
    Capturing,
}

/// Bytes to send, produced by [`SimDevice::step`].
pub type Outgoing = Vec<u8>;

/// A synthetic profiler device.
#[derive(Debug)]
pub struct SimDevice {
    engine: SimEngine,
    decoder: Decoder,
    state: State,
    seq: u8,
    info: DeviceInfo,
    faults: FaultInjection,

    /// Simulated wall clock, so a capture can run faster than real time in tests.
    started: Option<Instant>,
    /// Simulated device ticks already emitted.
    emitted_ticks: u64,
    last_sync: Instant,
    /// Cumulative samples the device admits to having dropped.
    dropped_samples: u32,
    buffer_overflows: u32,
    /// Set when the last block was dropped, so the next one is flagged.
    overflow_pending: bool,
    /// Stop generating after this much simulated time, if set.
    duration: Option<Duration>,
    finished: bool,
}

impl SimDevice {
    pub fn new(config: SimConfig) -> SimDevice {
        let faults = config.faults;
        let info = DeviceInfo {
            proto_version: PROTOCOL_VERSION,
            device_type: DEVICE_TYPE_SIMULATOR,
            serial: DeviceInfo::ascii_field("SIM-0001"),
            fw_version: DeviceInfo::ascii_field(env!("CARGO_PKG_VERSION")),
            fw_build_id: 0,
            timer_hz: config.timer_hz,
            max_sample_rate_hz: 1_000_000,
            shunt_micro_ohm: 100_000,
            adc_full_scale_ua: 1_000_000,
            channel_count: 1,
            gpio_count: 0,
            caps: Caps::MARKERS | Caps::WRAP_COUNT,
        };
        SimDevice {
            engine: SimEngine::new(config),
            decoder: Decoder::new(),
            state: State::Idle,
            seq: 0,
            info,
            faults,
            started: None,
            emitted_ticks: 0,
            last_sync: Instant::now(),
            dropped_samples: 0,
            buffer_overflows: 0,
            overflow_pending: false,
            duration: None,
            finished: false,
        }
    }

    /// Stop generating after this much simulated time.
    pub fn with_duration(mut self, duration: Option<Duration>) -> Self {
        self.duration = duration;
        self
    }

    pub fn is_capturing(&self) -> bool {
        self.state == State::Capturing
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn device_info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn dropped_samples(&self) -> u32 {
        self.dropped_samples
    }

    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn frame(&mut self, ty: FrameType, payload: &[u8], out: &mut Outgoing) {
        let seq = self.next_seq();
        let mut buf = [0u8; MAX_ENCODED];
        if let Ok(n) = encode_frame(ty, seq, payload, &mut buf) {
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Feed bytes received from the host and act on any commands they contain.
    pub fn receive(&mut self, bytes: &[u8]) -> Outgoing {
        let mut commands: Vec<(FrameType, Option<Config>, Option<Marker>)> = Vec::new();
        self.decoder.feed(bytes, &mut |_seq, frame| match frame {
            Frame::Hello(_) => commands.push((FrameType::Hello, None, None)),
            Frame::Config(c) => commands.push((FrameType::Config, Some(c), None)),
            Frame::StartCapture => commands.push((FrameType::StartCapture, None, None)),
            Frame::StopCapture => commands.push((FrameType::StopCapture, None, None)),
            Frame::Sync(_) => commands.push((FrameType::Sync, None, None)),
            Frame::Marker(m) => commands.push((FrameType::Marker, None, Some(m))),
            _ => {}
        });

        let mut out = Outgoing::new();
        for (ty, cfg, marker) in commands {
            match ty {
                FrameType::Hello => {
                    self.state = State::Greeted;
                    let mut payload = [0u8; DeviceInfo::LEN];
                    if self.info.encode(&mut payload).is_ok() {
                        self.frame(FrameType::DeviceInfo, &payload, &mut out);
                    }
                }
                FrameType::Config => {
                    let Some(c) = cfg else { continue };
                    if c.sample_rate_hz == 0 || c.sample_rate_hz > self.info.max_sample_rate_hz {
                        self.send_error(ErrorCode::BadConfig, &mut out);
                    } else {
                        self.state = State::Configured;
                    }
                }
                FrameType::StartCapture => {
                    // A device that streams before it is configured would produce samples at
                    // an unknown rate, which is worse than refusing.
                    if self.state == State::Idle || self.state == State::Greeted {
                        self.send_error(ErrorCode::NotConfigured, &mut out);
                    } else {
                        self.state = State::Capturing;
                        self.started = Some(Instant::now());
                    }
                }
                FrameType::StopCapture => {
                    self.state = State::Configured;
                    self.finished = true;
                }
                FrameType::Sync => self.send_sync(&mut out),
                FrameType::Marker => {
                    if let Some(m) = marker {
                        let stamped = Marker {
                            device_ticks: self.emitted_ticks as u32,
                            marker_id: m.marker_id,
                            value: m.value,
                        };
                        let mut payload = [0u8; Marker::LEN];
                        if stamped.encode(&mut payload).is_ok() {
                            self.frame(FrameType::Marker, &payload, &mut out);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn send_error(&mut self, code: ErrorCode, out: &mut Outgoing) {
        let err = DeviceError {
            code: code as u16,
            detail: 0,
            dropped_samples: self.dropped_samples,
            buffer_overflows: self.buffer_overflows,
            context: 0,
        };
        let mut payload = [0u8; DeviceError::LEN];
        if err.encode(&mut payload).is_ok() {
            self.frame(FrameType::Error, &payload, out);
        }
    }

    fn send_sync(&mut self, out: &mut Outgoing) {
        let ticks = self.emitted_ticks;
        let sync = SyncFrame {
            device_ticks: ticks as u32,
            // The real wrap count, from what would be the overflow ISR. This is what lets a
            // host verify its inferred epoch instead of only guessing at it.
            wrap_count: (ticks >> 32) as u16,
            flags: 0,
            host_time_ns: 0,
        };
        let mut payload = [0u8; SyncFrame::LEN];
        if sync.encode(&mut payload).is_ok() {
            self.frame(FrameType::Sync, &payload, out);
        }
    }

    /// Produce whatever the device would have transmitted since the last call.
    ///
    /// `budget_blocks` caps how much is generated in one call, so a caller stays responsive.
    pub fn step(&mut self, budget_blocks: usize) -> Outgoing {
        let mut out = Outgoing::new();
        if self.state != State::Capturing {
            return out;
        }

        let cfg_rate = self.engine.config().sample_rate_hz as u64;
        let timer_hz = self.engine.config().timer_hz as u64;
        let period_ticks = self.engine.config().period_ticks();

        // How much simulated time should have elapsed by now.
        let elapsed = self.started.map_or(Duration::ZERO, |s| s.elapsed());
        if let Some(limit) = self.duration
            && elapsed >= limit
        {
            self.finished = true;
        }
        let target_samples = (elapsed.as_secs_f64() * cfg_rate as f64) as u64;
        let generated = self.engine.sample_count();
        let mut owed = target_samples.saturating_sub(generated);
        if let Some(limit) = self.duration {
            let cap = (limit.as_secs_f64() * cfg_rate as f64) as u64;
            owed = owed.min(cap.saturating_sub(generated));
        }

        let mut blocks = 0usize;
        while owed >= SAMPLES_PER_BLOCK as u64 && blocks < budget_blocks {
            let block = self.engine.next_block(SAMPLES_PER_BLOCK);
            owed -= SAMPLES_PER_BLOCK as u64;
            blocks += 1;

            // Events first, so a start event never arrives after the samples it describes.
            if !block.events.is_empty() {
                let mut payload = [0u8; MAX_PAYLOAD];
                let with_value = block.events.iter().any(|e| e.value.is_some());
                if let Ok(n) = EventBlock::encode(&mut payload, &block.events, with_value) {
                    self.frame(FrameType::Event, &payload[..n], &mut out);
                }
            }

            let drop_this = self.faults.drop_block_prob > 0.0
                && self.engine.rng_mut().chance(self.faults.drop_block_prob);
            if drop_this {
                // A dropped block is exactly what a device buffer overflow looks like from
                // the host: a sequence gap and a timestamp jump. Both are reported.
                self.dropped_samples += SAMPLES_PER_BLOCK as u32;
                self.buffer_overflows += 1;
                self.overflow_pending = true;
                self.emitted_ticks += SAMPLES_PER_BLOCK as u64 * period_ticks as u64;
                let _ = self.next_seq(); // burn a sequence number, so the host sees the gap
                continue;
            }

            let mut payload = [0u8; MAX_PAYLOAD];
            let encoded = SampleBlock::encode_uniform(
                &mut payload,
                block.t0_ticks,
                period_ticks,
                &block.samples,
            );
            if let Ok(n) = encoded {
                let before = out.len();
                self.frame(FrameType::CurrentSamples, &payload[..n], &mut out);
                if self.overflow_pending {
                    self.overflow_pending = false;
                    // Report the loss the moment it can be attached to a frame.
                    self.send_error(ErrorCode::BufferOverflow, &mut out);
                }
                // Corrupt a byte occasionally, so the host's CRC rejection and COBS resync
                // are exercised by something other than a unit test.
                if self.faults.corrupt_byte_prob > 0.0
                    && self.engine.rng_mut().chance(self.faults.corrupt_byte_prob)
                    && out.len() > before + 2
                {
                    let idx = before + 1;
                    out[idx] ^= 0x40;
                }
            }
            self.emitted_ticks += SAMPLES_PER_BLOCK as u64 * period_ticks as u64;
        }

        // At least 1 Hz, unprompted, as the spec requires.
        if self.last_sync.elapsed() >= Duration::from_millis(500) {
            self.send_sync(&mut out);
            self.last_sync = Instant::now();
        }

        let _ = timer_hz;
        out
    }

    /// Generate `count` samples immediately, ignoring wall time.
    ///
    /// Used by tests and by the golden-file generator, where a capture must be reproducible
    /// rather than paced.
    pub fn step_samples(&mut self, count: usize) -> Outgoing {
        let mut out = Outgoing::new();
        let period_ticks = self.engine.config().period_ticks();
        let mut remaining = count;
        while remaining >= SAMPLES_PER_BLOCK {
            let block = self.engine.next_block(SAMPLES_PER_BLOCK);
            remaining -= SAMPLES_PER_BLOCK;

            if !block.events.is_empty() {
                let mut payload = [0u8; MAX_PAYLOAD];
                let with_value = block.events.iter().any(|e| e.value.is_some());
                if let Ok(n) = EventBlock::encode(&mut payload, &block.events, with_value) {
                    self.frame(FrameType::Event, &payload[..n], &mut out);
                }
            }

            let mut payload = [0u8; MAX_PAYLOAD];
            if let Ok(n) = SampleBlock::encode_uniform(
                &mut payload,
                block.t0_ticks,
                period_ticks,
                &block.samples,
            ) {
                self.frame(FrameType::CurrentSamples, &payload[..n], &mut out);
            }
            self.emitted_ticks += SAMPLES_PER_BLOCK as u64 * period_ticks as u64;
        }
        out
    }

    /// Force the device into the capturing state, for tests that skip the handshake.
    pub fn force_capturing(&mut self) {
        self.state = State::Capturing;
        self.started = Some(Instant::now());
    }
}

/// The magic a simulated device reports, so a capture can be identified as synthetic.
pub const SIM_DEVICE_MAGIC: u32 = DEVICE_MAGIC;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Profile;
    use wattson_protocol::{Hello, ids::EVENT_RADIO_START};

    fn hello_bytes() -> Vec<u8> {
        let hello = Hello {
            proto_version: PROTOCOL_VERSION,
            nonce: 1,
            reserved: 0,
        };
        let mut payload = [0u8; Hello::LEN];
        hello.encode(&mut payload).unwrap();
        let mut buf = [0u8; MAX_ENCODED];
        let n = encode_frame(FrameType::Hello, 0, &payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn command(ty: FrameType, payload: &[u8], seq: u8) -> Vec<u8> {
        let mut buf = [0u8; MAX_ENCODED];
        let n = encode_frame(ty, seq, payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn config_bytes(rate: u32) -> Vec<u8> {
        let cfg = Config {
            sample_rate_hz: rate,
            averaging: 1,
            conv_time_code: 0,
            gpio_mask: 0,
            shunt_micro_ohm: 100_000,
            flags: 0,
            reserved: 0,
        };
        let mut payload = [0u8; Config::LEN];
        cfg.encode(&mut payload).unwrap();
        command(FrameType::Config, &payload, 1)
    }

    fn decode_all(bytes: &[u8]) -> Vec<u8> {
        let mut d = Decoder::new();
        let mut kinds = Vec::new();
        d.feed(bytes, &mut |_, f| kinds.push(f.type_byte()));
        kinds
    }

    fn device() -> SimDevice {
        SimDevice::new(SimConfig::new(Profile::ble_sensor()))
    }

    #[test]
    fn hello_is_answered_with_device_info() {
        let mut d = device();
        let out = d.receive(&hello_bytes());
        assert_eq!(decode_all(&out), vec![FrameType::DeviceInfo.as_u8()]);
    }

    /// Streaming before configuration would produce samples at an unknown rate.
    #[test]
    fn starting_before_configuring_is_refused() {
        let mut d = device();
        d.receive(&hello_bytes());
        let out = d.receive(&command(FrameType::StartCapture, &[], 2));
        assert_eq!(decode_all(&out), vec![FrameType::Error.as_u8()]);
        assert!(!d.is_capturing());
    }

    /// A device that cannot meet a rate must say so rather than quietly dropping samples.
    #[test]
    fn an_impossible_rate_is_refused_rather_than_silently_missed() {
        let mut d = device();
        d.receive(&hello_bytes());
        let out = d.receive(&config_bytes(50_000_000));
        assert_eq!(decode_all(&out), vec![FrameType::Error.as_u8()]);

        let out = d.receive(&config_bytes(50_000));
        assert!(out.is_empty(), "a valid config needs no reply in v1.0");
    }

    #[test]
    fn a_full_handshake_reaches_the_capturing_state() {
        let mut d = device();
        d.receive(&hello_bytes());
        d.receive(&config_bytes(50_000));
        d.receive(&command(FrameType::StartCapture, &[], 2));
        assert!(d.is_capturing());
        d.receive(&command(FrameType::StopCapture, &[], 3));
        assert!(!d.is_capturing());
        assert!(d.is_finished());
    }

    #[test]
    fn a_sync_request_is_echoed_with_a_tick_count_and_wrap_count() {
        let mut d = device();
        d.force_capturing();
        d.step_samples(SAMPLES_PER_BLOCK * 4);

        let sync = SyncFrame {
            device_ticks: 0,
            wrap_count: 0,
            flags: 0,
            host_time_ns: 999,
        };
        let mut payload = [0u8; SyncFrame::LEN];
        sync.encode(&mut payload).unwrap();
        let out = d.receive(&command(FrameType::Sync, &payload, 9));

        let mut decoder = Decoder::new();
        let mut got: Option<SyncFrame> = None;
        decoder.feed(&out, &mut |_, f| {
            if let Frame::Sync(s) = f {
                got = Some(s);
            }
        });
        let s = got.expect("a SYNC echo");
        assert!(
            s.device_ticks > 0,
            "the echo must carry the device's own tick count"
        );
    }

    #[test]
    fn generated_blocks_decode_as_samples_and_events() {
        let mut d = device();
        d.force_capturing();
        let out = d.step_samples(SAMPLES_PER_BLOCK * 200);

        let mut decoder = Decoder::new();
        let mut samples = 0usize;
        let mut events = 0usize;
        decoder.feed(&out, &mut |_, f| match f {
            Frame::CurrentSamples(b) => samples += b.len(),
            Frame::Event(b) => events += b.len(),
            _ => {}
        });
        assert_eq!(samples, SAMPLES_PER_BLOCK * 200);
        assert!(
            events > 0,
            "a BLE sensor profile must produce firmware events"
        );
        assert_eq!(
            decoder.stats().errors(),
            0,
            "the device must emit only valid frames"
        );
    }

    #[test]
    fn markers_come_back_stamped_in_device_time() {
        let mut d = device();
        d.force_capturing();
        d.step_samples(SAMPLES_PER_BLOCK * 10);

        let m = Marker {
            device_ticks: 0,
            marker_id: 7,
            value: 42,
        };
        let mut payload = [0u8; Marker::LEN];
        m.encode(&mut payload).unwrap();
        let out = d.receive(&command(FrameType::Marker, &payload, 5));

        let mut decoder = Decoder::new();
        let mut got: Option<Marker> = None;
        decoder.feed(&out, &mut |_, f| {
            if let Frame::Marker(m) = f {
                got = Some(m);
            }
        });
        let m = got.expect("a stamped marker");
        assert_eq!(m.marker_id, 7);
        assert_eq!(m.value, 42);
        assert!(
            m.device_ticks > 0,
            "the device must stamp the marker on receipt"
        );
    }

    /// Fault injection has to actually reach the host's recovery paths, or those paths are
    /// only ever exercised by unit tests that construct the damage by hand.
    #[test]
    fn fault_injection_produces_real_damage() {
        let mut cfg = SimConfig::new(Profile::ble_sensor());
        cfg.faults = FaultInjection {
            drop_block_prob: 0.05,
            corrupt_byte_prob: 0.05,
            error_frame_prob: 0.0,
        };
        let mut d = SimDevice::new(cfg);
        d.force_capturing();

        // `step` is wall-clock paced, so drive it until it has produced enough.
        let mut all = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline && all.len() < 400_000 {
            all.extend(d.step(64));
        }

        let mut decoder = Decoder::new();
        decoder.feed(&all, &mut |_, _| {});
        let stats = decoder.stats();
        assert!(
            all.len() > 10_000,
            "the device produced almost nothing to damage"
        );
        assert!(
            stats.crc_errors > 0 || stats.seq_gaps > 0,
            "fault injection produced no observable damage: {stats:?}"
        );
        assert!(
            d.dropped_samples() > 0,
            "dropped blocks must be admitted to"
        );
    }

    #[test]
    fn events_precede_the_samples_they_describe() {
        let mut d = device();
        d.force_capturing();
        let out = d.step_samples(SAMPLES_PER_BLOCK * 400);

        let mut decoder = Decoder::new();
        let mut last_sample_end = 0u32;
        let mut violations = 0;
        decoder.feed(&out, &mut |_, f| match f {
            Frame::CurrentSamples(b) => {
                last_sample_end = b.t0() + b.len() as u32 * b.period_ticks();
            }
            Frame::Event(b) => {
                for e in b.iter() {
                    if e.id == EVENT_RADIO_START && e.timestamp + 1 < last_sample_end {
                        violations += 1;
                    }
                }
            }
            _ => {}
        });
        assert_eq!(
            violations, 0,
            "an event arrived after the samples it describes"
        );
    }
}
