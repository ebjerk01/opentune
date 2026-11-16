// OpenTune ESP32-S3 interface — USB-CDC host link (USB Serial/JTAG backend).
#include "usb_cdc.h"
#include "driver/usb_serial_jtag.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

void ot_usb_init(void)
{
    usb_serial_jtag_driver_config_t cfg = {
        .tx_buffer_size = 1024,
        .rx_buffer_size = 1024,
    };
    usb_serial_jtag_driver_install(&cfg);
}

int ot_usb_read_byte(uint8_t *out, uint32_t timeout_ms)
{
    int n = usb_serial_jtag_read_bytes(out, 1, pdMS_TO_TICKS(timeout_ms));
    return n == 1 ? 1 : 0;
}

void ot_usb_write(const uint8_t *data, size_t len)
{
    size_t off = 0;
    while (off < len) {
        int n = usb_serial_jtag_write_bytes(data + off, len - off, pdMS_TO_TICKS(100));
        if (n <= 0) break;
        off += (size_t)n;
    }
}
