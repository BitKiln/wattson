# Firmware instrumentation

How your firmware tells the profiler what it is doing, so a current waveform becomes an
answer rather than a picture.

The target-side C library lives in [`target/`](../target). It is one header and one `.c` file,
C99, no allocation, and it is verified on every CI run: `target/rust` compiles it, drives it,
and checks the bytes it produces against the host decoder and against the Rust encoder, frame
for frame. No device is involved in any of that, which is the point — the wire is exercised
from both ends before any silicon exists.

What has *not* happened is a run on real hardware. The profiler board is phase 7. Timing
figures below are budgets, not measurements.

---

## The idea

```c
#include "wattson.h"
#include "pp_events.h"   /* generated; see "Keeping the ids in step" below */

void radio_send(void)
{
    PP_EVENT(PP_EVT_RADIO_TX);
    radio_enable();
    radio_transmit();
    PP_EVENT(PP_EVT_RADIO_TX_STOP);
}
```

Scoped, so a `return` in the middle cannot leave the scope open:

```c
PP_SCOPE(PP_EVT_SENSOR_READ) {
    sensor_read();
}
```

`PP_SCOPE` derives its stop id as `start + 1`. Where the compiler has
`__attribute__((cleanup))` — GCC and Clang, so every embedded toolchain that matters — the stop
is emitted even when the block is left by `return` or `goto`. Elsewhere it is not, and the host
then reports the occurrence as **unterminated** rather than inventing a duration for it.
`PP_SCOPE_IS_SAFE` is `1` on the first and `0` on the second, so code that must not leak a
scope can check at compile time.

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
0x0600..=0x06FF     RTOS and interrupts
0x1000..=0xFEFF     application-defined
0xFF00..=0xFFFF     reserved for profiler-internal events
```

A scope's stop id is its start id plus one, which is what lets `PP_SCOPE` take a single
argument.

### Keeping the ids in step

The ids appear twice — in the firmware and in the metadata — and nothing detects it when they
diverge. An opaque id relabelled is still a valid capture, just a wrong one. So generate one
from the other, from the build rather than by hand:

```bash
wattson gen-header events.toml -o firmware/src/pp_events.h
```

The generated header refuses to be produced when two event names would collapse onto the same C
macro, so an ambiguous rename fails at build time instead of silently.

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

The interesting version of all this is automatic — and
[`target/examples/freertos.c`](../target/examples/freertos.c) sketches it. Hooking a FreeRTOS
build's
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

The sketch compiles against FreeRTOS and nothing in it is subtle, but nobody has run it on
hardware; treat it as a starting point rather than a port.

---

## Using the library

Add two files to your build:

```bash
cc -Itarget/include -DPP_SINGLE_CONTEXT target/src/wattson.c your_firmware.c
```

Configure it with `-D`, or with a `wattson_config.h` of your own if you define
`PP_USE_CONFIG_HEADER`. Every knob has a working default; three are worth deciding
deliberately.

| Setting | Default | Decide it because |
|---|---|---|
| `PP_TIMESTAMP()` | returns `0` | This **must** read the same timer that stamps power samples. Correlation is the whole point, and it is lost the moment two clocks are involved. Leaving it at zero hands timestamping to the profiler on arrival, which makes transport latency the accuracy floor. |
| `PP_ENTER_CRITICAL` / `PP_EXIT_CRITICAL` | interrupt masking on ARM; a compile error elsewhere | A data race on the ring corrupts the timeline in a way that looks exactly like a firmware bug. If only one context ever records events, say so with `PP_SINGLE_CONTEXT` — the header refuses to guess. |
| `PP_AUTO_FLUSH_THRESHOLD` | half the ring | Set it to `0` if `PP_EVENT` can run in an ISR and your transport cannot. Flushing does the CRC and COBS work that recording deliberately avoids. |

On a Cortex-M0+ at `-Os` the default configuration costs **1232 bytes of flash and 3616 bytes
of RAM**; dropping to a 32-event ring and 16 events per frame costs 1148 and 780. Almost all of
the RAM is the three buffers, and `PP_MAX_EVENTS_PER_FRAME` shrinks two of them.

`PP_ENABLED=0` compiles every macro down to nothing: no buffer, no code, no calls. The
instrumented code itself still runs, and CI builds that configuration on every commit so it
cannot rot.

### Where the events go

The library hands framed `EVENT` frames to a write callback you supply, and counts everything
that could go wrong on the way — `events_dropped`, `write_failures`, `frames_sent` — because a
lost event becomes a wrong energy figure in someone's CI gate. Read them with `pp_get_stats()`.
A ring that fills drops the **newest** events, never overwriting history to make room for the
present.

### Without the library

The library is one way to speak the format, not the only one. Anything that can emit the
`EVENT` frame described in [protocol.md](protocol.md) works with every part of this tool —
a script, a logic analyser bridge, a few lines of hand-written firmware, or GPIO toggling with
no data path at all (see [`target/examples/gpio.c`](../target/examples/gpio.c)).
