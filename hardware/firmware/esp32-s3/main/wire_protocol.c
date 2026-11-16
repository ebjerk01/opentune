// OpenTune ESP32-S3 interface — frame codec and CRC-8.
#include "wire_protocol.h"

uint8_t ot_crc8_update(uint8_t crc, const uint8_t *data, uint32_t len)
{
    for (uint32_t i = 0; i < len; i++) {
        crc ^= data[i];
        for (int b = 0; b < 8; b++) {
            crc = (crc & 0x80) ? (uint8_t)((crc << 1) ^ 0x07) : (uint8_t)(crc << 1);
        }
    }
    return crc;
}

uint8_t ot_crc8(const uint8_t *data, uint32_t len)
{
    return ot_crc8_update(0x00, data, len);
}
