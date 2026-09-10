/*
 * GPIO instrumentation - when the event is too short for a UART.
 *
 * The firmware toggles a pin; the profiler board timestamps the edge with the same timer that
 * stamps power samples. Latency is a single register write, well under a microsecond, so a
 * 50 us event boundary is resolved to better than 2%. Nothing else in the latency budget comes
 * close.
 *
 * The cost is expressiveness. A pin carries one bit, so this scheme trades the wire format's
 * 65535 event ids for however many pins you can spare, and carries no value field at all. That
 * is a real limitation, not a detail to work around: use GPIO for the few boundaries that are
 * too short to measure any other way, and UART for everything else. Both at once is normal -
 * they share the profiler's timer, so their events land on one timeline.
 *
 * Note that this file never calls pp_event(). GPIO signalling bypasses the instrumentation
 * library entirely: the profiler board observes the pins and emits GPIO_EVENT frames itself.
 * The library is for events that travel as data.
 *
 * Build: cc -Itarget/include target/examples/gpio.c
 */

#include "example_hal.h"

/*
 * One pin per instrumented state.
 *
 * Record the mapping in the capture metadata - gpio.<pin>.name in events.toml - or the capture
 * arrives with an unlabelled digital track and nobody remembers what pin 3 meant.
 */
#define PP_PIN_RADIO_TX 0u
#define PP_PIN_SENSOR 1u
#define PP_PIN_FLASH 2u

/*
 * Toggle with the pin high for the duration of the state.
 *
 * Level, not pulse. A level makes both boundaries observable and survives a missed edge: the
 * profiler samples the full 16-pin state at every edge, so a pin's waveform reconstructs from
 * a linear scan with no initial-state bookkeeping. A pulse scheme loses the duration entirely
 * if either edge is missed.
 */
#define PP_GPIO_ENTER(pin) gpio_write((pin), 1)
#define PP_GPIO_EXIT(pin) gpio_write((pin), 0)

void instrumentation_init(void)
{
    gpio_init_output(PP_PIN_RADIO_TX);
    gpio_init_output(PP_PIN_SENSOR);
    gpio_init_output(PP_PIN_FLASH);

    /* Drive them low before the capture starts. A pin left floating reads as noise, and noise
     * on the digital track becomes hundreds of phantom state changes. */
    PP_GPIO_EXIT(PP_PIN_RADIO_TX);
    PP_GPIO_EXIT(PP_PIN_SENSOR);
    PP_GPIO_EXIT(PP_PIN_FLASH);
}

void radio_send(const uint8_t *payload, uint16_t length)
{
    /* Innermost possible placement. Every instruction between the pin write and the thing
     * being measured is error in the measurement, and the whole reason to use GPIO here is
     * that the event is short enough for that to matter. */
    PP_GPIO_ENTER(PP_PIN_RADIO_TX);
    radio_enable();
    radio_transmit(payload, length);
    radio_disable();
    PP_GPIO_EXIT(PP_PIN_RADIO_TX);
}

void sensor_sample(void)
{
    PP_GPIO_ENTER(PP_PIN_SENSOR);
    sensor_read();
    PP_GPIO_EXIT(PP_PIN_SENSOR);
}

void flash_commit(uint32_t address)
{
    PP_GPIO_ENTER(PP_PIN_FLASH);
    flash_write_page(address);
    PP_GPIO_EXIT(PP_PIN_FLASH);
}
