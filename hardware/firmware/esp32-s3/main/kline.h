// OpenTune ESP32-S3 interface — K-line (ISO 9141 / ISO 14230) over UART1.
#pragma once
#include <stdint.h>
#include "esp_err.h"

#define OT_KLINE_MAX 260

void      ot_kline_init_hw(void);          // one-time UART + GPIO setup
void      ot_kline_set_baud(uint32_t baud); // default 10400

// ISO 14230 fast init: 25 ms low / 25 ms high wake-up pulse. If `startcomm`/
// `sc_len` is given it is sent after the pulse and the key bytes are returned.
esp_err_t ot_kline_init_fast(const uint8_t *startcomm, uint8_t sc_len,
                             uint8_t *keys, uint32_t *keys_len);

// 5-baud slow init: bit-bang `addr` at 5 bit/s, read 0x55 + two key bytes,
// acknowledge the inverted KB2. Returns [sync, kb1, kb2] in `keys`.
esp_err_t ot_kline_init_5baud(uint8_t addr, uint8_t *keys, uint32_t *keys_len);

// Send `req` on K (swallowing the half-duplex echo), then stream the reply back
// until the inter-byte gap expires. Returns reply length in *resp_len.
esp_err_t ot_kline_transceive(const uint8_t *req, uint32_t req_len,
                              uint8_t *resp, uint32_t *resp_len,
                              uint32_t timeout_ms);
