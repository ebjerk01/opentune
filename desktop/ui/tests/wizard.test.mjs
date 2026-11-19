// "Check my car" wizard (src/wizard-core.js + src/feat_wizard.js): the step
// state machine, plain-language errors, the voltage bands, the scan summary,
// explanations that only repeat the data, the live rating and the report;
// then the whole flow on the DOM stub against the preview backend driving the
// virtual car (and the OBD simulator), with skip / retry and a check that it
// only ever calls read-only commands. Synthetic data only.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";
// wizard-core.js is a classic script: importing it attaches globalThis.OTWizardCore.
import "../src/wizard-core.js";

const W = globalThis.OTWizardCore;
const { makeWindow, load } = dom;
const DESKTOP = path.join(dom.UI, "..");
const REPO = path.join(DESKTOP, "..");
const PREVIEW = path.join(DESKTOP, "preview");
const MODULES = "packs/vcar-demo/modules/vcar-demo.json";

// The Dashboard's channel table rows the live check needs (obd_pid_table shape).
const TABLE = [
  { pid: 5, key: "coolant_temp", name: "Coolant temperature", unit: "°C", warn: 105, crit: 115 },
  { pid: 6, key: "stft_b1", name: "Short-term fuel trim bank 1", unit: "%", warn: null, crit: null },
  { pid: 7, key: "ltft_b1", name: "Long-term fuel trim bank 1", unit: "%", warn: null, crit: null },
  { pid: 12, key: "rpm", name: "Engine speed", unit: "rpm", warn: null, crit: null },
  { pid: 13, key: "speed", name: "Vehicle speed", unit: "km/h", warn: null, crit: null },
  { pid: 66, key: "module_voltage", name: "Control module voltage", unit: "V", warn: null, crit: null },
  { pid: 11, key: "boost_kpa", name: "Boost", unit: "kPa", warn: null, crit: null, derived: true },
  { pid: 4, key: "engine_load", name: "Engine load", unit: "%", warn: null, crit: null },
];
const M = (key, name, state) => ({ key, name, continuous: false, state });
// An obd_inspect answer: engine with MIL on, codes in all three OBD lists.
const INSPECT = {
  t_ms: 1, silent: ["7E2", "7E3"],
  ecus: [
    { addr: { tx: "7E0", rx: "7E8", assumed: false }, label: "engine", responded: true, errors: {},
      readiness: { status: { mil: true, dtc_count: 2, ignition: "spark", monitors: [M("misfire", "Misfire", "complete"), M("catalyst", "Catalyst", "incomplete"), M("secondary_air", "Secondary air system", "not_supported")] } },
      dtcs: { stored: [{ code: "P0420", description: "Catalyst System Efficiency Below Threshold Bank 1", system: "powertrain" }],
        pending: [{ code: "P0171", description: "System Too Lean Bank 1", system: "powertrain" }],
        permanent: [{ code: "P0420", description: "Catalyst System Efficiency Below Threshold Bank 1", system: "powertrain" }], errors: {} } },
    { addr: { tx: "7E1", rx: "7E9", assumed: false }, label: "transmission", responded: true, errors: {},
      readiness: { status: { mil: false, dtc_count: 0, ignition: "spark", monitors: [] } },
      dtcs: { stored: [], pending: [], permanent: [], errors: { permanent: "mode 0A not supported" } } },
  ],
};
const AUTOSCAN = {
  vehicle: "Demo car", moduleCount: 3, presentCount: 2, dtcTotal: 2,
  modules: [
    { name: "Engine", present: true, identity: { vin: "TESTVCAR0D0000001", dids: [] }, dtcs: [{ code: "P0420", description: "Catalyst System Efficiency Below Threshold Bank 1", system: "P", status: "0x28" }], dtcCount: 1, error: null },
    { name: "ABS", present: true, identity: null, dtcs: [{ code: "C1234", description: "Manufacturer-specific / unknown code", system: "C", status: "0x2F" }], dtcCount: 1, error: null },
    { name: "Seat memory", present: false, identity: null, dtcs: [], dtcCount: 0, error: null },
  ],
};

// ---- state machine ---------------------------------------------------------------

test("steps: run / fail / retry / skip, Next only after done or skipped", () => {
  const r = W.newRun(0);
  assert.deepEqual(W.STEPS.map((s) => s.id), ["connect", "identify", "scan", "explain", "live", "report"]);
  assert.equal(W.canNext(r), false, "a pending step blocks Next");
  W.begin(r, "connect");
  assert.equal(W.busy(r), true);
  W.skip(r, "connect");
  assert.equal(r.steps.connect.status, "running", "a running step cannot be skipped");
  W.failStep(r, "connect", "adapter busy: Dashboard");
  assert.equal(r.steps.connect.status, "failed");
  assert.match(r.steps.connect.error.text, /Another panel is using the adapter/);
  assert.equal(r.steps.connect.error.raw, "adapter busy: Dashboard");
  assert.equal(W.canNext(r), false, "a failed step blocks Next until retried or skipped");
  W.begin(r, "connect");
  W.succeed(r, "connect", { ok: 1 });
  assert.equal(r.steps.connect.attempts, 2);
  assert.equal(r.steps.connect.error, null);
  assert.equal(W.canNext(r), true);
  W.next(r);
  assert.equal(r.current, 1);
  assert.equal(W.reachable(r, 3), false, "cannot jump past an unresolved step");
  W.skip(r, "identify");
  W.next(r);
  W.goTo(r, 0);
  assert.equal(r.current, 0);
  W.goTo(r, 2);
  assert.equal(r.current, 2);
  W.back(r);
  assert.equal(r.current, 1);
});

test("steps: re-running or skipping the scan makes Explain stale", () => {
  const r = W.newRun(0);
  W.succeed(r, "scan", { summary: {} });
  W.succeed(r, "explain", { items: [1] });
  W.succeed(r, "scan", { summary: {} });
  assert.equal(r.steps.explain.status, "pending");
  W.succeed(r, "explain", { items: [1] });
  W.skip(r, "scan");
  assert.equal(r.steps.explain.status, "pending");
  assert.equal(r.steps.explain.data, null);
});

// ---- errors ----------------------------------------------------------------------

test("errors: plain language with the raw text kept", () => {
  const cases = [
    ["adapter busy: Inspection is running", /Another panel/],
    ["open /dev/ttyUSB0: No such file or directory (os error 2)", /could not be opened/],
    ["no response from 7E0/7E8 within 1000 ms (ISO-TP timeout)", /car did not answer/],
    ["UNABLE TO CONNECT", /could not talk to the car/],
    ["parse module list: missing field", /module list could not be read/],
    ["weird failure 42", /Something went wrong/],
  ];
  for (const [raw, re] of cases) {
    const e = W.errorInfo(raw);
    assert.match(e.text, re, raw);
    assert.equal(e.raw, raw);
  }
  assert.equal(W.errorInfo(new Error("boom")).raw, "boom");
  assert.doesNotMatch(W.errorInfo("PID not supported by ECU").text, /could not be opened/, "'supported' is not a port");
});

// ---- voltage -------------------------------------------------------------------

test("voltage: documented bands, unknown without a reading", () => {
  assert.equal(W.voltageVerdict(null).level, "unknown");
  assert.equal(W.voltageVerdict(10900).level, "bad");
  assert.equal(W.voltageVerdict(11000).level, "warn");
  assert.equal(W.voltageVerdict(11999).level, "warn");
  assert.equal(W.voltageVerdict(12000).level, "ok");
  assert.match(W.voltageVerdict(12600).text, /12\.6 V .*engine off/);
  assert.match(W.voltageVerdict(14100).text, /14\.1 V .*charging range/);
  assert.equal(W.voltageVerdict(15000).level, "ok");
  assert.equal(W.voltageVerdict(15100).level, "warn");
  assert.deepEqual(W.VOLT, { NO_POWER: 11000, MIN_WRITE: 12000, RUNNING: 13000, HIGH: 15000 });
});

test("connect: probe answers become a result or a plain failure", () => {
  const ok = W.connectResult({ connected: true, driver: "vcar", name: "Virtual car", battery_mv: 14100, capabilities: { can: true, battery_voltage: true } }, { driver: "vcar" });
  assert.equal(ok.sim, false);
  assert.equal(ok.voltage.level, "ok");
  const noV = W.connectResult({ connected: true, name: "DoIP", battery_mv: null, capabilities: { doip: true, battery_voltage: false } }, { driver: "doip" });
  assert.equal(noV.voltage.level, "unknown");
  const sim = W.connectResult(null, { driver: "obdsim" });
  assert.equal(sim.sim, true);
  assert.throws(() => W.connectResult({ connected: false, error: "open COM9: Access is denied" }, { driver: "elm327obd" }), /Access is denied/);
  assert.throws(() => W.connectResult({ connected: true, name: "K-line", capabilities: { kline: true } }, { driver: "kkl" }), /cannot carry diagnostics/);
  assert.throws(() => W.connectResult(null, { driver: "elm327obd" }));
});

// ---- identify / scan / explain ---------------------------------------------------------

test("identify: VIN from mode 09, else the fallback; decoded fields carried", () => {
  const insp = { ecus: [{ responded: true, addr: { tx: "7E0", rx: "7E8" }, info: { vin: "TESTVCAR0D0000001", ecu_name: "ECM-Demo", calids: ["CAL-1"], cvns: ["0000BEEF"], errors: {} }, errors: {} }] };
  const a = W.identity(insp, null, { manufacturer: "Demo", model_year: { resolved: 2013, candidates: [2013] }, flags: [] });
  assert.equal(a.vin, "TESTVCAR0D0000001");
  assert.equal(a.vinSource, "OBD mode 09");
  assert.equal(a.target, "7E0/7E8");
  assert.equal(a.decoded.modelYear, 2013);
  const b = W.identity({ ecus: [{ responded: true, info: { calids: [], cvns: [], errors: { vin: "NRC 0x31" } }, errors: {} }] }, { vin: "TESTVCAR0D0000002" }, null);
  assert.equal(b.vin, "TESTVCAR0D0000002");
  assert.match(b.vinSource, /F190/);
  assert.equal(b.errors.vin, "NRC 0x31");
});

test("scan summary: MIL, readiness and codes merged per code across views", () => {
  const s = W.scanSummary({ obd: INSPECT, modules: AUTOSCAN });
  assert.equal(s.mil, true);
  assert.deepEqual(s.milBy, ["Engine (OBD 7E0)"]);
  assert.deepEqual(s.obdModules, ["Engine (OBD 7E0)", "Transmission (OBD 7E1)"]);
  assert.deepEqual(s.readiness.incomplete.map((m) => m.name), ["Catalyst"]);
  assert.deepEqual(s.readiness.unsupported.map((m) => m.name), ["Secondary air system"]);
  assert.deepEqual(s.dtcs.map((d) => d.code), ["C1234", "P0171", "P0420"]);
  const p0420 = s.dtcs.find((d) => d.code === "P0420");
  assert.deepEqual(p0420.seenAs.map((x) => x.kind), ["stored", "permanent", "module"]);
  assert.deepEqual(s.modules.absent, ["Seat memory"]);
  assert.match(s.notes[0], /permanent codes not read/);
  assert.equal(W.scanSummary({ obd: { ecus: [], silent: [] } }).mil, null, "no readiness: MIL unknown");
});

test("explain: only what the data says — database text, status bits, readiness", () => {
  const s = W.scanSummary({ obd: INSPECT, modules: AUTOSCAN });
  const lookups = { P0420: { description: "Catalyst System Efficiency Below Threshold Bank 1", system: "Powertrain (engine / transmission / emissions)" } };
  const items = W.explain(s, lookups, {});
  const mil = items.find((i) => i.kind === "mil");
  assert.match(mil.title, /ON/);
  const p = items.find((i) => i.code === "P0420");
  assert.equal(p.title, "P0420 — Catalyst System Efficiency Below Threshold Bank 1");
  assert.equal(p.text, W.CODE_TEXT, "no cause beyond the code's own description");
  assert.equal(p.system, "Powertrain (engine / transmission / emissions)");
  assert.ok(p.seen.some((x) => /^Engine: stored in the module's fault memory — status 0x28: confirmed and stored, has failed since/.test(x)), p.seen.join("|"));
  const pend = items.find((i) => i.code === "P0171");
  assert.equal(pend.level, "warn", "pending only");
  const unk = items.find((i) => i.code === "C1234");
  assert.match(unk.title, /no description in OpenTune's code table/);
  assert.match(unk.text, /manufacturer-specific or unknown/);
  assert.ok(unk.seen[0].includes("failing at the moment of the scan"), "0x2F has testFailed");
  const rd = items.find((i) => i.kind === "readiness" && i.level === "warn");
  assert.equal(rd.text, W.INCOMPLETE_TEXT);
  assert.deepEqual(rd.list, ["Catalyst"]);
  assert.ok(items.some((i) => i.kind === "modules" && /Seat memory/.test(i.text)));
  // No word of the explanations hints at a cause the data does not give.
  const all = items.map((i) => [i.title, i.text, ...(i.seen || [])].join(" ")).join(" ");
  assert.doesNotMatch(all, /\b(likely|probably|caused by|replace|faulty|bad sensor)\b/i);
  // Clean car / skipped scan.
  const clean = W.explain(W.scanSummary({ obd: { ecus: [{ ...INSPECT.ecus[1] }], silent: [] } }), {}, {});
  assert.ok(clean.some((i) => i.title === "No trouble codes" && i.level === "ok"));
  assert.match(clean.find((i) => i.kind === "mil").title, /off/);
  assert.equal(W.explain(null)[0].kind, "nodata");
});

test("UDS status bits in plain words", () => {
  assert.deepEqual(W.udsStatusText("0x08"), ["confirmed and stored"]);
  assert.equal(W.udsStatusText(0x89).length, 3);
  assert.deepEqual(W.udsStatusText(null), []);
  assert.deepEqual(W.udsStatusText("zz"), []);
});

// ---- live -----------------------------------------------------------------------

test("live: pids from the table and the ECU's support; ratings from warn / crit only", () => {
  assert.deepEqual(W.livePids(TABLE, null), [5, 6, 7, 12, 13, 66]);
  assert.deepEqual(W.livePids(TABLE, [5, 12, 13]), [5, 12, 13]);
  const read = (rpm, cool, speed, mv) => ({ battery_mv: mv, values: { rpm, coolant_temp: cool, speed, stft_b1: null, ltft_b1: 2.3, module_voltage: 14.1 }, errors: { stft_b1: "not supported by ECU" } });
  const s = W.liveSummary([read(820, 90, 0, 14100), read(800, 106, 0, 14050)], TABLE);
  const row = (k) => s.rows.find((r) => r.key === k);
  assert.equal(row("coolant_temp").level, "warn");
  assert.equal(row("coolant_temp").max, 106);
  assert.match(row("coolant_temp").note, /warning mark of 105/);
  assert.equal(row("stft_b1").last, null);
  assert.match(row("stft_b1").note, /not supported by ECU/);
  assert.equal(row("ltft_b1").level, "", "no threshold, no rating");
  assert.equal(row("ltft_b1").last, 2.3);
  assert.match(row("ltft_b1").note, /no threshold/);
  assert.equal(s.idle.state, "idle");
  assert.match(s.idle.text, /800–820 rpm/);
  assert.equal(s.battery.mv, 14050);
  assert.equal(W.liveSummary([read(820, 116, 0, 14100)], TABLE).rows.find((r) => r.key === "coolant_temp").level, "crit");
  assert.equal(W.liveSummary([read(0, 20, 0, 12500)], TABLE).idle.state, "off");
  assert.equal(W.liveSummary([read(1900, 80, 50, 14000)], TABLE).idle.state, "moving");
  assert.equal(W.liveSummary([], TABLE).battery.level, "unknown");
});

// ---- report ------------------------------------------------------------------------

function fullRun() {
  const r = W.newRun(0);
  W.succeed(r, "connect", W.connectResult({ connected: true, name: "Virtual <car>", battery_mv: 14100, capabilities: { can: true } }, { driver: "vcar" }));
  W.succeed(r, "identify", W.identity({ ecus: [{ responded: true, addr: { tx: "7E0", rx: "7E8" }, info: { vin: "TESTVCAR0D0000001", calids: [], cvns: [], errors: {} }, errors: {} }] }, null, null));
  const summary = W.scanSummary({ obd: INSPECT, modules: AUTOSCAN });
  W.succeed(r, "scan", { summary, problems: [], modulesNote: null });
  W.succeed(r, "explain", { items: W.explain(summary, {}, {}) });
  W.failStep(r, "live", "adapter busy: Dashboard");
  return r;
}

test("report: HTML and Markdown carry every step, escape data, say it was read-only", () => {
  const r = fullRun();
  const html = W.reportHtml(r, { now: Date.UTC(2026, 0, 2), app: "OpenTune" }, true);
  assert.match(html, /^<!doctype html>/);
  assert.match(html, /<title>OpenTune car check — TESTVCAR0D0000001<\/title>/);
  assert.ok(html.includes("Virtual &lt;car&gt;"), "data is escaped");
  assert.ok(!html.includes("Virtual <car>"));
  assert.match(html, /P0420/);
  assert.match(html, /Live check<span class="st failed">failed/);
  assert.match(html, /Another panel is using the adapter/);
  assert.doesNotMatch(html, /<script/i, "a static page");
  assert.ok(html.includes(W.DISCLAIMER.replace(/'/g, "&#39;")));
  const md = W.reportMarkdown(r, { now: 0 });
  assert.match(md, /^# OpenTune car check/);
  assert.match(md, /## Scan — done/);
  assert.match(md, /\| P0420 \| Catalyst System Efficiency Below Threshold Bank 1 \|/);
  assert.match(md, /## Live check — failed/);
  assert.match(md, /Detail: `adapter busy: Dashboard`/);
  assert.match(md, /Read-only check/);
  assert.match(W.reportFileName(r, "html", Date.UTC(2026, 0, 2, 12)), /^opentune-car-check-2026-01-02-000001\.html$/);
  W.skip(r, "live");
  assert.match(W.reportMarkdown(r), /## Live check — skipped\n\nThis step was skipped\./);
});

// ---- the panel against the preview backend -----------------------------------------

const READ_ONLY = new Set(["build_channel", "packs_list", "packs_open", "probe_adapter", "obd_inspect", "obd_supported", "vin_decode", "autoscan_run", "dtc_lookup", "obd_pid_table", "obd_read"]);

function preview() {
  const files = { [MODULES]: JSON.parse(fs.readFileSync(path.join(REPO, MODULES), "utf8")) };
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  sandbox.__PREVIEW__ = { vcar: { profile, files }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const list = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of list) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, profile };
}

async function until(cond, ms = 8000) {
  const t0 = Date.now();
  while (!cond()) {
    if (Date.now() - t0 > ms) throw new Error("timed out waiting");
    await new Promise((r) => setTimeout(r, 20));
  }
}

function panel(driver, { failOnce } = {}) {
  const { F, profile } = preview();
  const calls = [];
  const failed = new Set();
  const conn = { driver, port: driver === "vcar" ? "demo" : "", bus: "hs", txId: "", rxId: "" };
  const stub = {
    core: {
      invoke: async (cmd, args) => {
        calls.push([cmd, args]);
        if (cmd === "build_channel") return "bench";
        if (failOnce && failOnce[cmd] && !failed.has(cmd)) { failed.add(cmd); throw failOnce[cmd]; }
        return F[cmd](args || {});
      },
    },
    dialog: { open: async () => null, save: async () => null },
  };
  const { ctx, window, document } = makeWindow({ tauri: stub });
  window.setTimeout = (fn, ms) => setTimeout(fn, Math.min(ms || 0, 5));
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  const nz = (s) => (s ? s : null);
  window.OT = {
    conn,
    adapter: null,
    connArgs: () => ({ driver: conn.driver, port: nz(conn.port), baud: null, bus: nz(conn.bus), txId: nz(conn.txId), rxId: nz(conn.rxId) }),
    setConn: (p) => Object.assign(conn, p),
    async probe() { const st = await stub.core.invoke("probe_adapter", window.OT.connArgs()); window.OT.adapter = st; return st; },
    onCaps() {},
    gate() {},
  };
  load(ctx, "nav.js");
  load(ctx, "pack-picker.js");
  load(ctx, "wizard-core.js");
  load(ctx, "feat_wizard.js");
  const p = document.getElementById("panel-wizard");
  const $ = (id) => p.querySelector("#" + id);
  const Z = window.OTWizard;
  const step = () => W.STEPS[Z.run.current].id;
  async function runCurrent() {
    $("wizRun").click();
    await until(() => !W.busy(Z.run));
    return Z.run.steps[step()];
  }
  async function next() {
    $("wizNext").click();
    await until(() => !W.busy(Z.run));
  }
  return { window, document, p, $, Z, calls, step, runCurrent, next, profile, conn };
}

test("panel: the whole flow on the virtual car, read-only from start to report", async () => {
  const t = panel("vcar");
  const { $, Z, step } = t;
  const nav = t.document.getElementById("nav").querySelector('.nav-item[data-panel="wizard"]');
  assert.equal(nav.dataset.tier, "bench", "Bench tier nav entry");
  assert.equal(step(), "connect");
  assert.equal($("wizNext").disabled, true, "Next waits for the step");

  let s = await t.runCurrent();
  assert.equal(s.status, "done", JSON.stringify(s.error));
  assert.equal(s.data.voltage.level, "ok");
  assert.match($("wizResult").innerHTML, /charging range/);
  await t.next();

  assert.equal(step(), "identify");
  s = await t.runCurrent();
  assert.equal(s.status, "done", JSON.stringify(s.error));
  assert.equal(s.data.vin, t.profile.vin);
  await t.next();

  assert.equal(step(), "scan");
  assert.equal($("wizPackBox").hidden, false, "module-list picker shown on the scan step");
  Z.state.modulesPath = MODULES;
  s = await t.runCurrent();
  assert.equal(s.status, "done", JSON.stringify(s.error));
  const sm = s.data.summary;
  assert.equal(sm.mil, true);
  assert.ok(sm.dtcs.some((d) => d.code === "P0101"));
  assert.ok(sm.modules.total > 0 && sm.modules.absent.length >= 1);
  await t.next();

  assert.equal(step(), "explain");
  await until(() => Z.run.steps.explain.status === "done");
  const items = Z.run.steps.explain.data.items;
  const p0101 = items.find((i) => i.code === "P0101");
  assert.match(p0101.title, /Mass or Volume Air Flow/);
  assert.ok($("wizOpenScan"), "a link to Auto-scan for clearing, not a clear button");
  await t.next();

  assert.equal(step(), "live");
  s = await t.runCurrent();
  assert.equal(s.status, "done", JSON.stringify(s.error));
  assert.equal(s.data.summary.samples, W.LIVE_SAMPLES);
  assert.equal(s.data.summary.idle.state, "idle");
  assert.ok(s.data.summary.rows.find((r) => r.key === "coolant_temp").level);
  assert.equal(s.data.summary.battery.level, "ok");
  await t.next();

  assert.equal(step(), "report");
  await until(() => Z.run.steps.report.status === "done");
  assert.ok($("wizSaveHtml") && $("wizSaveMd") && $("wizPrint"));
  const html = Z.reportText("html");
  assert.ok(html.includes(t.profile.vin));
  assert.match(html, /P0101/);
  assert.match(Z.reportText("md"), /## Live check — done/);

  const cmds = new Set(t.calls.map(([c]) => c));
  for (const c of cmds) assert.ok(READ_ONLY.has(c), `wizard called ${c}, which is not on the read-only list`);
  for (const c of ["probe_adapter", "obd_inspect", "autoscan_run", "dtc_lookup", "obd_read"]) assert.ok(cmds.has(c), c);
  const scan = t.calls.find(([c, a]) => c === "obd_inspect" && a.allEcus);
  assert.deepEqual(Array.from(scan[1].sections), ["readiness", "dtcs"]);
  assert.equal(scan[1].driver, "vcar");
});

test("panel: OBD simulator needs no adapter and skips the module list", async () => {
  const t = panel("obdsim");
  let s = await t.runCurrent();
  assert.equal(s.status, "done");
  assert.equal(s.data.sim, true);
  assert.ok(!t.calls.some(([c]) => c === "probe_adapter"), "no probe for the simulator");
  await t.next();
  await t.runCurrent();
  await t.next();
  t.Z.state.modulesPath = MODULES;
  s = await t.runCurrent();
  assert.equal(s.status, "done");
  assert.match(s.data.modulesNote, /only answers OBD/);
  assert.ok(!t.calls.some(([c]) => c === "autoscan_run"));
});

test("panel: a failed step shows plain language + raw detail; Retry and Skip work", async () => {
  const t = panel("vcar", { failOnce: { obd_inspect: "adapter busy: Dashboard is running" } });
  const { $, Z, step } = t;
  await t.runCurrent();
  await t.next();
  let s = await t.runCurrent();
  assert.equal(s.status, "failed");
  assert.match($("wizResult").innerHTML, /Another panel is using the adapter/);
  assert.match($("wizResult").innerHTML, /<details/);
  assert.equal($("wizRun").textContent, "Retry");
  assert.equal($("wizNext").disabled, true);
  s = await t.runCurrent();
  assert.equal(s.status, "done", "retry succeeds");
  assert.equal(s.attempts, 2);
  await t.next();
  // Skip the scan: Explain says there is nothing to explain.
  $("wizSkip").click();
  await until(() => Z.run.steps.explain.status === "done");
  assert.equal(step(), "explain");
  assert.equal(Z.run.steps.scan.status, "skipped");
  assert.equal(Z.run.steps.explain.data.items[0].kind, "nodata");
  // The connect step can be redone from the step bar.
  W.goTo(Z.run, 0);
  assert.equal(step(), "connect");
});

test("panel: the Use the virtual car button points the shared connection at it", async () => {
  const t = panel("elm327obd");
  t.$("wizUseVcar").click();
  assert.equal(t.conn.driver, "vcar");
  assert.equal(t.conn.port, "demo");
  t.$("wizUseSim").click();
  assert.equal(t.conn.driver, "obdsim");
});
