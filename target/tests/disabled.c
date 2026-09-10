/*
 * Compiled with PP_ENABLED=0.
 *
 * The whole file exists to prove that instrumentation really does vanish: every macro must
 * still compile in every position a real call site puts it, and none of them may reference a
 * symbol, because with instrumentation off wattson.c defines none.
 *
 * A macro that only compiles out on the author's machine is a macro that breaks someone's
 * release build.
 */

#include "wattson.h"

uint32_t pp_disabled_smoke(uint16_t id, uint32_t value)
{
    uint32_t n = 0;

    PP_EVENT(id);
    PP_EVENT_U32(id, value);

    PP_SCOPE(id)
    {
        n++;
    }

    PP_SCOPE_ID(id, (uint16_t)(id + 1u))
    {
        n++;
    }

    /* An if with no braces around the scope: the macro must not expand to something that
     * swallows the else. */
    if (n != 0u) {
        PP_SCOPE(id)
        {
            n++;
        }
    } else {
        n = 0;
    }

    pp_event(id);
    pp_event_u32(id, value);
    n += (uint32_t)pp_flush();
    n += pp_pending();

    return n;
}
