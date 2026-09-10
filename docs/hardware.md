# Measurement hardware

**Status: phase 7.** No board exists yet, and the software does not need one.

---

## The deliberate scope decision

v1 targets **firmware power analysis**, not **precision laboratory instrumentation**:

```text
✓  kHz to tens-of-kHz sampling
✓  MCU-scale currents
✓  firmware event correlation
✓  energy and charge measurement
✓  automated profiling and CI regression gates
✓  RTOS correlation

later, if at all
○  automatic µA → A range switching
○  MHz-class acquisition
○  precision calibration
○  programmable target supply
```

Competing with a Joulescope on measurement quality is a hardware project. Answering *what was
my firmware doing when this spike happened* is a software project, and it stays valuable
regardless of what board is eventually attached.

---

## What the host already assumes

Nothing about a specific board. The interface is the wire protocol, and everything the host
needs is in `DEVICE_INFO`:

| Field | Why the host cares |
|---|---|
| `timer_hz` | The single time base everything is stamped against |
| `max_sample_rate_hz` | So an impossible rate is refused rather than silently missed |
| `shunt_micro_ohm` | Recorded in the capture for traceability |
| `adc_full_scale_ua` | Range, so clipping can be identified |
| `caps` | Whether GPIO, markers, and an explicit wrap count are available |

Calibration — offset and a rational gain — is stored in the capture header, so a capture can be
recalibrated after the fact instead of being silently wrong.

---

## Bring your own board

The measurement driver interface in `firmware/drivers/` exists so this software is useful to
someone who builds their own front end:

```c
struct pp_measurement_driver {
    int (*init)(void);
    int (*start)(void);
    int (*stop)(void);
    int (*read)(pp_sample_t *);
};
```

An INA226 on a breakout board and an RP2040 is enough to get real measurements, and that is
the intended first target.

---

## Throughput, which is the real constraint

50 ksps × 8 bytes is **400 KB/s** of payload. Realistically:

| Path | Throughput |
|---|---|
| RP2040 USB CDC-ACM | 0.5–1 MB/s |
| STM32 full-speed CDC | ~700 KB/s |
| WinUSB / vendor bulk on Windows | typically 2–3× CDC |

Headroom is about 2×, not 10×. The protocol already buys a third of it back with
block-relative timestamps, and the reserved `DELTA16` flag can buy another quarter.

The host's transport layer is a deliberately narrow byte-pipe trait, so moving from CDC to a
vendor bulk interface is a new implementation rather than a refactor.

---

## An eventual open board

When it happens, the wanted features are the usual ones: multiple current ranges with
automatic switching, a high-speed ADC, a programmable target supply, an external trigger, and
GPIO capture. Schematics, layout and BOM would live in this directory.

It stays last on purpose. The software is the part that keeps its value.
