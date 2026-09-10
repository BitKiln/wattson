# Instrumentation examples

| File | Transport | When to use it |
|---|---|---|
| `uart.c` | Framed EVENT frames over a spare UART | The default. Every id and value the wire format supports, at ~10 µs of latency at 1 Mbaud. |
| `gpio.c` | A pin per state, timestamped by the profiler | Events too short for a UART. Sub-microsecond, but one bit per pin and no value field. |
| `freertos.c` | RTOS trace hooks | Per-task energy attribution. A worked sketch of the phase-6 integration, not a tested port. |

`example_hal.c` and `example_hal.h` are do-nothing board functions so `uart.c` and `gpio.c`
compile in CI. An example that is never built is an example that stops being true.
`freertos.c` needs FreeRTOS headers and is not compiled.

The latency budget that decides between them is in
[docs/instrumentation.md](../../docs/instrumentation.md).
