/*
 * FreeRTOS task tracing - per-task energy attribution.
 *
 * Hooking the trace macros turns the power timeline into an answer no oscilloscope can give:
 * *which task is responsible for this battery drain?* The RTOS already calls these on every
 * context switch, so instrumenting them costs one ring-buffer push per switch and no changes
 * to application code at all.
 *
 * Add to FreeRTOSConfig.h:
 *
 *     #include "wattson_freertos.h"
 *
 * where that header contains the trace macro definitions below. They must be visible to
 * FreeRTOS's own sources, not just yours.
 *
 * Status: this is a worked sketch of the phase-6 integration, not a tested port. It compiles
 * against FreeRTOS and nothing here is subtle, but nobody has run it on hardware. Treat it as
 * a starting point.
 *
 * Cost: a context switch on a Cortex-M4 is a few hundred cycles, and pp_event adds roughly
 * twenty. Below 10% overhead on the switch itself, and unmeasurable against anything the task
 * actually does. Verify it on your own build before trusting a number: instrumentation that
 * changes the energy of the thing it measures is instrumentation that lies.
 */

#include "wattson.h"

/* ------------------------------------------------------------------ *
 * Event ids
 * ------------------------------------------------------------------ */

/*
 * Task switches use a category of their own, with the task number in the low byte, so a
 * 32-task system fits without colliding with application events.
 *
 * The names come from the metadata file, which for tasks is best generated at build time from
 * the same list that creates them - see docs/instrumentation.md.
 */
#define PP_EVT_TASK_BASE 0x0500u
#define PP_EVT_TASK(number) (uint16_t)(PP_EVT_TASK_BASE + ((number) & 0xFFu))

#define PP_EVT_ISR_ENTER 0x0600u
#define PP_EVT_ISR_EXIT 0x0601u
#define PP_EVT_IDLE_ENTER 0x0602u
#define PP_EVT_IDLE_EXIT 0x0603u

/* ------------------------------------------------------------------ *
 * Trace hooks
 * ------------------------------------------------------------------ */

/*
 * A switch is one event, not a stop and a start.
 *
 * The scheduler always runs exactly one task, so the previous task ends precisely when the
 * next begins. Emitting both halves would double the event rate to say the same thing, and the
 * host reconstructs each task's occupancy from the switch sequence anyway.
 */
void pp_trace_task_switched_in(uint32_t task_number)
{
    pp_event(PP_EVT_TASK(task_number));
}

/*
 * Tickless idle deserves its own pair. It is where a low-power design spends almost all of its
 * time, and the interesting number is usually how little of the timeline it covers.
 */
void pp_trace_idle_enter(void)
{
    pp_event(PP_EVT_IDLE_ENTER);
}

void pp_trace_idle_exit(void)
{
    pp_event(PP_EVT_IDLE_EXIT);
}

/*
 * ISR entry and exit carry the vector number, so one pair of ids covers every interrupt and
 * the host can still separate them.
 */
void pp_trace_isr_enter(uint32_t vector)
{
    pp_event_u32(PP_EVT_ISR_ENTER, vector);
}

void pp_trace_isr_exit(uint32_t vector)
{
    pp_event_u32(PP_EVT_ISR_EXIT, vector);
}

/*
 * The macros themselves. In a real port these live in wattson_freertos.h, included from
 * FreeRTOSConfig.h.
 *
 *     #define traceTASK_SWITCHED_IN() \
 *         pp_trace_task_switched_in((uint32_t)pxCurrentTCB->uxTCBNumber)
 *
 *     #define traceISR_ENTER()  pp_trace_isr_enter(__get_IPSR())
 *     #define traceISR_EXIT()   pp_trace_isr_exit(__get_IPSR())
 *
 * Two things to get right, both of which produce a timeline that looks plausible and is wrong:
 *
 *   1. PP_ENTER_CRITICAL must actually mask interrupts. These hooks run from the scheduler and
 *      from ISRs, which is the multi-context case the ring buffer needs protection for. The
 *      default on ARM does this; if you have overridden it with a FreeRTOS mutex, that is a
 *      deadlock waiting inside a context switch.
 *
 *   2. Do not flush from these hooks. Call pp_flush() from the idle task or a low-priority
 *      task, and set PP_AUTO_FLUSH_THRESHOLD to 0 so nothing flushes from an ISR behind your
 *      back.
 */

/* ------------------------------------------------------------------ *
 * Where flushing belongs
 * ------------------------------------------------------------------ */

/*
 * The lowest-priority task in the system, so framing work never delays real work. It is
 * charged to the idle time the profiler is measuring, which is honest: the instrumentation
 * really does cost that.
 */
void pp_flush_task(void *arg)
{
    (void)arg;
    for (;;) {
        (void)pp_flush();
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}
