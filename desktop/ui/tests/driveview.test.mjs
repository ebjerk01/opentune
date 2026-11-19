// Drive view (src/driveview-core.js + src/feat_driveview.js): unit-aware
// formatting, channel choice from a log header, thresholds, stale / lost
// states, peak hold, the replay clock and sparklines; then the panel on the
// DOM stub against the preview backend (replay of the synthetic drive, live
// frames from the Dashboard feed). Synthetic data only.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";
// driveview-core.js is a classic script: importing it attaches globalThis.OTDrive.
import "../src/driveview-core.js";

const D = globalThis.OTDrive;
const { makeWindow, load } = dom;
const DESKTOP = path.join(dom.UI, "..");
const REPO = path.join(DESKTOP, "..");
const PREVIEW = path.join(DESKTOP, "preview");
const tick = (ms = 300) => new Promise((r) => setTimeout(r, ms));

// The Dashboard's channel table rows the tests need (as obd_pid_table sends them).
const TABLE = [
  { key: "rpm", name: "Engine speed", unit: "rpm", min: 0, max: 8000, warn: null, crit: null, pid: 12 },
  { key: "speed", name: "Vehicle speed", unit: "km/h", min: 0, max: 255, warn: null, crit: null, pid: 13 },
  { key: "coolant_temp", name: "Coolant temperature", unit: "°C", min: -40, max: 130, warn: 105, crit: 115, pid: 5 },
  { key: "intake_air_temp", name: "Intake air temperature", unit: "°C", min: -40, max: 100, warn: 60, crit: 75, pid: 15 },
  { key: "boost_kpa", name: "Boost pressure (MAP − baro)", unit: "kPa", min: -100, max: 250, warn: null, crit: null, pid: 11, derived: true },
  { key: "engine_load", name: "Calculated engine load", unit: "%", min: 0, max: 100, warn: null, crit: null, pid: 4 },
];
const ch = (name, unit, key) => ({ key: key || name, name, unit });

// ---- formatting ------------------------------------------------------------------

test("units: spellings normalise, kPa follows the bar/kPa switch, nothing else converts", () => {
  assert.equal(D.normUnit("[1/min]"), "rpm");
  assert.equal(D.normUnit(" °C "), "degc");
  assert.equal(D.normUnit("C"), "degc");
  assert.equal(D.normUnit("KPH"), "km/h");
  assert.equal(D.normUnit(""), "");
  assert.deepEqual(D.fmtValue(152.4, "kPa", "bar"), { text: "1.52", unit: "bar" });
  assert.deepEqual(D.fmtValue(152.4, "kPa", "kPa"), { text: "152", unit: "kPa" });
  assert.deepEqual(D.fmtValue(1.5, "bar", "kPa"), { text: "150", unit: "kPa" });
  assert.deepEqual(D.fmtValue(88.6, "°F", "bar"), { text: "89", unit: "°F" }, "°F stays °F");
  assert.deepEqual(D.fmtValue(3245.7, "rpm", "bar"), { text: "3246", unit: "rpm" });
  assert.deepEqual(D.fmtValue(14.06, "V", "bar"), { text: "14.1", unit: "V" });
  assert.deepEqual(D.fmtValue(0.987, "λ", "bar"), { text: "0.99", unit: "λ" });
  assert.equal(D.fmtValue(3.14159, "", "bar").text, "3.14", "unknown unit: by magnitude");
  assert.equal(D.fmtValue(123.4, "", "bar").text, "123");
});

test("formatting: no data is a dash, never -0, signed values get a plus", () => {
  assert.equal(D.fmtNum(null, 0), "—");
  assert.equal(D.fmtNum(NaN, 1), "—");
  assert.equal(D.fmtNum(Infinity, 1), "—");
  assert.equal(D.fmtNum(-0.2, 0), "0");
  assert.equal(D.fmtNum(-0.04, 1), "0.0");
  assert.equal(D.fmtNum(-3.6, 0), "-4");
  assert.equal(D.fmtNum(2.6, 0, true), "+3");
  assert.equal(D.fmtNum(0.2, 0, true), "0", "a value that rounds to zero has no sign");
  assert.equal(D.fmtValue(undefined, "kPa", "bar").unit, "bar", "the unit is shown even without a value");
});

// ---- channel choice --------------------------------------------------------------

test("default channels from a Dashboard recording / Live-log header", () => {
  // Names as the Log viewer reads a Dashboard recording (one channel per param).
  const chans = [
    ch("Calculated engine load", "%"), ch("Coolant temperature", "°C"), ch("Intake air temperature", "°C"),
    ch("Intake manifold pressure", "kPa"), ch("Engine speed", "rpm"), ch("Vehicle speed", "km/h"),
    ch("Boost pressure (MAP − baro)", "kPa"), ch("Exhaust gas temperature bank 1 S1", "°C"),
    ch("GPS speed over ground", "km/h"), ch("Speed error (vehicle − GPS)", "km/h"), ch("GPS latitude", "deg"),
  ];
  assert.deepEqual(D.pickDefault(chans), [
    "Engine speed", "Vehicle speed", "Coolant temperature", "Intake air temperature",
    "Boost pressure (MAP − baro)", "Calculated engine load", "GPS speed over ground", "Speed error (vehicle − GPS)",
  ]);
});

test("default channels from a wide CSV header: units decide, commanded values are skipped", () => {
  const chans = [
    ch("Time", "s"), ch("Commanded boost", "kPa"), ch("Boost pressure", "kPa"), ch("Engine speed", "1/min"),
    ch("Speed", "mph"), ch("Engine load target", "%"), ch("ECT", "°C"),
  ];
  assert.deepEqual(D.pickDefault(chans), ["Engine speed", "Speed", "ECT", "Boost pressure"]);
  // A "speed" in the wrong unit is not the vehicle speed; GPS speed never is.
  assert.deepEqual(D.pickDefault([ch("Speed", "rpm"), ch("GPS speed", "km/h")]), ["GPS speed"]);
  assert.deepEqual(D.pickDefault([]), []);
});

test("default channels by id first (live keys), and saved selections", () => {
  const live = TABLE.map((t) => ({ key: t.key, name: t.name, unit: t.unit })).concat([
    { key: "gps_speed_kmh", name: "GPS speed over ground", unit: "km/h" },
  ]);
  assert.deepEqual(D.pickDefault(live), ["rpm", "speed", "coolant_temp", "intake_air_temp", "boost_kpa", "engine_load", "gps_speed_kmh"]);
  // A Live-log definition's param ids
  assert.deepEqual(D.pickDefault([ch("Motor", "rpm", "n_engine"), ch("Fahrzeug", "km/h", "vss")]), ["n_engine", "vss"]);
  assert.deepEqual(D.selectionFor(live, ["speed", "gone", "rpm"]), ["speed", "rpm"]);
  assert.deepEqual(D.selectionFor(live, ["gone"]), D.pickDefault(live), "nothing left: the default");
  assert.equal(D.roleOf(ch("Speed error (vehicle − GPS)", "km/h")).signed, true);
  assert.equal(D.roleOf(ch("Throttle", "%")), null);
});

// ---- thresholds ------------------------------------------------------------------

test("thresholds come from the Dashboard table only when the units agree", () => {
  const byKey = D.thresholdsFor({ key: "coolant_temp", name: "x", unit: "°C" }, TABLE);
  assert.equal(byKey.warn, 105);
  assert.equal(byKey.crit, 115);
  const byName = D.thresholdsFor(ch("Coolant temperature", "C"), TABLE);
  assert.equal(byName.warn, 105, "C = °C");
  const byRole = D.thresholdsFor(ch("ECT", "degC"), TABLE);
  assert.equal(byRole.crit, 115);
  assert.equal(byRole.min, -40);
  const fahrenheit = D.thresholdsFor(ch("Coolant temperature", "°F"), TABLE);
  assert.equal(fahrenheit.warn, null, "never compared across units");
  assert.equal(fahrenheit.unitMismatch, true);
  assert.equal(D.thresholdsFor(ch("Coolant temperature", ""), TABLE).warn, null, "unknown unit: no colour");
  assert.equal(D.thresholdsFor(ch("Mystery", "%"), TABLE).row, null);
  assert.equal(D.level(104.9, byKey), "");
  assert.equal(D.level(105, byKey), "warn");
  assert.equal(D.level(120, byKey), "crit");
  assert.equal(D.level(null, byKey), "");
  assert.deepEqual(D.barRange(byKey, null), { lo: -40, hi: 130 });
  const p = D.peakAdd(D.peakAdd(D.peakNew(), 40, 0), 90, 1);
  assert.deepEqual(D.barRange({ min: null, max: null }, p), { lo: 0, hi: 90 });
  assert.equal(D.barRange({}, D.peakNew()), null);
  assert.equal(D.frac(45, { lo: 0, hi: 90 }), 0.5);
  assert.equal(D.frac(200, { lo: 0, hi: 90 }), 1);
  assert.equal(D.frac(null, { lo: 0, hi: 90 }), null);
});

// ---- stale / link ----------------------------------------------------------------

test("stale detection: a dropped link never shows a frozen number as live", () => {
  assert.equal(D.staleAfter(0.25), 2.5, "never below 2.5 s");
  assert.equal(D.staleAfter(2), 6);
  assert.equal(D.staleAfter(undefined), 2.5);
  const staleS = 2.5;
  assert.equal(D.tileState({ link: "up", lastT: 9, now: 10, staleS }), "live");
  assert.equal(D.tileState({ link: "up", lastT: 7, now: 10, staleS }), "stale");
  assert.equal(D.tileState({ link: "up", lastT: null, now: 10, staleS }), "nodata");
  assert.equal(D.tileState({ link: "lost", lastT: 9.9, now: 10, staleS }), "lost", "fresh value, but the link is down");
  assert.equal(D.tileState({ link: "stopped", lastT: 9.9, now: 10, staleS }), "lost");
  assert.equal(D.tileState({ link: "waiting", lastT: null, now: 10, staleS }), "nodata");
  assert.equal(D.tileState({ link: "replay", lastT: 4, now: 10, staleS }), "stale");
  assert.equal(D.linkState({ now: 10, lastFrameT: null, staleS }), "waiting");
  assert.equal(D.linkState({ now: 10, lastFrameT: 9, staleS }), "up");
  assert.equal(D.linkState({ now: 10, lastFrameT: 7, staleS }), "lost", "no frame for too long");
  assert.equal(D.linkState({ now: 10, lastFrameT: 9.9, staleS, failing: true }), "lost");
  assert.equal(D.linkState({ now: 10, lastFrameT: 9.9, staleS, stopped: true }), "stopped");
  assert.equal(D.fmtAge(4.7), "4 s ago");
  assert.equal(D.fmtAge(125), "2 min ago");
  assert.equal(D.fmtAge(-1), "");
});

// ---- peaks + history -------------------------------------------------------------

test("peak hold keeps min and max with their times and skips gaps", () => {
  const p = D.peakNew();
  [[50, 0], [null, 1], [NaN, 2], [120, 3], [-5, 4], [80, 5]].forEach(([v, t]) => D.peakAdd(p, v, t));
  assert.deepEqual(p, { min: -5, max: 120, tMin: 4, tMax: 3, n: 4 });
});

test("history keeps the last 60 s and sparkline paths break on gaps", () => {
  const h = D.histNew();
  for (let t = 0; t <= 100; t += 10) D.histPush(h, t, t === 70 ? null : t * 2, 60);
  assert.deepEqual(h.t, [40, 50, 60, 70, 80, 90, 100]);
  assert.equal(h.v[3], null);
  const d = D.sparkPath(h.t, h.v, 40, 100, 100, 30);
  assert.equal((d.match(/M/g) || []).length, 2, "the gap starts a new segment");
  assert.match(d, /^M0\.0 29\.0/, "lowest value at the bottom-left");
  assert.match(d, /L100\.0 1\.0$/, "highest value at the top-right");
  assert.equal(D.sparkPath([0, 1], [5, 5], 0, 1, 100, 30), "M0.0 15.0L100.0 15.0", "flat line at mid-height");
  assert.equal(D.sparkPath([0, 1], [null, null], 0, 1, 100, 30), "");
  assert.equal(D.sparkPath([0, 1], [1, 2], 5, 6, 100, 30), "", "nothing in the window");
});

// ---- replay ----------------------------------------------------------------------

// A log_replay result: 10 Hz, rpm with a one-sample spike and a 4 s gap.
function replayResult() {
  const t = [], rpm = [], cool = [];
  for (let i = 0; i <= 200; i++) {
    t.push(i / 10);
    rpm.push(i >= 50 && i < 90 ? null : i === 123 ? 6000 : 800 + i);
    cool.push(i % 2 ? null : 80);
  }
  return { rows: 201, stride: 1, t, series: [{ name: "Engine speed", unit: "rpm", t: [], v: rpm }, { name: "Coolant", unit: "°C", t: [], v: cool }], time_label: "s", median_dt: 0.1 };
}

test("replay: value at a time is the latest valid sample, stale across a gap", () => {
  const d = D.replayPrepare(replayResult());
  assert.equal(d.t0, 0);
  assert.equal(d.t1, 20);
  assert.equal(d.rate, 1);
  assert.equal(d.index, false);
  const rpm = d.byKey["Engine speed"];
  assert.deepEqual(D.replaySample(d, rpm, 1.04), { v: 810, at: 1 });
  assert.deepEqual(D.replaySample(d, rpm, -1), { v: null, at: null }, "before the log");
  const inGap = D.replaySample(d, rpm, 8.5);
  assert.deepEqual(inGap, { v: 849, at: 4.9 });
  const staleS = D.staleAfter(d.dt);
  assert.equal(D.tileState({ link: "replay", lastT: inGap.at, now: 8.5, staleS }), "stale");
  assert.equal(D.tileState({ link: "replay", lastT: inGap.at, now: 6, staleS }), "live", "a short gap is not stale yet");
  const cool = d.byKey.Coolant;
  assert.deepEqual(D.replaySample(d, cool, 0.15), { v: 80, at: 0 }, "every other sample missing: still live");
  assert.equal(D.idxAt([0, 1, 2], 1.5), 1);
  assert.equal(D.idxAt([0, 1, 2], 2), 2);
  assert.equal(D.idxAt([], 2), -1);
});

test("replay: peaks fold every played sample, so a spike is never skipped at 8x", () => {
  const d = D.replayPrepare(replayResult());
  const rpm = d.byKey["Engine speed"];
  const c = D.clockNew(d.t0, d.t1, d.rate);
  D.clockSpeed(c, 8, 0);
  D.clockPlay(c, 0);
  const p = D.peakNew();
  // One frame per second of wall time at 8x = 8 s of log per frame.
  for (let wall = 1000; wall <= 3000; wall += 1000) {
    const s = D.clockTick(c, wall);
    D.replayFold(d, rpm, p, s.from, s.to);
  }
  assert.equal(p.max, 6000, "the one-sample spike at 12.3 s is caught");
  assert.equal(p.tMax, 12.3);
  assert.equal(p.min, 801, "the sample at t0 itself belongs to the seek, not the fold");
  const w = D.replayWindow(d, rpm, 0, 20, 21);
  assert.ok(w.t.length <= 22);
  assert.equal(w.t[w.t.length - 1], 20, "the window always ends on the current sample");
  assert.deepEqual(D.replayWindow(d, rpm, -10, -5, 10), { t: [], v: [] });
});

test("replay clock: speeds, pause, seek, end of log and index-time logs", () => {
  const c = D.clockNew(10, 70, 1);
  assert.equal(D.clockPos(c, 5000), 10, "paused at the start");
  D.clockPlay(c, 0);
  assert.equal(D.clockPos(c, 2000), 12);
  D.clockSpeed(c, 4, 2000);
  assert.equal(D.clockPos(c, 3000), 16);
  D.clockSpeed(c, 3, 3000);
  assert.equal(c.speed, 1, "only 1/2/4/8x");
  D.clockPause(c, 4000);
  assert.equal(c.pos, 17);
  assert.equal(D.clockPos(c, 9000), 17, "paused stays put");
  D.clockSeek(c, 500, 9000);
  assert.equal(c.pos, 70, "seek clamps to the log");
  D.clockSeek(c, -5, 9000);
  assert.equal(c.pos, 10);
  D.clockSpeed(c, 8, 9000);
  D.clockPlay(c, 9000);
  let s = D.clockTick(c, 14000);
  assert.deepEqual(s, { from: 10, to: 50, ended: false });
  s = D.clockTick(c, 99000);
  assert.deepEqual(s, { from: 50, to: 70, ended: true });
  assert.equal(c.playing, false, "stops at the end");
  D.clockPlay(c, 100000);
  assert.equal(c.pos, 10, "play at the end starts over");
  // A log without a time column: positions are samples, INDEX_HZ per second.
  const d = D.replayPrepare({ t: [0, 1, 2, 3], series: [], time_label: "samples", median_dt: 1 });
  assert.equal(d.index, true);
  const ci = D.clockNew(d.t0, d.t1, d.rate);
  D.clockPlay(ci, 0);
  assert.equal(D.clockPos(ci, 400), 2, `${D.INDEX_HZ} samples per second`);
  assert.equal(D.fmtClock(75), "1:15");
  assert.equal(D.fmtClock(3725), "1:02:05");
  assert.equal(D.fmtClock(12, true), "#12");
  assert.equal(D.fmtClock(null), "—");
});

// ---- live frames, feed, keys ------------------------------------------------------------

test("live frames from a Dashboard poll and Live-log samples", () => {
  const byKey = Object.fromEntries(TABLE.concat([{ key: "egt_b1", name: "EGT bank 1", unit: "°C" }]).map((t) => [t.key, t]));
  const f = D.dashFrame({ rpm: 812.5, speed: null, egt_b1: [420, null, 433, null], vin: "WVWZZZ00000000000", coolant_temp: NaN }, byKey);
  assert.deepEqual(f, [
    { key: "rpm", name: "Engine speed", unit: "rpm", value: 812.5 },
    { key: "speed", name: "Vehicle speed", unit: "km/h", value: null },
    { key: "egt_b1#0", name: "EGT bank 1 S1", unit: "°C", value: 420 },
    { key: "egt_b1#2", name: "EGT bank 1 S3", unit: "°C", value: 433 },
    { key: "coolant_temp", name: "Coolant temperature", unit: "°C", value: null },
  ]);
  const s = D.samplesFrame([
    { paramId: "vss", name: "Vehicle speed", value: 51, units: "km/h" },
    { paramId: "id_sw", name: "Software", value: null, units: "", text: "1037" },
    { paramId: "gps_speed_kmh", name: "GPS speed over ground", value: "x", units: "km/h" },
  ]);
  assert.deepEqual(s, [
    { key: "vss", name: "Vehicle speed", unit: "km/h", value: 51 },
    { key: "gps_speed_kmh", name: "GPS speed over ground", unit: "km/h", value: null },
  ]);
});

test("the feed delivers to every subscriber even when one throws", () => {
  const feed = D.makeFeed();
  const got = [];
  const off = feed.on(() => { throw new Error("broken view"); });
  feed.on((ev) => got.push(ev.type));
  feed.emit({ type: "frame" });
  off();
  assert.equal(feed.size, 1);
  feed.emit({ type: "stop" });
  assert.deepEqual(got, ["frame", "stop"]);
  assert.equal(typeof D.feed.emit, "function", "one app-wide feed");
});

test("keys: F, R, Space, Escape; not while typing or with modifiers", () => {
  const k = (key, target, mods) => D.keyAction(Object.assign({ key, target: target || { tagName: "BODY" } }, mods));
  assert.equal(k("f"), "fullscreen");
  assert.equal(k("F"), "fullscreen");
  assert.equal(k("r"), "reset");
  assert.equal(k(" "), "pause");
  assert.equal(k("Escape"), "exit");
  assert.equal(k("x"), null);
  assert.equal(k("f", { tagName: "INPUT", type: "text" }), null);
  assert.equal(k("r", { tagName: "SELECT" }), null);
  assert.equal(k("f", { tagName: "DIV", isContentEditable: true }), null);
  assert.equal(k("f", undefined, { ctrlKey: true }), null);
  assert.equal(k("f", { tagName: "INPUT", type: "range" }), "fullscreen", "the scrub bar keeps F/R");
  assert.equal(k(" ", { tagName: "BUTTON" }), null, "Space activates a focused button instead");
  assert.equal(k("r", { tagName: "BUTTON" }), "reset");
});

// ---- the panel on the DOM stub ------------------------------------------------------------

function preview() {
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  sandbox.__PREVIEW__ = { vcar: { profile, files: {} }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const files = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of files) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, tauri: sandbox.__TAURI__ };
}

function panel(channel) {
  const { F, tauri } = preview();
  const calls = [];
  const stub = {
    core: { invoke: async (cmd, args) => { calls.push([cmd, args]); if (cmd === "build_channel") return channel; return F[cmd](args || {}); } },
    dialog: tauri.dialog,
  };
  const { ctx, window, document } = makeWindow({ tauri: stub });
  const nav = document.createElement("nav");
  nav.id = "nav";
  nav.innerHTML = ["calibration", "vehicle", "reference", "help", "bench"].map((g) => `<div class="nav-group" data-group="${g}"></div>`).join("");
  document.body.appendChild(nav);
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  window.showPanel = () => {};
  load(ctx, "util.js");
  load(ctx, "nav.js");
  load(ctx, "driveview-core.js");
  load(ctx, "feat_driveview.js");
  const p = document.getElementById("panel-driveview");
  const tiles = () => p.querySelectorAll(".dv-tile");
  return { window, document, p, $: (id) => p.querySelector("#" + id), calls, tiles };
}

test("panel: public nav entry; live mode only on Bench builds", async () => {
  const stable = panel("stable");
  const btn = stable.document.querySelector('#nav .nav-item[data-panel="driveview"]');
  assert.equal(btn.dataset.tier, "public");
  assert.equal(btn.parentElement.dataset.group, "calibration", "next to the Log viewer");
  await tick();
  assert.equal(btn.hidden, false, "shown on stable builds");
  assert.equal(stable.$("dvModeLive").hidden, true, "no live mode on stable");
  assert.equal(stable.$("dvMode").hidden, true);
  assert.ok(stable.calls.some(([c]) => c === "obd_pid_table"), "thresholds from the offline channel table");
  const bench = panel("bench");
  await tick();
  assert.equal(bench.$("dvModeLive").hidden, false, "live on bench builds");
});

test("panel: replays the preview's synthetic drive with the default tiles", async () => {
  const { window, $, tiles, calls } = panel("stable");
  await tick();
  await window.OTDriveView.openLog("demo/preview-drive-60s.csv");
  assert.ok(calls.some(([c]) => c === "log_open") && calls.some(([c]) => c === "log_replay"));
  const keys = tiles().map((t) => t.dataset.k);
  assert.deepEqual(keys, ["Engine speed", "Vehicle speed", "Coolant temperature", "Boost pressure", "Engine load"]);
  assert.ok(tiles()[0].classList.contains("dv-hero") && !tiles()[2].classList.contains("dv-hero"), "the first two are large");
  assert.match($("dvNote").textContent, /not in this log: Intake air/);
  assert.equal($("dvTransport").hidden, false);
  const rpm = tiles()[0];
  assert.equal(rpm.dataset.state, "live");
  assert.match(rpm.querySelector(".dv-val").textContent, /^\d{3,4}$/);
  assert.equal(rpm.querySelector(".dv-unit").textContent, "rpm");
  assert.equal($("dvTime").textContent, "0:00 / 1:00");
  // Scrub to the end: the clock follows, values stay live.
  const scrub = $("dvScrub");
  scrub.value = "1000";
  scrub.dispatch("input");
  assert.equal($("dvTime").textContent, "1:00 / 1:00");
  assert.equal(tiles()[1].dataset.state, "live");
  assert.notEqual(tiles()[0].querySelector(".dv-max").textContent, "max —");
  // Space plays (replay), R clears min / max to the value now shown.
  $("dvPlay").click();
  assert.equal($("dvPlay").textContent, "❚❚");
  $("dvPlay").click();
  assert.equal($("dvPlay").textContent, "▶");
  // Pressure unit: boost (kPa in the log) shows in bar.
  const boost = tiles()[3];
  assert.equal(boost.querySelector(".dv-unit").textContent, "bar");
  assert.match(boost.querySelector(".dv-val").textContent, /^\d\.\d\d$/);
});

test("panel: live frames from the Dashboard feed, then a lost link greys the tiles", async () => {
  const { window, $, tiles } = panel("bench");
  await tick();
  window.OTDriveView.showLive();
  const feed = window.OTDrive.feed;
  feed.emit({ type: "frame", source: "dashboard", tMs: Date.now(), values: [
    { key: "rpm", name: "Engine speed", unit: "rpm", value: 2400 },
    { key: "coolant_temp", name: "Coolant temperature", unit: "°C", value: 117 },
    { key: "speed", name: "Vehicle speed", unit: "km/h", value: null },
  ] });
  window.OTDriveView.refresh();
  const byKey = () => Object.fromEntries(tiles().map((t) => [t.dataset.k, t]));
  assert.deepEqual(tiles().slice(0, 3).map((t) => t.dataset.k), ["rpm", "speed", "coolant_temp"]);
  assert.equal($("dvStatus").dataset.s, "up");
  assert.equal(byKey().rpm.querySelector(".dv-val").textContent, "2400");
  assert.equal(byKey().rpm.dataset.state, "live");
  assert.equal(byKey().coolant_temp.dataset.level, "crit", "117 °C is past the critical threshold");
  assert.equal(byKey().speed.dataset.state, "nodata");
  assert.equal(byKey().gps_speed_kmh.hidden, true, "GPS tiles stay hidden without a GPS source");
  // Two failed polls: the link is lost and no tile shows a number.
  feed.emit({ type: "error", source: "dashboard", message: "timeout", busy: false });
  feed.emit({ type: "error", source: "dashboard", message: "timeout", busy: false });
  window.OTDriveView.refresh();
  assert.equal($("dvStatus").dataset.s, "lost");
  assert.equal(byKey().rpm.dataset.state, "lost");
  assert.equal(byKey().rpm.querySelector(".dv-val").textContent, "—");
  assert.match(byKey().rpm.querySelector(".dv-sub").textContent, /last 2400 rpm/);
  assert.equal(byKey().coolant_temp.dataset.level, "", "no alarm colour on a value that is not live");
  // A fresh frame brings it back.
  feed.emit({ type: "frame", source: "dashboard", tMs: Date.now(), values: [{ key: "rpm", name: "Engine speed", unit: "rpm", value: 2500 }] });
  window.OTDriveView.refresh();
  assert.equal($("dvStatus").dataset.s, "up");
  assert.equal(byKey().rpm.querySelector(".dv-val").textContent, "2500");
  // The source stopping is shown, not hidden.
  feed.emit({ type: "stop", source: "dashboard" });
  window.OTDriveView.refresh();
  assert.equal($("dvStatus").dataset.s, "stopped");
  assert.equal(byKey().rpm.dataset.state, "lost");
});
