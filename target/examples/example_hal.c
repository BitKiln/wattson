/* Do-nothing board functions, so the examples compile with no board attached. */

#include "example_hal.h"

void uart_init(uint32_t baud)
{
    (void)baud;
}

int uart_tx_ready(void)
{
    return 1;
}

void uart_tx_byte(uint8_t byte)
{
    (void)byte;
}

void gpio_init_output(uint8_t pin)
{
    (void)pin;
}

void gpio_write(uint8_t pin, int level)
{
    (void)pin;
    (void)level;
}

void radio_enable(void) {}

void radio_disable(void) {}

void radio_transmit(const uint8_t *payload, uint16_t length)
{
    (void)payload;
    (void)length;
}

void sensor_read(void) {}

void flash_write_page(uint32_t address)
{
    (void)address;
}
