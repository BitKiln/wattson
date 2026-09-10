/*
 * UART instrumentation - the default choice.
 *
 * The firmware writes framed EVENT frames out a spare UART; the profiler board forwards them
 * to the host interleaved with its power samples. Every event id and value the wire format
 * supports gets through, which is what GPIO signalling cannot offer.
 *
 * Latency: about 10 us of byte time at 1 Mbaud plus the call overhead, or roughly 1% of a
 * 950 us radio transmission. Below about 100 us of event duration, use GPIO instead - see
 * gpio.c and the latency budget in docs/instrumentation.md.
 *
 * Two rules that are easy to get wrong:
 *
 *   1. pp_flush() belongs in a task or the main loop, never in an ISR. pp_event() is the cheap
 *      half by design; flushing does the CRC and the COBS encoding.
 *   2. The UART write must not block forever. Returning short is fine - wattson counts the
 *      loss and carries on - but a blocking write inside instrumentation stalls the very code
 *      whose timing is being measured.
 *
 * This example records from one context - the application - so it is built with
 * -DPP_SINGLE_CONTEXT. Firmware that instruments an ISR as well must supply
 * PP_ENTER_CRITICAL/PP_EXIT_CRITICAL instead; wattson.h refuses to guess, because a data race
 * on the ring corrupts the timeline in a way that looks exactly like a firmware bug.
 *
 * Build: cc -Itarget/include -DPP_SINGLE_CONTEXT target/src/wattson.c target/examples/uart.c
 */

#include "wattson.h"

#include "example_hal.h"

/* Event ids. Generate this header from your metadata with:
 *
 *     wattson gen-header events.toml -o pp_events.h
 *
 * so the ids the firmware sends and the names the host shows cannot drift apart. Spelled out
 * here to keep the example self-contained. */
#define PP_EVT_RADIO_TX 0x0100u
#define PP_EVT_RADIO_TX_STOP 0x0101u
#define PP_EVT_SENSOR_READ 0x0200u
#define PP_EVT_PACKET_TX 0x0110u

/* ------------------------------------------------------------------ *
 * Board glue
 * ------------------------------------------------------------------ */

/*
 * The transport. Non-blocking: it moves what fits in the FIFO and reports how much.
 *
 * A short write loses the frame it was part of, which wattson counts in write_failures. If
 * that counter is not zero on a real run, the UART is too slow for the event rate and the
 * answer is a faster baud rate or fewer instrumented call sites - not a bigger buffer, which
 * only moves the loss later.
 */
static size_t uart_write(void *ctx, const uint8_t *data, size_t len)
{
    size_t sent = 0;
    (void)ctx;

    while (sent < len && uart_tx_ready()) {
        uart_tx_byte(data[sent]);
        sent++;
    }
    return sent;
}

void instrumentation_init(void)
{
    uart_init(1000000u); /* 1 Mbaud; see the latency budget before going lower */
    pp_init(uart_write, NULL);
}

/* Call from the idle task or the bottom of the main loop. */
void instrumentation_poll(void)
{
    (void)pp_flush();
}

/* ------------------------------------------------------------------ *
 * Instrumented code
 * ------------------------------------------------------------------ */

void radio_send(const uint8_t *payload, uint16_t length)
{
    /* The value rides with the event, so the host can plot energy against packet size and find
     * the point where a longer packet stops being cheaper than a second one. */
    PP_EVENT_U32(PP_EVT_PACKET_TX, length);

    PP_SCOPE_ID(PP_EVT_RADIO_TX, PP_EVT_RADIO_TX_STOP)
    {
        radio_enable();
        radio_transmit(payload, length);
        radio_disable();
    }
}

void sensor_sample(void)
{
    /* PP_SCOPE derives the stop id as start + 1, which is the convention every id in
     * protocol/spec/event-ids.md follows. */
    PP_SCOPE(PP_EVT_SENSOR_READ)
    {
        sensor_read();
    }
}
