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
    // DoIP has no battery reading and no raw CAN (see fixtures-doip.js).
    doip: { name: "DoIP (ISO 13400, UDS over Ethernet)", firmware: "192.0.2.10:13400 → 0x0010", caps: { doip: true, read_memory: true }, noBattery: true },
  };

  // Session log shown by the Debug tab; fixtures push their exchanges here.
  const LOG = ["preview: demo backend started — nothing here talks to hardware"];
  const log = (line) => { LOG.push(line); if (LOG.length > 400) LOG.shift(); };
  const DIAG_LEVELS = ["debug", "info", "warn", "error"];
  // Diagnostics upload status of the public app: nothing configured.
  const DIAG_UP = {
    configured: false, mode: "none", source: null, host: null, label: null, policy: null, env_off: false, insecure: false,
    allow_send: false, auto_off: false, auto_active: false, notice_needed: false, crash_offer: null, history: [],
    queue: { items: 0, bytes: 0, cap: 2097152 }, next_slot: null, limits: { min_gap_s: 600, per_day: 20, max_payload: 2097152 }, notes: [],
  };
  const DIAG = [
    { ts: new Date(t0).toISOString(), level: "info", src: "startup", msg: "session start", ctx: { session: true, version: VERSION, channel: "alpha", os: "linux", webkit: "2.46.0" } },
    { ts: new Date(t0).toISOString(), level: "info", src: "startup", msg: "setup plugin: start", ctx: { ms: 12.5 } },
    { ts: new Date(t0).toISOString(), level: "warn", src: "cmd", msg: "image_info failed: demo/missing.bin: no such file", ctx: { cmd: "image_info" } },
  ];

  // Each fixture: (args) => value | Promise. Shapes mirror desktop/src-tauri.
  const FIXTURES = {
    build_channel: () => "alpha",
    startup_mark: () => null,
    // Render check (startup.rs render_env): a healthy Wayland session.
    render_env: () => ({
      os: "linux",
      vars: {
        GDK_BACKEND: null, OPENTUNE_KEEP_WEBKIT_ENV: null, WEBKIT_DISABLE_COMPOSITING_MODE: null,
        WEBKIT_DISABLE_DMABUF_RENDERER: null, XDG_SESSION_TYPE: "wayland",
      },
      present: { APPIMAGE: true, DISPLAY: true, WAYLAND_DISPLAY: true },
      dri_render_nodes: ["renderD128"],
      set_by_opentune: [],
      perf: false,
      dmabuf: {
        enabled: true, rule: "none", reason: "none, DMA-BUF on", set_env: false, drivers: ["amdgpu"],
        knob: "auto", heal: "on", notice: null,
      },
    }),
    render_note: ({ line }) => { log("render: " + line); return null; },
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
      ["gsusb", "gs_usb / candleLight USB-CAN"], ["j2534", "J2534 PassThru (generic)"], ["efp02c", "EFI EFP02C USB-CAN (unverified)"], ["vcar", "Virtual car (offline simulator)"], ["udssim", "UDS simulator (offline)"],
      // alpha builds only (Bench tier)
      ["isotp-ble", "ESP32 ISO-TP BLE bridge (Switchleg1 fw, unverified)"],
      ["doip", "DoIP (ISO 13400, UDS over Ethernet)"],
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
      const battery_mv = a.noBattery ? null : engine().battery_mv + (driver === "vcar" ? 200 : 0);
      return { connected: true, driver, name: a.name, firmware: a.firmware, serial: null, battery_mv, capabilities: caps };
    },
    obd_live: async () => { await wait(120); return engine(); },
    update_status: () => ({ current_version: VERSION, busy: null }),
    update_check: async () => { await wait(300); return { available: false, current_version: VERSION }; },
    update_install: async () => { await wait(300); return { installed: false, up_to_date: true }; },
    update_relaunch: () => { throw "Preview: there is no app to restart here."; },
    debug_log_dump: () => LOG.slice(),
    debug_log_clear: () => { LOG.length = 0; return null; },
    // Diagnostics log (diaglog.rs): a short demo file, kept in memory.
    diag_log: (r) => { DIAG.push({ ts: new Date().toISOString(), level: r.level || "error", src: r.src === "cmd" ? "cmd" : "js", msg: String(r.msg || ""), ctx: r.ctx || undefined }); return null; },
    diag_log_tail: ({ limit, level }) => {
      const min = DIAG_LEVELS.indexOf(level || "debug");
      return DIAG.filter((r) => DIAG_LEVELS.indexOf(r.level) >= min).slice(-(limit || 100));
    },
    diag_log_info: () => ({ state: "on", dir: "demo/logs", file: "demo/logs/opentune.log", files: 1, bytes: JSON.stringify(DIAG).length, max_file_bytes: 1048576, rotated_files: 5 }),
    diag_log_open_folder: () => { throw "Preview: there is no folder to open here."; },
    diag_log_clear: () => { DIAG.length = 1; return null; },
    // Diagnostics upload (diagupload/): the public app has no upload target.
    diag_upload_status: () => DIAG_UP,
    diag_upload_set: () => DIAG_UP,
    diag_upload_preview: () => { throw "no diagnostics upload is configured in this build"; },
    diag_upload_send: () => { throw "no diagnostics upload is configured in this build"; },
    diag_upload_payload: () => { throw "that payload is no longer kept"; },
    diag_upload_panel: () => null,
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
    if (name === "Definition pack") return "demo/community-pack/pack.json";
    if (name === "Controller definition") return "ctrldefs/vag-dq250-02e.json";
    if (name === "Logger definition") return "demo/preview-logger.json";
    if (name === "CSV log") return "demo/preview-drive-60s.csv";
    if (name === "CAN log") return "demo/preview-drivetrain.log";
    if (name === "Definition") return "demo/preview-logview-def.json";
    if (name === "SA2 script") return "demo/synthetic.sa2";
    if (name === "Flash container") return "demo/synthetic-sa2.odx";
    if (name === "FRF key") return "demo/frf.key";
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
