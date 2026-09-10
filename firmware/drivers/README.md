# Measurement drivers

**Status: deferred to phase 2.**

One interface, many current-sense front ends, so the host software stays useful to someone who
builds their own measurement board:

```c
struct pp_measurement_driver {
    int (*init)(void);
    int (*start)(void);
    int (*stop)(void);
    int (*read)(pp_sample_t *);
};
```

Planned implementations: `ina226/`, `ina228/`, `simulated/`, with room for INA229, INA237, an
external SPI ADC, and a custom shunt amplifier.
