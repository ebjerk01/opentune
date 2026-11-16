// OpenTune ESP32-S3 interface — CAN via the TWAI peripheral.
//
// Thin wrapper over ESP-IDF's `driver/twai.h`. The host drives raw frames and
// ISO-TP through this; nothing here interprets payloads.
#include "twai_can.h"
#include "pins.h"
#include "driver/twai.h"
#include "driver/gpio.h"

static uint32_t s_bitrate = 500000;
static bool     s_running = false;

void ot_can_set_bitrate(uint32_t bitrate)
{
    s_bitrate = bitrate;
}

// Map a requested bitrate onto a TWAI timing preset (ESP-IDF ships exact macros
// for the standard automotive rates).
static twai_timing_config_t timing_for(uint32_t bitrate)
{
    switch (bitrate) {
        case 25000:   return (twai_timing_config_t)TWAI_TIMING_CONFIG_25KBITS();
        case 50000:   return (twai_timing_config_t)TWAI_TIMING_CONFIG_50KBITS();
        case 100000:  return (twai_timing_config_t)TWAI_TIMING_CONFIG_100KBITS();
        case 125000:  return (twai_timing_config_t)TWAI_TIMING_CONFIG_125KBITS();
        case 250000:  return (twai_timing_config_t)TWAI_TIMING_CONFIG_250KBITS();
        case 800000:  return (twai_timing_config_t)TWAI_TIMING_CONFIG_800KBITS();
        case 1000000: return (twai_timing_config_t)TWAI_TIMING_CONFIG_1MBITS();
        case 500000:
        default:      return (twai_timing_config_t)TWAI_TIMING_CONFIG_500KBITS();
    }
}

esp_err_t ot_can_start(uint8_t flags)
{
    ot_can_stop();

#if PIN_CAN_STBY >= 0
    gpio_set_direction(PIN_CAN_STBY, GPIO_MODE_OUTPUT);
    gpio_set_level(PIN_CAN_STBY, 0); // transceiver normal mode
#endif

    twai_general_config_t g = TWAI_GENERAL_CONFIG_DEFAULT(PIN_CAN_TX, PIN_CAN_RX, TWAI_MODE_NORMAL);
    if (flags & 0x01) g.mode = TWAI_MODE_LISTEN_ONLY;
    if (flags & 0x02) g.mode = TWAI_MODE_NO_ACK; // loopback / self-test (no bus ack needed)
    g.tx_queue_len = 16;
    g.rx_queue_len = 32;

    twai_timing_config_t t = timing_for(s_bitrate);
    twai_filter_config_t f = TWAI_FILTER_CONFIG_ACCEPT_ALL();

    esp_err_t err = twai_driver_install(&g, &t, &f);
    if (err != ESP_OK) return err;
    err = twai_start();
    if (err != ESP_OK) {
        twai_driver_uninstall();
        return err;
    }
    s_running = true;
    return ESP_OK;
}

void ot_can_stop(void)
{
    if (!s_running) return;
    twai_stop();
    twai_driver_uninstall();
    s_running = false;
}

bool ot_can_running(void) { return s_running; }

esp_err_t ot_can_send(const ot_can_frame_t *f)
{
    if (!s_running) return ESP_ERR_INVALID_STATE;
    twai_message_t m = {0};
    m.identifier = f->id;
    m.extd = f->extended ? 1 : 0;
    m.rtr  = f->rtr ? 1 : 0;
    m.data_length_code = f->dlc > 8 ? 8 : f->dlc;
    for (int i = 0; i < m.data_length_code; i++) m.data[i] = f->data[i];
    return twai_transmit(&m, pdMS_TO_TICKS(20));
}

esp_err_t ot_can_recv(ot_can_frame_t *out, uint32_t timeout_ms)
{
    if (!s_running) return ESP_ERR_INVALID_STATE;
    twai_message_t m;
    esp_err_t err = twai_receive(&m, pdMS_TO_TICKS(timeout_ms));
    if (err != ESP_OK) return err;
    out->id = m.identifier;
    out->extended = m.extd;
    out->rtr = m.rtr;
    out->dlc = m.data_length_code;
    for (int i = 0; i < m.data_length_code; i++) out->data[i] = m.data[i];
    return ESP_OK;
}
