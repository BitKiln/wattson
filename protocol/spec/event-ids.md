# Event ids

Companion to [frames.md](frames.md) §3.5.

An event id is an opaque `u16`. There is no name on the wire, no string, no format specifier,
and no enter/exit bit. Everything human-readable lives in the capture metadata, which the host
loads and stores inside the `.pprof` so the file stays self-describing after the firmware that
produced it is gone.

That is a deliberate cost decision. Instrumentation that measurably changes the energy of the
thing it measures is instrumentation that lies, and a six-byte record is cheap enough to put on
a path that runs thousands of times a second. A string is not.

The consequence is that **an id carries no meaning the metadata does not give it**. Everything
below is convention: useful, recommended, and not enforced by any decoder. A capture whose
metadata disagrees with these rules is still valid; it is just harder for a person to read.

---

## 1. Reserved values

| Range | Meaning |
|---|---|
| `0x0000` | **Invalid.** Never transmitted. The instrumentation library counts an attempt as a dropped event. |
| `0xFF00`–`0xFFFF` | Reserved for profiler-internal events. Application firmware must not use these. |

Everything else is available.

---

## 2. Categories

The high byte groups events; the low byte selects one within the group. It exists so a
timeline can be filtered and coloured by category without a lookup table, and so two teams
instrumenting the same firmware do not collide.

```text
0x0100 .. 0x01FF   radio
0x0200 .. 0x02FF   sensors
0x0300 .. 0x03FF   storage
0x0400 .. 0x04FF   compute
0x0500 .. 0x05FF   power management, sleep, wake
0x0600 .. 0x06FF   RTOS and interrupts
0x1000 .. 0xFEFF   application-defined
```

The `category` field in the metadata is what the host actually groups by, so a project that
allocates ids differently loses nothing but the convenience.

---

## 3. Pairing: stop = start + 1

A scope needs two ids, and the recommended allocation is **adjacent**: the stop id is the start
id plus one.

```text
0x0100  BLE_TX          start
0x0101  BLE_TX          stop
0x0200  SENSOR_READ     start
0x0201  SENSOR_READ     stop
0x0110  PACKET_TX       point event - no stop id at all
```

This is what lets `PP_SCOPE(PP_EVT_BLE_TX)` derive its own stop id, which halves the number of
identifiers a call site has to get right. A pair that is not adjacent is perfectly legal and
needs `PP_SCOPE_ID(start, stop)` instead; `wattson gen-header` emits a comment next to every
generated pair saying which case it is.

Pairing itself lives **only** in the metadata:

```toml
[[event]]
name = "BLE_TX"
start_id = 0x0100
stop_id = 0x0101
```

The host refuses definitions that would make analysis ambiguous — a duplicate name, one id
claimed by two events, a start and stop that share an id, or id `0`.

### Point events

An event with no `stop_id` has a time but no duration, and therefore no energy. That is not a
degenerate case to be avoided: `PACKET_TX` carrying a length, sampled once per packet, is often
the most useful thing on the timeline.

---

## 4. Keeping firmware and metadata in step

The ids in the firmware and the ids in the metadata are the same numbers written twice, and
nothing detects it when they diverge — an opaque id relabelled is still a valid capture, just a
wrong one.

Generate one from the other:

```bash
wattson gen-header events.toml -o firmware/src/pp_events.h
```

Run it from the build, not by hand. The generated header refuses to be produced at all when two
event names would collapse onto the same C macro, so a rename that would have been silently
ambiguous fails at build time instead.

---

## 5. Allocation advice

- **Leave gaps.** Allocate `0x0100`, `0x0110`, `0x0120` rather than `0x0100`, `0x0102`,
  `0x0104`; the room between them is where next year's sub-events go without renumbering
  anything.
- **Never reuse a retired id.** Old captures are still readable, and a reused id makes an
  archived capture quietly describe the wrong thing. There are 65,000 of them.
- **Instrument the boundary, not the function.** An id marks a state the hardware is in —
  radio transmitting, flash erasing — rather than a call being made. The energy question is
  about the former.
