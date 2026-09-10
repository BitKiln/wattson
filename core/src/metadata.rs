//! Capture metadata: what the numbers on the wire mean.
//!
//! The wire carries opaque `u16` event ids and nothing else — no names, no enter/exit bit.
//! That keeps firmware overhead at six bytes per event. Everything human-readable lives here
//! and is stored in the capture's METADATA chunk, so a `.pprof` file is self-describing
//! forever after, even if the firmware it came from is long gone.
//!
//! Authored as TOML, stored as CBOR. TOML because a person writes it; CBOR because it is
//! compact, preserves integer keys, and is self-describing enough for a third-party tool to
//! inspect without this crate.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CaptureError;

/// One named firmware event, and how to pair its start with its stop.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDef {
    pub name: String,
    /// Id emitted when the scope is entered.
    pub start_id: u16,
    /// Id emitted when it is left. Absent for point events, which have no duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// What the event's `value` field means, e.g. `bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_units: Option<String>,
    /// Signalling latency to subtract from this event's timestamps, in nanoseconds.
    ///
    /// The instrumentation transport is the real accuracy floor for event boundaries: UART at
    /// 1 Mbaud costs about 10 µs of byte time, roughly 1% of a 950 µs transmit window, while
    /// a GPIO toggle costs well under a microsecond. Carrying the number per event means a
    /// capture can be corrected after the fact instead of being quietly wrong.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub latency_compensation_ns: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl EventDef {
    pub fn point(name: impl Into<String>, start_id: u16) -> EventDef {
        EventDef {
            name: name.into(),
            start_id,
            ..Default::default()
        }
    }

    pub fn scope(name: impl Into<String>, start_id: u16, stop_id: u16) -> EventDef {
        EventDef {
            name: name.into(),
            start_id,
            stop_id: Some(stop_id),
            ..Default::default()
        }
    }

    /// `true` if this event has a duration, and therefore an energy cost.
    pub const fn is_scope(&self) -> bool {
        self.stop_id.is_some()
    }
}

/// Everything a capture knows about itself beyond its raw samples.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureMetadata {
    /// Event definitions, in declaration order.
    #[serde(default, rename = "event")]
    pub events: Vec<EventDef>,
    /// Free-form notes: firmware git hash, board revision, test name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<String, String>,
    /// The exact command that produced this capture, so it can be reproduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<String>,
    /// The device URI the capture came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Version of the tool that wrote the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_version: Option<String>,
    /// Host OS description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Fitted host-clock relationship, `host_ns ≈ a * device_ticks + b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_fit: Option<StoredClockFit>,
    /// Supply voltage to assume when the capture has no voltage channel, in microvolts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assumed_supply_uv: Option<u32>,
}

/// A persisted clock fit.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredClockFit {
    pub a: f64,
    pub b: f64,
    pub samples: usize,
    pub residual_ns: f64,
}

impl From<crate::time::ClockFit> for StoredClockFit {
    fn from(f: crate::time::ClockFit) -> Self {
        StoredClockFit {
            a: f.a,
            b: f.b,
            samples: f.samples,
            residual_ns: f.residual_ns,
        }
    }
}

impl CaptureMetadata {
    /// Read a TOML metadata file.
    pub fn from_toml_file(path: &Path) -> Result<CaptureMetadata, CaptureError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<CaptureMetadata, CaptureError> {
        let meta: CaptureMetadata =
            toml::from_str(text).map_err(|e| CaptureError::Metadata(e.to_string()))?;
        meta.validate()?;
        Ok(meta)
    }

    pub fn to_toml_string(&self) -> Result<String, CaptureError> {
        toml::to_string_pretty(self).map_err(|e| CaptureError::Metadata(e.to_string()))
    }

    /// Encode for the METADATA chunk.
    pub fn to_cbor(&self) -> Result<Vec<u8>, CaptureError> {
        let mut out = Vec::new();
        ciborium::into_writer(self, &mut out).map_err(|e| CaptureError::Metadata(e.to_string()))?;
        Ok(out)
    }

    pub fn from_cbor(bytes: &[u8]) -> Result<CaptureMetadata, CaptureError> {
        ciborium::from_reader(bytes).map_err(|e| CaptureError::Metadata(e.to_string()))
    }

    /// Reject definitions that would make analysis ambiguous.
    pub fn validate(&self) -> Result<(), CaptureError> {
        let mut seen_ids: BTreeMap<u16, &str> = BTreeMap::new();
        let mut seen_names: BTreeMap<&str, ()> = BTreeMap::new();

        for def in &self.events {
            if def.name.is_empty() {
                return Err(CaptureError::Metadata("an event has an empty name".into()));
            }
            if def.start_id == 0 {
                return Err(CaptureError::Metadata(format!(
                    "event {:?} uses id 0, which is reserved as invalid",
                    def.name
                )));
            }
            if seen_names.insert(def.name.as_str(), ()).is_some() {
                return Err(CaptureError::Metadata(format!(
                    "event name {:?} is defined twice",
                    def.name
                )));
            }
            for id in [Some(def.start_id), def.stop_id].into_iter().flatten() {
                if let Some(other) = seen_ids.insert(id, &def.name) {
                    if other != def.name {
                        return Err(CaptureError::Metadata(format!(
                            "event id 0x{id:04X} is claimed by both {other:?} and {:?}",
                            def.name
                        )));
                    }
                }
            }
            if def.stop_id == Some(def.start_id) {
                return Err(CaptureError::Metadata(format!(
                    "event {:?} uses the same id for start and stop, so its scopes cannot be paired",
                    def.name
                )));
            }
        }
        Ok(())
    }

    /// Build the id lookup used by the statistics layer.
    pub fn event_map(&self) -> EventMap {
        EventMap::new(self.events.clone())
    }

    /// Look an event up by name.
    pub fn event(&self, name: &str) -> Option<&EventDef> {
        self.events.iter().find(|e| e.name == name)
    }
}

/// Fast lookup from a wire id to what it means.
#[derive(Clone, Debug, Default)]
pub struct EventMap {
    defs: Vec<EventDef>,
    /// id -> (index into `defs`, whether it is the stop id)
    by_id: BTreeMap<u16, (usize, bool)>,
}

impl EventMap {
    pub fn new(defs: Vec<EventDef>) -> EventMap {
        let mut by_id = BTreeMap::new();
        for (i, d) in defs.iter().enumerate() {
            by_id.insert(d.start_id, (i, false));
            if let Some(stop) = d.stop_id {
                by_id.insert(stop, (i, true));
            }
        }
        EventMap { defs, by_id }
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn defs(&self) -> &[EventDef] {
        &self.defs
    }

    /// The definition an id belongs to, and whether the id is its stop.
    pub fn lookup(&self, id: u16) -> Option<(&EventDef, bool)> {
        self.by_id
            .get(&id)
            .map(|&(i, is_stop)| (&self.defs[i], is_stop))
    }

    /// A display name for an id, falling back to hex for ids the capture never declared.
    ///
    /// Unknown ids are shown, not hidden: an event the metadata forgot is still evidence.
    pub fn name_of(&self, id: u16) -> String {
        match self.lookup(id) {
            Some((def, false)) => def.name.clone(),
            Some((def, true)) => format!("{}:stop", def.name),
            None => format!("0x{id:04X}"),
        }
    }

    pub fn by_name(&self, name: &str) -> Option<&EventDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// Every event that has a duration, and therefore an energy cost.
    pub fn scopes(&self) -> impl Iterator<Item = &EventDef> {
        self.defs.iter().filter(|d| d.is_scope())
    }
}

/// The event map the reference simulator profiles produce, so a capture made without an
/// explicit `--metadata` file is still readable.
pub fn default_simulator_metadata() -> CaptureMetadata {
    use wattson_protocol::ids::*;
    CaptureMetadata {
        events: vec![
            EventDef::scope("BLE_TX", EVENT_RADIO_START.0, EVENT_RADIO_STOP.0),
            EventDef::scope(
                "SENSOR_READ",
                EVENT_SENSOR_READ_START.0,
                EVENT_SENSOR_READ_STOP.0,
            ),
            EventDef::scope("SLEEP", EVENT_SLEEP_ENTER.0, EVENT_SLEEP_EXIT.0),
            EventDef {
                name: "PACKET_TX".into(),
                start_id: EVENT_PACKET_TX.0,
                value_units: Some("bytes".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
invocation = "wattson capture --device sim://ble-sensor"

[[event]]
name = "BLE_TX"
start_id = 0x0101
stop_id = 0x0102
category = "radio"
latency_compensation_ns = 10000

[[event]]
name = "SENSOR_READ"
start_id = 0x0201
stop_id = 0x0202

[[event]]
name = "PACKET_TX"
start_id = 0x0110
value_units = "bytes"

[notes]
firmware = "v1.3.0-4-gabc1234"
board = "rev-C"
"#;

    #[test]
    fn parses_the_documented_toml_shape() {
        let m = CaptureMetadata::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(m.events.len(), 3);
        let tx = m.event("BLE_TX").unwrap();
        assert_eq!(tx.start_id, 0x0101);
        assert_eq!(tx.stop_id, Some(0x0102));
        assert_eq!(tx.latency_compensation_ns, 10_000);
        assert!(tx.is_scope());
        assert!(!m.event("PACKET_TX").unwrap().is_scope());
        assert_eq!(m.notes.get("board").map(String::as_str), Some("rev-C"));
    }

    /// A capture must stay self-describing forever, so metadata survives the CBOR chunk
    /// exactly as it was authored.
    #[test]
    fn metadata_round_trips_through_cbor() {
        let m = CaptureMetadata::from_toml_str(EXAMPLE).unwrap();
        let back = CaptureMetadata::from_cbor(&m.to_cbor().unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn metadata_round_trips_through_toml() {
        let m = CaptureMetadata::from_toml_str(EXAMPLE).unwrap();
        let back = CaptureMetadata::from_toml_str(&m.to_toml_string().unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let bad = r#"
[[event]]
name = "A"
start_id = 1
[[event]]
name = "A"
start_id = 2
"#;
        let err = CaptureMetadata::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("defined twice"), "{err}");
    }

    #[test]
    fn an_id_claimed_by_two_events_is_rejected() {
        let bad = r#"
[[event]]
name = "A"
start_id = 1
stop_id = 2
[[event]]
name = "B"
start_id = 2
"#;
        let err = CaptureMetadata::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("claimed by both"), "{err}");
    }

    /// Start and stop sharing an id makes pairing impossible, so it must be caught at load
    /// time rather than producing nonsense durations later.
    #[test]
    fn an_event_that_starts_and_stops_on_the_same_id_is_rejected() {
        let bad = r#"
[[event]]
name = "A"
start_id = 7
stop_id = 7
"#;
        let err = CaptureMetadata::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("same id"), "{err}");
    }

    #[test]
    fn id_zero_is_rejected_because_the_wire_reserves_it() {
        let bad = "[[event]]\nname = \"A\"\nstart_id = 0\n";
        let err = CaptureMetadata::from_toml_str(bad).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn empty_metadata_is_valid() {
        let m = CaptureMetadata::from_toml_str("").unwrap();
        assert!(m.events.is_empty());
        assert!(m.event_map().is_empty());
    }

    #[test]
    fn the_map_resolves_starts_stops_and_strangers() {
        let m = CaptureMetadata::from_toml_str(EXAMPLE).unwrap();
        let map = m.event_map();
        assert_eq!(map.name_of(0x0101), "BLE_TX");
        assert_eq!(map.name_of(0x0102), "BLE_TX:stop");
        // An id the metadata never declared is still shown, not hidden: an unexplained event
        // is evidence, and swallowing it makes a capture lie by omission.
        assert_eq!(map.name_of(0xABCD), "0xABCD");
        assert_eq!(map.scopes().count(), 2);
        assert!(map.by_name("BLE_TX").is_some());
        let (def, is_stop) = map.lookup(0x0202).unwrap();
        assert_eq!(def.name, "SENSOR_READ");
        assert!(is_stop);
    }

    #[test]
    fn the_simulator_default_map_is_valid_and_covers_the_documented_events() {
        let m = default_simulator_metadata();
        m.validate().unwrap();
        assert!(m.event("BLE_TX").unwrap().is_scope());
        assert!(m.event("SENSOR_READ").unwrap().is_scope());
    }
}
