// OpenTune ESP32-S3 interface — battery voltage sense via ADC.
#pragma once
#include <stdint.h>

void     ot_battery_init(void);
// Vehicle battery voltage in millivolts (reads the divider, averages a few
// samples). Returns 0 on read failure.
uint32_t ot_battery_millivolts(void);
