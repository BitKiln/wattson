//! Exporting a capture to formats other tools read.
//!
//! CSV and JSON ship now. VCD and Parquet are named in [`ExportFormat`] and return
//! [`crate::error::ExportError::NotImplemented`] rather than being silently absent, so the
//! CLI can list them as planned and a caller gets a clear answer instead of an unknown-value
//! error.

use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::capture::CaptureReader;
use crate::error::ExportError;
use crate::stats::{EventStats, RegionStats};
use crate::time::TimeSpan;

/// A destination format.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Json,
    /// Waveform format, for opening a capture alongside logic-analyser traces. Phase 3.
    Vcd,
    /// Columnar format, for analysis in a dataframe. Phase 3.
    Parquet,
}

impl ExportFormat {
    pub const fn name(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Vcd => "vcd",
            ExportFormat::Parquet => "parquet",
        }
    }

    /// `true` if this build can actually write it.
    pub const fn is_implemented(self) -> bool {
        matches!(self, ExportFormat::Csv | ExportFormat::Json)
    }
}

impl std::str::FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "csv" => Ok(ExportFormat::Csv),
            "json" => Ok(ExportFormat::Json),
            "vcd" => Ok(ExportFormat::Vcd),
            "parquet" => Ok(ExportFormat::Parquet),
            other => Err(format!(
                "unknown format {other:?}; expected csv, json, vcd, or parquet"
            )),
        }
    }
}

/// Write samples as CSV.
///
/// `decimate` keeps every Nth sample. A 50 ksps hour is 180 million rows, which no
/// spreadsheet will open; decimating is the difference between a usable export and a file
/// that exists only to disappoint.
pub fn samples_csv<W: Write>(
    reader: &mut CaptureReader,
    span: TimeSpan,
    decimate: u32,
    mut out: W,
) -> Result<u64, ExportError> {
    let step = decimate.max(1) as usize;
    writeln!(out, "time_s,current_uA,voltage_uV,power_uW")?;

    let samples = reader.samples_in(span)?;
    let mut written = 0u64;
    for s in samples.iter().step_by(step) {
        let power_uw = (s.current_ua as f64 / 1e6) * (s.voltage_uv as f64 / 1e6) * 1e6;
        writeln!(
            out,
            "{:.9},{},{},{:.3}",
            s.t_ns as f64 / 1e9,
            s.current_ua,
            s.voltage_uv,
            power_uw
        )?;
        written += 1;
    }
    Ok(written)
}

/// Write events as CSV, resolving ids to names through the capture's own metadata.
pub fn events_csv<W: Write>(reader: &mut CaptureReader, mut out: W) -> Result<u64, ExportError> {
    let map = reader.metadata().event_map();
    let events = reader.events()?;
    writeln!(out, "time_s,event_id,name,value")?;
    for e in &events {
        writeln!(
            out,
            "{:.9},0x{:04X},{},{}",
            e.t_ns as f64 / 1e9,
            e.id,
            map.name_of(e.id),
            e.value.map(|v| v.to_string()).unwrap_or_default()
        )?;
    }
    Ok(events.len() as u64)
}

/// Write GPIO edges as CSV.
pub fn gpio_csv<W: Write>(reader: &mut CaptureReader, mut out: W) -> Result<u64, ExportError> {
    let edges = reader.gpio()?;
    writeln!(out, "time_s,state")?;
    for g in &edges {
        writeln!(out, "{:.9},0x{:04X}", g.t_ns as f64 / 1e9, g.state)?;
    }
    Ok(edges.len() as u64)
}

/// Write per-event statistics as CSV.
pub fn event_stats_csv<W: Write>(stats: &[EventStats], mut out: W) -> Result<u64, ExportError> {
    writeln!(
        out,
        "name,occurrences,mean_duration_us,p95_duration_us,mean_energy_uJ,p95_energy_uJ,\
         total_energy_uJ,mean_current_uA,peak_current_uA,duty_cycle,unterminated,\
         orphaned_stops,unsampled"
    )?;
    for s in stats {
        writeln!(
            out,
            "{},{},{:.3},{:.3},{:.4},{:.4},{:.4},{:.1},{},{:.6},{},{},{}",
            s.name,
            s.occurrences,
            s.duration_us.mean,
            s.duration_us.p95,
            s.energy_uj.mean,
            s.energy_uj.p95,
            s.total_energy_uj,
            s.current_mean_ua,
            s.current_peak_ua,
            s.duty_cycle,
            s.unterminated,
            s.orphaned_stops,
            s.unsampled
        )?;
    }
    Ok(stats.len() as u64)
}

/// The machine-readable analysis report, as `analyze --json` prints it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub capture: CaptureInfo,
    pub region: Option<RegionStats>,
    pub events: Vec<EventStats>,
}

/// Everything `wattson info` reports about a capture.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureInfo {
    pub path: String,
    pub format_version: String,
    pub device_serial: String,
    pub firmware_version: String,
    pub timer_hz: u32,
    pub configured_rate_hz: u32,
    /// Derived from the data, which is the only rate safe to integrate with.
    pub effective_rate_hz: f64,
    pub sample_count: u64,
    pub event_count: u64,
    pub gpio_count: u64,
    pub duration_s: f64,
    pub compression: String,
    pub has_voltage: bool,
    pub finalized: bool,
    pub recovered: bool,
    pub gaps: Vec<crate::capture::Gap>,
    pub unknown_chunks: usize,
    pub corrupt_chunks: usize,
}

impl CaptureInfo {
    /// Gather everything `info` reports.
    pub fn gather(reader: &mut CaptureReader) -> Result<CaptureInfo, ExportError> {
        let header = reader.header().clone();
        let integrity = reader.integrity().clone();
        let span = reader.span();
        let events = reader.events()?.len() as u64;
        let gpio = reader.gpio()?.len() as u64;

        Ok(CaptureInfo {
            path: reader.path().display().to_string(),
            format_version: format!("{}.{}", header.format_major, header.format_minor),
            device_serial: header.serial_str().to_string(),
            firmware_version: header.fw_version_str().to_string(),
            timer_hz: header.device_timer_hz,
            configured_rate_hz: header.sample_rate_hz,
            effective_rate_hz: reader.effective_rate_hz(),
            sample_count: reader.sample_count(),
            event_count: events,
            gpio_count: gpio,
            duration_s: span.duration_s(),
            compression: header.compression.name().to_string(),
            has_voltage: header.has(crate::capture::HeaderFlags::HAS_VOLTAGE),
            finalized: integrity.finalized,
            recovered: integrity.recovered,
            gaps: integrity.gaps.clone(),
            unknown_chunks: integrity.unknown_chunks,
            corrupt_chunks: integrity.corrupt_chunks,
        })
    }
}

/// Bundle region and event statistics into one JSON value.
pub fn area_stats_json(region: Option<RegionStats>, events: &[EventStats]) -> serde_json::Value {
    serde_json::json!({ "region": region, "events": events })
}

/// Write any serialisable report as pretty JSON.
pub fn write_json<W: Write, T: Serialize>(value: &T, mut out: W) -> Result<(), ExportError> {
    let text =
        serde_json::to_string_pretty(value).map_err(|e| ExportError::Serialize(e.to_string()))?;
    out.write_all(text.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Refuse a format this build cannot write, by name.
pub fn ensure_implemented(format: ExportFormat) -> Result<(), ExportError> {
    if format.is_implemented() {
        Ok(())
    } else {
        Err(ExportError::NotImplemented(format.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureHeader, CaptureReader, CaptureWriter, WriterOptions};
    use crate::metadata::default_simulator_metadata;
    use std::path::PathBuf;

    fn make_capture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("x.pprof");
        let header = CaptureHeader {
            device_timer_hz: 1_000_000,
            sample_rate_hz: 50_000,
            device_serial: *b"SIM-0001\0\0\0\0\0\0\0\0",
            ..Default::default()
        };
        let mut w = CaptureWriter::create(&path, header, WriterOptions::default()).unwrap();
        for i in 0..1_000u64 {
            w.push_sample(i * 20_000, 3_000 + (i % 10) as i32, Some(3_300_000))
                .unwrap();
        }
        w.push_event(1_000_000, 0x0101, None).unwrap();
        w.push_event(1_950_000, 0x0102, None).unwrap();
        w.push_gpio(500_000, 0b0001).unwrap();
        w.set_metadata(default_simulator_metadata());
        w.finish().unwrap();
        path
    }

    #[test]
    fn samples_csv_has_a_header_and_one_row_per_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();

        let mut out = Vec::new();
        let n = samples_csv(&mut reader, TimeSpan::ALL, 1, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], "time_s,current_uA,voltage_uV,power_uW");
        assert_eq!(n, 1_000);
        assert_eq!(lines.len(), 1_001);
        assert!(lines[1].starts_with("0.000000000,3000,3300000,"));
    }

    /// A 50 ksps hour is 180 million rows; decimation is what makes an export usable.
    #[test]
    fn decimation_keeps_every_nth_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();

        let mut out = Vec::new();
        let n = samples_csv(&mut reader, TimeSpan::ALL, 10, &mut out).unwrap();
        assert_eq!(n, 100);
        // A decimation factor of 0 must not divide by zero or produce nothing.
        let mut out = Vec::new();
        assert_eq!(
            samples_csv(&mut reader, TimeSpan::ALL, 0, &mut out).unwrap(),
            1_000
        );
    }

    #[test]
    fn events_csv_resolves_ids_to_names_from_the_captures_own_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();

        let mut out = Vec::new();
        let n = events_csv(&mut reader, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(n, 2);
        assert!(text.contains("BLE_TX"), "{text}");
        assert!(text.contains("BLE_TX:stop"), "{text}");
        assert!(text.contains("0x0101"), "{text}");
    }

    #[test]
    fn gpio_csv_writes_edges() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();
        let mut out = Vec::new();
        assert_eq!(gpio_csv(&mut reader, &mut out).unwrap(), 1);
        assert!(String::from_utf8(out).unwrap().contains("0x0001"));
    }

    #[test]
    fn capture_info_reports_the_derived_rate_not_only_the_configured_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();
        let info = CaptureInfo::gather(&mut reader).unwrap();

        assert_eq!(info.configured_rate_hz, 50_000);
        assert!((info.effective_rate_hz - 50_000.0).abs() < 1.0);
        assert_eq!(info.sample_count, 1_000);
        assert_eq!(info.event_count, 2);
        assert_eq!(info.gpio_count, 1);
        assert_eq!(info.device_serial, "SIM-0001");
        assert_eq!(info.format_version, "1.0");
        assert!(info.finalized);
        assert!(!info.recovered);
        assert!(info.has_voltage);
    }

    #[test]
    fn info_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_capture(dir.path());
        let mut reader = CaptureReader::open(&path).unwrap();
        let info = CaptureInfo::gather(&mut reader).unwrap();

        let mut out = Vec::new();
        write_json(&info, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let back: CaptureInfo = serde_json::from_str(&text).unwrap();
        assert_eq!(back, info);
    }

    /// Naming a planned format must produce a clear answer, not an unknown-value error.
    #[test]
    fn planned_formats_are_named_and_refused_explicitly() {
        for f in [ExportFormat::Vcd, ExportFormat::Parquet] {
            assert!(!f.is_implemented());
            let err = ensure_implemented(f).unwrap_err().to_string();
            assert!(err.contains(f.name()), "{err}");
            assert!(err.contains("not implemented"), "{err}");
        }
        for f in [ExportFormat::Csv, ExportFormat::Json] {
            assert!(f.is_implemented());
            assert!(ensure_implemented(f).is_ok());
        }
    }

    #[test]
    fn format_names_round_trip() {
        for f in [
            ExportFormat::Csv,
            ExportFormat::Json,
            ExportFormat::Vcd,
            ExportFormat::Parquet,
        ] {
            assert_eq!(f.name().parse::<ExportFormat>().unwrap(), f);
        }
        assert!("xlsx".parse::<ExportFormat>().is_err());
    }

    #[test]
    fn event_stats_csv_has_one_row_per_event() {
        let stats = vec![EventStats {
            name: "BLE_TX".into(),
            occurrences: 5,
            ..Default::default()
        }];
        let mut out = Vec::new();
        assert_eq!(event_stats_csv(&stats, &mut out).unwrap(), 1);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .next()
                .unwrap()
                .starts_with("name,occurrences,")
        );
    }
}
