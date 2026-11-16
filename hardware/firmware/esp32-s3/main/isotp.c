// OpenTune ESP32-S3 interface — ISO 15765-2 (ISO-TP) over CAN.
//
// Classic (non-FD) ISO-TP: single frame, first frame + consecutive frames,
// flow control with Block Size / STmin, WAIT/OVERFLOW handling, and 0xF->0x0
// sequence-number wrap. Mirrors the host core's isotp.rs so device-side and
// host-side ISO-TP behave identically; the host normally calls this via
// CMD_ISOTP_REQUEST and runs UDS/KWP on top.
#include "isotp.h"
#include "twai_can.h"
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "esp_rom_sys.h" // esp_rom_delay_us

// PCI types (high nibble of byte 0).
#define PCI_SF 0x00
#define PCI_FF 0x10
#define PCI_CF 0x20
#define PCI_FC 0x30
#define PCI_MASK 0xF0

// Flow status (low nibble of an FC frame).
#define FS_CTS 0x0
#define FS_WAIT 0x1
#define FS_OVFLW 0x2

#define WFT_MAX 16

static ot_isotp_cfg_t s_cfg = {
    .tx_id = 0x7E0, .rx_id = 0x7E8, .extended = false,
    .block_size = 0, .st_min = 0, .pad = 0xAA,
};

void ot_isotp_configure(const ot_isotp_cfg_t *cfg) { s_cfg = *cfg; }

static void fill_frame(ot_can_frame_t *f, const uint8_t *d, uint8_t n)
{
    memset(f, 0, sizeof(*f));
    f->id = s_cfg.tx_id;
    f->extended = s_cfg.extended;
    memcpy(f->data, d, n);
    f->dlc = n;
    if (s_cfg.pad >= 0) {
        while (f->dlc < 8) f->data[f->dlc++] = (uint8_t)s_cfg.pad;
    }
}

static uint64_t stmin_micros(uint8_t stmin)
{
    if (stmin <= 0x7F) return (uint64_t)stmin * 1000;
    if (stmin >= 0xF1 && stmin <= 0xF9) return (uint64_t)(stmin - 0xF0) * 100;
    return 127000;
}

static void sleep_stmin(uint8_t stmin)
{
    uint64_t us = stmin_micros(stmin);
    if (us == 0) return;
    if (us < 1000) {
        esp_rom_delay_us((uint32_t)us);
    } else {
        vTaskDelay(pdMS_TO_TICKS((us + 999) / 1000));
    }
}

// Receive the next CAN frame addressed to us (rx_id), skipping others.
static esp_err_t recv_ours(ot_can_frame_t *f, uint32_t timeout_ms)
{
    for (;;) {
        esp_err_t err = ot_can_recv(f, timeout_ms);
        if (err != ESP_OK) return err;
        if (f->id == s_cfg.rx_id && f->dlc > 0) return ESP_OK;
    }
}

// Wait for a flow-control frame; honor WAIT (loop, bounded) and OVERFLOW (fail).
static esp_err_t await_fc(uint8_t *bs, uint8_t *stmin, uint32_t timeout_ms)
{
    int waits = 0;
    for (;;) {
        ot_can_frame_t f;
        esp_err_t err = recv_ours(&f, timeout_ms);
        if (err != ESP_OK) return err;
        if ((f.data[0] & PCI_MASK) != PCI_FC) return ESP_ERR_INVALID_RESPONSE;
        switch (f.data[0] & 0x0F) {
            case FS_CTS:
                if (f.dlc < 3) return ESP_ERR_INVALID_RESPONSE;
                *bs = f.data[1];
                *stmin = f.data[2];
                return ESP_OK;
            case FS_WAIT:
                if (++waits > WFT_MAX) return ESP_ERR_TIMEOUT;
                break; // keep waiting
            case FS_OVFLW:
            default:
                return ESP_ERR_INVALID_RESPONSE;
        }
    }
}

static esp_err_t send_payload(const uint8_t *p, uint32_t len, uint32_t timeout_ms)
{
    ot_can_frame_t f;
    if (len == 0 || len > 0x0FFF) return ESP_ERR_INVALID_ARG;

    if (len <= 7) {
        uint8_t d[8];
        d[0] = (uint8_t)len; // SF: high nibble 0, low nibble = length
        memcpy(&d[1], p, len);
        fill_frame(&f, d, (uint8_t)(1 + len));
        return ot_can_send(&f);
    }

    // First frame.
    uint8_t d[8];
    d[0] = PCI_FF | (uint8_t)((len >> 8) & 0x0F);
    d[1] = (uint8_t)(len & 0xFF);
    memcpy(&d[2], p, 6);
    fill_frame(&f, d, 8);
    esp_err_t err = ot_can_send(&f);
    if (err != ESP_OK) return err;

    uint8_t bs = 0, stmin = 0;
    err = await_fc(&bs, &stmin, timeout_ms);
    if (err != ESP_OK) return err;

    uint8_t sn = 1;
    uint32_t idx = 6;
    uint32_t cfs = 0;
    while (idx < len) {
        uint32_t end = idx + 7; if (end > len) end = len;
        uint8_t cf[8];
        cf[0] = PCI_CF | (sn & 0x0F);
        memcpy(&cf[1], &p[idx], end - idx);
        fill_frame(&f, cf, (uint8_t)(1 + (end - idx)));
        err = ot_can_send(&f);
        if (err != ESP_OK) return err;
        sn = (sn + 1) & 0x0F;
        idx = end;
        cfs++;
        if (idx >= len) break;
        if (bs != 0 && cfs >= bs) {
            err = await_fc(&bs, &stmin, timeout_ms);
            if (err != ESP_OK) return err;
            cfs = 0;
        } else {
            sleep_stmin(stmin);
        }
    }
    return ESP_OK;
}

static esp_err_t send_fc(void)
{
    uint8_t d[3] = { PCI_FC | FS_CTS, s_cfg.block_size, s_cfg.st_min };
    ot_can_frame_t f;
    fill_frame(&f, d, 3);
    return ot_can_send(&f);
}

static esp_err_t recv_payload(uint8_t *out, uint32_t *out_len, uint32_t timeout_ms)
{
    ot_can_frame_t f;
    esp_err_t err = recv_ours(&f, timeout_ms);
    if (err != ESP_OK) return err;

    if ((f.data[0] & PCI_MASK) == PCI_SF) {
        uint8_t len = f.data[0] & 0x0F;
        if (len == 0 || 1 + len > f.dlc) return ESP_ERR_INVALID_RESPONSE;
        memcpy(out, &f.data[1], len);
        *out_len = len;
        return ESP_OK;
    }
    if ((f.data[0] & PCI_MASK) != PCI_FF) return ESP_ERR_INVALID_RESPONSE;
    if (f.dlc < 8) return ESP_ERR_INVALID_RESPONSE;

    uint32_t len = (((uint32_t)(f.data[0] & 0x0F)) << 8) | f.data[1];
    if (len <= 7 || len > OT_ISOTP_MAX) return ESP_ERR_INVALID_RESPONSE;
    memcpy(out, &f.data[2], 6);
    uint32_t got = 6;

    err = send_fc();
    if (err != ESP_OK) return err;

    uint8_t expected = 1;
    uint32_t cfs = 0;
    while (got < len) {
        err = recv_ours(&f, timeout_ms);
        if (err != ESP_OK) return err;
        if ((f.data[0] & PCI_MASK) != PCI_CF) return ESP_ERR_INVALID_RESPONSE;
        if ((f.data[0] & 0x0F) != expected) return ESP_ERR_INVALID_RESPONSE;
        expected = (expected + 1) & 0x0F;
        uint32_t remaining = len - got;
        uint32_t take = (f.dlc >= 1) ? (uint32_t)(f.dlc - 1) : 0;
        if (take > remaining) take = remaining;
        memcpy(&out[got], &f.data[1], take);
        got += take;
        cfs++;
        if (got < len && s_cfg.block_size != 0 && cfs >= s_cfg.block_size) {
            err = send_fc();
            if (err != ESP_OK) return err;
            cfs = 0;
        }
    }
    *out_len = len;
    return ESP_OK;
}

esp_err_t ot_isotp_request(const uint8_t *req, uint32_t req_len,
                           uint8_t *resp, uint32_t *resp_len,
                           uint32_t timeout_ms)
{
    if (!ot_can_running()) return ESP_ERR_INVALID_STATE;
    esp_err_t err = send_payload(req, req_len, timeout_ms);
    if (err != ESP_OK) return err;

    // Handle UDS response-pending (0x7F .. 0x78): keep receiving.
    for (int i = 0; i < 32; i++) {
        err = recv_payload(resp, resp_len, timeout_ms);
        if (err != ESP_OK) return err;
        if (*resp_len >= 3 && resp[0] == 0x7F && resp[2] == 0x78) continue;
        return ESP_OK;
    }
    return ESP_ERR_TIMEOUT;
}
