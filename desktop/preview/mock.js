// OpenTune UI preview — a stand-in for the Tauri backend.
// Loaded BEFORE the app scripts by preview/build.py. Defines window.__TAURI__ with
// core.invoke + dialog.open/save that answer from demo FIXTURES, so the real UI
// files render in a browser with no Rust backend and no adapter. Demo data only:
// nothing here talks to hardware. Commands without a fixture reject with a clear
// "no demo data" message, which the panels show like any backend error.
"use strict";
(function () {
  const P = window.__PREVIEW__ || {};
  const t0 = Date.now();
  const secs = () => (Date.now() - t0) / 1000;
  const jitter = (a) => (Math.random() - 0.5) * 2 * a;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  // A warm idling diesel: coolant climbs toward 90 °C, RPM wanders round 820.
  function engine() {
    const s = secs();
    return {
      rpm: 820 + 25 * Math.sin(s / 1.7) + jitter(12),
      coolant_c: Math.round(Math.min(90, 72 + s / 6)),
      speed_kph: 0,
      battery_mv: Math.round(13900 + jitter(60)),
    };
  }

  const ELM = {
    connected: true,
    driver: "elm327obd",
    name: "ELM327 serial (OBD read diagnostics)",
    firmware: "ELM327 v1.5",
    serial: null,
    battery_mv: 12600,
    capabilities: {
      can: true, can_fd: false, kline: false, read_memory: false, write_memory: false,
      bench_boot: false, battery_voltage: true, programming_voltage: false,
    },
  };

  // Each fixture: (args) => value | Promise. Shapes mirror desktop/src-tauri.
  const FIXTURES = {
    build_channel: () => "alpha",
    list_drivers: () => [
      { id: "mock", name: "Mock / file driver" },
      { id: "elm327obd", name: "ELM327 serial (OBD read diagnostics)" },
      { id: "elm327", name: "ELM327 K-line" },
      { id: "elm327net", name: "ELM327 WiFi / Bluetooth" },
      { id: "kkl", name: "KKL 409.1 (raw K-line)" },
      { id: "slcan", name: "slcan (CANable, USBtin)" },
      { id: "socketcan", name: "SocketCAN" },
      { id: "gsusb", name: "gs_usb / candleLight" },
      { id: "openport2", name: "Tactrix OpenPort 2.0" },
      { id: "j2534", name: "J2534 PassThru" },
    ],
    data_paths: () => ({ ecudefs: "ecudefs/", catalog: "catalog/", guide: "guide/" }),
    list_ecudefs: () => P.ecudefs || [],
    guide_list: () => (P.guides || []).map((g) => ({ id: g.id, title: g.title })),
    guide_read: ({ id }) => {
      const g = (P.guides || []).find((x) => x.id === id);
      if (!g) throw `guide page '${id}' not found`;
      return g.body;
    },
    probe_adapter: async () => { await wait(350); return { ...ELM, battery_mv: engine().battery_mv }; },
    obd_live: async () => { await wait(120); return engine(); },
    update_status: () => ({ current_version: "0.1.0-preview", busy: null }),
    update_check: () => ({ available: false }),
    vault_list: () => [],
    seedkey_algos: () => [],
    debug_log_dump: () => "preview: no session log (demo backend)",
    debug_log_clear: () => null,
  };

  async function invoke(cmd, args) {
    const f = FIXTURES[cmd];
    if (!f) throw `Preview: '${cmd}' has no demo data yet.`;
    return await f(args || {});
  }

  window.__TAURI__ = {
    core: { invoke },
    dialog: {
      open: async () => "demo/golf-1.9tdi-edc16u34-stock.bin",
      save: async () => "demo/out.bin",
    },
  };
  window.__PREVIEW_FIXTURES__ = FIXTURES; // so fixture files can extend it
})();
