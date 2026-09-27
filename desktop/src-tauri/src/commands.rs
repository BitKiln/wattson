//! The Tauri command surface.
//!
//! **Scaffold: phase 2.** Nothing here is wired to a window yet.
//!
//! It is written out anyway because it is the argument for how `wattson-core` is shaped. Every
//! command below is a serialisation boundary and nothing else — open a capture, call one core
//! function, return the typed result. There is no arithmetic, no formatting decision, and no
//! second opinion about what a number means.
//!
//! That is only possible because `core` never prints, never reads argv, and never exits. If a
//! command in this file ever needs to compute something, the computation belongs in `core`
//! where the CLI can reach it too — otherwise the project grows two analysis engines that
//! quietly disagree.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use wattson_core::assert::{AssertReport, AssertRule, evaluate_assertions};
use wattson_core::capture::{Bucket, CaptureReader};
use wattson_core::export::CaptureInfo;
use wattson_core::stats::{EventStats, RegionStats, StatsOptions, event_stats, region_stats};
use wattson_core::time::TimeSpan;
use wattson_core::transport::enumerate_devices;

/// One open capture, shared across commands.
#[derive(Default)]
pub struct AppState {
    pub capture: Mutex<Option<CaptureReader>>,
}

/// Errors crossing the IPC boundary keep their shape, so the UI can tell "no device" from
/// "the file is truncated" and say something useful about each.
#[derive(Debug, Serialize, Deserialize)]
pub struct UiError {
    pub kind: String,
    pub message: String,
}

impl<E: std::error::Error> From<E> for UiError {
    fn from(e: E) -> Self {
        UiError {
            kind: std::any::type_name::<E>().to_string(),
            message: e.to_string(),
        }
    }
}

type UiResult<T> = Result<T, UiError>;

/// A window into a capture, as the chart asks for it.
#[derive(Debug, Deserialize)]
pub struct ViewRequest {
    pub start_ns: u64,
    pub end_ns: u64,
    /// One bucket per horizontal pixel.
    pub buckets: usize,
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// List connected devices.
///
/// Blocking, and slow on Windows. Tauri must run this off the UI thread; `core` keeps it a
/// plain function precisely so the caller decides where it runs.
#[tauri::command]
pub async fn list_devices() -> UiResult<Vec<String>> {
    let found = tauri::async_runtime::spawn_blocking(enumerate_devices)
        .await
        .map_err(|e| UiError {
            kind: "join".into(),
            message: e.to_string(),
        })??;
    Ok(found.into_iter().map(|d| d.uri.label()).collect())
}

#[tauri::command]
pub fn open_capture(state: tauri::State<'_, AppState>, path: PathBuf) -> UiResult<CaptureInfo> {
    let mut reader = CaptureReader::open(&path)?;
    let info = CaptureInfo::gather(&mut reader)?;
    *state.capture.lock().expect("state mutex") = Some(reader);
    Ok(info)
}

/// The waveform at a given zoom level.
///
/// This is the whole reason `CaptureReader::downsample` was built in phase 1 with no GUI to
/// use it: one bucket per pixel, min and max preserved, answered from the stored pyramid when
/// the window is wide enough that reading samples would be wasteful.
#[tauri::command]
pub fn waveform(state: tauri::State<'_, AppState>, view: ViewRequest) -> UiResult<Vec<Bucket>> {
    let mut guard = state.capture.lock().expect("state mutex");
    let reader = guard.as_mut().ok_or_else(no_capture)?;
    Ok(reader.downsample(TimeSpan::new(view.start_ns, view.end_ns), view.buckets)?)
}

/// Statistics for the selected region — what the panel under the plot shows.
#[tauri::command]
pub fn selection_stats(
    state: tauri::State<'_, AppState>,
    start_ns: u64,
    end_ns: u64,
) -> UiResult<RegionStats> {
    let mut guard = state.capture.lock().expect("state mutex");
    let reader = guard.as_mut().ok_or_else(no_capture)?;
    Ok(region_stats(
        reader,
        TimeSpan::new(start_ns, end_ns),
        &StatsOptions::lenient(),
    )?)
}

/// Per-event statistics — the table that makes this a firmware profiler.
#[tauri::command]
pub fn event_table(state: tauri::State<'_, AppState>) -> UiResult<Vec<EventStats>> {
    let mut guard = state.capture.lock().expect("state mutex");
    let reader = guard.as_mut().ok_or_else(no_capture)?;
    let map = reader.metadata().event_map();
    Ok(event_stats(reader, &map, &StatsOptions::lenient())?)
}

/// The firmware event timeline, for the track under the waveform.
#[tauri::command]
pub fn event_marks(state: tauri::State<'_, AppState>) -> UiResult<Vec<EventMark>> {
    let mut guard = state.capture.lock().expect("state mutex");
    let reader = guard.as_mut().ok_or_else(no_capture)?;
    let map = reader.metadata().event_map();
    Ok(reader
        .events()?
        .into_iter()
        .map(|e| {
            let (name, is_stop) = match map.lookup(e.id) {
                Some((def, is_stop)) => (def.name.clone(), is_stop),
                // An id the metadata never declared is shown as its hex id rather than
                // hidden. An event the metadata forgot is still evidence.
                None => (format!("0x{:04X}", e.id), false),
            };
            EventMark {
                t_ns: e.t_ns,
                id: e.id,
                name,
                is_stop,
                value: e.value,
            }
        })
        .collect())
}

/// One event on the timeline.
///
/// `is_stop` comes from the metadata rather than from the id's low bit: the start/stop
/// convention is advisory, and a UI that re-derives the pairing is a second implementation of
/// something `event_stats` already does properly.
#[derive(Debug, Serialize)]
pub struct EventMark {
    pub t_ns: u64,
    pub id: u16,
    pub name: String,
    pub is_stop: bool,
    pub value: Option<u32>,
}

/// Check budgets from the UI, so the numbers a developer sees are the ones CI will use.
#[tauri::command]
pub fn check_budgets(
    state: tauri::State<'_, AppState>,
    rules: Vec<AssertRule>,
) -> UiResult<AssertReport> {
    let mut guard = state.capture.lock().expect("state mutex");
    let reader = guard.as_mut().ok_or_else(no_capture)?;
    let map = reader.metadata().event_map();
    let stats = event_stats(reader, &map, &StatsOptions::lenient())?;
    Ok(evaluate_assertions(&stats, None, &rules))
}

/// Parse a quantity typed into an input field.
///
/// The CLI's parser, reached from the UI, so `260uJ` means the same thing in both. A second
/// parser would drift within a release.
#[tauri::command]
pub fn parse_energy(text: String) -> UiResult<f64> {
    let e: wattson_core::units::Energy = text.parse()?;
    Ok(e.0 * 1e6)
}

fn no_capture() -> UiError {
    UiError {
        kind: "no_capture".into(),
        message: "no capture is open".into(),
    }
}
