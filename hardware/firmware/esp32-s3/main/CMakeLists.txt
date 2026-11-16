idf_component_register(
    SRCS
        "opentune_esp32.c"
        "wire_protocol.c"
        "usb_cdc.c"
        "twai_can.c"
        "isotp.c"
        "kline.c"
        "battery.c"
    INCLUDE_DIRS "."
    REQUIRES
        driver              # TWAI (CAN), UART (K-line), GPIO
        esp_adc             # battery voltage sense
        usb                 # usb_serial_jtag CDC backend
)
