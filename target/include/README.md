# Public headers

`wattson.h` — the whole API: `PP_EVENT`, `PP_EVENT_U32`, `PP_SCOPE`, the transport callback,
and the configuration knobs. Put this directory on your include path and compile
`../src/wattson.c`.

The generated event-id header (`wattson gen-header events.toml -o pp_events.h`) belongs in your
firmware's own source tree, not here — it is specific to your metadata.
