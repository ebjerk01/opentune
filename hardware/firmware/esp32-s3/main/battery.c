// OpenTune ESP32-S3 interface — battery voltage sense via ADC.
//
// OBD pin 16 (12 V) -> 47k/10k divider -> PIN_BATT_ADC. The ADC reads the
// divided voltage; we scale back up by (R_top + R_bot)/R_bot. Uses the ESP-IDF
// oneshot ADC driver with curve-fitting calibration when the chip supports it.
#include "battery.h"
#include "pins.h"
#include "esp_adc/adc_oneshot.h"
#include "esp_adc/adc_cali.h"
#include "esp_adc/adc_cali_scheme.h"

// Divider: top 47k, bottom 10k -> multiply measured mV by 57/10 = 5.7.
#define DIV_NUM 57
#define DIV_DEN 10

static adc_oneshot_unit_handle_t s_adc;
static adc_cali_handle_t         s_cali;
static bool                      s_have_cali = false;
static adc_channel_t             s_chan = ADC_CHANNEL_0; // GPIO1 -> ADC1_CH0

void ot_battery_init(void)
{
    adc_oneshot_unit_init_cfg_t unit = { .unit_id = ADC_UNIT_1 };
    adc_oneshot_new_unit(&unit, &s_adc);

    adc_oneshot_chan_cfg_t ch = {
        .atten = ADC_ATTEN_DB_11,     // ~0..3.1 V range
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    adc_oneshot_config_channel(s_adc, s_chan, &ch);

#if ADC_CALI_SCHEME_CURVE_FITTING_SUPPORTED
    adc_cali_curve_fitting_config_t cal = {
        .unit_id = ADC_UNIT_1,
        .chan = s_chan,
        .atten = ADC_ATTEN_DB_11,
        .bitwidth = ADC_BITWIDTH_DEFAULT,
    };
    s_have_cali = (adc_cali_create_scheme_curve_fitting(&cal, &s_cali) == ESP_OK);
#endif
}

uint32_t ot_battery_millivolts(void)
{
    int acc = 0, got = 0;
    for (int i = 0; i < 8; i++) {
        int raw = 0;
        if (adc_oneshot_read(s_adc, s_chan, &raw) != ESP_OK) continue;
        int mv = raw;
        if (s_have_cali) {
            int cal_mv = 0;
            if (adc_cali_raw_to_voltage(s_cali, raw, &cal_mv) == ESP_OK) mv = cal_mv;
        }
        acc += mv;
        got++;
    }
    if (got == 0) return 0;
    uint32_t adc_mv = (uint32_t)(acc / got);
    return adc_mv * DIV_NUM / DIV_DEN; // scale back to battery side
}
