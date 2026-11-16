// OpenTune ESP32-S3 interface — K-line (ISO 9141 / ISO 14230) over UART1.
//
// Same split as the Arduino K-line bridge: the device does the timing-critical
// part (fast / 5-baud init, inter-byte gaps, half-duplex echo removal) and the
// host keeps the KWP2000 framing/checksums (opentune-core's Kwp2000Client).
// A hardware UART drives K through an L9637D; for the init pulse and the 5-baud
// address we detach the UART and bit-bang the TX GPIO directly.
#include "kline.h"
#include "pins.h"
#include <string.h>
#include "driver/uart.h"
#include "driver/gpio.h"
#include "esp_rom_sys.h" // esp_rom_delay_us
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#define KLINE_UART   UART_NUM_1
#define P4_US        5000   // inter-byte send gap (ISO 14230 P4)
#define RX_GAP_MS    60     // end-of-response inter-byte timeout

static uint32_t s_baud = 10400;

void ot_kline_set_baud(uint32_t baud) { s_baud = baud; }

static void uart_begin(void)
{
    uart_config_t c = {
        .baud_rate = (int)s_baud,
        .data_bits = UART_DATA_8_BITS,
        .parity    = UART_PARITY_DISABLE,
        .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    uart_param_config(KLINE_UART, &c);
    uart_set_pin(KLINE_UART, PIN_KLINE_TX, PIN_KLINE_RX,
                 UART_PIN_NO_CHANGE, UART_PIN_NO_CHANGE);
}

void ot_kline_init_hw(void)
{
    uart_driver_install(KLINE_UART, 512, 512, 0, NULL, 0);
    uart_begin();
}

// Detach the UART from the TX pin so we can bit-bang it as a GPIO.
static void tx_gpio_mode(void)
{
    uart_driver_delete(KLINE_UART);
    gpio_reset_pin(PIN_KLINE_TX);
    gpio_set_direction(PIN_KLINE_TX, GPIO_MODE_OUTPUT);
    gpio_set_level(PIN_KLINE_TX, 1); // idle high
}

static void uart_resume(void)
{
    uart_driver_install(KLINE_UART, 512, 512, 0, NULL, 0);
    uart_begin();
    uart_flush_input(KLINE_UART);
}

// Send one byte and discard its half-duplex echo.
static esp_err_t write_echoed(uint8_t b)
{
    uart_flush_input(KLINE_UART);
    uart_write_bytes(KLINE_UART, (const char *)&b, 1);
    uint8_t echo;
    int n = uart_read_bytes(KLINE_UART, &echo, 1, pdMS_TO_TICKS(100));
    return (n == 1) ? ESP_OK : ESP_ERR_TIMEOUT;
}

esp_err_t ot_kline_init_fast(const uint8_t *startcomm, uint8_t sc_len,
                             uint8_t *keys, uint32_t *keys_len)
{
    tx_gpio_mode();
    gpio_set_level(PIN_KLINE_TX, 0); vTaskDelay(pdMS_TO_TICKS(25)); // Tinil low
    gpio_set_level(PIN_KLINE_TX, 1); vTaskDelay(pdMS_TO_TICKS(25)); // Twup high
    uart_resume();

    *keys_len = 0;
    if (startcomm && sc_len) {
        for (uint8_t i = 0; i < sc_len; i++) {
            if (write_echoed(startcomm[i]) != ESP_OK) return ESP_ERR_TIMEOUT;
            esp_rom_delay_us(P4_US);
        }
        // Collect the StartCommunication positive response, if any.
        uint8_t buf[OT_KLINE_MAX];
        int n = uart_read_bytes(KLINE_UART, buf, sizeof(buf), pdMS_TO_TICKS(300));
        if (n > 0) {
            memcpy(keys, buf, n);
            *keys_len = (uint32_t)n;
        }
    }
    return ESP_OK;
}

esp_err_t ot_kline_init_5baud(uint8_t addr, uint8_t *keys, uint32_t *keys_len)
{
    tx_gpio_mode();
    gpio_set_level(PIN_KLINE_TX, 1); vTaskDelay(pdMS_TO_TICKS(300));
    gpio_set_level(PIN_KLINE_TX, 0); vTaskDelay(pdMS_TO_TICKS(200)); // start bit
    for (int i = 0; i < 8; i++) {                                    // 8 data bits, LSB first
        gpio_set_level(PIN_KLINE_TX, (addr >> i) & 1);
        vTaskDelay(pdMS_TO_TICKS(200));
    }
    gpio_set_level(PIN_KLINE_TX, 1); vTaskDelay(pdMS_TO_TICKS(200)); // stop bit
    uart_resume();

    uint8_t out[3]; int n = 0;
    TickType_t t0 = xTaskGetTickCount();
    while (n < 3 && (xTaskGetTickCount() - t0) < pdMS_TO_TICKS(500)) {
        n += uart_read_bytes(KLINE_UART, &out[n], 3 - n, pdMS_TO_TICKS(100));
    }
    if (n < 3) return ESP_ERR_TIMEOUT; // no 0x55 sync / key bytes
    write_echoed((uint8_t)~out[2]); // ack inverted KB2
    keys[0] = out[0]; keys[1] = out[1]; keys[2] = out[2];
    *keys_len = 3;
    return ESP_OK;
}

esp_err_t ot_kline_transceive(const uint8_t *req, uint32_t req_len,
                              uint8_t *resp, uint32_t *resp_len,
                              uint32_t timeout_ms)
{
    uart_flush_input(KLINE_UART);
    for (uint32_t i = 0; i < req_len; i++) {
        if (write_echoed(req[i]) != ESP_OK) return ESP_ERR_TIMEOUT;
        esp_rom_delay_us(P4_US);
    }
    uint32_t n = 0;
    TickType_t last = xTaskGetTickCount();
    TickType_t deadline = last + pdMS_TO_TICKS(timeout_ms);
    while (n < OT_KLINE_MAX) {
        uint8_t b;
        int r = uart_read_bytes(KLINE_UART, &b, 1, pdMS_TO_TICKS(RX_GAP_MS));
        if (r == 1) {
            resp[n++] = b;
            last = xTaskGetTickCount();
        } else {
            // Inter-byte gap expired: end of response (if we got anything).
            if (n > 0) break;
            if (xTaskGetTickCount() > deadline) break;
        }
    }
    *resp_len = n;
    return (n > 0) ? ESP_OK : ESP_ERR_TIMEOUT;
}
