/*
 * wattson.h - firmware instrumentation for the Wattson power profiler.
 *
 * Include this in the firmware under test, call PP_EVENT() around the code paths whose energy
 * you care about, and the current waveform stops being a picture and starts being an answer.
 *
 *     #include "wattson.h"
 *
 *     void radio_send(void)
 *     {
 *         PP_SCOPE(PP_EVT_RADIO) {
 *             radio_enable();
 *             radio_transmit();
 *         }
 *     }
 *
 * Design constraints, in the order they mattered:
 *
 *   1. Nothing but numbers on the wire. An event is a u32 timestamp and a u16 id, or ten bytes
 *      with a u32 value. Names live in a metadata file the host loads; see
 *      protocol/spec/event-ids.md. Instrumentation that measurably changes the energy of the
 *      thing it measures is instrumentation that lies.
 *   2. The timestamp is taken first, before any buffering, so queueing delay lands on the
 *      transport latency budget and not on the measurement.
 *   3. Loss is loud. A full ring buffer increments a counter the host can read; it never
 *      blocks, and it never silently forgets an event.
 *
 * C99. No allocation, no libc beyond <string.h>. One translation unit: add
 * target/src/wattson.c to your build and put target/include on the include path.
 *
 * Configuration is by -D, or by a wattson_config.h of your own if you define
 * PP_USE_CONFIG_HEADER. Every knob has a default that works.
 */

#ifndef WATTSON_H
#define WATTSON_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifdef PP_USE_CONFIG_HEADER
#include "wattson_config.h"
#endif

/* ------------------------------------------------------------------ *
 * Configuration
 * ------------------------------------------------------------------ */

/* Set to 0 to compile every macro in this header down to nothing. The calls disappear, no
 * buffer is reserved, and wattson.c contributes no code. */
#ifndef PP_ENABLED
#define PP_ENABLED 1
#endif

/* Events buffered between flushes. Must be a power of two. A queued event is 12 bytes, so the default costs 1.5 KiB of RAM. */
#ifndef PP_RING_CAPACITY
#define PP_RING_CAPACITY 128u
#endif

/* Ticks of the profiler timer. This MUST be the same timer that stamps power samples - see
 * section 3.2 of protocol/spec/frames.md. Correlation is the entire point of this tool, and it
 * is lost the moment two clocks are involved.
 *
 * If the device under test cannot read that timer directly, leave this at its default of zero
 * and let the profiler stamp events on arrival instead. The transport latency then sets the
 * accuracy floor, and belongs in latency_compensation_ns in the capture metadata. */
#ifndef PP_TIMESTAMP
#define PP_TIMESTAMP() ((uint32_t)0)
#endif

/* Flush automatically once this many events are queued. 0 disables it, leaving flushing
 * entirely to your call site - which is the right choice if PP_EVENT can run in an ISR and
 * your transport cannot. */
#ifndef PP_AUTO_FLUSH_THRESHOLD
#define PP_AUTO_FLUSH_THRESHOLD (PP_RING_CAPACITY / 2u)
#endif

/* Events packed into one EVENT frame, which is what sizes the two framing buffers.
 *
 * 100 fills a 1024-byte payload with ten-byte records, the largest the wire format allows, and
 * costs about 2 KiB of RAM in buffers. On a part where that is a real fraction of the budget,
 * lowering it is the first thing to cut: 16 events per frame costs about 350 bytes and adds one
 * frame header per 16 events, which is nothing against a 400 KB/s sample stream. */
#ifndef PP_MAX_EVENTS_PER_FRAME
#define PP_MAX_EVENTS_PER_FRAME 100u
#endif
#if PP_MAX_EVENTS_PER_FRAME < 1u || PP_MAX_EVENTS_PER_FRAME > 102u
#error "PP_MAX_EVENTS_PER_FRAME must be 1..102; 102 ten-byte records fill the 1024-byte payload"
#endif

/* Critical section around the ring buffer.
 *
 * Needed if PP_EVENT can be reached from more than one context - an ISR and a task, or two
 * tasks. On ARM with GCC or Clang this defaults to masking interrupts. Everywhere else you
 * must either supply your own or declare the single-context case, because a data race here
 * corrupts the timeline in a way that looks exactly like a firmware bug. */
#if !defined(PP_ENTER_CRITICAL) && !defined(PP_SINGLE_CONTEXT)
#if defined(__ARM_ARCH) && (defined(__GNUC__) || defined(__clang__))
#define PP_CRITICAL_STATE uint32_t _pp_primask
#define PP_ENTER_CRITICAL()                                                                    \
    do {                                                                                       \
        __asm__ volatile("mrs %0, primask" : "=r"(_pp_primask)::"memory");                     \
        __asm__ volatile("cpsid i" ::: "memory");                                              \
    } while (0)
#define PP_EXIT_CRITICAL()                                                                     \
    do {                                                                                       \
        if (!(_pp_primask & 1u)) {                                                             \
            __asm__ volatile("cpsie i" ::: "memory");                                          \
        }                                                                                      \
    } while (0)
#elif PP_ENABLED
#error "wattson: define PP_ENTER_CRITICAL and PP_EXIT_CRITICAL, or PP_SINGLE_CONTEXT if only one context ever calls PP_EVENT"
#endif
#endif

#ifndef PP_CRITICAL_STATE
#define PP_CRITICAL_STATE int _pp_critical_unused
#endif
#ifndef PP_ENTER_CRITICAL
#define PP_ENTER_CRITICAL() ((void)0)
#endif
#ifndef PP_EXIT_CRITICAL
#define PP_EXIT_CRITICAL() ((void)0)
#endif

/* ------------------------------------------------------------------ *
 * Transport
 * ------------------------------------------------------------------ */

/*
 * Write encoded bytes out. Return the number accepted.
 *
 * Returning less than len is not an error you need to handle: wattson counts the shortfall as
 * a transport loss and carries on. It must not block indefinitely, and it must not call back
 * into any pp_ function.
 */
typedef size_t (*pp_write_fn)(void *ctx, const uint8_t *data, size_t len);

/* Counters. Every one of these is a way the timeline can be wrong, which is why they are
 * readable rather than internal. */
typedef struct {
    uint32_t events_recorded; /* accepted into the ring                    */
    uint32_t events_sent;     /* framed and written out                    */
    uint32_t events_dropped;  /* lost to a full ring                       */
    uint32_t frames_sent;     /* EVENT frames written                      */
    uint32_t bytes_sent;      /* encoded bytes written                     */
    uint32_t write_failures;  /* short or refused writes from pp_write_fn  */
} pp_stats_t;

/* The logical frame at that size: frame header, block header, records, CRC. */
#define PP_MAX_LOGICAL_FRAME_BYTES (4u + 4u + 10u * PP_MAX_EVENTS_PER_FRAME + 4u)

/* And encoded: COBS adds a byte per 254, plus the leading code byte and the delimiter. */
#define PP_MAX_FRAME_BYTES                                                                     \
    (PP_MAX_LOGICAL_FRAME_BYTES + PP_MAX_LOGICAL_FRAME_BYTES / 254u + 2u)

/* ------------------------------------------------------------------ *
 * API
 * ------------------------------------------------------------------ */

#if PP_ENABLED

/* Install the transport. Safe to call again to swap it; clears the ring and the counters. */
void pp_init(pp_write_fn write, void *ctx);

/* Record a point event. The timestamp is taken immediately. Safe from an ISR. */
void pp_event(uint16_t id);

/* Record an event carrying a value - a packet length, a retry count, a temperature. */
void pp_event_u32(uint16_t id, uint32_t value);

/*
 * Encode everything queued and hand it to the transport.
 *
 * Returns encoded bytes written. Call it from a task or the main loop, not an ISR: it does the
 * CRC and COBS work that pp_event deliberately avoids.
 */
size_t pp_flush(void);

/* Snapshot of the counters. */
void pp_get_stats(pp_stats_t *out);

/* Events currently queued. */
uint32_t pp_pending(void);

/*
 * Encode one EVENT frame from a caller-supplied array, without touching the ring.
 *
 * For a device that already holds its records elsewhere, and for the conformance tests. `dst`
 * needs PP_MAX_FRAME_BYTES. `values` may be NULL, which gives 6-byte records. Returns the
 * encoded length, or 0 if the arguments do not fit.
 */
size_t pp_encode_event_frame(uint8_t *dst, size_t dst_len, uint8_t seq,
                             const uint32_t *timestamps, const uint16_t *ids,
                             const uint32_t *values, size_t count);

#else /* !PP_ENABLED */

#define pp_init(write, ctx) ((void)0)
#define pp_event(id) ((void)0)
#define pp_event_u32(id, value) ((void)(value))
#define pp_flush() ((size_t)0)
#define pp_get_stats(out) ((void)0)
#define pp_pending() ((uint32_t)0)

#endif /* PP_ENABLED */

/* ------------------------------------------------------------------ *
 * Macros
 * ------------------------------------------------------------------ */

#if PP_ENABLED

#define PP_EVENT(id) pp_event((uint16_t)(id))
#define PP_EVENT_U32(id, value) pp_event_u32((uint16_t)(id), (uint32_t)(value))

/*
 * PP_SCOPE - emit a start on entry and a stop on exit.
 *
 *     PP_SCOPE(PP_EVT_SENSOR_READ) {
 *         sensor_read();
 *     }
 *
 * The stop id is start_id + 1, following the paired-id convention in
 * protocol/spec/event-ids.md. Use PP_SCOPE_ID for a pair that does not follow it.
 *
 * Where the compiler supports __attribute__((cleanup)) - GCC and Clang, so every embedded
 * toolchain that matters - the stop survives a return, a goto, and a break out of the middle
 * of the block. PP_SCOPE_IS_SAFE is 1 there and 0 on the fallback, so code that must not leak
 * an unterminated scope can check at compile time.
 */
#if defined(__GNUC__) || defined(__clang__)
#define PP_SCOPE_IS_SAFE 1

typedef struct {
    uint16_t stop_id;
    int done;
} pp_scope_t;

void pp_scope_close(pp_scope_t *s);

#define PP_SCOPE_ID(start_id, stop_id)                                                         \
    for (pp_scope_t _pp_scope __attribute__((cleanup(pp_scope_close))) =                       \
             (pp_event((uint16_t)(start_id)), pp_scope_make((uint16_t)(stop_id)));             \
         !_pp_scope.done; _pp_scope.done = 1)

/* A function rather than a compound literal so the macro is valid C++ too. */
static inline pp_scope_t pp_scope_make(uint16_t stop_id)
{
    pp_scope_t s;
    s.stop_id = stop_id;
    s.done = 0;
    return s;
}

#else
#define PP_SCOPE_IS_SAFE 0

/* Fallback: a plain loop. A return or goto out of the body skips the stop, and the host then
 * reports the occurrence as unterminated rather than inventing a duration for it. */
#define PP_SCOPE_ID(start_id, stop_id)                                                         \
    for (int _pp_scope = (pp_event((uint16_t)(start_id)), 0); !_pp_scope;                      \
         _pp_scope = 1, pp_event((uint16_t)(stop_id)))
#endif

#define PP_SCOPE(start_id) PP_SCOPE_ID((start_id), (uint16_t)((start_id) + 1u))

#else /* !PP_ENABLED */

#define PP_SCOPE_IS_SAFE 1
#define PP_EVENT(id) ((void)0)
#define PP_EVENT_U32(id, value) ((void)(value))
#define PP_SCOPE_ID(start_id, stop_id)
#define PP_SCOPE(start_id)

#endif /* PP_ENABLED */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WATTSON_H */
