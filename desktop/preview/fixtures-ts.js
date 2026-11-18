// Demo fixtures for the Aftermarket ECU panel. Preview only: a synthetic
// Speeduino-like INI and tune (the shapes of samples/ts/opentune-demo.ini), a
// simulated ECU and made-up realtime values. Describes no real ECU.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const t0 = Date.now();
  const clone = (x) => JSON.parse(JSON.stringify(x));

  const meta = (name, cls, units, min, max, digits, extra) => Object.assign({ name, page: 1, pc: false, class: cls, type: "U08", units, cols: null, rows: null, options: null, len: null, min, max, digits }, extra || {});
  const C = {
    reqFuel: { m: meta("reqFuel", "scalar", "ms", 0, 25.5, 1), v: 8.5 },
    crankRPM: { m: meta("crankRPM", "scalar", "rpm", 100, 1000, 0), v: 300 },
    cltOffset: { m: meta("cltOffset", "scalar", "C", -40, 40, 0), v: 0 },
    injLayout: { m: meta("injLayout", "bits", "", null, null, null, { options: ["Paired", "Semi-Sequential", "INVALID", "Sequential"] }), v: 1 },
    nCyl: { m: meta("nCyl", "bits", "", null, null, null, { options: ["INVALID", "1", "2", "3", "4", "5", "6", "7", "8"] }), v: 4 },
    injOpen: { m: meta("injOpen", "scalar", "ms", -32, 32, 3), v: 0.95 },
    tuneName: { m: meta("tuneName", "string", "", null, null, null, { len: 16 }), v: "street" },
    veTable: { m: meta("veTable", "array", "%", 0, 255, 0, { page: 2, cols: 8, rows: 6 }), v: [] },
    rpmBins: { m: meta("rpmBins", "array", "RPM", 100, 25500, 0, { page: 2, cols: 1, rows: 8 }), v: [500, 1000, 1500, 2500, 3500, 4500, 5500, 6500] },
    mapBins: { m: meta("mapBins", "array", "kPa", 0, 510, 0, { page: 2, cols: 1, rows: 6 }), v: [30, 50, 70, 100, 140, 180] },
    wueBins: { m: meta("wueBins", "array", "C", -40, 215, 0, { cols: 1, rows: 6 }), v: [-40, -10, 10, 30, 50, 70] },
    wueRates: { m: meta("wueRates", "array", "%", 100, 255, 0, { cols: 1, rows: 6 }), v: [180, 160, 145, 130, 115, 100] },
  };
  for (let y = 0; y < 6; y++) for (let x = 0; x < 8; x++) C.veTable.v.push(Math.round(35 + x * 6 + y * 7 - (x > 5 ? (x - 5) * 4 : 0)));
  let ecu = null; // what the simulated ECU holds
  let conn = null;
  const snapshot = () => Object.fromEntries(Object.entries(C).map(([k, c]) => [k, clone(c.v)]));
  const display = (c, v) => c.m.class === "bits" ? `${v} "${c.m.options[v]}"` : c.m.class === "string" ? JSON.stringify(v)
    : c.m.class === "array" ? `[${c.m.cols}x${c.m.rows}] ${Math.min(...v)} .. ${Math.max(...v)} ${c.m.units}` : `${Number(v).toFixed(c.m.digits || 0)} ${c.m.units}`;
  const tagged = (c, v) => ({ type: c.m.class === "bits" ? "bits" : c.m.class === "string" ? "text" : c.m.class, value: clone(v) });
  const diff = (a, b) => Object.keys(C).filter((k) => JSON.stringify(a[k]) !== JSON.stringify(b[k])).map((k) => {
    const c = C[k];
    if (c.m.class === "array") {
      const n = a[k].filter((v, i) => v !== b[k][i]).length;
      return { name: k, page: c.m.page, change: `${n} of ${a[k].length} cells changed` };
    }
    return { name: k, page: c.m.page, change: `${display(c, a[k])} -> ${display(c, b[k])}` };
  });

  F.ts_status = () => ({ ini: iniLoaded ? "demo/speeduino-demo.ini" : null, signature: "opentune demo 1", working: iniLoaded ? workingSrc : null, ecu_read: !!ecuRead, connected: conn, sim: !!conn });
  let iniLoaded = false, workingSrc = "blank", ecuRead = null;
  F.ts_load_ini = async ({ path }) => {
    await wait(200);
    iniLoaded = true; conn = null; ecuRead = null; workingSrc = "blank";
    return {
      path, signature: "opentune demo 1", framing: "envelope",
      pages: [{ page: 1, size: 64, burnable: true }, { page: 2, size: 112, burnable: true }],
      constants: 21, pc_variables: 3,
      tables: [{ id: "veTbl", title: "VE Table", z: "veTable" }],
      curves: [{ id: "wueCurve", title: "Warmup enrichment" }],
      channels: ["secl", "rpm", "map", "coolant", "afr", "tps", "advance", "batteryV"].map((name) => ({ name, units: "", class: "scalar" })),
      setting_groups: [{ name: "temp_units", label: "Temperature", options: [{ name: "DEFAULT", label: "Celsius" }, { name: "FAHRENHEIT", label: "Fahrenheit" }] }],
      conditions: [], warnings: { invalid: 0, reference: 0, ignored: 3, first_invalid: [] },
    };
  };
  F.ts_load_msq = async ({ path }) => { await wait(150); workingSrc = String(path).split(/[\\/]/).pop(); return { path, signature: "opentune demo 1", ini_signature: "opentune demo 1", entries: 19, report: { applied: 19, unknown: [], skipped: [], out_of_range: [], notes: [] }, conditions_added: [], base: ecuRead ? "ecu" : "blank" }; };
  F.ts_save_msq = async ({ path }) => ({ path, entries: 19, skipped: [] });
  F.ts_diff_msq = async ({ path }) => { const other = snapshot(); other.reqFuel = 7.9; other.crankRPM = 350; return { path, rows: diff(other, snapshot()), count: 2, report: { applied: 19 }, direction: "file -> working tune" }; };
  const vjson = (k) => ({ name: k, value: tagged(C[k], C[k].v), display: display(C[k], C[k].v), error: null, meta: C[k].m });
  F.ts_values = () => ({ source: workingSrc, values: Object.keys(C).filter((k) => C[k].m.class !== "array").map(vjson) });
  F.ts_get = ({ name }) => vjson(name);
  F.ts_set = ({ name, value }) => {
    const c = C[name];
    if (!c) throw `'${name}' is not defined in the INI`;
    const chk = (x) => { if ((c.m.min != null && x < c.m.min) || (c.m.max != null && x > c.m.max)) throw `refused: ${name} = ${x} outside ${c.m.min}..${c.m.max} (the INI's limits)`; };
    if (c.m.class === "array") value.forEach(chk); else if (c.m.class === "scalar") chk(value);
    if (c.m.class === "bits" && c.m.options[value] === "INVALID") throw `${name}: option ${value} is marked INVALID in the INI`;
    c.v = clone(value);
    return vjson(name);
  };
  const axis = (k, channel) => ({ name: k, channel, units: C[k].m.units, digits: 0, values: C[k].v, error: null });
  F.ts_table = () => ({ id: "veTbl", title: "VE Table", x: axis("rpmBins", "rpm"), y: axis("mapBins", "map"), z: C.veTable.m, cols: 8, rows: 6,
    cells: Array.from({ length: 6 }, (_, r) => C.veTable.v.slice(r * 8, r * 8 + 8)), help: null });
  F.ts_curve = () => ({ id: "wueCurve", title: "Warmup enrichment", labels: [], x: axis("wueBins", "coolant"), ys: [Object.assign(axis("wueRates", null), { meta: C.wueRates.m })] });

  F.ts_connect = async ({ port, forceSignature }) => {
    await wait(300);
    if (!port || !port.trim()) throw "choose a serial port (or 'sim')";
    if (port.trim().toLowerCase() !== "sim") throw `open ${port}: no serial ports in the preview — use 'sim'`;
    conn = "sim (simulated ECU, no hardware)";
    if (!ecu) ecu = snapshot();
    return { label: conn, sim: true, framing: "envelope", signature: { ecu: "opentune demo 1", ini: "opentune demo 1", matches: true }, forced: !!forceSignature };
  };
  F.ts_disconnect = async () => { const was = conn; conn = null; return { closed: was }; };
  F.ts_read = async () => {
    if (!conn) throw "not connected — press Connect first";
    await wait(400);
    Object.entries(ecu).forEach(([k, v]) => (C[k].v = clone(v)));
    ecuRead = snapshot(); workingSrc = "ECU";
    return { pages: 2, bytes: 176, elapsed_s: 0.38 };
  };
  F.ts_plan_write = () => {
    if (!ecuRead) throw "read the ECU's tune first (Read from ECU) — the plan compares against it";
    const rows = diff(ecuRead, snapshot());
    const pages = [...new Set(rows.map((r) => r.page))].sort().map((page) => ({ page, ranges: [[0, 8]], changed_bytes: rows.filter((r) => r.page === page).length * 2, burnable: true }));
    return { empty: !rows.length, changed_bytes: pages.reduce((a, p) => a + p.changed_bytes, 0), pages, constants: rows, connected: conn, signature: "opentune demo 1" };
  };
  F.ts_write = async ({ confirmed, burn, backupPath }) => {
    if (!confirmed) throw "refusing to write without an explicit confirmation";
    if (!backupPath) throw "choose where to save the backup of the ECU's current tune before writing";
    await wait(700);
    const p = F.ts_plan_write();
    ecu = snapshot(); ecuRead = snapshot();
    const pages = p.pages.map((x) => x.page);
    return { written: true, bytes_written: p.changed_bytes, verified_pages: pages, burned: burn ? pages : [], not_burnable: [], crc_checked: burn ? pages : [], burn, backup: backupPath, raw_backup: String(backupPath).replace(/\.msq$/, ".pages.bin") };
  };
  F.ts_poll = async ({ channels }) => {
    if (!conn) throw "not connected — press Connect first";
    await wait(30);
    const s = (Date.now() - t0) / 1000, load = Math.max(0, Math.sin(s / 2.5)) ** 2;
    const vals = { secl: Math.floor(s) % 256, rpm: 900 + load * 4200, map: 35 + load * 120, coolant: Math.min(88, 60 + s / 4), afr: 14.7 - load * 2.4, tps: load * 100, advance: 12 + load * 18, batteryV: 13.8 };
    const units = { rpm: "rpm", map: "kPa", coolant: "C", afr: "AFR", tps: "%", advance: "deg", batteryV: "V", secl: "s" };
    return { t_s: s, samples: (channels && channels.length ? channels : Object.keys(vals)).map((name) => (name in vals
      ? { name, raw: Math.round(vals[name]), value: vals[name], units: units[name], note: "" }
      : { name, raw: null, value: null, units: "", note: `'${name}' is not an output channel` })) };
  };
})();
