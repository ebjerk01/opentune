// OpenTune ESP32-S3 interface — ISO 15765-2 (ISO-TP) over CAN.
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"

#define OT_ISOTP_MAX 4096

typedef struct {
    uint32_t tx_id;
    uint32_t rx_id;
    bool     extended;     // 29-bit addressing
    uint8_t  block_size;   // BS we advertise when receiving (0 = unlimited)
    uint8_t  st_min;       // STmin we advertise when receiving
    int16_t  pad;          // <0 = no padding, else pad byte (0..255)
} ot_isotp_cfg_t;

void ot_isotp_configure(const ot_isotp_cfg_t *cfg);

// Perform a request/response: segment and send `req`, honoring the ECU's flow
// control, then reassemble its response into `resp` (capacity OT_ISOTP_MAX).
// Returns the response length in *resp_len, or an esp_err_t on failure.
esp_err_t ot_isotp_request(const uint8_t *req, uint32_t req_len,
                           uint8_t *resp, uint32_t *resp_len,
                           uint32_t timeout_ms);
