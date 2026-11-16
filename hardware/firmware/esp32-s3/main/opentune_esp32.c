// OpenTune ESP32-S3 interface — main command dispatcher.
//
// Reads framed commands from USB-CDC (see wire_protocol.h), runs CAN / ISO-TP /
// K-line / battery on the device, and sends framed replies. One request, one
// reply (synchronous), matching how the host serial drivers poll.
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "esp_log.h"

#include "wire_protocol.h"
#include "usb_cdc.h"
#include "twai_can.h"
#include "isotp.h"
#include "kline.h"
#include "battery.h"

static const char *TAG = "opentune";
static const char ID_STRING[] = "OpenTune-ESP32S3";

static uint8_t  s_payload[OT_MAX_PAYLOAD];
static uint8_t  s_reply[OT_MAX_PAYLOAD];

static uint16_t be16(const uint8_t *p) { return (uint16_t)((p[0] << 8) | p[1]); }
static uint32_t be32(const uint8_t *p)
{
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
           ((uint32_t)p[2] << 8) | (uint32_t)p[3];
}

// Send a framed reply: CMD = req_cmd | reply-bit, payload = status + body.
static void send_reply(uint8_t req_cmd, uint8_t status, const uint8_t *body, uint32_t body_len)
{
    uint8_t hdr[4];
    uint32_t plen = 1 + body_len;
    hdr[0] = OT_SYNC;
    hdr[1] = req_cmd | OT_REPLY_BIT;
    hdr[2] = (uint8_t)(plen >> 8);
    hdr[3] = (uint8_t)(plen & 0xFF);

    // CRC-8 over CMD, LEN_HI, LEN_LO, then the payload (status + body).
    uint8_t crc = ot_crc8(&hdr[1], 3);
    crc = ot_crc8_update(crc, &status, 1);
    crc = ot_crc8_update(crc, body, body_len);

    ot_usb_write(hdr, 4);
    ot_usb_write(&status, 1);
    if (body_len) ot_usb_write(body, body_len);
    ot_usb_write(&crc, 1);
}

static void reply_status(uint8_t cmd, uint8_t status) { send_reply(cmd, status, NULL, 0); }

// ---- per-command handlers -------------------------------------------------

static void handle_ping(uint8_t cmd)
{
    uint8_t b[6 + sizeof(ID_STRING)];
    uint32_t n = 0;
    b[n++] = OT_PROTO_VERSION;
    b[n++] = OT_FW_MAJOR;
    b[n++] = OT_FW_MINOR;
    b[n++] = OT_FW_PATCH;
    b[n++] = OT_CAP_CAN | OT_CAP_ISOTP | OT_CAP_KLINE | OT_CAP_BATTERY;
    memcpy(&b[n], ID_STRING, sizeof(ID_STRING) - 1);
    n += sizeof(ID_STRING) - 1;
    send_reply(cmd, ST_OK, b, n);
}

static void handle_can_send(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    if (len < 6) { reply_status(cmd, ST_ERR_BAD_LEN); return; }
    ot_can_frame_t f = {0};
    f.id = be32(p);
    f.extended = (p[4] & 0x01) != 0;
    f.rtr = (p[4] & 0x02) != 0;
    f.dlc = p[5] > 8 ? 8 : p[5];
    if (len < 6u + f.dlc) { reply_status(cmd, ST_ERR_BAD_LEN); return; }
    memcpy(f.data, &p[6], f.dlc);
    esp_err_t err = ot_can_send(&f);
    reply_status(cmd, err == ESP_OK ? ST_OK :
                 (err == ESP_ERR_INVALID_STATE ? ST_ERR_CAN_DOWN : ST_ERR_CAN_TX));
}

static void handle_can_recv(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    uint32_t timeout = (len >= 2) ? be16(p) : 1000;
    ot_can_frame_t f;
    esp_err_t err = ot_can_recv(&f, timeout);
    if (err == ESP_ERR_TIMEOUT) { reply_status(cmd, ST_ERR_TIMEOUT); return; }
    if (err != ESP_OK) { reply_status(cmd, ST_ERR_CAN_DOWN); return; }
    uint8_t b[6 + 8];
    b[0] = (uint8_t)(f.id >> 24); b[1] = (uint8_t)(f.id >> 16);
    b[2] = (uint8_t)(f.id >> 8);  b[3] = (uint8_t)f.id;
    b[4] = (f.extended ? 0x01 : 0) | (f.rtr ? 0x02 : 0);
    b[5] = f.dlc;
    memcpy(&b[6], f.data, f.dlc);
    send_reply(cmd, ST_OK, b, 6u + f.dlc);
}

static void handle_isotp_config(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    if (len < 12) { reply_status(cmd, ST_ERR_BAD_LEN); return; }
    ot_isotp_cfg_t c;
    c.tx_id = be32(p);
    c.rx_id = be32(p + 4);
    c.extended = (p[8] & 0x01) != 0;
    c.block_size = p[9];
    c.st_min = p[10];
    c.pad = (p[11] == 0) ? -1 : (int16_t)p[11]; // 0 -> no padding, else pad byte
    ot_isotp_configure(&c);
    reply_status(cmd, ST_OK);
}

static void handle_isotp_request(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    if (len < 2) { reply_status(cmd, ST_ERR_BAD_LEN); return; }
    uint32_t timeout = be16(p);
    uint32_t rlen = 0;
    esp_err_t err = ot_isotp_request(p + 2, len - 2, s_reply, &rlen, timeout);
    if (err == ESP_ERR_INVALID_STATE) { reply_status(cmd, ST_ERR_CAN_DOWN); return; }
    if (err == ESP_ERR_TIMEOUT) { reply_status(cmd, ST_ERR_TIMEOUT); return; }
    if (err != ESP_OK) { reply_status(cmd, ST_ERR_ISOTP); return; }
    send_reply(cmd, ST_OK, s_reply, rlen);
}

static void handle_kline_transceive(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    if (len < 2) { reply_status(cmd, ST_ERR_BAD_LEN); return; }
    uint32_t timeout = be16(p);
    uint8_t resp[OT_KLINE_MAX]; uint32_t rlen = 0;
    esp_err_t err = ot_kline_transceive(p + 2, len - 2, resp, &rlen, timeout);
    if (err != ESP_OK) { reply_status(cmd, ST_ERR_KLINE_NODATA); return; }
    send_reply(cmd, ST_OK, resp, rlen);
}

static void handle_kline_init(uint8_t cmd, const uint8_t *p, uint32_t len, bool fast)
{
    uint8_t keys[OT_KLINE_MAX]; uint32_t klen = 0;
    esp_err_t err = fast ? ot_kline_init_fast(len ? p : NULL, (uint8_t)len, keys, &klen)
                         : ot_kline_init_5baud(len ? p[0] : 0x01, keys, &klen);
    if (err != ESP_OK) { reply_status(cmd, ST_ERR_KLINE_INIT); return; }
    send_reply(cmd, ST_OK, keys, klen);
}

static void handle_battery(uint8_t cmd)
{
    uint32_t mv = ot_battery_millivolts();
    uint8_t b[2] = { (uint8_t)(mv >> 8), (uint8_t)mv };
    send_reply(cmd, ST_OK, b, 2);
}

static void dispatch(uint8_t cmd, const uint8_t *p, uint32_t len)
{
    switch (cmd) {
        case CMD_PING:            handle_ping(cmd); break;
        case CMD_CAN_SET_BITRATE: if (len >= 4) ot_can_set_bitrate(be32(p)); reply_status(cmd, len >= 4 ? ST_OK : ST_ERR_BAD_LEN); break;
        case CMD_CAN_START:       reply_status(cmd, ot_can_start(len ? p[0] : 0) == ESP_OK ? ST_OK : ST_ERR_CAN_DOWN); break;
        case CMD_CAN_STOP:        ot_can_stop(); reply_status(cmd, ST_OK); break;
        case CMD_CAN_SEND:        handle_can_send(cmd, p, len); break;
        case CMD_CAN_RECV:        handle_can_recv(cmd, p, len); break;
        case CMD_ISOTP_CONFIG:    handle_isotp_config(cmd, p, len); break;
        case CMD_ISOTP_REQUEST:   handle_isotp_request(cmd, p, len); break;
        case CMD_KLINE_SET_BAUD:  if (len >= 4) ot_kline_set_baud(be32(p)); reply_status(cmd, len >= 4 ? ST_OK : ST_ERR_BAD_LEN); break;
        case CMD_KLINE_INIT_FAST: handle_kline_init(cmd, p, len, true); break;
        case CMD_KLINE_INIT_5BAUD:handle_kline_init(cmd, p, len, false); break;
        case CMD_KLINE_TRANSCEIVE:handle_kline_transceive(cmd, p, len); break;
        case CMD_BATTERY:         handle_battery(cmd); break;
        case CMD_RESET:           ot_can_stop(); reply_status(cmd, ST_OK); break;
        default:                  reply_status(cmd, ST_ERR_BAD_CMD); break;
    }
}

// Read one framed command (SYNC, CMD, LEN_HI, LEN_LO, PAYLOAD, CRC8). Returns
// false on a framing/CRC fault so the caller just resyncs on the next SYNC.
static bool read_frame(uint8_t *cmd, uint32_t *len)
{
    uint8_t b;
    // Resync to SYNC.
    do {
        if (!ot_usb_read_byte(&b, 1000)) return false;
    } while (b != OT_SYNC);

    uint8_t hdr[3];
    for (int i = 0; i < 3; i++) if (!ot_usb_read_byte(&hdr[i], 500)) return false;
    *cmd = hdr[0];
    uint32_t plen = ((uint32_t)hdr[1] << 8) | hdr[2];
    if (plen > OT_MAX_PAYLOAD) return false;
    for (uint32_t i = 0; i < plen; i++) if (!ot_usb_read_byte(&s_payload[i], 500)) return false;
    uint8_t crc_rx;
    if (!ot_usb_read_byte(&crc_rx, 500)) return false;

    uint8_t crc = ot_crc8(hdr, 3);
    crc = ot_crc8_update(crc, s_payload, plen);
    if (crc != crc_rx) {
        reply_status(*cmd, ST_ERR_CRC);
        return false;
    }
    *len = plen;
    return true;
}

void app_main(void)
{
    ESP_LOGI(TAG, "OpenTune ESP32-S3 interface starting");
    ot_usb_init();
    ot_kline_init_hw();
    ot_battery_init();
    // CAN is started on demand by CMD_CAN_START once the host knows the bitrate.

    for (;;) {
        uint8_t cmd; uint32_t len;
        if (read_frame(&cmd, &len)) {
            dispatch(cmd, s_payload, len);
        }
    }
}
