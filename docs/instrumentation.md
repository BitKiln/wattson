# Firmware instrumentation

How your firmware tells the profiler what it is doing, so a current waveform becomes an
answer rather than a picture.

The target-side C library is **phase 3** and not written yet. This document is the contract it
will be written against, and the parts that already exist — the wire format, the metadata
file, the latency budget — are real today.

---

## The idea

```c
#include "wattson.h"

void radio_send(void)
{
    PP_EVENT(PP_EVT_RADIO_START);
    radio_enable();
    radio_transmit();
    PP_EVENT(PP_EVT_RADIO_STOP);
}
```

Scoped, so a return in the middle cannot leave the scope open:

```c
PP_SCOPE(PP_EVT_SENSOR_READ) {
    sensor_read();
}
```

With a value, for things whose cost depends on a parameter:

```c
PP_EVENT_U32(PP_EVT_PACKET_TX, packet_length);
```

---

## What actually goes on the wire

Six bytes, or ten with a value:

```text
u32 timestamp_ticks
u16 event_id
[u32 value]
```

**No strings.** Not the name, not a format, not a category. An event id is an opaque `u16`, and
everything human-readable lives in a metadata file the host loads. That is what keeps the cost
of instrumenting a code path low enough to ignore — which matters, because instrumentation that
measurably changes the energy of the thing it measures is instrumentation that lies.

Events are also **batched**: a burst of instrumented calls must not cost one USB frame each.

---

## The metadata file

Names, pairing, and units live here. It is TOML because a person writes it, and it is stored
inside the capture as CBOR so the file stays self-describing after the firmware that produced
it is gone.

```toml
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

# A point event has no stop, so it has a time but no duration and no energy.
[[event]]
name = "PACKET_TX"
start_id = 0x0110
value_units = "bytes"

[notes]
firmware = "v1.3.0-4-gabc1234"
board = "rev-C"
```

Pass it with `wattson capture --metadata events.toml`.

Definitions that would make analysis ambiguous are rejected when the file is loaded, not
quietly tolerated:

- two events with the same name;
- one id claimed by two events;
- a start and stop that share an id, which makes pairing impossible;
- id `0`, which the wire format reserves as invalid.

### Id allocation

Recommended and unenforced; see `protocol/spec/event-ids.md`.

```text
high byte = category, low byte = slot

0x0000              invalid, never transmitted
0x0100..=0x01FF     radio
0x0200..=0x02FF     sensors
0x0300..=0x03FF     storage
0x0400..=0x04FF     compute
0x0500..=0x05FF     power management / sleep
0x1000..=0xFEFF     application-defined
0xFF00..=0xFFFF     reserved for profiler-internal events
```

---

## The latency budget

**This is the accuracy floor for every event boundary, and it is worth choosing deliberately
rather than discovering later.**

The profiler timestamps an event when it *learns* about it. The gap between the firmware
acting and the profiler learning is signalling latency, and it lands directly on the measured
duration and energy.

| Transport | Latency | Against a 950 µs BLE TX |
|---|---|---|
| GPIO toggle | < 1 µs | < 0.1% |
| SPI, dedicated | a few µs | ~0.3% |
| UART @ 1 Mbaud | ~10 µs byte time, plus call overhead | ~1% |
| UART @ 115200 | ~90 µs per byte | ~10%, unusable for short events |
| SWO / RTT | depends on the probe and buffering | measure it |

Two consequences:

1. **Pick the transport for the shortest event you care about**, not the longest. A 50 µs
   scope measured over 115200 baud UART is noise.
2. **Record what you chose.** `latency_compensation_ns` in the metadata is subtracted from
   that event's timestamps, so a capture can be corrected after the fact instead of being
   quietly wrong. It applies to both the start and the stop, so a constant latency cancels out
   of the duration and shifts only the position.

---

## What the analysis does with it

Starts and stops are matched with a stack, so nested occurrences of the same scope pair
correctly — which real firmware produces the moment a scoped macro appears on a re-entrant
path.

Three things are counted and reported rather than dropped:

- **unterminated** starts: the capture ended mid-scope, or a stop was lost;
- **orphaned** stops: the capture began mid-scope;
- **unsampled** occurrences: the event was shorter than the sample period, so its energy
  cannot be measured at this rate.

That last one matters most. An event reported as costing 0 µJ because nobody looked is how a
power budget quietly becomes fiction. `wattson assert` treats it as a failure.

---

## RTOS integration (phase 6)

The interesting version of all this is automatic. Hooking a FreeRTOS build's
`traceTASK_SWITCHED_IN`, task create and delete, ISR entry and exit, and tickless idle turns
the timeline into a per-task energy attribution:

```text
Current  ▁▁▁▁▇▇▇▇▁▁▁▁▁▁████▁▁▁▁▁▁▁▁

Tasks
  BLE     ███████
  Sensor         ████
  Idle               █████████
  BLE                          █████
```

At which point the tool answers a question no oscilloscope can: *which RTOS task is
responsible for this battery drain?* Zephyr's tracing hooks map the same way.

---

## Doing it today, without the C library

The instrumentation library is not written, but the format it will speak is. Anything that can
emit the `EVENT` frame described in [protocol.md](protocol.md) already works with every part
of this tool — including a script, a logic analyser bridge, or a few lines of hand-written
firmware.
