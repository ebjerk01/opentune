// OpenTune K-line bridge — ATmega328 (Arduino Uno/Nano @16 MHz)
//
// A "timed K-line adapter": the host (OpenTune) speaks KWP2000/ISO-14230 and
// sends the raw request bytes over USB; this firmware does the timing-critical
// part on-chip — ISO-14230 fast-init or 5-baud init, half-duplex echo removal,
// inter-byte timing — and streams the ECU's reply back. Protocol framing and
// checksums stay on the host (opentune-core's Kwp2000Client).
//
// Wiring (via an L9637D K-line transceiver):
//   L9637D TXD  <- D4 (KTX)      ; we drive K through the transceiver
//   L9637D RXD  -> D3 (KRX)      ; K-line level, echoes our own TX
//   L9637D Vbatt = car 12V (pin 16), GND = pins 4/5, K = OBD pin 7
//   (no transceiver yet? jumper D4->D3 for a USB-only loopback bring-up)
//
// Host link: USB serial @115200. Binary frames: 0xB5 <cmd> <len> <payload...>.
//   Host->device: 0x01 fast-init | 0x02 5baud-init <addr> |
//                 0x03 set-baud <hi> <lo> | 0x10 transceive <bytes...>
//   Device->host: 0x81 init-result <status>[ keys...] |
//                 0x90 rx <bytes...> | 0xEE error <code>
#include <SoftwareSerial.h>

static const uint8_t PIN_KRX = 3;   // from L9637D RXD
static const uint8_t PIN_KTX = 4;   // to   L9637D TXD
static const uint8_t SYNC = 0xB5;

SoftwareSerial kline(PIN_KRX, PIN_KTX);
static uint32_t kbaud = 10400;
static const uint16_t P4_US = 5000;      // inter-byte send gap (P4, max 20ms)
static const uint16_t RX_GAP_MS = 60;    // end-of-response inter-byte timeout

static void sendFrame(uint8_t cmd, const uint8_t *data, uint8_t len) {
  Serial.write(SYNC); Serial.write(cmd); Serial.write(len);
  for (uint8_t i = 0; i < len; i++) Serial.write(data[i]);
}
static void err(uint8_t code) { sendFrame(0xEE, &code, 1); }

// Hold the K-line low for `lo_ms`, high for `hi_ms` (ISO-14230 fast init).
static void klineBreak(uint16_t lo_ms, uint16_t hi_ms) {
  kline.end();
  pinMode(PIN_KTX, OUTPUT);
  digitalWrite(PIN_KTX, LOW);  delay(lo_ms);
  digitalWrite(PIN_KTX, HIGH); delay(hi_ms);
  kline.begin(kbaud);
}

// Send one byte on K and swallow its half-duplex echo.
static bool kWriteEchoed(uint8_t b) {
  kline.write(b);
  uint32_t t0 = millis();
  while (!kline.available()) if (millis() - t0 > 100) return false;
  (void)kline.read();          // discard echo
  return true;
}

// 5-baud slow init: bit-bang the address at 5 bit/s, read 0x55 + key bytes.
static void init5baud(uint8_t addr) {
  kline.end();
  pinMode(PIN_KTX, OUTPUT);
  digitalWrite(PIN_KTX, HIGH); delay(300);
  digitalWrite(PIN_KTX, LOW);  delay(200);              // start bit
  for (uint8_t i = 0; i < 8; i++) {                     // 8 data bits, LSB first
    digitalWrite(PIN_KTX, (addr & (1 << i)) ? HIGH : LOW);
    delay(200);
  }
  digitalWrite(PIN_KTX, HIGH); delay(200);              // stop bit
  kline.begin(kbaud);
  uint8_t out[4]; uint8_t n = 0;
  uint32_t t0 = millis();
  while (n < 3 && millis() - t0 < 500)
    if (kline.available()) out[n++] = kline.read();     // 0x55, KB1, KB2
  if (n >= 3) { kWriteEchoed(~out[2]); out[0] = 0x00; } // ack inverted KB2
  uint8_t resp[4] = {0x00, out[0], out[1], out[2]};     // status + sync/keys
  sendFrame(0x81, resp, n >= 3 ? 4 : 1);
}

static void initFast() {
  klineBreak(25, 25);
  uint8_t ok = 0x00;
  sendFrame(0x81, &ok, 1);
}

// Send request bytes on K, then stream back the reply until the RX gap expires.
static void transceive(const uint8_t *req, uint8_t len) {
  while (kline.available()) kline.read();   // flush
  for (uint8_t i = 0; i < len; i++) {
    if (!kWriteEchoed(req[i])) { err(0x01); return; }
    delayMicroseconds(P4_US);
  }
  uint8_t buf[80]; uint8_t n = 0;
  uint32_t last = millis();
  while (millis() - last < RX_GAP_MS) {
    if (kline.available()) { if (n < sizeof(buf)) buf[n++] = kline.read(); last = millis(); }
  }
  sendFrame(0x90, buf, n);
}

void setup() {
  Serial.begin(115200);
  pinMode(PIN_KTX, OUTPUT);
  digitalWrite(PIN_KTX, HIGH);
  kline.begin(kbaud);
}

void loop() {
  if (Serial.read() != SYNC) return;
  while (Serial.available() < 2) {}
  uint8_t cmd = Serial.read();
  uint8_t len = Serial.read();
  uint8_t pl[80];
  for (uint8_t i = 0; i < len; i++) { while (!Serial.available()) {} pl[i] = Serial.read(); }
  switch (cmd) {
    case 0x01: initFast(); break;
    case 0x02: init5baud(len ? pl[0] : 0x01); break;
    case 0x03: if (len == 2) { kbaud = ((uint32_t)pl[0] << 8) | pl[1]; kline.begin(kbaud); } break;
    case 0x10: transceive(pl, len); break;
    default: err(0xFF);
  }
}
