// OpenTune ESP32-S3 interface — USB-CDC host link.
//
// Uses the ESP32-S3's built-in USB Serial/JTAG controller, which enumerates as
// a CDC-ACM virtual serial port with no external USB chip. (To ship a custom
// VID/PID or a composite device, swap this file for a TinyUSB `tusb_cdc_acm`
// backend — the rest of the firmware only calls these four functions.)
#pragma once
#include <stdint.h>
#include <stddef.h>

void    ot_usb_init(void);
// Blocking read of exactly one byte (used by the frame reader's state machine).
// Returns 1 on success, 0 on timeout.
int     ot_usb_read_byte(uint8_t *out, uint32_t timeout_ms);
void    ot_usb_write(const uint8_t *data, size_t len);
