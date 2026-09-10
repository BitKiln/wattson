# The `.pprof` capture format

Version 1.0. Little-endian throughout.

The authoritative description is the code in `core/src/capture/`, which is tested against every
claim below. This document explains *why* the format is shaped this way, because the reasons
are what should survive contact with a future change.

---

## What it has to do, in priority order

1. **Be appendable while capturing at 50 ksps.** Nothing may require rewriting earlier bytes.
2. **Be seekable by time**, so a zoomable UI can jump to a window without reading the file.
3. **Be readable when truncated.** Ctrl-C, a crash, or a yanked USB cable must cost at most the
   chunk in flight, not the forty minutes before it.
4. **Be compressed.** An hour at 50 ksps is about 2 GB raw.
5. **Be additively extensible**, so a v1.1 writer's files still open in a v1.0 reader.

---

## Structure

```text
[ FileHeader   128 B, uncompressed, frozen ]
[ Chunk ]*                 appended during capture
[ INDEX chunk ]            written at close
[ Footer       32 B ]      written at close
```

The header is rewritten exactly once, at the very end, to set the `FINALIZED` flag and record
the footer offset. That single backward write is the only one in the format.

---

## FileHeader — 128 bytes

```text
off  size  field
0     8    magic "PPROFCAP"
8     2    format_major u16
10    2    format_minor u16
12    4    header_len u32 = 128
16    8    created_unix_ns u64        host wall clock, for humans; never for a duration
24    8    capture_start_mono_ns u64
32    4    device_timer_hz u32
36    4    sample_rate_hz u32         what was REQUESTED
40    4    flags u32
44    4    compression u32            0 none, 1 zstd, 2 lz4
48   16    device_serial[16]          ascii, NUL-padded
64   16    fw_version[16]             ascii, NUL-padded
80    8    fw_build_id u64
88    4    shunt_micro_ohm i32
92    4    current_offset_ua i32      calibration
96    4    current_gain_num u32
100   4    current_gain_den u32
104   8    footer_offset u64          0 while the capture is open
112   4    target_chunk_bytes u32
116   8    reserved                   additive v1.x fields go here
124   4    header_crc32
```

Flags:

```text
bit0  FINALIZED     the writer completed; an index and footer are present
bit1  HAS_VOLTAGE
bit2  HAS_GPIO
bit3  HAS_EVENTS
bit4  SYNTHETIC     the samples came from a simulator, not measurement hardware
```

`SYNTHETIC` is set from what the *device* reports about itself, not from how the host reached
it — a simulator over TCP is still a simulator. A synthetic capture must never be mistaken for
a measurement of real hardware.

`sample_rate_hz` is what was asked for. What actually happened is derived from the timestamps,
and only the derived figure is safe to integrate with.

---

## Chunks

**Chunk header — 40 bytes, frozen forever:**

```text
off  size  field
0     4    magic "CHNK"
4     2    kind u16
6     2    flags u16      bit0 payload_compressed
8     4    uncompressed_len u32
12    4    stored_len u32       bytes following this header
16    8    first_time_ns u64    capture-relative; u64::MAX if not time-bearing
24    8    last_time_ns u64
32    4    payload_crc32        over the stored (possibly compressed) bytes
36    4    header_crc32         over bytes [0..36)
```

Kinds:

```text
0x0001 SAMPLES   0x0002 EVENTS  0x0003 GPIO   0x0004 MARKERS  0x0005 SYNC
0x0010 METADATA  0x0011 ANNOTATIONS           0x0020 SUMMARY
0x00F0 INDEX     0x00FF END
```

**A reader must skip unknown kinds using `stored_len`.** That rule, plus the frozen 40-byte
header, is the entire forward-compatibility story: it is what lets a v1.0 reader walk a v1.1
file it does not fully understand. It is tested by splicing a well-formed chunk of an invented
kind into a real capture and requiring every sample to still come back.

`first_time_ns` and `last_time_ns` are what let whole chunks be skipped without decompressing
them, which is what makes a zoomed-in view cheap on a multi-gigabyte capture.

---

## SAMPLES payload — structure of arrays

```text
0    4   count u32
4    4   layout_flags u32   bit0 UNIFORM, bit1 HAS_VOLTAGE, bit2 CURRENT_DELTA (reserved)
8    8   t0_ns u64          capture-relative
16   4   period_ns u32      0 if not uniform
20   4   reserved
24   ..  [ u32 dt_ns   * count ]    only if not UNIFORM
     ..  [ i32 current * count ]
     ..  [ u32 voltage * count ]    only if HAS_VOLTAGE
```

**All the timestamps, then all the currents, then all the voltages** — not interleaved records.
Adjacent current readings in a real trace differ by tens of microamps out of a 32-bit word, so
column-major layout puts long runs of identical high bytes next to each other. That is worth
roughly 3–6× under zstd against about 1.5× for interleaved, and there is a test that measures
it rather than asserting it on faith.

When timing is regular, `UNIFORM` is set and no delta array is stored. When it is not — a
dropped block, a rate change, a stall — the writer falls back to explicit per-sample deltas
for that chunk. Storing `count × period` across a hiccup is exactly how a capture silently
under-reports energy.

`CURRENT_DELTA` is reserved for `i16` deltas, and a v1.0 reader **rejects** a chunk with it set
rather than guessing at the layout.

### Other payloads

```text
EVENTS : u32 count, u32 flags, then [ u64 t_ns, u16 id, u16 flags, u32 value ] * count  (16 B)
GPIO   : u32 count, u32 flags, then [ u64 t_ns, u16 state, u16 pad ]           * count  (12 B)
SYNC   : u32 count, u32 flags, then [ u64 host_send, u64 host_recv, u64 ticks ] * count (24 B)
```

The raw SYNC triples are stored, not just the fitted line, so clock-drift correction can be
re-derived offline with a better algorithm without recapturing.

---

## Compression

**zstd level 3, one independent frame per chunk.** No dictionary, no streaming state carried
across chunks, so any chunk decodes standalone — which is precisely what makes random seek
work. A single stream compressed end to end would force a full scan to reach the last second
of a capture.

`lz4` is available for builds without a C compiler. The `compression` header field means
captures stay readable either way.

### Chunk size

64 KiB uncompressed by default: about 8192 samples with voltage, or 164 ms at 50 ksps. That is
roughly six file writes per second, zstd finishes well inside a millisecond, and seek
granularity is finer than one pixel at a whole-capture zoom.

It is an educated guess, which is exactly why it lives in the file header rather than in the
code: it can be re-tuned after measurement without a format change.

---

## The summary pyramid

`SUMMARY` stores min, max, mean and count per fixed time bucket at **10 ms and 1 s**. A
whole-capture overview then renders from a few thousand buckets without decompressing a single
sample chunk. Intermediate zoom levels decompress only the chunks that intersect the window.

**Min and max are stored, not just the mean.** A 91 mA spike that falls between two rendered
pixels is exactly the spike someone is looking for; averaging it away turns it into a flat
line. A test asserts the peak survives at every zoom level.

1 ms buckets were rejected on arithmetic: an hour of them is 3.6 M buckets at 16 bytes, or
57 MB — larger than the compressed samples they summarise. At 10 ms and 1 s it is about 5.8 MB
per capture-hour.

**An empty bucket stays empty.** Missing data renders as a gap, never as zero current;
rendering a hole as 0 mA is how a tool convinces someone their device sleeps beautifully.

---

## Index and footer

```text
INDEX payload : u32 entry_count, u32 reserved, then per entry (32 B):
                u16 kind, u16 flags, u64 file_offset, u32 stored_len,
                u32 record_count, u64 first_time_ns

Footer (32 B) : u64 index_offset, u64 index_len, u64 total_samples,
                u32 footer_crc32, u32 magic "PEND"
```

---

## Recovery

Opening reads the last 32 bytes. If the footer magic or CRC is wrong, or `FINALIZED` is clear,
the reader falls back to a **forward scan** from offset 128, validating each chunk by its
magic, header CRC and length, rebuilding the index in memory, and stopping at the first thing
that is not a chunk.

A capture killed mid-write therefore loses at most one chunk — 164 ms at the default size.

This is about fifty lines of code and it is the difference between "my forty-minute capture is
gone" and "fine, the last moment is missing". It is tested by truncating a real capture at two
hundred offsets spanning every structural boundary and requiring, at each one, either a clean
error or a genuine prefix of what was written: never a panic, never a fabricated sample.

---

## Measured cost

From `cargo bench -p wattson-core --bench pipeline` on a development machine. The number that
matters is **400 KB/s** — 50 ksps × 8 bytes — because that is what a device produces.

| Path | Measured | Headroom |
|---|---|---|
| Frame decode to samples | ~125 MiB/s | ~300× |
| Capture write, zstd | ~20 M samples/s | ~400× |
| Capture read, all samples | ~51 M samples/s | — |
| Region statistics | ~31 M samples/s | — |
| Overview, 64 buckets | ~28 µs | answered from the pyramid |
| Overview, 1024 buckets | ~13 ms | falls through to sample chunks |

Two things worth reading off that table:

- **The pyramid earns its place.** 64 buckets over a ten-second capture is answered in 28 µs
  because it never touches a sample chunk; 1024 buckets over the same capture costs 13 ms
  because it does. That is the difference between a zoom that feels instant and one that
  stutters, and it is why the summary is written at capture time rather than computed on
  demand.
- **Compression is not a cost here, it is a saving.** Writing with zstd is *faster* than
  writing uncompressed, because the file is several times smaller and the I/O dominates.

Re-measure before changing the chunk size; the header field exists so the answer can change
without the format changing.

## Versioning

- **Major bump** — incompatible. A reader refuses, naming the version that wrote the file.
- **Minor bump** — additive only: new chunk kinds, new flag bits whose absence means the v1.0
  behaviour, new fields in the header's reserved region.

The rule that makes minor bumps safe is the one stated above: frozen header sizes, and readers
that skip unknown chunk kinds by length.

---

## Gaps

Gaps are recorded where they are observed, never inferred afterwards:

```text
Gap { start_ns, end_ns, cause, lost_estimate }

cause: DeviceOverflow | SequenceGap | HostOverrun | TimestampJump | Truncation
```

Statistics over a span containing a gap **fail by default**. Integrating across missing data
produces a plausible number that is too low, and too low is the direction that turns a
regression into a passing build. `--on-gap=skip` accepts a partial answer explicitly.
