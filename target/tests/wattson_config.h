/*
 * Build configuration for the conformance tests, and a worked example of the
 * PP_USE_CONFIG_HEADER path a real firmware project would use.
 */

#ifndef WATTSON_CONFIG_H
#define WATTSON_CONFIG_H

#include <stdint.h>

/* A tick source the test controls, so an asserted timestamp is the one the test chose. On a
 * device this is the profiler timer - the same one that stamps power samples. */
uint32_t pp_test_timestamp(void);
#define PP_TIMESTAMP() pp_test_timestamp()

/* The tests drive everything from one thread. */
#define PP_SINGLE_CONTEXT 1

/* Flushing is explicit here, so a test can fill the ring and observe the overflow counter
 * rather than having an auto-flush quietly rescue it. */
#define PP_AUTO_FLUSH_THRESHOLD 0

#endif /* WATTSON_CONFIG_H */
