# `wattson` command-line reference

Everything below works with no hardware attached: `wattson sim` provides a device that speaks
the real wire protocol.

---

## Exit codes

These are a contract. A CI pipeline branches on them, and they are pinned by tests.

| Code | Meaning |
|------|---------|
| 0 | Success, or every assertion passed |
| 1 | An assertion failed |
| 2 | Usage error: bad arguments |
| 3 | I/O or capture-format error |
| 4 | Capture integrity failure: missing data under a strict gap policy |

---

## `wattson sim`

Run a synthetic device.

```bash
wattson sim --listen 127.0.0.1:9000 --profile ble-sensor
```

| Option | Meaning |
|---|---|
| `--listen ADDR` | Address to bind. Port `0` lets the OS choose; the chosen address is printed on stdout as `listening on ADDR`. |
| `--profile NAME` | Which workload. `--list-profiles` shows them. |
| `--seed N` | Seed for the deterministic random stream. The same seed always produces the same trace. |
| `--rate HZ` | Samples per second. Accepts `50k`, `50000`, `50kHz`. |
| `--duration D` | Stop each connection's device after this long. |
| `--inject-faults` | Drop blocks and corrupt bytes, so the host's recovery paths get exercised. |

Profiles:

| Profile | What it does |
|---|---|
| `ble-sensor` | Sleep, read a sensor, transmit. `BLE_TX` costs about 245 µJ. |
| `ble-sensor-regressed` | The same node with a 21% longer transmission, about 296 µJ. |
| `lora-node` | Rare, long, high-current transmissions. |
| `always-on` | A flat baseline that never sleeps. |

`ble-sensor` and `ble-sensor-regressed` exist as a pair so a power budget can be shown to both
pass and fail. A gate that only ever passes proves nothing.

---

## `wattson devices`

List connected profilers.

```bash
wattson devices
wattson devices --all --json
```

Enumeration cannot be certain a port is a profiler without opening it and shaking hands, so
`--all` lists every serial port and the default lists only likely candidates.

On Windows this can take several hundred milliseconds — that is `SetupAPI`, not the tool.

---

## `wattson capture`

Record from a device.

```bash
wattson capture --device tcp://127.0.0.1:9000 --duration 10s --out capture.pprof
wattson capture --device "sim://ble-sensor?rate=50000&seed=42" --duration 10s --out cap.pprof
wattson capture --device serial:COM7 --rate 50k --metadata events.toml --out cap.pprof
```

| Option | Meaning |
|---|---|
| `--device URI` | `auto`, `serial:COM7`, `serial:/dev/ttyACM0?baud=921600`, `tcp://host:port`, `sim://profile?rate=&seed=` |
| `--rate HZ` | Samples per second. Refused up front if the device says it cannot meet it. |
| `--duration D` | How long to record. Omit to run until Ctrl-C. |
| `--out PATH` | Where to write the capture. |
| `--metadata FILE` | TOML naming the firmware events this capture will contain. See [instrumentation.md](instrumentation.md). |
| `--shunt R` | Shunt resistance, e.g. `100mohm`. |
| `--gpio MASK` | Which GPIO pins to capture edges on, e.g. `0x0F`. |
| `--compression` | `zstd` (default), `lz4`, or `none`. |
| `--supply V` | Supply voltage to assume when the device has no voltage channel. |

**Ctrl-C finishes the capture cleanly.** The interrupt sets a flag the capture loop checks, so
the index and footer are still written. Even if the process is killed outright, the reader
recovers everything but the chunk in flight — but that is a safety net, not a plan.

**Loss is always reported.** Gaps, dropped frames and CRC failures are printed at the end and
recorded in the file. A capture quietly missing data still produces confident-looking numbers.

---

## `wattson info`

What is in a capture file.

```bash
wattson info capture.pprof
wattson info capture.pprof --json
```

Two sample rates are shown, and they differ:

- **Configured rate** is what was requested.
- **Effective rate** is derived from the timestamps in the file. This is the one that is safe
  to reason with; a large divergence is the first sign a capture is missing data.

Anything wrong with the file — never finalized, index rebuilt by scanning, chunks that failed
their CRC, gaps — is reported after the numbers, so it qualifies what was just read.

---

## `wattson analyze`

What a capture, or part of it, cost.

```bash
wattson analyze capture.pprof
wattson analyze capture.pprof --events --top 5
wattson analyze capture.pprof --from 1.5s --to 3s --regions
wattson analyze capture.pprof --json
wattson analyze capture.pprof --plot
```

| Option | Meaning |
|---|---|
| `--events` | Per-event statistics. |
| `--regions` | Statistics over the selected window. |
| `--from D` / `--to D` | Window bounds, e.g. `1.5s`, `250ms`. |
| `--top N` | Only the N events costing the most in total. |
| `--on-gap POLICY` | `error`, `skip` (default), `interpolate`. |
| `--supply V` | Supply to assume when the capture has no voltage channel. |
| `--plot` | An ASCII overview of the current trace. |
| `--json` | Machine-readable output. |

With neither `--events` nor `--regions`, both are shown.

Region statistics report duration, min/max/mean/RMS current, mean voltage, mean and peak
power, energy, and charge in both coulombs and mAh.

Per-event statistics report occurrences, duration and energy distributions (mean, P95, min,
max), mean and peak current, total energy, and duty cycle — plus warnings for occurrences that
could not be measured.

---

## `wattson export`

Write a capture out for other tools.

```bash
wattson export capture.pprof --format csv --what samples --decimate 100 --out samples.csv
wattson export capture.pprof --format csv --what events
wattson export capture.pprof --format json --what stats
```

| Option | Meaning |
|---|---|
| `--format` | `csv`, `json`. `vcd` and `parquet` are planned and refuse with a clear message. |
| `--what` | `samples`, `events`, `gpio`, `stats`. |
| `--decimate N` | Keep every Nth sample. An hour at 50 ksps is 180 million rows. |
| `--from` / `--to` | Limit to a window. |
| `--out PATH` | Defaults to standard output. |

Event exports resolve ids to names using the capture's own metadata, so a `.pprof` stays
readable long after the firmware that produced it is gone.

---

## `wattson assert`

Power budgets as a build gate. **Exits 1 when a budget is exceeded.**

```bash
wattson assert capture.pprof --event BLE_TX --max-energy 260uJ
wattson assert capture.pprof --event BLE_TX --max-p95-energy 300uJ --min-occurrences 10
wattson assert capture.pprof --baseline base.json --tolerance 5%
wattson assert capture.pprof --event BLE_TX --max-energy 260uJ --format junit
```

| Option | Meaning |
|---|---|
| `--event NAME` | Which event to check. Repeat for several. Omit to apply the limits to every event. |
| `--max-energy E` | Mean energy per occurrence, e.g. `260uJ`. |
| `--max-p95-energy E` | P95 energy per occurrence. |
| `--max-total-energy E` | Total across all occurrences. |
| `--max-mean-current I` | Mean current during the event, e.g. `30mA`. |
| `--max-peak-current I` | Peak current during the event. |
| `--max-duration D` | Mean duration, e.g. `1.2ms`. |
| `--min-occurrences N` | Fail if the event fired fewer than N times. |
| `--baseline FILE` | Compare against a previous run. |
| `--tolerance PCT` | Allowed regression against the baseline, e.g. `5%`. |
| `--write-baseline FILE` | Record this run's numbers for future comparisons. |
| `--report FILE` | Write the full report as JSON. |
| `--format` | `human` (default), `json`, `junit`. |
| `--on-gap POLICY` | Defaults to `error` here, and only here. |

### What fails, and why

A gate is only useful if it fails for the right reasons. These all exit 1:

- **Over budget.** The obvious one.
- **The event is not declared in the capture's metadata.** A typo in a budget must not
  silently disable the gate it was meant to be.
- **The event never occurred.** Instrumentation that stopped firing should turn the build
  red, not green.
- **Every occurrence was too short to sample.** The energy is unknown, and unknown is not
  zero.
- **The capture has gaps** (by default). Integrating across missing data under-reports energy,
  which is the direction that turns a regression into a pass.

### Failure output

```text
POWER REGRESSION

BLE_TX mean energy
  Expected: <= 260.000 uJ
  Measured: 281.927 uJ
  Over budget by +8.4%
```

With a baseline, the last line becomes `Regression: +22.5% against the baseline`.

### In CI

```yaml
- run: wattson sim --listen 127.0.0.1:9000 --profile ble-sensor &
- run: wattson capture --device tcp://127.0.0.1:9000 --duration 30s --out ci.pprof
- run: wattson assert ci.pprof --event BLE_TX --max-energy 260uJ --format junit > power.xml
```

Replace the first two steps with a real device once you have one; nothing downstream changes.

---

## `wattson completions`

```bash
wattson completions bash > /etc/bash_completion.d/wattson
wattson completions powershell | Out-String | Invoke-Expression
```

---

## Global options

| Option | Meaning |
|---|---|
| `-q`, `--quiet` | Suppress progress output. Results are still printed. |
| `-v`, `--verbose` | More logging. Repeat for more. |
| `--version`, `--help` | The usual. |

Logging honours the `WATTSON_LOG` environment variable, using `tracing-subscriber` filter
syntax (`WATTSON_LOG=wattson_core::session=debug`).
