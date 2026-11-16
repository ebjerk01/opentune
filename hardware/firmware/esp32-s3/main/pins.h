// OpenTune ESP32-S3 interface — board pinout.
//
// Reference assignment for a bare ESP32-S3-WROOM devkit plus an SN65HVD230 (or
// TJA1051) CAN transceiver and an L9637D K-line transceiver. Change to match
// your PCB; every pin here is a normal GPIO except the USB D+/D- pair, which is
// fixed by the chip.
#pragma once

// -- CAN (TWAI) -> SN65HVD230 / TJA1051 --
#define PIN_CAN_TX   4   // ESP32-S3 TWAI TX -> transceiver TXD (D)
#define PIN_CAN_RX   5   // ESP32-S3 TWAI RX <- transceiver RXD (R)
// Optional: STB/EN control on the transceiver (SN65HVD230 Rs, TJA1051 S).
#define PIN_CAN_STBY 6   // drive LOW for normal mode, HIGH for standby (-1 to disable)

// -- K-line -> L9637D --
#define PIN_KLINE_TX 17  // UART1 TX -> L9637D TX (drives K through the transceiver)
#define PIN_KLINE_RX 18  // UART1 RX <- L9637D RX (K-line level, echoes our own TX)

// -- Battery sense: OBD pin 16 (12 V) -> divider -> ADC --
// Divider 47k (top) / 10k (bottom): Vadc = Vbat * 10/57. At 14.4 V -> ~2.53 V,
// inside the 11 dB / ~3.1 V ADC range. Calibrated in battery.c.
#define PIN_BATT_ADC 1   // ADC1 channel 0 (GPIO1 on S3)

// -- Status LED (most S3 devkits have a WS2812 on 48, or a plain LED on 2) --
#define PIN_LED      2   // -1 to disable

// -- USB (native) is fixed on the ESP32-S3: D- = GPIO19, D+ = GPIO20 --
