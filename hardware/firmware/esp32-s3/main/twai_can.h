// OpenTune ESP32-S3 interface — CAN via the TWAI peripheral.
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"

typedef struct {
    uint32_t id;       // 11- or 29-bit identifier
    bool     extended; // true for 29-bit
    bool     rtr;      // remote frame
    uint8_t  dlc;      // 0..8
    uint8_t  data[8];
} ot_can_frame_t;

// Set the bitrate used by the next ot_can_start(). Common rates are supported
// exactly (125k/250k/500k/1M, plus 100k/800k); others snap to the nearest.
void      ot_can_set_bitrate(uint32_t bitrate);

// Install + start the TWAI driver. flags: bit0 listen-only, bit1 loopback (no
// transceiver / self-test). Safe to call again; it restarts the driver.
esp_err_t ot_can_start(uint8_t flags);

// Stop and uninstall the driver.
void      ot_can_stop(void);
bool      ot_can_running(void);

// Transmit one frame (blocks up to ~20 ms for a TX slot).
esp_err_t ot_can_send(const ot_can_frame_t *f);

// Receive one frame, waiting up to timeout_ms. ESP_ERR_TIMEOUT on expiry.
esp_err_t ot_can_recv(ot_can_frame_t *out, uint32_t timeout_ms);
