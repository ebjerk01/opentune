// Actuator tests panel (src/feat_actuator.js) over its preview fixtures
// (preview/fixtures-actuator.js) and the virtual car's definition pack:
// declared channels only, client-side range / duration checks, the per-run
// confirmation, the timed run with STOP, the automatic stop when the panel is
// left or the adapter disconnects, SecurityAccess, the Bench-tier gate on
// stable builds and the Debug free-form path with its second confirmation.
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
const tick = async (n = 12) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };
const ADP = "packs/vcar-demo/adaptation/";

// The preview backend with the virtual car's adaptation defs bundled, as build.py does.
function preview() {
  const files = {};
  for (const f of fs.readdirSync(path.join(REPO, ADP))) files[ADP + f] = JSON.parse(fs.readFileSync(path.join(REPO, ADP, f), "utf8"));
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  sandbox.__PREVIEW__ = { vcar: { profile, files }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const list = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of list) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, tauri: sandbox.__TAURI__ };
}

async function panel(channel) {
  const { F, tauri } = preview();
  const calls = [];
  const subs = [];
  const stub = {
    core: {
      invoke: async (cmd, args) => {
        calls.push([cmd, args]);
        if (cmd === "build_channel") return channel;
        return F[cmd](args || {});
      },
    },
    dialog: tauri.dialog,
  };
  const { ctx, window, document } = makeWindow({ tauri: stub });
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  window.OT = {
    connArgs: () => ({ driver: "vcar", port: "demo", baud: null, bus: "hs", txId: null, rxId: null }),
    onCaps: (fn) => subs.push(fn),
    gate() {},
  };
  load(ctx, "nav.js");
  load(ctx, "pack-picker.js");
  load(ctx, "feat_actuator.js");
  await tick();
  const p = document.getElementById("panel-actuator");
  const A = window.OTActuator;
  const $ = (id) => p.querySelector("#" + id);
  const named = (cmd) => calls.filter(([c]) => c === cmd).map(([, a]) => a);
  async function list(file) {
    A.state.defPath = ADP + file;
    $("actList").click();
    await tick();
  }
  async function run() {
    $("actAck").checked = true;
    $("actRun").click();
    await tick();
  }
  return { F, window, p, $, A, calls, named, subs, list, run };
}

test("validate: declared option, value range and duration cap", () => {
  const { window } = makeWindow();
  window.OTNav = { register() {}, onChannel() {}, benchEnabled: () => false };
  window.OTPacks = { picker: () => ({ el: window.document.createElement("div") }) };
  const ctx = vm.createContext(window);
  load(ctx, "feat_actuator.js");
  const v = window.OTActuator.validate;
  const fan = { options: ["return_control_to_ecu", "short_term_adjustment"], max_duration_ms: 10000, value: { min: 0, max: 100, unit: "%" } };
  assert.deepEqual(JSON.parse(JSON.stringify(v(fan, "short_term_adjustment", "40", "3", null))), { value: 40, durationMs: 3000 });
  assert.match(v(fan, "short_term_adjustment", "101", "3", null).error, /outside the declared range/);
  assert.match(v(fan, "short_term_adjustment", "", "3", null).error, /Enter a value/);
  assert.match(v(fan, "short_term_adjustment", "50", "10.5", null).error, /limit of 10 s/);
  assert.match(v(fan, "short_term_adjustment", "50", "0", null).error, /duration/);
  assert.match(v(fan, "freeze_current_state", "", "3", null).error, /not declared/);
  assert.equal(v(fan, "return_control_to_ecu", "", "", null).durationMs, 0, "return needs no duration");
  assert.match(v(null, "short_term_adjustment", "", "1", null).error, /Choose/);
  assert.equal(v(null, "freeze_current_state", "", "5", { max: 5000 }).durationMs, 5000);
  assert.match(v(null, "freeze_current_state", "", "6", { max: 5000 }).error, /limit of 5 s/);
});

test("panel: declared channels, plan, timed run and STOP", async () => {
  const { $, A, named, list, run, window } = await panel("bench");
  assert.equal($("actLive").hidden, false, "live controls on bench");
  assert.equal($("actStableNote").hidden, true);
  await list("engine.json");
  assert.equal($("actChan").value, "Radiator fan");
  assert.equal($("actChan").querySelectorAll("option").length, 1, "only actuator channels are listed");
  assert.equal($("actOpt").querySelectorAll("option").length, 4);
  assert.equal($("actOpt").value, "short_term_adjustment");
  assert.equal($("actValueRow").hidden, false);
  assert.equal($("actValueHint").textContent, "0 … 100 %");
  assert.equal($("actSecRow").hidden, true, "the engine needs no SecurityAccess");

  // Out of range: refused before anything is asked of the backend.
  $("actValue").value = "150";
  $("actPlan").click();
  await tick();
  assert.match($("actPlanOut").innerHTML, /outside the declared range/);
  assert.equal(named("actuator_plan").length, 0);
  $("actValue").value = "40";
  $("actDur").value = "3";
  $("actPlan").click();
  await tick();
  assert.deepEqual(JSON.parse(JSON.stringify(named("actuator_plan")[0])), {
    option: "short_term_adjustment", durationMs: 3000, value: 40, defPath: ADP + "engine.json", channel: "Radiator fan",
  });
  assert.equal($("actPlanOut").querySelectorAll("tbody tr").length, 5, "session, control, hold, return, leave");

  // No confirmation: nothing is sent. Cancelled dialog: nothing is sent.
  $("actRun").click();
  await tick();
  assert.match($("actResult").innerHTML, /Tick the confirmation/);
  window.confirm = () => false;
  await run();
  assert.match($("actResult").innerHTML, /Cancelled/);
  assert.equal($("actAck").checked, false, "the confirmation clears after every attempt");
  assert.equal(named("actuator_start").length, 0);

  window.confirm = () => true;
  await run();
  const start = named("actuator_start")[0];
  assert.equal(start.ack, true);
  assert.equal(start.freeformAck, false);
  assert.equal(start.driver, "vcar");
  assert.equal(start.value, 40);
  assert.equal(A.state.running, true);
  assert.equal($("actStop").disabled, false);
  assert.equal($("actRun").disabled, true);
  await A.poll();
  assert.ok($("actLog").querySelectorAll("tbody tr").length >= 2, "session + control logged");

  $("actStop").click();
  await tick();
  assert.equal(named("actuator_stop")[0].reason, "STOP pressed");
  assert.equal(A.state.running, false);
  assert.match($("actResult").innerHTML, /stopped early/);
  assert.match($("actResult").innerHTML, /control returned yes/);
  assert.match($("actResult").innerHTML, /Stopped: STOP pressed/);
  assert.ok($("actLog").querySelectorAll("tbody tr").length >= 4, "return + leave logged");
  assert.equal($("actStop").disabled, true);
});

test("panel: leaving the panel or losing the adapter stops the test", async () => {
  const { $, A, named, list, run, subs, window } = await panel("bench");
  window.confirm = () => true;
  await list("body.json");
  $("actValue").value = "2";
  await run();
  assert.equal(A.state.running, true);
  await A.panelHidden();
  assert.equal(named("actuator_stop").pop().reason, "panel closed");
  assert.equal(A.state.running, false);
  await run();
  assert.equal(A.state.running, true);
  subs.forEach((fn) => fn(null, null));
  await tick();
  assert.equal(named("actuator_stop").pop().reason, "adapter disconnected");
  assert.equal(A.state.running, false);
});

test("panel: a secured module needs a seed/key id", async () => {
  const { $, A, named, list, run, window } = await panel("bench");
  window.confirm = () => true;
  await list("airbag.json");
  assert.equal($("actSecRow").hidden, false);
  assert.match($("actDefInfo").innerHTML, /SecurityAccess level 0x01/);
  assert.equal($("actValueRow").hidden, true, "no adjustable value on this channel");
  await run();
  assert.match($("actResult").innerHTML, /SecurityAccess/);
  assert.equal(A.state.running, false);
  $("actSeedkey").value = "vcar-test";
  await run();
  assert.equal(named("actuator_start").pop().seedkey, "vcar-test");
  assert.equal(A.state.running, true);
  await A.stopNow("test over");
});

test("stable build: Plan only, no live controls, no free-form", async () => {
  const { $, named, list } = await panel("stable");
  assert.equal($("actLive").hidden, true);
  assert.equal($("actStableNote").hidden, false);
  assert.equal($("actFfCard").hidden, true);
  await list("engine.json");
  $("actValue").value = "10";
  $("actPlan").click();
  await tick();
  assert.equal(named("actuator_plan").length, 1);
  assert.equal(named("actuator_start").length, 0);
});

test("Debug free-form DID needs the second confirmation", async () => {
  const { $, A, named, run, window } = await panel("alpha");
  window.confirm = () => true;
  assert.equal($("actFfCard").hidden, false);
  $("actFfOn").checked = true;
  $("actFfDid").value = "0500";
  $("actFfState").value = "01";
  $("actFfOn").dispatch("change");
  assert.equal($("actOpt").querySelectorAll("option").length, 4);
  $("actOpt").value = "short_term_adjustment";
  $("actDur").value = "2";
  await run();
  assert.match($("actResult").innerHTML, /second confirmation/);
  assert.equal(named("actuator_start").length, 0);
  $("actFfAck").checked = true;
  window.OT.connArgs = () => ({ driver: "vcar", port: "demo", baud: null, bus: "hs", txId: "7E0", rxId: "7E8" });
  await run();
  const s = named("actuator_start")[0];
  assert.deepEqual(JSON.parse(JSON.stringify([s.freeformDid, s.state, s.freeformAck, s.durationMs])), ["0500", "01", true, 2000]);
  assert.equal(A.state.running, true);
  assert.equal($("actFfAck").checked, false, "the second confirmation is per run too");
  await A.stopNow("test over");
  $("actDur").value = "6";
  await run();
  assert.match($("actResult").innerHTML, /limit of 5 s/);
});

test("preview fixtures mirror the backend gates", () => {
  const { F } = preview();
  const base = { defPath: ADP + "engine.json", channel: "Radiator fan", option: "adjust", durationMs: 1000 };
  assert.throws(() => F.actuator_plan({ ...base, value: 101 }), /declared range/);
  assert.throws(() => F.actuator_plan({ ...base, option: "freeze", durationMs: 99999 }), /limit/);
  assert.throws(() => F.actuator_plan({ ...base, state: "FF" }), /free-form/);
  const p = F.actuator_plan({ ...base, value: 30 });
  assert.equal(p.record, "1E");
  assert.deepEqual(JSON.parse(JSON.stringify(p.steps.map((s) => s.request))), ["10 03", "2F 05 00 03 1E", "3E 00", "2F 05 00 00", "10 01"]);
  const lamp = F.actuator_list({ defPath: ADP + "airbag.json" });
  assert.equal(lamp.requires_security, true);
  assert.deepEqual(JSON.parse(JSON.stringify(lamp.channels[0].options)), ["return_control_to_ecu", "short_term_adjustment", "reset_to_default"]);
});
