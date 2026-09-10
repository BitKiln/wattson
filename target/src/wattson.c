/*
 * wattson.c - the whole target-side instrumentation library.
 *
 * See target/include/wattson.h for the API and target/README.md for the contract. The wire
 * format this file produces is normative in protocol/spec/frames.md, and
 * target/rust/tests/conformance.rs checks the bytes against the same golden vectors an
 * independent implementation would be held to.
 *
 * Split of work, which is the only structural decision here that matters:
 *
 *   pp_event()  takes a timestamp and pushes 12 bytes into a ring. Bounded, allocation-free,
 *               interrupt-safe, and nothing else. Roughly twenty instructions.
 *   pp_flush()  does the CRC, the COBS encoding and the write. Called from a task.
 *
 * Doing the framing in pp_event() would put a CRC over a kilobyte inside whatever ISR happened
 * to be instrumented, which is exactly the kind of overhead that changes the energy of the
 * thing being measured.
 */

#include "wattson.h"

#if PP_ENABLED

#include <string.h>

#if (PP_RING_CAPACITY & (PP_RING_CAPACITY - 1u)) != 0u
#error "PP_RING_CAPACITY must be a power of two"
#endif
#if PP_RING_CAPACITY < 2u
#error "PP_RING_CAPACITY must be at least 2"
#endif

/* Wire constants, from protocol/spec/frames.md. */
#define PP_FRAME_EVENT 0x11u
#define PP_FRAME_HEADER 4u
#define PP_EVENT_BLOCK_HEADER 4u
#define PP_RECORD_SMALL 6u
#define PP_RECORD_WITH_VALUE 10u
#define PP_MAX_PAYLOAD 1024u

/* ------------------------------------------------------------------ *
 * CRC-32/ISO-HDLC
 * ------------------------------------------------------------------ */

/*
 * Nibble-at-a-time, so the table is 64 bytes rather than the usual kilobyte. On a part with
 * 264 KiB of RAM that trade is obvious; on the smallest ones it is the difference between
 * fitting and not. Roughly 8 cycles per byte on a Cortex-M0+, so a full 1024-byte frame costs
 * about 8 us at 125 MHz - well under the 780 us between frames at any realistic event rate.
 *
 * A device with a CRC peripheral (STM32) or a DMA sniffer (RP2040) should skip this entirely;
 * both compute this exact polynomial in hardware, which is why it was chosen.
 */
static const uint32_t PP_CRC_TABLE[16] = {
    0x00000000u, 0x1DB71064u, 0x3B6E20C8u, 0x26D930ACu, 0x76DC4190u, 0x6B6B51F4u,
    0x4DB26158u, 0x5005713Cu, 0xEDB88320u, 0xF00F9344u, 0xD6D6A3E8u, 0xCB61B38Cu,
    0x9B64C2B0u, 0x86D3D2D4u, 0xA00AE278u, 0xBDBDF21Cu,
};

static uint32_t pp_crc32(const uint8_t *data, size_t len)
{
    uint32_t crc = 0xFFFFFFFFu;
    size_t i;
    for (i = 0; i < len; i++) {
        crc ^= data[i];
        crc = (crc >> 4) ^ PP_CRC_TABLE[crc & 0x0Fu];
        crc = (crc >> 4) ^ PP_CRC_TABLE[crc & 0x0Fu];
    }
    return ~crc;
}

/* ------------------------------------------------------------------ *
 * COBS
 * ------------------------------------------------------------------ */

/*
 * Consistent Overhead Byte Stuffing, per Cheshire and Baker. Encodes `len` bytes into `dst`
 * and appends the 0x00 delimiter. `dst` must hold len + len/254 + 2 bytes.
 *
 * The delimiter is what lets a host attach to an already-running device and resynchronise
 * without guessing where a frame starts.
 */
static size_t pp_cobs_encode(uint8_t *dst, const uint8_t *src, size_t len)
{
    size_t read = 0;
    size_t code_at = 0; /* where the pending run length goes */
    size_t write = 1;
    uint8_t code = 1;

    for (read = 0; read < len; read++) {
        if (src[read] == 0u) {
            dst[code_at] = code;
            code_at = write++;
            code = 1;
        } else {
            dst[write++] = src[read];
            code++;
            if (code == 0xFFu) {
                dst[code_at] = code;
                code_at = write++;
                code = 1;
            }
        }
    }
    dst[code_at] = code;
    dst[write++] = 0x00u;
    return write;
}

/* ------------------------------------------------------------------ *
 * Little-endian stores
 * ------------------------------------------------------------------ */

/*
 * Byte at a time rather than a cast. The wire is little-endian regardless of the target, an
 * unaligned u32 store faults on a Cortex-M0+, and any compiler worth using folds these back
 * into a single store when the target allows it.
 */
static void pp_put_u16(uint8_t *p, uint16_t v)
{
    p[0] = (uint8_t)(v & 0xFFu);
    p[1] = (uint8_t)(v >> 8);
}

static void pp_put_u32(uint8_t *p, uint32_t v)
{
    p[0] = (uint8_t)(v & 0xFFu);
    p[1] = (uint8_t)((v >> 8) & 0xFFu);
    p[2] = (uint8_t)((v >> 16) & 0xFFu);
    p[3] = (uint8_t)((v >> 24) & 0xFFu);
}

/* ------------------------------------------------------------------ *
 * State
 * ------------------------------------------------------------------ */

typedef struct {
    uint32_t timestamp;
    uint32_t value;
    uint16_t id;
    uint8_t has_value;
} pp_record_t;

static struct {
    pp_record_t ring[PP_RING_CAPACITY];
    volatile uint32_t head; /* producer; only pp_event advances it   */
    uint32_t tail;          /* consumer; only pp_flush advances it   */
    pp_write_fn write;
    void *ctx;
    uint8_t seq;
    pp_stats_t stats;
    uint8_t scratch[PP_MAX_LOGICAL_FRAME_BYTES];
    uint8_t encoded[PP_MAX_FRAME_BYTES];
} pp;

void pp_init(pp_write_fn write, void *ctx)
{
    PP_CRITICAL_STATE;
    PP_ENTER_CRITICAL();
    memset(&pp, 0, sizeof(pp));
    pp.write = write;
    pp.ctx = ctx;
    PP_EXIT_CRITICAL();
}

void pp_get_stats(pp_stats_t *out)
{
    PP_CRITICAL_STATE;
    if (out == NULL) {
        return;
    }
    PP_ENTER_CRITICAL();
    *out = pp.stats;
    PP_EXIT_CRITICAL();
}

uint32_t pp_pending(void)
{
    PP_CRITICAL_STATE;
    uint32_t n;
    PP_ENTER_CRITICAL();
    n = pp.head - pp.tail;
    PP_EXIT_CRITICAL();
    return n;
}

/* ------------------------------------------------------------------ *
 * Recording
 * ------------------------------------------------------------------ */

/*
 * The ring is full-drops-newest rather than full-overwrites-oldest.
 *
 * Overwriting would corrupt already-recorded history to make room for the present, which turns
 * a burst into a timeline with a hole nobody can see. Dropping the newest loses the same
 * number of events but keeps everything already recorded true, and the counter says exactly
 * how many went missing. Loss is loud.
 */
static void pp_push(uint16_t id, uint32_t value, uint8_t has_value, uint32_t timestamp)
{
    PP_CRITICAL_STATE;
    uint32_t used;

    /* Id 0 is reserved as invalid by the wire format. Silently transmitting it would produce
     * an event the host cannot attribute to anything, so it is counted as a drop instead. */
    if (id == 0u) {
        PP_ENTER_CRITICAL();
        pp.stats.events_dropped++;
        PP_EXIT_CRITICAL();
        return;
    }

    PP_ENTER_CRITICAL();
    used = pp.head - pp.tail;
    if (used >= PP_RING_CAPACITY) {
        pp.stats.events_dropped++;
        PP_EXIT_CRITICAL();
        return;
    }
    {
        pp_record_t *r = &pp.ring[pp.head & (PP_RING_CAPACITY - 1u)];
        r->timestamp = timestamp;
        r->value = value;
        r->id = id;
        r->has_value = has_value;
    }
    pp.head++;
    pp.stats.events_recorded++;
    used++;
    PP_EXIT_CRITICAL();

#if PP_AUTO_FLUSH_THRESHOLD > 0
    if (used >= (uint32_t)(PP_AUTO_FLUSH_THRESHOLD)) {
        (void)pp_flush();
    }
#endif
}

void pp_event(uint16_t id)
{
    /* Timestamp first. Everything after this point is queueing, and queueing must not be
     * charged to the code being measured. */
    uint32_t t = PP_TIMESTAMP();
    pp_push(id, 0u, 0u, t);
}

void pp_event_u32(uint16_t id, uint32_t value)
{
    uint32_t t = PP_TIMESTAMP();
    pp_push(id, value, 1u, t);
}

#if defined(__GNUC__) || defined(__clang__)
void pp_scope_close(pp_scope_t *s)
{
    pp_event(s->stop_id);
}
#endif

/* ------------------------------------------------------------------ *
 * Framing
 * ------------------------------------------------------------------ */

size_t pp_encode_event_frame(uint8_t *dst, size_t dst_len, uint8_t seq,
                             const uint32_t *timestamps, const uint16_t *ids,
                             const uint32_t *values, size_t count)
{
    uint8_t *scratch = pp.scratch;
    uint16_t record_size = (values != NULL) ? PP_RECORD_WITH_VALUE : PP_RECORD_SMALL;
    size_t payload_len = PP_EVENT_BLOCK_HEADER + (size_t)record_size * count;
    size_t frame_len = PP_FRAME_HEADER + payload_len + 4u;
    size_t off;
    size_t i;
    uint32_t crc;

    if (count == 0u || count > PP_MAX_EVENTS_PER_FRAME || dst == NULL || timestamps == NULL ||
        ids == NULL) {
        return 0;
    }
    if (payload_len > PP_MAX_PAYLOAD || dst_len < frame_len + frame_len / 254u + 2u) {
        return 0;
    }

    scratch[0] = PP_FRAME_EVENT;
    pp_put_u16(&scratch[1], (uint16_t)payload_len);
    scratch[3] = seq;

    pp_put_u16(&scratch[PP_FRAME_HEADER + 0u], (uint16_t)count);
    pp_put_u16(&scratch[PP_FRAME_HEADER + 2u], record_size);

    off = PP_FRAME_HEADER + PP_EVENT_BLOCK_HEADER;
    for (i = 0; i < count; i++) {
        pp_put_u32(&scratch[off], timestamps[i]);
        pp_put_u16(&scratch[off + 4u], ids[i]);
        if (values != NULL) {
            pp_put_u32(&scratch[off + 6u], values[i]);
        }
        off += record_size;
    }

    crc = pp_crc32(scratch, PP_FRAME_HEADER + payload_len);
    pp_put_u32(&scratch[PP_FRAME_HEADER + payload_len], crc);

    return pp_cobs_encode(dst, scratch, frame_len);
}

/*
 * Drain one run of records that share a record size.
 *
 * A block is all six-byte records or all ten-byte ones - the wire format has one record_size
 * per block - so a mixed queue becomes several frames. Runs rather than two separate queues,
 * because reordering events to save a frame would reorder the timeline, and the timeline is
 * the product.
 *
 * Records are serialised straight out of the ring into the frame scratch buffer. Copying them
 * into local arrays first would read better and cost a kilobyte of stack in whatever task
 * calls flush, which is not a trade an MCU library gets to make.
 *
 * Returns encoded bytes written, or 0 when there is nothing left to send.
 */
static size_t pp_flush_one(void)
{
    uint8_t *scratch = pp.scratch;
    size_t count = 0;
    uint16_t record_size;
    uint8_t has_value;
    uint32_t head;
    size_t off;
    size_t payload_len;
    size_t frame_len;
    size_t encoded_len;
    size_t written;

    PP_CRITICAL_STATE;
    PP_ENTER_CRITICAL();
    head = pp.head;
    PP_EXIT_CRITICAL();

    if (pp.tail == head) {
        return 0;
    }

    has_value = pp.ring[pp.tail & (PP_RING_CAPACITY - 1u)].has_value;
    record_size = has_value ? PP_RECORD_WITH_VALUE : PP_RECORD_SMALL;

    off = PP_FRAME_HEADER + PP_EVENT_BLOCK_HEADER;
    while (pp.tail != head && count < PP_MAX_EVENTS_PER_FRAME) {
        const pp_record_t *r = &pp.ring[pp.tail & (PP_RING_CAPACITY - 1u)];
        if (r->has_value != has_value) {
            break;
        }
        pp_put_u32(&scratch[off], r->timestamp);
        pp_put_u16(&scratch[off + 4u], r->id);
        if (has_value) {
            pp_put_u32(&scratch[off + 6u], r->value);
        }
        off += record_size;
        count++;
        pp.tail++;
    }

    if (pp.write == NULL) {
        /* No transport installed. The records are consumed rather than left to block the ring
         * forever, and counted as dropped so the loss is visible. */
        pp.stats.events_dropped += (uint32_t)count;
        return 0;
    }

    payload_len = PP_EVENT_BLOCK_HEADER + (size_t)record_size * count;
    frame_len = PP_FRAME_HEADER + payload_len + 4u;

    scratch[0] = PP_FRAME_EVENT;
    pp_put_u16(&scratch[1], (uint16_t)payload_len);
    scratch[3] = pp.seq;
    pp_put_u16(&scratch[PP_FRAME_HEADER + 0u], (uint16_t)count);
    pp_put_u16(&scratch[PP_FRAME_HEADER + 2u], record_size);
    pp_put_u32(&scratch[PP_FRAME_HEADER + payload_len],
               pp_crc32(scratch, PP_FRAME_HEADER + payload_len));

    encoded_len = pp_cobs_encode(pp.encoded, scratch, frame_len);
    pp.seq++;

    written = pp.write(pp.ctx, pp.encoded, encoded_len);
    if (written != encoded_len) {
        /* A partial frame is not a partial event: the host discards the region at the next
         * delimiter, so every record in it is gone. Count them, do not pretend otherwise. */
        pp.stats.write_failures++;
        pp.stats.events_dropped += (uint32_t)count;
        pp.stats.bytes_sent += (uint32_t)written;
        return written;
    }

    pp.stats.frames_sent++;
    pp.stats.events_sent += (uint32_t)count;
    pp.stats.bytes_sent += (uint32_t)encoded_len;
    return encoded_len;
}

size_t pp_flush(void)
{
    size_t total = 0;
    size_t n;

    /* Re-entrancy guard. The auto-flush path can reach pp_flush from inside a transport that
     * itself instruments something, and a nested drain would corrupt the tail. */
    static volatile int in_flush = 0;
    PP_CRITICAL_STATE;
    PP_ENTER_CRITICAL();
    if (in_flush) {
        PP_EXIT_CRITICAL();
        return 0;
    }
    in_flush = 1;
    PP_EXIT_CRITICAL();

    while ((n = pp_flush_one()) > 0u) {
        total += n;
    }

    in_flush = 0;
    return total;
}

#else /* !PP_ENABLED */

/* ISO C forbids an empty translation unit, and a build system that compiles this file with
 * instrumentation off should not have to know that. */
typedef int pp_disabled_translation_unit;

#endif /* PP_ENABLED */
