# Target instrumentation library

The tiny C library an application under test includes so its firmware events appear on the
power timeline. This is what makes wattson a *firmware* profiler rather than a desktop current
monitor.

> **On the directory name.** This is *not* cargo's build directory. Cargo builds into `build/`
> here — see `.cargo/config.toml`. The name refers to the *target device*, the thing being
> measured.

## Layout

```text
include/wattson.h     the API: PP_EVENT, PP_EVENT_U32, PP_SCOPE, and the configuration knobs
src/wattson.c         the whole implementation - ring buffer, CRC-32, COBS, framing
examples/             uart.c, gpio.c, freertos.c, and the stub board functions they compile against
tests/               the C side of the conformance harness
rust/                 the harness: compiles the C, drives it, checks its bytes
```

## Using it

```bash
cc -Itarget/include -DPP_SINGLE_CONTEXT target/src/wattson.c your_firmware.c
```

```c
#include "wattson.h"
#include "pp_events.h"   /* wattson gen-header events.toml -o pp_events.h */

void radio_send(const uint8_t *payload, uint16_t length)
{
    PP_EVENT_U32(PP_EVT_PACKET_TX, length);

    PP_SCOPE(PP_EVT_RADIO_TX) {
        radio_enable();
        radio_transmit(payload, length);
        radio_disable();
    }
}
```

Configuration, the transport callback, and the latency budget are documented in
[docs/instrumentation.md](../docs/instrumentation.md). Three settings deserve a real decision:
`PP_TIMESTAMP()`, the critical-section hooks, and `PP_AUTO_FLUSH_THRESHOLD`.

## Contract

- **Nothing but numbers on the wire.** An event is 6 bytes — `u32` timestamp, `u16` id — or 10
  with a `u32` value. Names live in the metadata file the host loads; see
  [`protocol/spec/event-ids.md`](../protocol/spec/event-ids.md).
- **Overhead small enough to ignore.** `pp_event()` takes a timestamp and pushes 12 bytes into
  a ring; that is all. The CRC and COBS work happens in `pp_flush()`, which you call from a
  task. Instrumentation that measurably changes the energy of the thing it measures is
  instrumentation that lies.
- **Loss is loud.** A full ring drops the newest events and counts them; it never overwrites
  recorded history, never blocks, and never silently forgets. `pp_get_stats()` reports every
  way the timeline could be wrong.
- **Latency is the accuracy floor.** UART at 1 Mbaud costs roughly 10 µs of byte time — about
  1% of a 950 µs BLE transmission. A GPIO toggle gets under a microsecond. Captures carry
  `latency_compensation_ns` per event so the host can correct for whichever you chose.

## Footprint

Measured, not estimated — `arm-none-eabi-gcc 15.1, -Os, Cortex-M0+`:

| Configuration | Flash | RAM |
|---|---|---|
| Default (`PP_RING_CAPACITY=128`, `PP_MAX_EVENTS_PER_FRAME=100`) | 1232 B | 3616 B |
| Small (`PP_RING_CAPACITY=32`, `PP_MAX_EVENTS_PER_FRAME=16`) | 1148 B | 780 B |
| `PP_ENABLED=0` | 0 B | 0 B |

RAM is almost entirely the three buffers: the event ring, the frame scratch, and the encoded
frame. Lower `PP_MAX_EVENTS_PER_FRAME` first — it shrinks two of the three, and costs only one
extra frame header per batch. CI rebuilds these configurations on every commit.

```bash
arm-none-eabi-gcc -c -std=c99 -Os -Wall -Wextra -Wpedantic -Werror -mcpu=cortex-m0plus -mthumb -Itarget/include -o wattson.o target/src/wattson.c
arm-none-eabi-size wattson.o
```

## Verification

`cargo test -p wattson-target` compiles this library on the host, runs it, and checks that:

- the frames it emits are **byte-identical** to the ones `wattson-protocol` encodes for the
  same records — two independent implementations of `protocol/spec/frames.md` §3.5;
- everything it emits decodes cleanly through the host decoder, including across timer
  wraparound, nesting, and mixed record sizes;
- a full ring, a short write, a missing transport and event id `0` all lose loudly and count
  correctly;
- `PP_ENABLED=0` still compiles, links, and does nothing;
- the whole chain — instrumented C, wire, capture file, `event_stats` — produces the expected
  per-event energy.

None of that needs a device. What it cannot tell you is how the library behaves on real
silicon under real interrupt load; that arrives with the profiler board in phase 7.
