//! Golden byte vectors: the conformance test for any independent implementation.
//!
//! An implementation is conformant when it reproduces these bytes exactly and accepts them on
//! decode. That includes the phase-2 C firmware, which will be written from
//! `protocol/spec/frames.md` rather than from this crate — so these files, not this code, are
//! what the two have in common.
//!
//! To regenerate after a deliberate format change:
//!
//! ```text
//! WATTSON_BLESS=1 cargo test -p wattson-protocol --test vectors
//! ```
//!
//! A change to any of these bytes is a wire-format change. If it is not accompanied by a
//! version bump and a note in the spec, it is a bug.

use std::path::{Path, PathBuf};

use wattson_protocol::{
    Caps, Config, Decoder, DeviceError, DeviceInfo, ErrorCode, EventBlock, EventId, EventRecord,
    Frame, FrameType, GpioBlock, GpioRecord, Hello, MAX_ENCODED, MAX_PAYLOAD, Marker,
    PROTOCOL_VERSION, Sample, SampleBlock, SyncFrame, encode_frame,
};

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../spec/vectors")
}

/// Render bytes as annotated hex: 16 per line, so a diff points at the field that changed.
fn to_hex(name: &str, bytes: &[u8]) -> String {
    let mut out = format!("# {name}\n# {} bytes on the wire\n", bytes.len());
    for chunk in bytes.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
        out.push_str(&hex.join(" "));
        out.push('\n');
    }
    out
}

fn from_hex(text: &str) -> Vec<u8> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .filter_map(|t| u8::from_str_radix(t, 16).ok())
        .collect()
}

/// Compare against the checked-in vector, or write it when blessing.
fn check(name: &str, bytes: &[u8]) {
    let path = vectors_dir().join(format!("{name}.hex"));

    if std::env::var("WATTSON_BLESS").is_ok() {
        std::fs::create_dir_all(vectors_dir()).expect("create vectors dir");
        std::fs::write(&path, to_hex(name, bytes)).expect("write vector");
        return;
    }

    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden vector {}: {e}\n\
             Run WATTSON_BLESS=1 cargo test -p wattson-protocol --test vectors to create it.",
            path.display()
        )
    });
    let expected = from_hex(&text);

    assert_eq!(
        bytes,
        expected.as_slice(),
        "\n{name} no longer matches its golden vector.\n\
         This is a WIRE FORMAT CHANGE. Any independent implementation, including firmware,\n\
         will now disagree with this build.\n\
         If it is deliberate: bump the version, update protocol/spec/frames.md, and re-bless.\n\
         got:      {}\nexpected: {}\n",
        bytes
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
        expected
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Frame one payload onto the wire.
fn framed(ty: FrameType, seq: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; MAX_ENCODED];
    let n = encode_frame(ty, seq, payload, &mut buf).expect("encode");
    buf[..n].to_vec()
}

/// Canonical instances. Values are fixed and arbitrary-looking on purpose: a byte pattern
/// that is all zeros hides endianness and field-offset mistakes.
mod canonical {
    use super::*;

    pub fn hello() -> Hello {
        Hello {
            proto_version: PROTOCOL_VERSION,
            nonce: 0x5741,
            reserved: 0,
        }
    }

    pub fn device_info() -> DeviceInfo {
        DeviceInfo {
            proto_version: PROTOCOL_VERSION,
            device_type: 0x0001,
            serial: DeviceInfo::ascii_field("WS-0001"),
            fw_version: DeviceInfo::ascii_field("1.2.3"),
            fw_build_id: 0x0123_4567_89AB_CDEF,
            timer_hz: 1_000_000,
            max_sample_rate_hz: 50_000,
            shunt_micro_ohm: 100_000,
            adc_full_scale_ua: 1_000_000,
            channel_count: 1,
            gpio_count: 4,
            caps: Caps::GPIO | Caps::MARKERS | Caps::WRAP_COUNT,
        }
    }

    pub fn config() -> Config {
        Config {
            sample_rate_hz: 50_000,
            averaging: 4,
            conv_time_code: 7,
            gpio_mask: 0x0000_000F,
            shunt_micro_ohm: 100_000,
            flags: 0,
            reserved: 0,
        }
    }

    /// Four samples, including a negative current, so sign handling is pinned.
    pub fn samples() -> [(i32, u32); 4] {
        [
            (3_000, 3_300_000),
            (27_000, 3_296_000),
            (78_000, 3_288_300),
            (-250, 3_300_100),
        ]
    }

    pub fn explicit_samples() -> [Sample; 2] {
        [
            Sample {
                timestamp: 0x0001_0000,
                current_ua: 3_000,
                voltage_uv: 3_300_000,
            },
            Sample {
                timestamp: 0x0001_0014,
                current_ua: -250,
                voltage_uv: 3_300_100,
            },
        ]
    }

    pub fn events() -> [EventRecord; 2] {
        [
            EventRecord {
                timestamp: 0x0001_0000,
                id: EventId(0x0101),
                value: None,
            },
            EventRecord {
                timestamp: 0x0001_03B6,
                id: EventId(0x0102),
                value: None,
            },
        ]
    }

    pub fn events_with_value() -> [EventRecord; 1] {
        [EventRecord {
            timestamp: 0x0001_0100,
            id: EventId(0x0110),
            value: Some(64),
        }]
    }

    pub fn gpio() -> [GpioRecord; 2] {
        [
            GpioRecord {
                timestamp: 0x0001_0000,
                state: 0b0000_0001,
            },
            GpioRecord {
                timestamp: 0x0001_0032,
                state: 0b0000_1011,
            },
        ]
    }

    pub fn sync() -> SyncFrame {
        SyncFrame {
            device_ticks: 0x1234_5678,
            wrap_count: 3,
            flags: 0,
            host_time_ns: 0x0000_00AB_CDEF_0123,
        }
    }

    pub fn marker() -> Marker {
        Marker {
            device_ticks: 0x0001_0000,
            marker_id: 7,
            value: 42,
        }
    }

    pub fn error() -> DeviceError {
        DeviceError {
            code: ErrorCode::BufferOverflow as u16,
            detail: 0x0002,
            dropped_samples: 128,
            buffer_overflows: 1,
            context: 0,
        }
    }
}

#[test]
fn golden_vectors_match() {
    let mut payload = [0u8; MAX_PAYLOAD];

    let n = canonical::hello().encode(&mut payload).unwrap();
    check("hello", &framed(FrameType::Hello, 0, &payload[..n]));

    let n = canonical::device_info().encode(&mut payload).unwrap();
    check(
        "device_info",
        &framed(FrameType::DeviceInfo, 1, &payload[..n]),
    );

    let n = canonical::config().encode(&mut payload).unwrap();
    check("config", &framed(FrameType::Config, 2, &payload[..n]));

    check("start_capture", &framed(FrameType::StartCapture, 3, &[]));
    check("stop_capture", &framed(FrameType::StopCapture, 4, &[]));

    // The default sample layout: no per-sample timestamp, reconstructed from t0 and period.
    let n =
        SampleBlock::encode_uniform(&mut payload, 0x0001_0000, 20, &canonical::samples()).unwrap();
    check(
        "current_samples_uniform",
        &framed(FrameType::CurrentSamples, 5, &payload[..n]),
    );

    let n = SampleBlock::encode_explicit(&mut payload, &canonical::explicit_samples()).unwrap();
    check(
        "current_samples_explicit",
        &framed(FrameType::CurrentSamples, 6, &payload[..n]),
    );

    let n = EventBlock::encode(&mut payload, &canonical::events(), false).unwrap();
    check("event", &framed(FrameType::Event, 7, &payload[..n]));

    let n = EventBlock::encode(&mut payload, &canonical::events_with_value(), true).unwrap();
    check(
        "event_with_value",
        &framed(FrameType::Event, 8, &payload[..n]),
    );

    let n = GpioBlock::encode(&mut payload, &canonical::gpio()).unwrap();
    check(
        "gpio_event",
        &framed(FrameType::GpioEvent, 9, &payload[..n]),
    );

    let n = canonical::sync().encode(&mut payload).unwrap();
    check("sync", &framed(FrameType::Sync, 10, &payload[..n]));

    let n = canonical::marker().encode(&mut payload).unwrap();
    check("marker", &framed(FrameType::Marker, 11, &payload[..n]));

    let n = canonical::error().encode(&mut payload).unwrap();
    check("error", &framed(FrameType::Error, 12, &payload[..n]));
}

/// Every vector must decode back to the value that produced it.
///
/// The other half of conformance: reproducing the bytes is not enough if a decoder cannot
/// read them.
#[test]
fn golden_vectors_decode_to_the_values_that_produced_them() {
    let dir = vectors_dir();
    let read = |name: &str| -> Vec<u8> {
        let path = dir.join(format!("{name}.hex"));
        from_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing {}: {e}; run with WATTSON_BLESS=1 first",
                path.display()
            )
        }))
    };

    let decode_one = |bytes: &[u8], f: &mut dyn FnMut(Frame<'_>)| {
        let mut dec = Decoder::new();
        let mut seen = 0;
        dec.feed(bytes, &mut |_, frame| {
            seen += 1;
            f(frame);
        });
        assert_eq!(seen, 1, "a vector must contain exactly one frame");
        assert_eq!(
            dec.stats().errors(),
            0,
            "a golden vector must decode cleanly"
        );
    };

    decode_one(&read("hello"), &mut |f| match f {
        Frame::Hello(h) => assert_eq!(h, canonical::hello()),
        other => panic!("expected HELLO, got {other:?}"),
    });

    decode_one(&read("device_info"), &mut |f| match f {
        Frame::DeviceInfo(i) => {
            assert_eq!(i, canonical::device_info());
            assert_eq!(i.serial_str(), "WS-0001");
            assert_eq!(i.fw_version_str(), "1.2.3");
        }
        other => panic!("expected DEVICE_INFO, got {other:?}"),
    });

    decode_one(&read("config"), &mut |f| match f {
        Frame::Config(c) => assert_eq!(c, canonical::config()),
        other => panic!("expected CONFIG, got {other:?}"),
    });

    decode_one(&read("start_capture"), &mut |f| {
        assert!(matches!(f, Frame::StartCapture));
    });
    decode_one(&read("stop_capture"), &mut |f| {
        assert!(matches!(f, Frame::StopCapture));
    });

    decode_one(&read("current_samples_uniform"), &mut |f| match f {
        Frame::CurrentSamples(b) => {
            let got: Vec<Sample> = b.iter().collect();
            let want = canonical::samples();
            assert_eq!(got.len(), want.len());
            for (i, s) in got.iter().enumerate() {
                assert_eq!(s.current_ua, want[i].0);
                assert_eq!(s.voltage_uv, want[i].1);
                // Reconstructed, not transmitted.
                assert_eq!(s.timestamp, 0x0001_0000 + 20 * i as u32);
            }
        }
        other => panic!("expected CURRENT_SAMPLES, got {other:?}"),
    });

    decode_one(&read("current_samples_explicit"), &mut |f| match f {
        Frame::CurrentSamples(b) => {
            let got: Vec<Sample> = b.iter().collect();
            assert_eq!(got, canonical::explicit_samples().to_vec());
        }
        other => panic!("expected CURRENT_SAMPLES, got {other:?}"),
    });

    decode_one(&read("event"), &mut |f| match f {
        Frame::Event(b) => {
            assert!(!b.has_values());
            assert_eq!(b.iter().collect::<Vec<_>>(), canonical::events().to_vec());
        }
        other => panic!("expected EVENT, got {other:?}"),
    });

    decode_one(&read("event_with_value"), &mut |f| match f {
        Frame::Event(b) => {
            assert!(b.has_values());
            assert_eq!(
                b.iter().collect::<Vec<_>>(),
                canonical::events_with_value().to_vec()
            );
        }
        other => panic!("expected EVENT, got {other:?}"),
    });

    decode_one(&read("gpio_event"), &mut |f| match f {
        Frame::GpioEvent(b) => {
            assert_eq!(b.iter().collect::<Vec<_>>(), canonical::gpio().to_vec());
        }
        other => panic!("expected GPIO_EVENT, got {other:?}"),
    });

    decode_one(&read("sync"), &mut |f| match f {
        Frame::Sync(s) => {
            assert_eq!(s, canonical::sync());
            assert_eq!(
                s.wrap_count, 3,
                "the wrap count is what makes wraparound verifiable"
            );
        }
        other => panic!("expected SYNC, got {other:?}"),
    });

    decode_one(&read("marker"), &mut |f| match f {
        Frame::Marker(m) => assert_eq!(m, canonical::marker()),
        other => panic!("expected MARKER, got {other:?}"),
    });

    decode_one(&read("error"), &mut |f| match f {
        Frame::Error(e) => {
            assert_eq!(e, canonical::error());
            assert!(e.lost_data(), "this vector reports lost samples");
        }
        other => panic!("expected ERROR, got {other:?}"),
    });
}

/// No encoded frame may contain the delimiter, or a receiver cannot find frame boundaries.
///
/// Checked against the stored bytes rather than freshly encoded ones, so it holds for whatever
/// is actually committed.
#[test]
fn no_stored_vector_contains_an_embedded_delimiter() {
    let dir = vectors_dir();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));

    let mut checked = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("hex") {
            continue;
        }
        let bytes = from_hex(&std::fs::read_to_string(&path).unwrap());
        assert!(!bytes.is_empty(), "{} is empty", path.display());
        assert_eq!(
            *bytes.last().expect("non-empty"),
            0x00,
            "{} must end with the frame delimiter",
            path.display()
        );
        assert!(
            !bytes[..bytes.len() - 1].contains(&0x00),
            "{} contains an embedded delimiter, which would break resynchronisation",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 12,
        "expected a vector per frame type, found {checked}"
    );
}
