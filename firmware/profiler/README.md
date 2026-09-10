# Profiler acquisition application

**Status: deferred to phase 2.**

The acquisition loop: configure the measurement driver, sample on a hardware timer, batch into
`CURRENT_SAMPLES` blocks, interleave `EVENT` / `GPIO_EVENT` / `SYNC` frames, stream over USB.

Targets roughly 50 ksps sustained. At 8 bytes per sample that is 400 KB/s of payload against
roughly 0.5-1 MB/s of realistic USB CDC-ACM throughput - about 2x headroom, not 10x. The
protocol's block-relative timestamps already buy back a third of the bandwidth; the reserved
`DELTA16` flag can buy another quarter if measurement shows it is needed.
