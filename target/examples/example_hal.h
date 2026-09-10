/*
 * The board functions the examples pretend to have.
 *
 * Real firmware has its own; this header exists so the examples compile in CI. An example that
 * is never built is an example that stops being true, and instrumentation advice that no
 * longer compiles is worse than none.
 *
 * example_hal.c provides do-nothing definitions.
 */

#ifndef WATTSON_EXAMPLE_HAL_H
#define WATTSON_EXAMPLE_HAL_H

#include <stddef.h>
#include <stdint.h>

void uart_init(uint32_t baud);
int uart_tx_ready(void);
void uart_tx_byte(uint8_t byte);

void gpio_init_output(uint8_t pin);
void gpio_write(uint8_t pin, int level);

void radio_enable(void);
void radio_disable(void);
void radio_transmit(const uint8_t *payload, uint16_t length);
void sensor_read(void);
void flash_write_page(uint32_t address);

#endif /* WATTSON_EXAMPLE_HAL_H */
