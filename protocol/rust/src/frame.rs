//! Frame type discriminants and the decoded frame enum.

use crate::types::{
    Config, DeviceError, DeviceInfo, EventBlock, GpioBlock, Hello, Marker, SampleBlock, SyncFrame,
};

/// Frame type byte.
///
/// The table below is normative and is cross-checked against `protocol/spec/frames.md`
/// by `tests/spec_sync.rs` — edit both or neither.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum FrameType {
    /// Host -> device. Handshake request.
    Hello = 0x01,
    /// Device -> host. Identity and capabilities.
    DeviceInfo = 0x02,
    /// Host -> device. Sample rate, GPIO mask, shunt value.
    Config = 0x03,
    /// Host -> device. Begin streaming.
    StartCapture = 0x05,
    /// Host -> device. Stop streaming.
    StopCapture = 0x06,
    /// Device -> host. The bulk sample stream.
    CurrentSamples = 0x10,
    /// Device -> host. Batched firmware events.
    Event = 0x11,
    /// Device -> host. Batched digital edges.
    GpioEvent = 0x12,
    /// Either direction. Clock correlation.
    Sync = 0x13,
    /// Either direction. Host-injected annotation, device-timestamped.
    Marker = 0x14,
    /// Either direction. Typed failure plus loss counters.
    Error = 0x7F,
}

impl FrameType {
    /// Every frame type defined in v1.0, in ascending discriminant order.
    pub const ALL: [FrameType; 11] = [
        FrameType::Hello,
        FrameType::DeviceInfo,
        FrameType::Config,
        FrameType::StartCapture,
        FrameType::StopCapture,
        FrameType::CurrentSamples,
        FrameType::Event,
        FrameType::GpioEvent,
        FrameType::Sync,
        FrameType::Marker,
        FrameType::Error,
    ];

    /// Parse a type byte. Returns `None` for reserved, vendor, and future types — callers
    /// must surface those as [`Frame::Unknown`] rather than an error.
    #[inline]
    pub const fn from_u8(b: u8) -> Option<FrameType> {
        Some(match b {
            0x01 => FrameType::Hello,
            0x02 => FrameType::DeviceInfo,
            0x03 => FrameType::Config,
            0x05 => FrameType::StartCapture,
            0x06 => FrameType::StopCapture,
            0x10 => FrameType::CurrentSamples,
            0x11 => FrameType::Event,
            0x12 => FrameType::GpioEvent,
            0x13 => FrameType::Sync,
            0x14 => FrameType::Marker,
            0x7F => FrameType::Error,
            _ => return None,
        })
    }

    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Canonical uppercase name, matching the spec tables.
    pub const fn name(self) -> &'static str {
        match self {
            FrameType::Hello => "HELLO",
            FrameType::DeviceInfo => "DEVICE_INFO",
            FrameType::Config => "CONFIG",
            FrameType::StartCapture => "START_CAPTURE",
            FrameType::StopCapture => "STOP_CAPTURE",
            FrameType::CurrentSamples => "CURRENT_SAMPLES",
            FrameType::Event => "EVENT",
            FrameType::GpioEvent => "GPIO_EVENT",
            FrameType::Sync => "SYNC",
            FrameType::Marker => "MARKER",
            FrameType::Error => "ERROR",
        }
    }
}

/// A decoded frame.
///
/// Bulk variants borrow the decoder's internal buffer rather than copying, which is why the
/// decoder hands frames to a callback instead of returning an iterator — see
/// [`crate::decode::Decoder::feed`].
#[derive(Debug)]
pub enum Frame<'a> {
    Hello(Hello),
    DeviceInfo(DeviceInfo),
    Config(Config),
    StartCapture,
    StopCapture,
    CurrentSamples(SampleBlock<'a>),
    Event(EventBlock<'a>),
    GpioEvent(GpioBlock<'a>),
    Sync(SyncFrame),
    Marker(Marker),
    Error(DeviceError),
    /// A type byte this version does not know. Forward compatibility: never an error.
    Unknown {
        ty: u8,
        payload: &'a [u8],
    },
}

impl Frame<'_> {
    /// The type byte this frame was decoded from.
    pub const fn type_byte(&self) -> u8 {
        match self {
            Frame::Hello(_) => FrameType::Hello as u8,
            Frame::DeviceInfo(_) => FrameType::DeviceInfo as u8,
            Frame::Config(_) => FrameType::Config as u8,
            Frame::StartCapture => FrameType::StartCapture as u8,
            Frame::StopCapture => FrameType::StopCapture as u8,
            Frame::CurrentSamples(_) => FrameType::CurrentSamples as u8,
            Frame::Event(_) => FrameType::Event as u8,
            Frame::GpioEvent(_) => FrameType::GpioEvent as u8,
            Frame::Sync(_) => FrameType::Sync as u8,
            Frame::Marker(_) => FrameType::Marker as u8,
            Frame::Error(_) => FrameType::Error as u8,
            Frame::Unknown { ty, .. } => *ty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_roundtrips_through_u8() {
        for ty in FrameType::ALL {
            assert_eq!(FrameType::from_u8(ty.as_u8()), Some(ty), "{}", ty.name());
        }
    }

    #[test]
    fn reserved_types_are_unknown_not_errors() {
        for b in [0x00, 0x04, 0x07, 0x15, 0x40, 0x6F, 0x7E, 0x80, 0xFF] {
            assert_eq!(FrameType::from_u8(b), None, "0x{b:02X} must stay unknown");
        }
    }

    #[test]
    fn all_is_complete_and_sorted() {
        let mut prev = 0u8;
        for ty in FrameType::ALL {
            assert!(ty.as_u8() > prev, "FrameType::ALL must be ascending");
            prev = ty.as_u8();
        }
        let known = (0u8..=0xFF)
            .filter(|b| FrameType::from_u8(*b).is_some())
            .count();
        assert_eq!(
            known,
            FrameType::ALL.len(),
            "FrameType::ALL is missing a variant"
        );
    }
}
