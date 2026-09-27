# Desktop application

Tauri 2 + React/TypeScript. The current waveform and the firmware-event lane on **one shared
time axis** — which is the whole product. A current trace and an event log in two windows is
two pictures; on one axis it is an answer to "what was the firmware doing when that happened".

Excluded from the cargo workspace, so `cargo build --workspace` stays green without
`npm install` and a webview toolchain.

```bash
cd desktop
npm install
npm run tauri dev      # the app
npm run build          # typecheck + bundle the front end only
```

## What it wraps

Every number on screen already exists in `wattson-core`, which is why this is a thin layer
rather than a second implementation:

| UI element | `wattson-core` entry point |
|---|---|
| Waveform at any zoom level | `CaptureReader::downsample` |
| Selection statistics panel | `region_stats` |
| Event statistics table | `event_stats` |
| Event lane, start/stop marks | `CaptureReader::events` + `EventMap::lookup` |
| Capture facts and integrity | `CaptureInfo::gather` |
| Device picker | `enumerate_devices` |
| Unit entry fields | `core::units` |

The rule that makes this work is enforced upstream: `wattson-core` never prints, never reads
argv, and never exits. A Tauri command is a serialization boundary, nothing more — open a
capture, call one core function, return the typed result. No arithmetic happens in
`commands.rs`, and none happens in TypeScript. If it did, this window and `wattson analyze`
could disagree about the same capture, and only one of them would be in the test suite.

## Rendering rules that are not cosmetic

- **An empty bucket is a gap, not zero.** `Bucket.count == 0` means nothing was sampled in
  that pixel column. Drawing it at 0 µA invents a measurement, and the trace line breaks there
  rather than spanning it.
- **Min and max are drawn, not just the mean.** A 90 mA peak that falls between two pixels is
  exactly the peak someone opened the window to find.
- **Integrity is shown, loudly.** A truncated capture, a dropped frame, a host overrun — each
  produces perfectly plausible energy numbers that are too low. The header says so, and the
  gap is shaded on the plot.
- **The vertical axis autoscales to the visible window**, so zooming into the idle floor shows
  the idle floor's structure instead of a flat line under a 90 mA ceiling.
- **A stop mark is drawn as the dim half of a bracket**, and `is_stop` comes from the
  metadata via `EventMap::lookup` — never guessed from the id's low bit. The start/stop id
  convention is advisory; pairing belongs to `event_stats`.

## Interaction

| Input | Effect |
|---|---|
| Wheel | Zoom about the cursor |
| Drag | Select a span; the statistics panel follows |
| Shift-drag | Pan |
| Double-click | Back to the whole capture |
| Click an event row | Jump to its first occurrence |

## Not here yet

- **Live capture.** `Session::poll` plus `CaptureWriter` already do this for the CLI; the
  window needs a streaming view and a stop control rather than new analysis.
- **A voltage track.** `Bucket` carries current only. Adding voltage is a `core` change — and
  a `SUMMARY` chunk change — not a UI one, which is exactly why it is not faked here.
- **GPIO lanes.** The data is captured and stored; the lane is not drawn.
- **Baseline comparison.** `evaluate_assertions` is reachable through `check_budgets`, but
  nothing in the window calls it yet.
