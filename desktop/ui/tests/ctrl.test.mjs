// Controllers panel (src/feat_ctrl.js) and its preview fixtures
// (preview/fixtures-ctrl.js): the preview's DBC bit layout agrees with the
// vectors opentune-ctrldef's signal.rs is tested with, the fixtures list /
// show / decode the repository's ctrldefs, and the panel's list → detail →
// decode flow, its pack-picker wiring and its DSG / Haldex links work on the
// DOM stub. Synthetic data only.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";

const { makeWindow, load } = dom;
const DESKTOP = path.join(dom.UI, "..");
const REPO = path.join(DESKTOP, "..");
const PREVIEW = path.join(DESKTOP, "preview");
// The fixtures answer after short timers (like the app's async commands).
const tick = (ms = 400) => new Promise((r) => setTimeout(r, ms));

function ctrldefs() {
  const dir = path.join(REPO, "ctrldefs");
  return fs.readdirSync(dir).filter((f) => f.endsWith(".json")).sort()
    .map((f) => ({ path: `ctrldefs/${f}`, def: JSON.parse(fs.readFileSync(path.join(dir, f), "utf8")) }));
}

// The preview backend: mock.js plus every fixtures file, as build.py bundles it.
function preview() {
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON, BigInt };
  sandbox.window = sandbox;
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  sandbox.__PREVIEW__ = { vcar: { profile, files: {} }, packs: [], ecudefs: [], guides: [], units: [], ctrldefs: ctrldefs() };
  const ctx = vm.createContext(sandbox);
  const files = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of files) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, tauri: sandbox.__TAURI__, C: sandbox.__PREVIEW_CTRL__ };
}

test("preview DBC layout matches opentune-ctrldef's signal vectors", () => {
  const { C } = preview();
  // signal.rs little_endian_matches_dbc_intel / big_endian_matches_dbc_motorola
  const d = [0, 0, 12000 & 0xff, 12000 >> 8, 0, 0, 0, 0];
  assert.equal(C.extract(d, 16, 16, "little"), 12000n);
  assert.equal(C.extract([(5000 << 1) & 0xff, (5000 << 1) >> 8], 1, 15, "little"), 5000n);
  assert.equal(C.extract([0, 0, 0], 16, 16, "little"), null);
  assert.equal(C.extract([0x12, 0x34, 0, 0, 0, 0, 0, 0], 7, 16, "big"), 0x1234n);
  assert.equal(C.extract([0, 0, 0xa5, 0, 0, 0, 0, 0], 19, 4, "big"), 5n);
  assert.equal(C.extract([0b11100001, 0xff], 4, 13, "big"), 0x1ffn);
  // insert round-trips both orders
  for (const order of ["little", "big"]) {
    const buf = new Array(8).fill(0);
    assert.ok(C.insert(buf, order === "big" ? 12 : 9, 11, order, 1234));
    assert.equal(C.extract(buf, order === "big" ? 12 : 9, 11, order), 1234n);
  }
  // signed + scaled + invalid marker
  const s = { name: "t", start: 0, len: 8, signed: true, factor: 0.5, offset: 0, invalid: [0x80] };
  assert.deepEqual(JSON.parse(JSON.stringify(C.decodeSignal(s, [0xfe]).value)), { type: "number", value: -1 });
  assert.equal(C.decodeSignal(s, [0x80]).value.type, "invalid");
});

test("preview fixtures: list, search, show and a synthetic decode", async () => {
  const { F } = preview();
  const all = await F.ctrl_list({});
  assert.ok(all.defs.length >= 8 && all.total === all.defs.length, `${all.defs.length} defs`);
  assert.ok(all.defs.every((d) => d.source === "builtin" && d.path.startsWith("ctrldefs/")));
  const byPart = await F.ctrl_list({ q: "0AY907554C" });
  assert.ok(byPart.defs.some((d) => d.id === "borgwarner-haldex-gen4"), "identify keys are searched");
  const awd = await F.ctrl_list({ kind: "awd" });
  assert.ok(awd.defs.length && awd.defs.every((d) => d.kind === "awd"));
  const dsg = await F.ctrl_get({ path: "ctrldefs/vag-dq250-02e.json" });
  assert.equal(dsg.related.dsg, true);
  assert.equal(dsg.def.id, "vag-dq250-02e");
  await assert.rejects(F.ctrl_get({ path: "ctrldefs/nope.json" }), /no such file/);
  const r = await F.ctrl_decode_log({ path: "ctrldefs/mercedes-egs5x-7226.json", log: "demo/x.log", bus: "" });
  assert.equal(r.bus, "egs51");
  assert.ok(r.decoded > 0 && r.per_frame.length > 1 && r.rows.length > 0);
  assert.equal(r.frames_in_log, r.decoded + 15, "the stray 0x7FF frames are not decoded");
  assert.ok(r.live.length > 0, "live channels from CAN signals");
  await assert.rejects(F.ctrl_decode_log({ path: "ctrldefs/mercedes-egs5x-7226.json", bus: "egs53" }), /unknown frame set/);
});

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
  nav.innerHTML = ["vehicle", "reference", "bench"].map((g) => `<div class="nav-group" data-group="${g}"></div>`).join("");
  document.body.appendChild(nav);
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  const shown = [];
  window.showPanel = (id) => shown.push(id);
  load(ctx, "nav.js");
  load(ctx, "pack-picker.js");
  // the Haldex panel's own entry (Bench tier), as feat_haldex.js registers it
  window.OTNav.register({ id: "haldex", label: "Haldex AWD", icon: "x", group: "vehicle", tier: "bench" });
  load(ctx, "feat_ctrl.js");
  const p = document.getElementById("panel-controllers");
  return { window, document, p, $: (id) => p.querySelector("#" + id), calls, shown };
}

test("panel: public tier, list, detail and pack picker for controllers", async () => {
  const { $, p, calls, document, window } = panel("stable");
  assert.ok(p && !p.placeholder, "feat_ctrl.js creates #panel-controllers");
  const btn = document.querySelector('#nav .nav-item[data-panel="controllers"]');
  assert.equal(btn.dataset.tier, "public");
  assert.equal(btn.parentElement.dataset.group, "reference");
  await tick();
  assert.equal(btn.hidden, false, "shown on stable builds");
  const rows = $("ctList").querySelectorAll(".db-row");
  assert.ok(rows.length >= 8, `${rows.length} rows`);
  // the first definition is shown; the picker asks for the controllers kind
  assert.ok(calls.some(([c]) => c === "ctrl_get"));
  assert.equal($("ctPicker").querySelector(".pack-picker").dataset.kind, "controllers");
  assert.equal(window.OTPacks.KIND_LABEL.controllers, "controller definition");
  // ctrl_list carries the packs loaded from disk
  assert.ok(Array.isArray(calls.find(([c]) => c === "ctrl_list")[1].extra));

  const row = rows.find((r) => r.dataset.path === "ctrldefs/mercedes-egs5x-7226.json");
  row.click();
  await tick();
  const html = $("ctDetail").innerHTML;
  for (const part of ["CAN set", "egs51", "Live channels", "Gear ratios", "Safety", "Sources", "valid"]) assert.ok(html.includes(part), `detail shows ${part}`);
  assert.equal($("ctDecodeCard").hidden, false, "a definition with frame sets can decode");
  assert.equal($("ctDecode").disabled, true, "until a log is chosen");
  // on a stable build the Bench-tier Haldex panel is hidden: no button to it
  rows.find((r) => r.dataset.path === "ctrldefs/borgwarner-haldex-gen4.json").click();
  await tick();
  assert.match($("ctDetail").innerHTML, /Haldex tools/);
  assert.equal($("ctDetail").querySelector('[data-goto="haldex"]'), null);
});

test("panel: decode a log, DSG and Haldex links on a bench build", async () => {
  const { $, calls, shown } = panel("bench");
  await tick();
  const rows = $("ctList").querySelectorAll(".db-row");
  rows.find((r) => r.dataset.path === "ctrldefs/vag-dq250-02e.json").click();
  await tick();
  assert.match($("ctDetail").innerHTML, /opentune dsg identify/);

  rows.find((r) => r.dataset.path === "ctrldefs/borgwarner-haldex-gen4.json").click();
  await tick();
  const go = $("ctDetail").querySelector('[data-goto="haldex"]');
  assert.ok(go, "bench build: link to the Haldex panel");
  go.click();
  assert.deepEqual(shown, ["haldex"]);

  $("ctPick").click();
  await tick();
  assert.equal($("ctLogPath").textContent, "preview-drivetrain.log");
  assert.equal($("ctDecode").disabled, false);
  $("ctDecode").click();
  await tick();
  const call = calls.filter(([c]) => c === "ctrl_decode_log").pop()[1];
  assert.equal(call.path, "ctrldefs/borgwarner-haldex-gen4.json");
  assert.equal(call.log, "demo/preview-drivetrain.log");
  const out = $("ctDecodeOut");
  assert.match(out.innerHTML, /Per frame/);
  assert.match(out.innerHTML, /0x7FF ×15/);
  assert.ok(out.querySelectorAll("tbody tr").length > 5, "frame rows");
});

test("panel helpers format like the CLI", () => {
  const { window } = panel("stable");
  const C = window.OTCtrl;
  assert.equal(C.fmtNum(12), "12");
  assert.equal(C.fmtNum(0.1 + 0.2), "0.3");
  assert.equal(C.fmtValue({ type: "bool", value: true }), "1");
  assert.equal(C.fmtValue({ type: "enum", value: null }), "?");
  assert.equal(C.fmtValue({ type: "ascii", value: "D" }), "'D'");
  assert.equal(C.fmtValue({ type: "invalid" }), "n/a");
  assert.equal(C.statsText({ samples: 3, invalid: 1, min: 1, max: 2.5, last: { type: "number", value: 2 }, counts: {} }, "bar"), "1 .. 2.5 bar  last 2 (1 n/a)");
  assert.equal(C.statsText({ samples: 3, invalid: 0, counts: { P: 1, D: 2 }, last: { type: "enum", value: "P" } }, ""), "D ×2, P ×1  last P");
  assert.equal(C.sigScale({ factor: 0.25, offset: -40 }), "×0.25 − 40");
  assert.equal(C.sigScale({ curve: [[0, 0], [1, 1]] }), "curve (2 points)");
  assert.equal(C.liveSource({ kind: "kwp_measuring_block", group: 3, cell: 1 }), "KWP 0x21 group 3 cell 1");
});
