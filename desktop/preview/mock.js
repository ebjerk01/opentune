// OpenTune UI preview — a stand-in for the Tauri backend.
// Loaded BEFORE the app scripts by preview/build.py. Defines window.__TAURI__ with
// core.invoke + dialog.open/save that answer from demo FIXTURES, so the real UI
// files render in a browser with no Rust backend and no adapter. Demo data only:
// nothing here talks to hardware. Commands without a fixture reject with a clear
// "no demo data" message, which the panels show like any backend error.
// The fixtures-*.js files (also bundled by build.py) add the per-panel commands.
"use strict";
(function () {
  const P = window.__PREVIEW__ || {};
  const t0 = Date.now();
  const secs = () => (Date.now() - t0) / 1000;
  const jitter = (a) => (Math.random() - 0.5) * 2 * a;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const VERSION = "0.1.0-preview";

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

  const CAPS_ELM = {
    can: true, can_fd: false, kline: false, read_memory: false, write_memory: false,
    bench_boot: false, battery_voltage: true, programming_voltage: false,
  };
  // What "Detect adapter" finds per driver. Every driver reaches the same demo
  // vehicle (the virtual car's modules, see fixtures-vcar.js); `vcar` is the
  // app's own offline simulator, the others pretend a dongle is plugged in.
  const ADAPTERS = {
    vcar: { name: "Virtual car (offline simulator)", firmware: "offline", caps: { can: true, battery_voltage: true } },
    elm327obd: { name: "ELM327 serial (OBD read diagnostics)", firmware: "ELM327 v1.5", caps: CAPS_ELM },
    elm327net: { name: "ELM327 over WiFi / Bluetooth", firmware: "ELM327 v1.5", caps: CAPS_ELM },
    slcan: { name: "SLCAN (serial CAN)", firmware: "demo", caps: { can: true } },
    socketcan: { name: "SocketCAN", firmware: null, caps: { can: true, can_fd: true } },
    gsusb: { name: "gs_usb / candleLight USB-CAN", firmware: "demo", caps: { can: true } },
  };

  // Session log shown by the Debug tab; fixtures push their exchanges here.
  const LOG = ["preview: demo backend started — nothing here talks to hardware"];
  const log = (line) => { LOG.push(line); if (LOG.length > 400) LOG.shift(); };

  // Each fixture: (args) => value | Promise. Shapes mirror desktop/src-tauri.
  const FIXTURES = {
    build_channel: () => "alpha",
    startup_mark: () => null,
    checksum_algos: () => [
      ["edc16", "Bosch EDC16 (MPC56x)"], ["edc17", "Bosch EDC17 (TriCore)"], ["pcr21", "Continental Simos PCR2.1"],
      ["edc15c6", "Bosch EDC15 (VAG) / EDC15C6 (Mercedes CR1)"], ["me7", "Bosch ME7"], ["dq381", "VAG DQ381 TCU"],
      ["crc32-tail", "Generic CRC32 tail"],
    ].map(([id, name]) => ({ id, name, needs_reference: id === "edc17", notes: "preview" })),
    // Same ids and names as drivers::registry() in desktop/src-tauri.
    list_drivers: () => [
      ["mock", "Mock / file driver (offline)"], ["openport2", "OpenPort 2.0 (J2534/CDC-ACM)"], ["socketcan", "SocketCAN"],
      ["slcan", "SLCAN (serial CAN)"], ["elm327", "K-line (ELM327)"], ["kkl", "K-line (KKL 409.1 raw)"],
      ["elm327obd", "ELM327 serial (OBD diagnostics + CAN memory read)"], ["elm327net", "ELM327 over WiFi / Bluetooth"],
      ["gsusb", "gs_usb / candleLight USB-CAN"], ["j2534", "J2534 PassThru (generic)"], ["vcar", "Virtual car (offline simulator)"],
    ].map(([id, name]) => ({ id, name })),
    data_paths: () => ({ ecudefs: "ecudefs/", catalog: "catalog/", guide: "guide/", packs: "packs/" }),
    list_ecudefs: () => P.ecudefs || [],
    guide_list: () => (P.guides || []).map((g) => ({ id: g.id, title: g.title })),
    guide_read: ({ id }) => {
      const g = (P.guides || []).find((x) => x.id === id);
      if (!g) throw `guide page '${id}' not found`;
      return g.body;
    },
    probe_adapter: async ({ driver }) => {
      await wait(350);
      const a = ADAPTERS[driver];
      if (!a) {
        const e = `preview: no '${driver}' interface here — choose vcar (virtual car) or elm327obd`;
        log(`probe ${driver}: ${e}`);
        return { connected: false, error: e };
      }
      log(`probe ${driver}: FOUND ${a.name} ${a.firmware || ""}`);
      const caps = Object.assign({}, CAPS_ELM, { can: false, battery_voltage: false }, a.caps);
      return { connected: true, driver, name: a.name, firmware: a.firmware, serial: null, battery_mv: engine().battery_mv + (driver === "vcar" ? 200 : 0), capabilities: caps };
    },
    obd_live: async () => { await wait(120); return engine(); },
    update_status: () => ({ current_version: VERSION, busy: null }),
    update_check: async () => { await wait(300); return { available: false, current_version: VERSION }; },
    update_install: async () => { await wait(300); return { installed: false, up_to_date: true }; },
    update_relaunch: () => { throw "Preview: there is no app to restart here."; },
    debug_log_dump: () => LOG.slice(),
    debug_log_clear: () => { LOG.length = 0; return null; },
  };

  async function invoke(cmd, args) {
    const f = FIXTURES[cmd];
    if (!f) throw `Preview: '${cmd}' has no demo data yet.`;
    return await f(args || {});
  }

  // The file dialogs hand out demo paths that the fixtures recognise: the
  // virtual car's definition pack for the vehicle panels (each pick moves on to
  // the next module), a synthetic stock / tuned image pair for the calibration
  // panels and a synthetic CSV for the log viewer.
  const rot = {};
  const cycle = (key, list) => list[(rot[key] = ((rot[key] == null ? -1 : rot[key]) + 1) % list.length)];
  const packFiles = (dir) => Object.keys((P.vcar && P.vcar.files) || {}).filter((p) => p.startsWith(dir)).sort();
  const MODULE_ORDER = ["engine", "gearbox", "abs", "airbag", "body", "cluster"];
  const byModule = (a, b) => MODULE_ORDER.indexOf(a.split("/").pop().replace(".json", "")) - MODULE_ORDER.indexOf(b.split("/").pop().replace(".json", ""));
  function openPath(o) {
    o = o || {};
    if (o.directory) return "demo/opentune-vault";
    const f = (o.filters || [])[0] || {};
    const name = f.name || "";
    const ext = f.extensions || [];
    const title = String(o.title || "");
    const pack = (dir, key) => { const l = packFiles(dir).sort(byModule); return l.length ? cycle(key, l) : null; };
    if (name === "Coding def") return pack("packs/vcar-demo/coding/", "cod");
    if (name === "Adaptation def") return pack("packs/vcar-demo/adaptation/", "adp");
    if (name === "Service script") return pack("packs/vcar-demo/services/", "svc");
    if (name === "Modules") return "packs/vcar-demo/modules/vcar-demo.json";
    if (name === "Logger definition") return "demo/preview-logger.json";
    if (name === "CSV log") return "demo/preview-drive-60s.csv";
    if (name === "CAN log") return "demo/preview-drivetrain.log";
    if (name === "Definition") return "demo/preview-logview-def.json";
    if (/XDF|A2L/.test(name)) return "demo/preview-demo.xdf";
    if (ext.includes("otproj")) return "demo/preview-tune.otproj";
    if (ext.includes("ini")) return "demo/speeduino-demo.ini";
    if (ext.includes("msq")) return "demo/street.msq";
    if (/tuned/i.test(title)) return "demo/preview-tuned.bin";
    if (/data file/i.test(title)) return "demo/preview-patch.bin";
    return "demo/preview-stock.bin";
  }

  window.__TAURI__ = {
    core: { invoke },
    dialog: {
      open: async (o) => openPath(o),
      save: async (o) => {
        const d = String((o && o.defaultPath) || "out.bin").split(/[\\/]/).pop();
        return "demo/" + d;
      },
    },
  };
  window.__PREVIEW_FIXTURES__ = FIXTURES; // so fixture files can extend it
  window.__PREVIEW_LOG__ = log;
})();
