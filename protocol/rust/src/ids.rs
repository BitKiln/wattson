//! Event id allocation policy.
//!
//! Event ids are opaque `u16` on the wire — no strings, no enter/exit bit. Mapping an id to
//! a name, and pairing a start id with a stop id, is entirely the job of the capture's
//! metadata file. That keeps firmware overhead at 6 or 10 bytes per event.
//!
//! The conventions below are *recommended and unenforced*; nothing in the decoder depends on
//! them. They exist so that independently written firmware and tooling tend to agree.
//!
//! ```text
//! high byte = category, low byte = slot
//!   even slot -> start of a scope
//!   odd  slot -> matching stop
//!
//! 0x0000              invalid, never transmitted
//! 0x0100..=0x01FF     radio
//! 0x0200..=0x02FF     sensors
//! 0x0300..=0x03FF     storage
//! 0x0400..=0x04FF     compute
//! 0x0500..=0x05FF     power management / sleep
//! 0x0600..=0x0FFF     unassigned, free for applications
//! 0x1000..=0xFEFF     application-defined
//! 0xFF00..=0xFFFF     reserved for profiler-internal events
//! ```

use crate::types::EventId;

/// Not a valid event id; a device must never transmit this.
pub const EVENT_INVALID: EventId = EventId(0x0000);

/// First id in the profiler-internal reserved range.
pub const RESERVED_INTERNAL_START: u16 = 0xFF00;

// Profiler-internal events. These originate in the profiler MCU, not the device under test.

/// The profiler's sample ring buffer overflowed; samples were lost at this instant.
pub const EVENT_BUFFER_OVERFLOW: EventId = EventId(0xFF01);
/// The effective sample rate changed at this instant.
pub const EVENT_RATE_CHANGE: EventId = EventId(0xFF02);
/// Capture started.
pub const EVENT_CAPTURE_START: EventId = EventId(0xFF10);
/// Capture stopped.
pub const EVENT_CAPTURE_STOP: EventId = EventId(0xFF11);

// Conventional ids used by the reference simulator profiles and the examples.

pub const EVENT_RADIO_START: EventId = EventId(0x0101);
pub const EVENT_RADIO_STOP: EventId = EventId(0x0102);
pub const EVENT_PACKET_TX: EventId = EventId(0x0110);
pub const EVENT_SENSOR_READ_START: EventId = EventId(0x0201);
pub const EVENT_SENSOR_READ_STOP: EventId = EventId(0x0202);
pub const EVENT_SLEEP_ENTER: EventId = EventId(0x0501);
pub const EVENT_SLEEP_EXIT: EventId = EventId(0x0502);

impl EventId {
    /// `true` if this id lies in the profiler-internal reserved range.
    #[inline]
    pub const fn is_internal(self) -> bool {
        self.0 >= RESERVED_INTERNAL_START
    }

    /// `true` if this id may legally appear on the wire.
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// The recommended category byte.
    #[inline]
    pub const fn category(self) -> u8 {
        (self.0 >> 8) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_range_classified() {
        assert!(EVENT_BUFFER_OVERFLOW.is_internal());
        assert!(!EVENT_RADIO_START.is_internal());
        assert!(!EVENT_INVALID.is_valid());
        assert_eq!(EVENT_SENSOR_READ_START.category(), 0x02);
    }
}
