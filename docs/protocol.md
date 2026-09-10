# Wire protocol

The normative specification is [`protocol/spec/frames.md`](../protocol/spec/frames.md), and a
test parses its frame-type table and compares it against the `FrameType` enum, so the two
cannot drift apart silently. Golden byte vectors live in `protocol/spec/vectors/`.

This page is the short version and the reasoning.

---

## Shape

```text
logical frame:
  TYPE u8 | LEN u16 | SEQ u8 | PAYLOAD | CRC32 u32

on the wire:
  COBS(logical frame) || 0x00
```

- **CRC-32/ISO-HDLC**, not CRC-16. The two extra bytes are 0.25% overhead at realistic frame
  sizes, and both candidate profiler MCUs accelerate this exact polynomial in hardware: the
  RP2040 DMA sniffer computes it inline with the transfer at zero CPU cost, and the STM32 CRC
  peripheral is hardwired to `0x04C11DB7`. A CRC-16 would force a software loop on both.
- **COBS**, because it guarantees a byte (`0x00`) that never occurs inside an encoded frame.
  That is what lets a host resynchronise unambiguously after corruption, or when attaching to
  a device that is already streaming. Overhead is at most one byte per 254.

Maximum payload is 1024 bytes; a full frame is at most 1040 bytes on the wire.

---

## Frames

| Code | Name | Direction |
|------|------|-----------|
| 0x01 | HELLO | H→D |
| 0x02 | DEVICE_INFO | D→H |
| 0x03 | CONFIG | H→D |
| 0x05 | START_CAPTURE | H→D |
| 0x06 | STOP_CAPTURE | H→D |
| 0x10 | CURRENT_SAMPLES | D→H |
| 0x11 | EVENT | D→H |
| 0x12 | GPIO_EVENT | D→H |
| 0x13 | SYNC | both |
| 0x14 | MARKER | both |
| 0x7F | ERROR | both |

**An unrecognised type is not an error.** It is delivered to the application with its raw
payload. That is the forward-compatibility escape hatch that lets a v1.1 device talk to a v1.0
host, and it is tested rather than hoped for.

---

## The decision that matters most

`CURRENT_SAMPLES` **omits the per-sample timestamp by default**, reconstructing it as
`t0 + i * period_ticks`:

```text
default      (8 B/sample):  i32 current_uA, u32 voltage_uV
EXPLICIT_TS (12 B/sample):  u32 timestamp, i32 current_uA, u32 voltage_uV
```

At 50 ksps that is 400 KB/s rather than 600 KB/s, against roughly 0.5–1 MB/s of realistic USB
CDC-ACM throughput. Headroom is about 2×, not 10×, and 600 KB/s leaves none.

No timing information is lost: a device that hits any timing discontinuity must terminate the
block and start a new one with a fresh `t0`, rather than papering over the gap. The reserved
`DELTA16` flag can buy another 25% if measurement shows it is needed; a v1.0 decoder rejects a
block with that bit set rather than guessing.

---

## One timer

`DEVICE_INFO.timer_hz` is load-bearing. **One** hardware timer must stamp power samples,
firmware events, and GPIO edges alike. Correlating a current spike with the firmware that
caused it is this project's whole reason to exist, and that correlation is lost the moment two
clocks are involved.

---

## Wraparound, verified rather than inferred

At 1 MHz a `u32` tick counter wraps every **71.6 minutes**. A host that infers wraps only from
backward jumps is correct until it stalls for more than 35.8 minutes, after which it silently
emits a 71-minute jump — data that looks like data.

So `SYNC` carries an explicit `wrap_count` from the device's timer-overflow ISR, sent at least
once a second, and the host cross-checks its inference against it and raises an error on
disagreement.

Unwrapping happens **once**, in the host's decode path. Nothing above the session layer ever
sees a `u32` timestamp.

Samples advance the wrap epoch; events, GPIO edges and SYNC echoes resolve against it without
advancing it. They share one timer but arrive interleaved, so an event stamped before a sample
block routinely arrives after it — feeding all four streams into one advancing unwrapper reads
that ordinary ordering as time running backwards.

---

## Loss is never hidden

`ERROR` carries cumulative `dropped_samples` and `buffer_overflows`, and a sample block can be
flagged `OVERFLOW_BEFORE`. A device that cannot meet a requested rate replies `BAD_CONFIG`
rather than quietly dropping samples.

A silently dropped sample becomes a silently wrong energy figure in someone's CI gate. That is
the single worst failure mode this project has, and every layer is built to make it loud.

---

## Conformance

An independent implementation — notably the phase-2 C firmware — is conformant when it
reproduces the byte vectors in `protocol/spec/vectors/` exactly and accepts them on decode.
Those vectors are the acceptance test; the prose is the explanation.

CI builds `wattson-protocol` for `thumbv6m-none-eabi` on every change. That one line is what
guarantees RP2040 firmware can actually link this crate.
