# Wattson

**Open-source firmware-aware power profiling.**

Not a desktop current monitor. The question Wattson exists to answer is:

> *What was my firmware doing when this power spike happened?*

A profiler MCU measures current and voltage across a shunt and — using the **same hardware
timer** — timestamps firmware events, GPIO edges, and RTOS activity arriving from the device
under test. The host correlates all of it, computes energy per firmware event, and lets you
assert on those numbers in CI.

```text
 Device under test
 ┌──────────────────────────┐
 │ PP_EVENT("BLE_TX") ──────┼──────┐
 │ PP_EVENT("SLEEP")        │      │ UART / GPIO / SWO / RTT
 └──────────────────────────┘      │
                                   ▼
 Power rail ─── Shunt ──►┌──────────────────┐
                         │ Profiler MCU     │
                         │ current + voltage│
                         │ event capture    │
                         │ ONE timestamp    │
                         │ engine           │
                         └────────┬─────────┘
                                  │ USB
                                  ▼
                     ┌───────────────────────────┐
                     │ wattson  (CLI + desktop)  │
                     │ waveforms · event timeline│
                     │ energy · charge · stats   │
                     │ CI power regression gates │
                     └───────────────────────────┘
```

## The differentiator

Anyone can plot current. What makes this worth building is the combination:

```text
current waveform + firmware events + RTOS activity + energy statistics + CI regression tests
```

Select 500 occurrences of `BLE_TX` and get mean duration, mean current, peak current, mean
energy, and P95 energy. Then compare firmware revisions:

```text
Firmware v1.2   BLE TX energy: 282 µJ
Firmware v1.3   BLE TX energy: 241 µJ
Improvement: 14.5%
```

Then make it a build gate:

```bash
wattson assert capture.pprof --event BLE_TX --max-energy 260uJ
```

```text
POWER REGRESSION

BLE_TX energy
  Expected: <= 260 µJ
  Measured:    287 µJ
  Regression: +10.4%
```

Power consumption becomes a testable firmware metric.

## Status

**Phase 1, in progress: everything that works with zero measurement hardware.**

A simulator generates a realistic synthetic current/voltage stream with embedded firmware
events and speaks the real wire protocol, so the protocol, capture format, analysis engine and
CLI are all developed and tested end-to-end before a profiler MCU exists. The software is the
durable part of this project and stays valuable no matter which measurement board you attach.

| Component | Path | Status |
|---|---|---|
| Wire protocol (`no_std`) | `protocol/` | implemented |
| Core: transports, capture format, statistics, assertions | `core/` | implemented |
| Simulated device | `simulator/` | implemented |
| CLI: capture, info, analyze, export, assert, gen-header, sim | `cli/` | implemented |
| Target C instrumentation library | `target/` | implemented, untested on silicon |
| Desktop app (Tauri 2 + React) | `desktop/` | phase 2 |
| Profiler firmware | `firmware/` | phase 2 |
| Measurement board | `hardware/` | phase 7 |

### Instrumenting your firmware

Two files — `target/include/wattson.h` and `target/src/wattson.c`, C99, no allocation, 1.2 KB
of flash on a Cortex-M0+:

```c
#include "wattson.h"
#include "pp_events.h"          /* wattson gen-header events.toml -o pp_events.h */

PP_SCOPE(PP_EVT_RADIO_TX) {
    radio_transmit(payload, length);
}
PP_EVENT_U32(PP_EVT_PACKET_TX, length);
```

Six bytes per event on the wire — a `u32` timestamp and a `u16` id — and no strings at all.
Names live in the metadata file the host loads, which is what keeps the cost of instrumenting
a hot path low enough to ignore.

CI compiles it for Cortex-M0+, and `cargo test -p wattson-target` builds it on the host and
checks that the frames it produces are byte-identical to the ones the Rust encoder produces for
the same records. What nobody has done yet is run it on real silicon. See
[docs/instrumentation.md](docs/instrumentation.md).

## Quick start

No hardware required. The simulator speaks the real wire protocol, so nothing downstream
knows the difference.

```bash
cargo run --release -p wattson-cli -- capture --device "sim://ble-sensor" --duration 10s --out smoke.pprof
```

```bash
cargo run --release -p wattson-cli -- analyze smoke.pprof --events --plot
```

```text
  █▁▁██▁▁█▁▁▁█▁▁▁█▁▁█▄▁▁█▁▁▁█▁▁▁█▁▁██▁▁██▁▁█▁▁▁█▁▁█▆▁▁██▁▁█▁▁▁█▁▁▁█▁▁█▆▁▁█
  0 s                                          10.000 s   peak 83.680 mA

Selection: 10.000 s
  Duration             10.000 s
  Avg current          4.411 mA
  Energy               145.6 mJ
  Charge               44.1 mC (0.012 mAh)
  Battery life         49.9 h on a 220 mAh cell

Events, most expensive first:

BLE_TX
  Occurrences          100
  Duration             958.500 µs  (p95 996.000 µs, min 900.000 µs, max 1.008 ms)
  Mean current         74.485 mA
  Peak current         83.680 mA
  Energy               230.089 µJ  (p95 241.123 µJ, min 215.469 µJ, max 242.623 µJ)
  Total energy         23.0 mJ
  Duty cycle           1.002%
```

Then make it a build gate:

```bash
cargo run --release -p wattson-cli -- assert smoke.pprof --event BLE_TX --max-energy 260uJ
```

To watch it fail, capture from `sim://ble-sensor-regressed` — the same node with a 21% longer
transmission — and run the identical assertion. That pair exists so the gate can be shown to
both pass and fail; a gate that only ever passes proves nothing.

Over a socket instead, which is what CI does:

```bash
cargo run --release -p wattson-cli -- sim --listen 127.0.0.1:9000 --profile ble-sensor
```

```bash
cargo run --release -p wattson-cli -- capture --device tcp://127.0.0.1:9000 --duration 10s --out smoke.pprof
```

Full reference: [docs/cli.md](docs/cli.md).

## Layout

```text
protocol/   wire format: spec, golden byte vectors, no_std Rust implementation
core/       transports, .pprof capture format, statistics, assertions, export
simulator/  deterministic synthetic device
cli/        the `wattson` binary
desktop/    Tauri 2 + React front end            (phase 2)
firmware/   profiler MCU firmware and drivers    (phase 2)
target/     C instrumentation library for the device under test
examples/   baremetal / FreeRTOS / Zephyr integrations           (phase 3+)
hardware/   open measurement board                               (phase 7)
docs/       protocol, capture format, instrumentation, CLI reference
```

`protocol` is `no_std` and dependency-thin on purpose: phase-2 firmware links that same crate,
or is validated against the same golden vectors. `core` never prints, never reads argv, and
never exits — which is what lets the CLI and the future desktop backend share one engine
rather than growing two.

**Note:** cargo builds into `build/`, not `target/`, because `target/` here holds the C
instrumentation library for the *target device*. See `.cargo/config.toml`.

## Scope, deliberately

v1 aims at **firmware power analysis**, not **precision laboratory instrumentation**: kHz to
tens-of-kHz sampling, MCU-scale currents, firmware event correlation, energy statistics,
FreeRTOS correlation. Automatic µA-to-A range switching, MHz-class acquisition, precision
calibration, and a programmable target supply come later, if at all. That keeps the first
version achievable while the software stays useful regardless of the measurement hardware.

## License

MIT OR Apache-2.0, at your option.
