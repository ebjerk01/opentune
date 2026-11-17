//! The public driver registry.
use opentune_core::Registry;

/// Every interface driver the public build ships, in listing order.
pub fn public_registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r.register("kkl", "K-line (KKL 409.1 raw, FTDI/CH340)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r.register("kline-bridge", "Arduino K-line bridge", || {
        Box::new(opentune_driver_kline_bridge::BridgeDriver::new())
    });
    r.register("slcan", "slcan USB-CAN (CANable/CANtact)", || {
        Box::new(opentune_driver_slcan::SlcanDriver::new())
    });
    r.register("j2534", "J2534 PassThru (generic, Windows)", || {
        Box::new(opentune_driver_j2534::J2534Driver::new())
    });
    r.register("gsusb", "gs_usb / candleLight USB-CAN", || {
        Box::new(opentune_driver_gsusb::GsUsbDriver::new())
    });
    r.register("elm327net", "ELM327 over WiFi / Bluetooth", || {
        Box::new(opentune_driver_elm327net::Elm327NetDriver::new())
    });
    r.register(
        "elm327obd",
        "ELM327 serial (OBD diagnostics + CAN memory read)",
        || Box::new(opentune_driver_elm327_obd::Elm327Obd::new()),
    );
    r.register(
        "vcar",
        "Virtual car (offline multi-module simulator)",
        || Box::new(opentune_driver_vcar::VcarDriver::new()),
    );
    r
}
