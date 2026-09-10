// The typed edge of the Tauri command surface.
//
// Scaffold: phase 2. These types mirror `wattson-core`'s serialised shapes, so the UI never
// re-derives a number the core already computed. If something here starts doing arithmetic on
// a measurement, it belongs in the Rust side where the CLI can reach it too.

import { invoke } from "@tauri-apps/api/core";

/** One aggregated time bucket: one per horizontal pixel. */
export interface Bucket {
  t_ns: number;
  min_ua: number;
  max_ua: number;
  mean_ua: number;
  /** Zero means no data in this bucket. It must render as a gap, never as zero current. */
  count: number;
}

export interface CaptureInfo {
  path: string;
  format_version: string;
  device_serial: string;
  firmware_version: string;
  timer_hz: number;
  configured_rate_hz: number;
  /** Derived from the data. This is the one that is safe to reason with. */
  effective_rate_hz: number;
  sample_count: number;
  event_count: number;
  duration_s: number;
  finalized: boolean;
  recovered: boolean;
  gaps: unknown[];
}

export interface Distribution {
  n: number;
  mean: number;
  min: number;
  max: number;
  stddev: number;
  p50: number;
  p95: number;
  p99: number;
  total: number;
}

export interface EventStats {
  name: string;
  occurrences: number;
  duration_us: Distribution;
  energy_uj: Distribution;
  current_mean_ua: number;
  current_peak_ua: number;
  total_energy_uj: number;
  duty_cycle: number;
  /** Non-zero means the numbers above are incomplete, and the UI must say so. */
  unterminated: number;
  orphaned_stops: number;
  unsampled: number;
}

export interface RegionStats {
  start_ns: number;
  end_ns: number;
  sample_count: number;
  duration_s: number;
  current_min_ua: number;
  current_max_ua: number;
  current_mean_ua: number;
  current_rms_ua: number;
  voltage_mean_uv: number;
  charge_uc: number;
  charge_mah: number;
  energy_uj: number;
  power_mean_uw: number;
  power_peak_uw: number;
}

export interface EventMark {
  t_ns: number;
  id: number;
  name: string;
  value: number | null;
}

export const api = {
  listDevices: () => invoke<string[]>("list_devices"),
  openCapture: (path: string) => invoke<CaptureInfo>("open_capture", { path }),
  waveform: (start_ns: number, end_ns: number, buckets: number) =>
    invoke<Bucket[]>("waveform", { view: { start_ns, end_ns, buckets } }),
  selectionStats: (start_ns: number, end_ns: number) =>
    invoke<RegionStats>("selection_stats", { startNs: start_ns, endNs: end_ns }),
  eventTable: () => invoke<EventStats[]>("event_table"),
  eventMarks: () => invoke<EventMark[]>("event_marks"),
  parseEnergy: (text: string) => invoke<number>("parse_energy", { text }),
};
