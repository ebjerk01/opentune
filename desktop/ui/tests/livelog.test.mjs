// Live log panel (src/livelog-core.js + src/feat_logging.js): the link pill
// from a tick's link view and markers, what the Drive view is fed, the
// transport options for log_start and the gate per transport, the SSM
// summary; then the panel on the DOM stub against the preview backend (an
// SSM session on the simulator, and a session whose link drops and comes
// back). Synthetic data only.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";
// livelog-core.js is a classic script: importing it attaches globalThis.OTLiveLog.
import "../src/livelog-core.js";

const L = globalThis.OTLiveLog;
const { makeWindow, load } = dom;
const PREVIEW = path.join(dom.UI, "..", "preview");
const REPO = path.join(dom.UI, "..", "..");
const tick = async (n = 8) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const marker = (state, note) => ({ paramId: "link", name: "Link status", raw: null, value: null, units: "", note, text: state });
const chan = (id, value) => ({ paramId: id, name: id, raw: value, value, units: "rpm", note: "" });
const link = (o) => ({ state: "up", reconnect: true, dead_ticks: 0, dead_after: 5, attempt: 0, retry_in_ms: null, error: null, drops: 0, reconnects: 0, downtime_ms: 0, last_event: null, ...o });

// ---- pure logic -----------------------------------------------------------------

test("link pill: up, silent k/n, lost, reconnecting attempt k, reconnected, gave up", () => {
  assert.equal(L.linkBadge(link(), []), null, "nothing to show while the link is up");
  assert.equal(L.linkBadge(null, []), null, "a poll without a link view");
  const silent = L.linkBadge(link({ state: "silent", dead_ticks: 3, error: "timed out after 1000 ms" }), []);
  assert.deepEqual([silent.cls, silent.text], ["accent", "no answer 3/5"]);
  assert.match(silent.title, /timed out/);
  const lost = L.linkBadge(link({ state: "lost", error: "io error" }), [marker("lost", "link lost after 5 dead round(s): io error")]);
  assert.deepEqual([lost.cls, lost.text], ["bad", "link lost — reconnecting…"]);
  assert.match(lost.title, /5 dead round/);
  assert.equal(L.linkBadge(link({ state: "lost", reconnect: false }), []).text, "link lost");
  const re = L.linkBadge(link({ state: "reconnecting", attempt: 2, retry_in_ms: 2000, error: "open: no such device" }), []);
  assert.equal(re.text, "reconnecting (attempt 2)");
  assert.match(re.title, /no such device; next try in 2\.0 s/);
  const back = L.linkBadge(link(), [marker("reconnected", "reconnected after 3.4 s (attempt 2)")]);
  assert.deepEqual([back.cls, back.text, back.title], ["ok", "reconnected", "reconnected after 3.4 s (attempt 2)"]);
  assert.equal(L.linkBadge(link({ state: "gave-up" }), [marker("gave-up", "gave up")]).text, "gave up reconnecting");
  // The stateless poll's link view (log_poll_once): state + error only.
  assert.equal(L.linkBadge({ state: "lost", error: "link lost: interface adapter reset: banner" }, []).text, "link lost");
  assert.equal(L.linkText(link(), []), "");
  assert.match(L.linkText({ state: "silent", error: "timeout" }, []), /^no answer: timeout$/);
});

test("markers are split from the channels", () => {
  const s = [chan("rpm", 800), marker("lost", "x"), chan("ect", 90)];
  assert.deepEqual(L.linkMarkers(s).map((m) => m.text), ["lost"]);
  assert.deepEqual(L.channelSamples(s).map((m) => m.paramId), ["rpm", "ect"]);
  assert.deepEqual(L.linkMarkers(null), []);
});

test("Drive view: a frame while the link answers, an error while it is down", () => {
  const frameOf = (ch) => ch.map((c) => ({ key: c.paramId, value: c.value }));
  const up = L.driveEvent({ ts: 5, samples: [chan("rpm", 800), marker("reconnected", "back")], link: link() }, frameOf);
  assert.deepEqual(up, { type: "frame", source: "livelog", tMs: 5, values: [{ key: "rpm", value: 800 }] }, "markers are not channels");
  for (const state of ["silent", "lost", "reconnecting", "gave-up"]) {
    const ev = L.driveEvent({ ts: 5, samples: [], link: link({ state, attempt: 1, error: "e" }) }, frameOf);
    assert.equal(ev.type, "error", state);
    assert.equal(ev.source, "livelog");
    assert.equal(ev.busy, false, "counts toward the Drive view's lost link");
    assert.ok(ev.message.length > 0);
  }
  // A poll result without a link view (older backend) is a frame.
  assert.equal(L.driveEvent({ samples: [chan("rpm", 1)] }, frameOf).type, "frame");
});

test("transport options: log_start args, session use, gate needs", () => {
  assert.deepEqual(L.startArgs({ transport: "uds", reconnect: true }), { transport: "uds", reconnect: true });
  assert.deepEqual(L.startArgs({ transport: "ssm", target: "tcu" }), { transport: "ssm", reconnect: false, ssmTarget: "tcu", fast: false });
  assert.deepEqual(L.startArgs({ transport: "ssm-can", target: "x", sim: true }),
    { transport: "ssm-can", reconnect: false, ssmTarget: "ecu", fast: false, driver: "ssmsim" });
  assert.deepEqual(L.startArgs({ transport: "bogus", sim: true }), { transport: "uds", reconnect: false }, "the simulator is SSM only");
  assert.equal(L.usesSession({ transport: "uds" }), false, "plain polling stays stateless");
  assert.equal(L.usesSession({ transport: "uds", fast: true }), true);
  assert.equal(L.usesSession({ transport: "uds", reconnect: true }), true);
  assert.equal(L.usesSession({ transport: "ssm" }), true);
  assert.deepEqual(L.needsFor("uds", false), ["diag"]);
  assert.deepEqual(L.needsFor("ssm-can", false), ["diag"]);
  assert.deepEqual(L.needsFor("ssm", false), ["kline"]);
  assert.deepEqual(L.needsFor("ssm", true), []);
  assert.deepEqual(L.needsFor("uds", true), ["diag"], "no simulator for UDS here");
  assert.deepEqual(L.TRANSPORTS.map((t) => t.id), ["uds", "ssm", "ssm-can"]);
});

test("SSM summary: identification, logged count, reasons, ids not logged", () => {
  assert.equal(L.ssmSummary(null), null);
  const s = L.ssmSummary({ target: "ecu", wire: "k-line", ssm_id: "A05E01", rom_id: "5A5A000001", capability_bytes: 4, logged: 5,
    notes: ["1 parameter(s) not supported by this module (capability bit clear): P_NOPE"], not_logged: ["P_TCU"] });
  assert.equal(s.head, "ECU over K-line · SSM ID A05E01 · ROM ID 5A5A000001 · 4 capability byte(s)");
  assert.equal(s.logged, "5 parameter(s) logged");
  assert.deepEqual(s.notes, ["1 parameter(s) not supported by this module (capability bit clear): P_NOPE"]);
  assert.match(s.notLogged, /not logged.*P_TCU/);
  assert.match(L.ssmSummary({ target: "tcu", wire: "can", logged: 1 }).head, /^TCU over CAN/);
  assert.equal(L.ssmSummary({ target: "ecu", wire: "can", not_logged: [] }).notLogged, "");
});

test("table channels: what the session logs, else the selection", () => {
  const def = [{ id: "a" }, { id: "b" }, { id: "c" }];
  assert.deepEqual(L.tableParams([{ id: "P_RPM" }], def, ["a"]), [{ id: "P_RPM" }]);
  assert.deepEqual(L.tableParams(null, def, ["b"]), [{ id: "b" }]);
  assert.deepEqual(L.tableParams([], def, []), def);
});

// ---- the panel on the DOM stub ------------------------------------------------------

function preview() {
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  sandbox.__PREVIEW__ = { vcar: { profile, files: {} }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const files = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of files) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return sandbox.__PREVIEW_FIXTURES__;
}

const PACK = {
  id: "demo", name: "Demo", version: "1.0.0", dir: "/p/demo", source: "installed", loaded: 1, valid: true, notes: [],
  defs: [{ kind: "loggers", file: "loggers/demo.json", path: "/p/demo/loggers/demo.json", id: "demo", name: "Demo" }],
};

// The panel against the preview fixtures; `ticks` (optional) replaces
// log_tick with scripted replies. `has` is what the adapter can do.
function panel({ ticks = null, has = () => true } = {}) {
  const F = preview();
  const calls = [];
  const seen = []; // the link pill at each log_tick call (after the previous reply)
  const invoke = async (cmd, args) => {
    calls.push([cmd, args]);
    if (cmd === "packs_list") return { root: "/p", packs: [JSON.parse(JSON.stringify(PACK))], failed: [] };
    if (cmd === "log_tick" && ticks) {
      const pill = w.document.getElementById("logLink");
      seen.push(pill.hidden ? "" : pill.textContent);
      if (!ticks.length) throw "the live session has ended: scripted";
      return ticks.shift();
    }
    return F[cmd](args || {});
  };
  const w = makeWindow({ strictIds: true, tauri: { core: { invoke }, dialog: { open: async () => null, save: async () => null } } });
  const { ctx, window, document } = w;
  for (const id of ["nav", "main"]) {
    const el = document.createElement(id === "main" ? "main" : "div");
    el.id = id;
    document.body.appendChild(el);
  }
  const timers = [];
  window.setInterval = (fn) => { timers.push(fn); return timers.length; };
  window.clearInterval = () => { timers.length = 0; };
  window.OTNav = { register() {} };
  window.OT = { gate() { return { refresh() {} }; }, onCaps() {}, has, connArgs: () => ({ driver: "kkl", port: null, baud: null, bus: null, txId: null, rxId: null }) };
  for (const s of ["util.js", "pack-picker.js", "livelog-core.js", "driveview-core.js", "feat_logging.js"]) load(ctx, s);
  const drive = [];
  window.OTDrive.feed.on((ev) => drive.push(ev));
  const $ = (id) => document.getElementById(id);
  return { window, document, $, calls, seen, drive, timers, F };
}

async function chooseDef(p) {
  await tick();
  const sec = p.$("panel-logging");
  const pack = sec.querySelector('[data-pp="pack"]');
  pack.value = PACK.dir;
  pack.dispatch("change");
  const def = sec.querySelector('[data-pp="def"]');
  def.value = PACK.defs[0].path;
  def.dispatch("change");
  p.$("logLoad").click();
  await tick();
}

test("panel: SSM on the simulator — offline, identification and left-out reasons shown", async () => {
  const p = panel({ has: () => false }); // no adapter connected
  await chooseDef(p);
  assert.equal(p.$("logSsmOpts").hidden, true, "SSM options hidden for UDS");
  assert.equal(p.$("logStart").disabled, true, "UDS needs an adapter");
  p.$("logTransport").value = "ssm";
  p.$("logTransport").dispatch("change");
  assert.equal(p.$("logSsmOpts").hidden, false);
  assert.equal(p.$("logFast").disabled, true, "Fast (0x2C) is UDS only");
  assert.equal(p.$("logStart").disabled, true, "SSM over K-line needs a K-line adapter");
  p.$("logSsmSim").checked = true;
  p.$("logSsmSim").dispatch("change");
  assert.equal(p.$("logStart").disabled, false, "the simulator runs offline");
  p.$("logStart").click();
  await sleep(650); // the preview's start takes 500 ms
  await tick();
  const start = p.calls.find(([c]) => c === "log_start");
  assert.ok(start, "log_start invoked");
  assert.equal(start[1].transport, "ssm");
  assert.equal(start[1].ssmTarget, "ecu");
  assert.equal(start[1].driver, "ssmsim");
  assert.equal(start[1].fast, false);
  assert.equal(start[1].reconnect, false);
  const info = p.$("logSsmInfo");
  assert.equal(info.hidden, false);
  assert.match(info.innerHTML, /ROM ID 5A5A000001/);
  assert.match(info.innerHTML, /Left out \(3 reason/);
  assert.match(info.innerHTML, /capability bit clear\): P_NOPE/);
  assert.equal(p.$("logTransport").disabled, true, "the transport is fixed while running");
  // One tick: SSM values reach the Drive view as a frame.
  await p.timers[0]();
  await tick();
  const frame = p.drive.find((e) => e.type === "frame");
  assert.ok(frame && frame.values.some((v) => v.key === "P_RPM" && typeof v.value === "number"), JSON.stringify(p.drive));
  p.$("logStop").click();
  await tick();
  assert.ok(p.calls.some(([c]) => c === "log_stop"));
  assert.equal(p.$("logTransport").disabled, false);
});

test("panel: a dropped link — pill, CSV markers and the Drive view follow it", async () => {
  const base = { status: { mode: "polling", reason: "fast mode not requested", dynamic_dids: [], polled: [], notes: [], redefines: 0, ticks: 1, setup_requests: 0, tick_requests: 1 }, running: true, ended: null, rate: null };
  const ticks = [
    { ...base, ts: 1, samples: [chan("rpm", 800)], link: link() },
    { ...base, ts: 2, samples: [{ ...chan("rpm", null), note: "timed out" }], link: link({ state: "silent", dead_ticks: 1, error: "timed out" }) },
    { ...base, ts: 3, samples: [{ ...chan("rpm", null), note: "timed out" }, marker("lost", "link lost after 5 dead round(s): timed out")], link: link({ state: "lost", drops: 1, error: "timed out" }) },
    { ...base, ts: 4, samples: [marker("reopen-failed", "reopen attempt 1 failed: no such device; retrying in 1.0 s")], link: link({ state: "reconnecting", attempt: 1, retry_in_ms: 1000, error: "no such device" }) },
    { ...base, ts: 5, samples: [marker("reconnected", "reconnected after 2.5 s (attempt 2)"), chan("rpm", 820)], link: link({ reconnects: 1, drops: 1 }) },
    { ...base, ts: 6, samples: [chan("rpm", 830)], link: link({ reconnects: 1, drops: 1 }) },
  ];
  const p = panel({ ticks });
  await chooseDef(p);
  p.$("logReconnect").checked = true;
  p.$("logStart").click();
  await sleep(650);
  await tick();
  const start = p.calls.find(([c]) => c === "log_start");
  assert.ok(start, "Reconnect alone opens a session");
  assert.equal(start[1].reconnect, true);
  assert.equal(start[1].transport, "uds");
  // Start fired the first tick; six more run the script out.
  for (let i = 0; i < 6; i++) { await p.timers[0](); await tick(); }
  // The pill as each tick was applied (seen at the following call).
  assert.deepEqual(p.seen.slice(1), ["", "no answer 1/5", "link lost — reconnecting…", "reconnecting (attempt 1)", "reconnected", ""]);
  // The Drive view: frames while up, errors while down, frames again.
  const kinds = p.drive.filter((e) => e.type !== "stop").map((e) => e.type);
  assert.deepEqual(kinds.slice(0, 6), ["frame", "error", "error", "error", "frame", "frame"]);
  assert.ok(p.drive.slice(0, 6).filter((e) => e.type === "error").every((e) => e.busy === false));
  assert.match(p.drive[2].message, /link lost/);
  // The script ran out: the session ended and the panel stopped itself.
  await tick();
  assert.ok(p.calls.some(([c]) => c === "log_stop"));
  assert.ok(p.drive.some((e) => e.type === "stop"));
  assert.equal(p.$("logSaveCsv").disabled, false, "rows (markers included) to save");
});
