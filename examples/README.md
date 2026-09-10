# Integration examples

**Status: deferred to phase 6.**

End-to-end examples of instrumenting a real application.

- `baremetal/` - a superloop with manual `PP_EVENT` markers.
- `freertos/` - automatic task-switch, ISR entry/exit, and tickless-sleep events, so the
  timeline answers *which task spent the energy* rather than only *where I put a marker*.
- `zephyr/` - the same idea against Zephyr's tracing hooks.
