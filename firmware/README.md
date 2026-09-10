# Profiler firmware

**Status: deferred to phase 2.**

Firmware for the profiler MCU: the board that sits between the power rail and the host.

## Contract

Whatever MCU is used, the firmware owes the host exactly this:

- Speak the wire protocol in `protocol/spec/frames.md`, byte for byte. The golden vectors in
  `protocol/spec/vectors/` are the acceptance test; a C implementation must reproduce them
  exactly.
- Timestamp power samples, firmware events, and GPIO edges from **one** hardware timer.
  Correlation accuracy is the product's entire reason to exist, and it is lost the moment two
  clocks are involved.
- Report that timer's frequency in `DEVICE_INFO.timer_hz` and maintain a wrap counter surfaced
  in every `SYNC` frame, so the host can verify its own tick unwrapping rather than merely
  infer it.
- Never hide loss. Dropped samples and buffer overflows are reported in `ERROR` frames and
  flagged on the following sample block. A silent drop becomes a silently wrong energy figure
  in someone's CI gate.

## Planned layout

- `profiler/` - the acquisition application: sampling loop, ring buffer, USB streaming.
- `drivers/` - measurement-hardware drivers behind one interface, so the software is not welded
  to a single current-sense IC.
- `boards/` - per-board bring-up: pins, clocks, USB, build files.

RP2040 is the intended first target: PIO and the DMA sniffer (which computes the protocol's
CRC-32 inline at zero CPU cost) make deterministic capture cheap, and boards cost a few dollars.
