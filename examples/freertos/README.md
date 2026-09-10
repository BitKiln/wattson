# FreeRTOS example

**Status: deferred to phase 6.**

Hooks `traceTASK_SWITCHED_IN`, task create/delete, ISR entry/exit, and tickless idle into
profiler events, giving per-task energy attribution.

This is the point where the project stops being a nicer oscilloscope and starts answering a
question no scope can: *which RTOS task is responsible for this battery drain?*
