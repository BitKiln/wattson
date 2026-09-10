# Target instrumentation library

**Status: deferred to phase 3.**

The tiny C/C++ library an application under test optionally includes so its firmware events
appear on the power timeline.

> **On the directory name.** This is *not* cargo's build directory. Cargo builds into `build/`
> here - see `.cargo/config.toml`. The name matches the published project layout and refers to
> the *target device*, i.e. the thing being measured.

## Intended API

```c
#include "wattson.h"

void radio_send(void)
{
    PP_EVENT(PP_EVT_RADIO_START);
    radio_enable();
    radio_transmit();
    PP_EVENT(PP_EVT_RADIO_STOP);
}

PP_SCOPE(PP_EVT_SENSOR_READ) {
    sensor_read();
}

PP_EVENT_U32(PP_EVT_PACKET_TX, packet_length);
```

## Contract

- **Nothing but numbers on the wire.** An event is 6 bytes (`u32` timestamp, `u16` id), or 10
  with a `u32` value. Names live in a generated metadata file the host loads; see
  `protocol/spec/event-ids.md`.
- **Overhead must be small enough to ignore.** If instrumenting a code path measurably changes
  its energy, the tool is lying about the very thing it claims to measure.
- **Latency is the accuracy floor.** UART at 1 Mbaud costs roughly 10 us of byte time plus
  whatever the call itself takes - about 1% of a 950 us BLE TX window. A GPIO toggle gets that
  to sub-microsecond. The transport choice belongs in the latency budget in
  `docs/instrumentation.md`, and captures carry a `latency_compensation_ns` per event so the
  host can correct for it.
