// OpenTune ESP32-S3 interface — USB-CDC wire protocol.
//
// Framing (both directions), a deliberate extension of the kline-bridge
// `0xB5 <cmd> <len> <payload>` idea with a 16-bit length (ISO-TP payloads can be
// up to 4095 bytes) and a trailing CRC-8 for line integrity:
//
//   +------+------+--------+--------+-----------------+-------+
//   | SYNC | CMD  | LEN_HI | LEN_LO | PAYLOAD[LEN]    | CRC8  |
//   | 0xB5 | 1 B  |  1 B   |  1 B   |  LEN bytes      | 1 B   |
//   +------+------+--------+--------+-----------------+-------+
//
//   * SYNC  = 0xB5 (same magic as the Arduino K-line bridge).
//   * CMD   = command id (host->device) or the command id with bit7 set in a
//             reply (device->host), so a reply is matched to its request. The
//             device may also emit CMD_EVENT (0xEE) unsolicited (async CAN frame
//             / log) — the host must tolerate and route these.
//   * LEN   = payload length, big-endian u16 (0..=4096).
//   * CRC8  = CRC-8 (poly 0x07, init 0x00, no reflection) over CMD, LEN_HI,
//             LEN_LO and every PAYLOAD byte (SYNC excluded).
//
// A reply's PAYLOAD starts with a 1-byte STATUS (0x00 = OK). On OK the result
// bytes follow; on error STATUS is non-zero and the payload may carry one detail
// byte. This keeps every command's reply shape uniform and easy to codec-test.

#pragma once
#include <stdint.h>

#define OT_SYNC          0xB5u
#define OT_REPLY_BIT     0x80u    // device OR's this into the echoed cmd id
#define OT_PROTO_VERSION 0x01u
#define OT_MAX_PAYLOAD   4096u

// Firmware version, reported by CMD_PING.
#define OT_FW_MAJOR 0u
#define OT_FW_MINOR 1u
#define OT_FW_PATCH 0u

// ---- Command ids (host -> device) -----------------------------------------
enum ot_cmd {
    CMD_PING            = 0x01, // -> [proto, major, minor, patch, caps, "OpenTune-ESP32S3"]
    CMD_CAN_SET_BITRATE = 0x02, // [u32 bitrate BE]
    CMD_CAN_START       = 0x03, // [flags] bit0 listen-only, bit1 loopback (self-test)
    CMD_CAN_STOP        = 0x04, // []
    CMD_CAN_SEND        = 0x05, // [id(4 BE)][flags(1)][dlc(1)][data(dlc)]  flags: b0 ext, b1 rtr
    CMD_CAN_RECV        = 0x06, // [timeout_ms(2 BE)] -> [id(4)][flags(1)][dlc(1)][data(dlc)] or ERR_TIMEOUT
    CMD_ISOTP_CONFIG    = 0x07, // [tx(4)][rx(4)][flags(1 b0=ext)][bs(1)][stmin(1)][pad(1: 0=none else pad byte)]
    CMD_ISOTP_REQUEST   = 0x08, // [timeout_ms(2 BE)][request bytes...] -> [full response payload]
    CMD_KLINE_SET_BAUD  = 0x09, // [u32 baud BE] (default 10400)
    CMD_KLINE_INIT_FAST = 0x0A, // [opt StartComm bytes...] -> [key bytes...]
    CMD_KLINE_INIT_5BAUD= 0x0B, // [addr(1)] -> [sync, kb1, kb2]
    CMD_KLINE_TRANSCEIVE= 0x0C, // [timeout_ms(2 BE)][bytes...] -> [reply bytes...]
    CMD_BATTERY         = 0x0D, // [] -> [mV(2 BE)]
    CMD_RESET           = 0x0E, // [] soft-reset CAN+K-line state
    CMD_EVENT           = 0x6E, // device->host async (becomes 0xEE with reply bit): [kind][...]
};

// ---- Status / error codes (first byte of every reply payload) -------------
enum ot_status {
    ST_OK               = 0x00,
    ST_ERR_BAD_CMD      = 0x01,
    ST_ERR_BAD_LEN      = 0x02,
    ST_ERR_CRC          = 0x03,
    ST_ERR_CAN_DOWN     = 0x04, // CAN not started
    ST_ERR_CAN_TX       = 0x05,
    ST_ERR_TIMEOUT      = 0x06,
    ST_ERR_ISOTP        = 0x07,
    ST_ERR_KLINE_INIT   = 0x08,
    ST_ERR_KLINE_NODATA = 0x09,
    ST_ERR_BUSY         = 0x0A,
    ST_ERR_UNSUPPORTED  = 0x0B,
    ST_ERR_OVERFLOW     = 0x0C,
};

// ---- Capability bits reported by CMD_PING ---------------------------------
#define OT_CAP_CAN     0x01u
#define OT_CAP_ISOTP   0x02u
#define OT_CAP_KLINE   0x04u
#define OT_CAP_BATTERY 0x08u

// CRC-8 (poly 0x07, init 0x00). Shared, byte-identical, with the host driver.
uint8_t ot_crc8(const uint8_t *data, uint32_t len);
// Continue a running CRC-8 over more bytes (same poly). ot_crc8(d,n) ==
// ot_crc8_update(0x00, d, n).
uint8_t ot_crc8_update(uint8_t crc, const uint8_t *data, uint32_t len);
