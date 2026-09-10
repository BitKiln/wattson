# Wattson wire protocol — normative specification

Version `1.0` (`PROTOCOL_VERSION = 0x0100`).

This document is the source of truth for the wire format. The frame-type table below is
parsed by `protocol/rust/tests/spec_sync.rs` and compared against the `FrameType` enum, so
this file and the code cannot drift apart silently.

Everything is **little-endian**. Strings are fixed-width NUL-padded ASCII arrays, never
length-prefixed, so a firmware encoder can `memcpy` them out of a static buffer.

---

## 1. Framing

A logical frame:

```text
off   size  field
0     1     TYPE    u8
1     2     LEN     u16   payload length, 0..=1024
3     1     SEQ     u8    per-direction sequence, wraps 255 -> 0
4     LEN   PAYLOAD
4+L   4     CRC32   u32   CRC-32/ISO-HDLC over bytes [0 .. 4+LEN)
```

The complete logical frame is then COBS-encoded and a single `0x00` delimiter is appended.

```text
MAX_PAYLOAD  1024
MAX_FRAME    1032   = 4 + MAX_PAYLOAD + 4
MAX_ENCODED  1038   = MAX_FRAME + ceil(MAX_FRAME / 254) + 1 delimiter
```

### 1.1 CRC

**CRC-32/ISO-HDLC.** Polynomial `0x04C11DB7` reflected, init `0xFFFF_FFFF`, xorout
`0xFFFF_FFFF`, reflect in and out. Check value over `"123456789"` is `0xCBF4_3926`.

Chosen over a CRC-16 deliberately. The two extra bytes are 0.25% overhead at realistic frame
sizes, and both candidate profiler MCUs accelerate this exact polynomial in hardware: the
RP2040 DMA sniffer computes it inline with the transfer at zero CPU cost, and the STM32 CRC
peripheral is hardwired to `0x04C11DB7`. A CRC-16 would force a software loop on both.

### 1.2 COBS

Consistent Overhead Byte Stuffing, per Cheshire & Baker (SIGCOMM '97). COBS guarantees that
`0x00` never appears inside an encoded frame, which is what allows a receiver to resynchronise
unambiguously after corruption, or when attaching to an already-running device mid-stream.
Overhead is at most one byte per 254.

### 1.3 Receiver rules

1. Accumulate bytes until `0x00`. An empty run between two delimiters is idle line noise, not
   an error.
2. COBS-decode, then check `LEN` against the decoded length, then check the CRC. Any failure
   discards that region only; decoding continues at the next delimiter.
3. **An unrecognised `TYPE` is not an error.** It is surfaced to the application as an unknown
   frame carrying its raw payload. This is the forward-compatibility escape hatch that lets a
   v1.1 device talk to a v1.0 host.
4. Sequence gaps are counted and reported, never silently absorbed.

---

## 2. Frame types

| Code | Name | Direction | Payload |
|------|------|-----------|---------|
| 0x01 | HELLO | H->D | 8 |
| 0x02 | DEVICE_INFO | D->H | 72 |
| 0x03 | CONFIG | H->D | 24 |
| 0x05 | START_CAPTURE | H->D | 0 |
| 0x06 | STOP_CAPTURE | H->D | 0 |
| 0x10 | CURRENT_SAMPLES | D->H | variable |
| 0x11 | EVENT | D->H | variable |
| 0x12 | GPIO_EVENT | D->H | variable |
| 0x13 | SYNC | both | 16 |
| 0x14 | MARKER | both | 12 |
| 0x7F | ERROR | both | 16 |

Reserved and not yet assigned:

```text
0x04            CONFIG_ACK   (planned, v1.1)
0x15            STATUS       (planned, v1.1)
0x40 .. 0x6F    vendor-specific
0x70 .. 0x7E    reserved
```

---

## 3. Payloads

### 3.1 HELLO — 8 bytes

```text
0   2  proto_version u16    major << 8 | minor
2   2  nonce u16            host-chosen; a device may use it to seed a session id
4   4  reserved u32         must be zero in v1.0
```

### 3.2 DEVICE_INFO — 72 bytes

```text
0    4  magic u32 = 0x4652_5050 ("PPRF")
4    2  proto_version u16
6    2  device_type u16
8   16  serial[16]             ascii, NUL-padded
24  16  fw_version[16]         ascii, NUL-padded
40   8  fw_build_id u64
48   4  timer_hz u32           ticks/second of THE hardware timer
52   4  max_sample_rate_hz u32
56   4  shunt_micro_ohm i32
60   4  adc_full_scale_ua u32
64   2  channel_count u16
66   2  gpio_count u16
68   2  caps u16
70   2  reserved
```

`caps` bits:

```text
bit0  DELTA_SAMPLES   device can emit delta-encoded sample blocks
bit1  GPIO            device captures GPIO edges
bit2  MARKERS         device timestamps host-injected markers
bit3  WRAP_COUNT      device reports an explicit timer wrap count in SYNC
bit4  PRE_TRIGGER     device has a pre-trigger ring buffer
```

`timer_hz` is load-bearing. **One** hardware timer must stamp power samples, firmware events,
and GPIO edges alike. Correlation accuracy is this project's whole reason to exist, and it is
lost the moment two clocks are involved.

### 3.3 CONFIG — 24 bytes

```text
0    4  sample_rate_hz u32
4    2  averaging u16          device-specific; 0 or 1 means none
6    2  conv_time_code u16     device-specific ADC conversion time
8    4  gpio_mask u32          which pins to capture edges on
12   4  shunt_micro_ohm i32
16   4  flags u32
20   4  reserved u32
```

A host must not request a rate above `DEVICE_INFO.max_sample_rate_hz`. A device that cannot
meet a requested rate replies `ERROR{BadConfig}` rather than quietly dropping samples.

### 3.4 CURRENT_SAMPLES — variable

```text
0    4  t0_ticks u32        timestamp of sample[0]
4    4  period_ticks u32    nominal inter-sample ticks; ignored if EXPLICIT_TS
8    2  count u16
10   2  flags u16
12   .. records
```

`flags` bits:

```text
bit0  EXPLICIT_TS       each record carries its own u32 timestamp
bit1  NO_VOLTAGE        records omit the voltage field
bit2  OVERFLOW_BEFORE   the device dropped samples immediately before this block
bit3  DELTA16           RESERVED. Not implemented in v1.0; a v1.0 decoder must reject a
                        block with this bit set rather than guess at the layout.
```

Record layouts:

```text
default      (8 B):  i32 current_uA, u32 voltage_uV
EXPLICIT_TS (12 B):  u32 timestamp, i32 current_uA, u32 voltage_uV
NO_VOLTAGE   (4 B):  i32 current_uA
```

**The default mode omits the per-sample timestamp**, reconstructing it as
`t0 + i * period_ticks`. At 50 ksps that is 400 KB/s rather than 600 KB/s, against a realistic
0.5–1 MB/s of USB CDC-ACM throughput. No timing information is lost: a device that hits any
timing discontinuity must terminate the block and start a new one with a fresh `t0`, rather
than papering over the gap.

Timestamp arithmetic wraps: the device timer is a free-running `u32`, and the host unwraps it
exactly once, in its decode path, before anything else sees a timestamp.

Recommended block size is 64 samples (524-byte payload), giving roughly 780 blocks/s at
50 ksps.

### 3.5 EVENT — variable

```text
0    2  count u16
2    2  record_size u16     6 or 10; any other value is malformed
4    .. records:
        u32 timestamp_ticks
        u16 event_id
        [u32 value]         present iff record_size == 10
```

Events are batched because a burst of instrumented calls must not cost one USB frame each.

Event ids are **opaque `u16`**. There is no name, no string, and no enter/exit bit on the
wire. Pairing a start id with a stop id happens entirely in the capture metadata — see
`event-ids.md`.

### 3.6 GPIO_EVENT — variable

```text
0    2  count u16
2    2  reserved u16
4    .. records (6 B): u32 timestamp_ticks, u16 pin_state
```

`pin_state` is the **full 16-pin snapshot** at that edge, not a `(pin, level)` pair. Same
size, but any single pin's waveform then reconstructs from a linear scan with no
initial-state bookkeeping.

### 3.7 SYNC — 16 bytes

```text
0    4  device_ticks u32
4    2  wrap_count u16      from the device timer-overflow ISR
6    2  flags u16
8    8  host_time_ns u64
```

The host sends a SYNC with `host_time_ns = t_send` and the device fields zero. The device
echoes it with `device_ticks` and `wrap_count` filled in. The host records `t_recv` on
arrival, giving a `(t_send, t_recv, device_ticks)` triple.

A device should emit SYNC at **at least 1 Hz** even unprompted. The explicit `wrap_count` is
what makes `u32` tick wraparound *verifiable* rather than merely inferred: at 1 MHz the
counter wraps every 71.6 minutes, and a host that only infers wraps from backward jumps will
be wrong if it ever stalls for more than 35.8 minutes. A host must raise an error on
disagreement rather than silently emit a 71-minute time jump.

### 3.8 MARKER — 12 bytes

```text
0    4  device_ticks u32    device-filled; zero when sent by the host
4    4  marker_id u32
8    4  value u32
```

The device timestamps a marker on receipt so it lands in the device time base like everything
else.

### 3.9 ERROR — 16 bytes

```text
0    2  code u16
2    2  detail u16
4    4  dropped_samples u32
8    4  buffer_overflows u32
12   4  context u32
```

Codes:

```text
0x0000  NONE
0x0001  UNSUPPORTED_FRAME
0x0002  BAD_CONFIG
0x0003  NOT_CONFIGURED
0x0004  ALREADY_CAPTURING
0x0005  NOT_CAPTURING
0x0010  BUFFER_OVERFLOW
0x0011  ADC_FAULT
0x0012  OVERCURRENT
0x00FF  INTERNAL
```

An unknown code must be preserved verbatim, not normalised away.

**Loss is never hidden.** `dropped_samples` and `buffer_overflows` are cumulative counters. A
silently dropped sample becomes a silently wrong energy figure in someone's CI gate, which is
the single worst failure mode this project has.

---

## 4. Session flow

```text
host                          device
 |  HELLO                  ->  |
 |  <-              DEVICE_INFO |
 |  CONFIG                 ->  |
 |  START_CAPTURE          ->  |
 |  <- CURRENT_SAMPLES *       |
 |  <- EVENT *                 |
 |  <- GPIO_EVENT *            |
 |  SYNC  <->  SYNC            |   at least 1 Hz, throughout
 |  MARKER                 ->  |   whenever the host annotates
 |  STOP_CAPTURE           ->  |
```

A device that receives `START_CAPTURE` before `CONFIG` replies `ERROR{NOT_CONFIGURED}`.

---

## 5. Conformance

`protocol/spec/vectors/` holds golden byte vectors for every frame type. An independent
implementation — notably the phase-2 C firmware — is conformant when it reproduces those bytes
exactly and accepts them on decode. They are the acceptance test; this prose is the
explanation.
