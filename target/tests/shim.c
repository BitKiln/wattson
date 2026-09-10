/*
 * Test shim: the parts of a real integration that a Rust test cannot express directly.
 *
 * Two things live here rather than in the test itself. A controllable tick source, so the
 * timestamps in an assertion are the ones the test chose rather than whatever a clock did. And
 * thin wrappers around PP_SCOPE, PP_EVENT and friends - they are preprocessor macros, so the
 * only way to prove the macros work is to expand them in C.
 *
 * This file is not part of the library. Nothing here ships to a device.
 */

#include "wattson.h"

#include <string.h>

/* ------------------------------------------------------------------ *
 * Tick source
 * ------------------------------------------------------------------ */

static uint32_t pp_test_clock = 0;

uint32_t pp_test_timestamp(void)
{
    return pp_test_clock;
}

void pp_test_set_time(uint32_t t)
{
    pp_test_clock = t;
}

void pp_test_advance(uint32_t d)
{
    pp_test_clock += d;
}

/* ------------------------------------------------------------------ *
 * Capturing transport
 * ------------------------------------------------------------------ */

#define PP_TEST_SINK_CAPACITY 65536u

static uint8_t pp_test_sink[PP_TEST_SINK_CAPACITY];
static size_t pp_test_sink_len = 0;

/* Bytes the transport accepts per call before reporting a short write. 0 means unlimited. */
static size_t pp_test_write_limit = 0;

static size_t pp_test_write(void *ctx, const uint8_t *data, size_t len)
{
    size_t accept = len;
    (void)ctx;

    if (pp_test_write_limit != 0u && accept > pp_test_write_limit) {
        accept = pp_test_write_limit;
    }
    if (pp_test_sink_len + accept > PP_TEST_SINK_CAPACITY) {
        accept = PP_TEST_SINK_CAPACITY - pp_test_sink_len;
    }
    memcpy(&pp_test_sink[pp_test_sink_len], data, accept);
    pp_test_sink_len += accept;
    return accept;
}

void pp_test_reset(void)
{
    pp_test_sink_len = 0;
    pp_test_write_limit = 0;
    pp_test_clock = 0;
    pp_init(pp_test_write, NULL);
}

/* Install no transport at all, to exercise the unconfigured path. */
void pp_test_reset_without_transport(void)
{
    pp_test_sink_len = 0;
    pp_test_write_limit = 0;
    pp_test_clock = 0;
    pp_init(NULL, NULL);
}

void pp_test_set_write_limit(size_t limit)
{
    pp_test_write_limit = limit;
}

const uint8_t *pp_test_sink_data(void)
{
    return pp_test_sink;
}

size_t pp_test_sink_size(void)
{
    return pp_test_sink_len;
}

/* ------------------------------------------------------------------ *
 * Macro expansion sites
 * ------------------------------------------------------------------ */

void pp_test_macro_event(uint16_t id)
{
    PP_EVENT(id);
}

void pp_test_macro_event_u32(uint16_t id, uint32_t value)
{
    PP_EVENT_U32(id, value);
}

/* A plain scope, with time passing inside it. */
void pp_test_macro_scope(uint16_t start_id, uint32_t duration)
{
    PP_SCOPE(start_id)
    {
        pp_test_advance(duration);
    }
}

/* Nested occurrences of the same scope - what a re-entrant instrumented function produces,
 * and the case the host pairs with a stack. */
void pp_test_macro_scope_nested(uint16_t start_id, uint32_t step)
{
    PP_SCOPE(start_id)
    {
        pp_test_advance(step);
        PP_SCOPE(start_id)
        {
            pp_test_advance(step);
        }
        pp_test_advance(step);
    }
}

/* Returning out of the middle of a scope. With __attribute__((cleanup)) the stop is still
 * emitted; without it the host sees an unterminated start. PP_SCOPE_IS_SAFE says which. */
int pp_test_macro_scope_early_return(uint16_t start_id, uint32_t duration)
{
    PP_SCOPE(start_id)
    {
        pp_test_advance(duration);
        return 1;
    }
    return 0;
}

void pp_test_macro_scope_explicit(uint16_t start_id, uint16_t stop_id, uint32_t duration)
{
    PP_SCOPE_ID(start_id, stop_id)
    {
        pp_test_advance(duration);
    }
}

int pp_test_scope_is_safe(void)
{
    return PP_SCOPE_IS_SAFE;
}

/* Sizes the Rust side asserts against, so a configuration change that quietly grows the
 * library's RAM footprint shows up as a failing test rather than a link error on a device. */
size_t pp_test_ring_capacity(void)
{
    return (size_t)PP_RING_CAPACITY;
}
