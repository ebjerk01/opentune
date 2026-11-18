// Demo fixtures for the Dashboard, CAN capture and ECU database panels.
// Preview only: a simulated diesel drive, a synthetic CAN bus and a few demo
// database entries whose pinouts are placeholders (never wire from these).
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const t0 = Date.now();
  const secs = () => (Date.now() - t0) / 1000;
  const j = (a) => (Math.random() - 0.5) * 2 * a;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  // ---- Dashboard -----------------------------------------------------------
  const T = (pid, key, name, unit, min, max, group, warn, crit) => ({ pid, key, name, unit, min, max, group, warn: warn ?? null, crit: crit ?? null });
  F.obd_pid_table = () => [
    T(0x0c, "rpm", "Engine speed", "rpm", 0, 8000, "engine"),
    T(0x04, "engine_load", "Engine load", "%", 0, 100, "engine"),
    T(0x05, "coolant_temp", "Coolant", "°C", -40, 215, "engine", 105, 115),
    T(0x5c, "oil_temp", "Oil temp", "°C", -40, 210, "engine", 120, 135),
    T(0x0e, "timing_advance", "Timing advance", "°", -64, 64, "engine"),
    T(0x0f, "intake_air_temp", "Intake air", "°C", -40, 215, "air", 60, 75),
    T(0x0b, "intake_map", "Manifold pressure", "kPa", 0, 255, "air"),
    T(0x33, "baro_pressure", "Barometric", "kPa", 0, 255, "air"),
    T(0x10, "maf", "Air mass", "g/s", 0, 655, "air"),
    T(0x11, "throttle_pos", "Throttle", "%", 0, 100, "air"),
    T(0x06, "stft_b1", "Short fuel trim", "%", -100, 99, "fuel"),
    T(0x07, "ltft_b1", "Long fuel trim", "%", -100, 99, "fuel"),
    T(0x23, "fuel_rail_pressure", "Rail pressure", "kPa", 0, 655350, "fuel"),
    T(0x2f, "fuel_level", "Fuel level", "%", 0, 100, "fuel"),
    T(0x2c, "commanded_egr", "EGR commanded", "%", 0, 100, "exhaust"),
    T(0x78, "egt_b1", "EGT bank 1", "°C", -40, 6513, "exhaust", 750, 850),
    T(0x7c, "dpf_temp", "DPF temp", "°C", -40, 6513, "exhaust", 650, 750),
    T(0x49, "accel_pedal_d", "Pedal", "%", 0, 100, "vehicle"),
    T(0x0d, "speed", "Speed", "km/h", 0, 255, "vehicle"),
    T(0x46, "ambient_temp", "Ambient", "°C", -40, 215, "vehicle"),
    T(0x42, "module_voltage", "Module voltage", "V", 0, 65, "electrical"),
  ];
  const DIESEL = [0x04, 0x05, 0x0b, 0x0c, 0x0d, 0x0f, 0x10, 0x11, 0x23, 0x2c, 0x2f, 0x33, 0x42, 0x46, 0x49, 0x5c, 0x78, 0x7c];
  // Placeholder VIN (never a real one): shown as the dashboard's text tile.
  F.obd_supported = async () => { await wait(400); return { pids: DIESEL, vin: "WVWZZZ00000000000" }; };

  // Dashboard Record: pretend to write; the size grows with what was sent.
  let recBytes = 0;
  F.dash_record_write = async ({ path, csv, append }) => {
    await wait(20);
    recBytes = (append ? recBytes : 0) + String(csv || "").length;
    return { path, bytes: recBytes };
  };

  const s = { egt1: 210, egt2: 190, dpf: 240, spd: 0, cool: 72 };
  F.obd_read = async () => {
    await wait(60);
    const t = secs();
    const p = Math.max(0, Math.sin(t / 3.2)) ** 2 * 100 * (0.6 + 0.4 * Math.abs(Math.sin(t / 17)));
    s.egt1 += (230 + p * 6.2 - s.egt1) * 0.08;
    s.egt2 += (200 + p * 5.4 - s.egt2) * 0.06;
    s.dpf += (250 + p * 3.5 - s.dpf) * 0.03;
    s.spd += (p * 1.2 - s.spd) * 0.04;
    s.cool = Math.min(89, s.cool + 0.05);
    const baro = 99;
    const map = baro + p * 1.55 + j(1.5);
    return {
      t_ms: Date.now(), battery_mv: Math.round(14050 + j(60)),
      values: {
        rpm: 790 + p * 29 + j(10), engine_load: 14 + p * 0.82 + j(1), coolant_temp: Math.round(s.cool), oil_temp: Math.round(s.cool + 4),
        intake_air_temp: Math.round(26 + p * 0.16), intake_map: map, baro_pressure: baro, boost_kpa: map - baro,
        maf: 9 + p * 1.45 + j(0.6), throttle_pos: 80 + j(0.5), fuel_rail_pressure: 29000 + p * 1520 + j(400),
        fuel_level: 62, commanded_egr: Math.max(0, 42 - p * 0.5), egt_b1: [s.egt1 + j(3), s.egt2 + j(3), null, null], dpf_temp: s.dpf,
        accel_pedal_d: p, speed: Math.round(s.spd), ambient_temp: 9, module_voltage: 14.05 + j(0.05),
      },
      errors: {},
    };
  };

  // ---- CAN capture -----------------------------------------------------------
  const BUS = [[0x080, 100], [0x0c2, 100], [0x1a0, 100], [0x280, 100], [0x288, 50], [0x380, 50], [0x4a0, 50], [0x480, 20], [0x540, 20], [0x5a0, 10], [0x320, 10], [0x570, 5], [0x420, 2], [0x65d, 1]];
  const cap = { running: false, t0: 0, path: null, format: "candump", stoppedAt: 0 };
  const hex2 = (n) => (n & 255).toString(16).toUpperCase().padStart(2, "0");
  function frameData(id, k) {
    const b = [];
    for (let i = 0; i < 8; i++) b.push(((id >> (i % 3)) * (i + 3)) & 255);
    b[0] = (b[0] + k) & 255; // rolling counter
    if (id === 0x280) { const r = Math.round((800 + Math.max(0, Math.sin(k / 160)) * 2200) * 4); b[2] = r & 255; b[3] = r >> 8; }
    if (id === 0x1a0 || id === 0x4a0) b[5] = (k >> 4) & 255;
    if (id === 0x5a0) b[1] = (k >> 2) & 255;
    return b.map(hex2).join("");
  }
  function snap() {
    const el = ((cap.running ? Date.now() : cap.stoppedAt) - cap.t0) / 1000;
    const ids = BUS.map(([id, hz]) => {
      const count = Math.floor(el * hz);
      return { id, ext: false, count, hz: hz * (0.98 + Math.random() * 0.04), dlc: 8, last: frameData(id, count) };
    });
    const frames = ids.reduce((a, r) => a + r.count, 0);
    return { running: cap.running, listen_only: "not-guaranteed", path: cap.path, format: cap.format, frames, bytes: frames * 41, fps: el ? frames / el : 0, elapsed_s: el, dropped: 0, ids, error: null, adapter: "ELM327 v1.5 (demo)" };
  }
  F.candump_start = async (a) => { await wait(200); Object.assign(cap, { running: true, t0: Date.now(), path: a.path, format: a.format }); return { ok: true }; };
  F.candump_status = () => snap();
  F.candump_stop = () => { cap.running = false; cap.stoppedAt = Date.now(); const st = snap(); return { path: st.path, frames: st.frames, bytes: st.bytes, elapsed_s: st.elapsed_s, dropped: 0 }; };

  // ---- ECU database --------------------------------------------------------
  const DEMO_PINS = {
    name: "Demo connector (preview placeholder)",
    pins: [
      { pin: "A", function: "Permanent supply (placeholder)", signal: "12v_perm", note: "Preview data — not a real pin" },
      { pin: "B", function: "Ignition (placeholder)", signal: "12v_ign", note: "Preview data — not a real pin" },
      { pin: "C", function: "Ground (placeholder)", signal: "gnd", note: "Preview data — not a real pin" },
      { pin: "D", function: "CAN high (placeholder)", signal: "can_h", note: "Preview data — not a real pin" },
      { pin: "E", function: "CAN low (placeholder)", signal: "can_l", note: "Preview data — not a real pin" },
      { pin: "F", function: "K-line (placeholder)", signal: "kline", note: "Preview data — not a real pin" },
    ],
  };
  const REAL = (window.__PREVIEW__ || {}).units || [];
  const DEMO = [
    { id: "bosch-edc16u34", kind: "ecu", maker: "Bosch", family: "EDC16", name: "EDC16U34", cpu: "MPC561/562", flash_kib: 2048, eeprom: "95080 SPI",
      bus: ["can", "kline"], protocols: ["kwp2000", "tp20"], read_methods: { obd: "read + write", bench: true, boot: true, notes: "Full read over OBD on most variants." },
      checksum: "edc16", ecudef: "bosch-edc16u34", vehicles: ["VW Golf V 1.9 TDI", "VW Passat B6 2.0 TDI PD", "Audi A3 8P 1.9 TDI"], part_number_examples: ["03G906021"],
      notes: "Unit-pump (PD) diesel ECU. Preview demo entry.", connectors: [DEMO_PINS], bench: { power: ["A"], ign: ["B"], ground: ["C"], can_h: ["D"], can_l: ["E"], kline: ["F"], notes: "Placeholder layout for the preview only." },
      pinout_confidence: "unverified", sources: [] },
    { id: "bosch-edc17cp14", kind: "ecu", maker: "Bosch", family: "EDC17", name: "EDC17CP14", cpu: "TC1796", flash_kib: 2048, bus: ["can"], protocols: ["uds"],
      read_methods: { obd: "write only", bench: true, boot: true, notes: "OBD writes the calibration; a full read needs bench or boot." }, checksum: "edc17", ecudef: null,
      vehicles: ["VW Passat CC 2.0 TDI CR", "VW Golf VI 2.0 TDI"], part_number_examples: ["03L906022"], notes: "Common-rail diesel, TriCore. Preview demo entry.", connectors: [], bench: { notes: "" }, pinout_confidence: null, sources: [] },
    { id: "siemens-pcr21", kind: "ecu", maker: "Continental / Siemens", family: "Simos PCR", name: "Simos PCR2.1", cpu: "TC1796", flash_kib: 2048, bus: ["can"], protocols: ["uds"],
      read_methods: { obd: "write only", bench: true, boot: true }, checksum: "pcr21", ecudef: "siemens-pcr21", vehicles: ["VW Golf VI 1.6 TDI", "Seat Ibiza 1.6 TDI"],
      part_number_examples: ["03L906023"], notes: "Preview demo entry.", connectors: [], bench: {}, pinout_confidence: null, sources: [] },
    { id: "temic-dq250", kind: "tcu", maker: "Temic", family: "DSG", name: "DQ250 (6-speed wet DSG)", cpu: "—", flash_kib: 0, bus: ["can"], protocols: ["uds"],
      read_methods: { obd: "read + write", bench: true, boot: false }, checksum: "dq250", ecudef: null, vehicles: ["VW Golf GTI", "VW Passat 2.0 TDI DSG"],
      part_number_examples: ["02E927770"], notes: "Preview demo entry.", connectors: [], bench: {}, pinout_confidence: null, sources: [] },
  ];
  // Prefer the real database files bundled by build.py; fall back to the demo.
  const UNITS = REAL.length ? REAL : DEMO;
  const brief = (u) => ({ id: u.id, kind: u.kind, maker: u.maker, family: u.family, name: u.name, cpu: u.cpu, flash_kib: u.flash_kib, bus: u.bus,
    read_methods: u.read_methods, checksum: u.checksum, ecudef: u.ecudef, vehicle_count: (u.vehicles || []).length,
    has_pinout: (u.connectors || []).some((c) => c.pins && c.pins.length), pinout_confidence: u.pinout_confidence });
  F.db_list = ({ kind, q }) => {
    const n = (q || "").toLowerCase();
    return UNITS.filter((u) => (!kind || u.kind === kind) && (!n || JSON.stringify(u).toLowerCase().includes(n))).map(brief);
  };
  F.db_get = ({ id }) => { const u = UNITS.find((x) => x.id === id); if (!u) throw `unit '${id}' not found`; return u; };
})();
