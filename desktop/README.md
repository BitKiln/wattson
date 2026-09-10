# Desktop application

**Status: deferred to phase 2.**

Tauri 2 + React/TypeScript front end: synchronised current, voltage, firmware-event, and
RTOS-task timelines, where zooming one zooms all of them.

Scaffold only, and excluded from the cargo workspace so `cargo build --workspace` stays green
without `npm install`.

## What it wraps

Every number the UI shows already exists in `wattson-core`, which is why this is a thin layer
rather than a second implementation:

| UI element | `wattson-core` entry point |
|---|---|
| Waveform at any zoom level | `CaptureReader::downsample` |
| Selection statistics panel | `region_stats` |
| Event statistics table | `event_stats` |
| Device picker | `enumerate_devices` |
| Live capture | `Session::poll` plus `CaptureWriter` |
| Unit entry fields | `core::units` |

The rule that makes this work is enforced upstream: `wattson-core` never prints, never reads
argv, and never exits. A Tauri command is a serialization boundary, nothing more.
