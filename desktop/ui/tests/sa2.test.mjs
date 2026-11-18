// SA2 script panel (src/feat_sa2.js) and its preview fixtures
// (preview/fixtures-sa2.js): the preview's JS SA2 VM agrees with the reference
// vectors the Rust crate is checked against, and the panel's container →
// candidate → disassembly → key flow and Bench-tier arming work on the DOM
// stub. Synthetic scripts only.
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

// The preview backend: mock.js plus every fixtures file, as build.py bundles it.
function preview() {
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  sandbox.__PREVIEW__ = { vcar: { profile, files: {} }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const files = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of files) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, tauri: sandbox.__TAURI__, SA2: sandbox.__PREVIEW_SA2__ };
}

test("preview SA2 VM matches the reference vectors", () => {
  const { SA2 } = preview();
  const text = fs.readFileSync(path.join(REPO, "crates", "opentune-sa2", "tests", "fixtures", "sa2_vectors.txt"), "utf8");
  let n = 0, errs = 0;
  for (const line of text.split("\n")) {
    const [body, tag = ""] = line.split("#");
    const parts = body.trim().split(/\s+/);
    if (parts.length !== 3) continue;
    const [script, seed, want] = parts;
    const bytes = script.match(/../g).map((h) => parseInt(h, 16));
    let got;
    try { got = SA2.exec(bytes, parseInt(seed, 16), 1000000, null).key.toString(16).toUpperCase().padStart(8, "0"); } catch (e) { got = "ERR"; }
    assert.equal(got, want, `${script} ${seed} [${tag.trim()}]`);
    n++;
    if (want === "ERR") errs++;
  }
  assert.ok(n > 1000 && errs > 20, `checked ${n} vectors (${errs} errors)`);
});

test("preview SA2 fixtures: text, assembler, trace bound, arming", () => {
  const { F, SA2 } = preview();
  const hex = "87 FF FF FF FF 4C";
  assert.equal(F.sa2_inspect({ script: hex, seed: "00000001" }).run.key, "FFFFFFFE");
  const asm = F.sa2_inspect({ script: "loop 3\nrsl\nbcc skip\neor 0x5A5A5A5A\nskip: next\nadd 0x100\nfinish" });
  assert.equal(asm.input, "assembler");
  assert.deepEqual(asm.hex.split(" ").map((h) => parseInt(h, 16)), Array.from(SA2.SYNTH));
  assert.equal(asm.rows[2].text, "bcc L000A");
  assert.equal(F.sa2_inspect({ script: "add 5\nfinish" }).hex, "93 00 00 00 05 4C", "'add 5' is assembler, not AD D5");
  const big = F.sa2_inspect({ script: "loop 255\nloop 255\nrsl\nnext\nnext\nfinish", seed: "12345678", trace: true, traceLimit: 10 });
  assert.equal(big.run.trace.length, 10);
  assert.equal(big.run.trace_truncated, true);
  assert.match(F.sa2_inspect({ script: "81" }).lints[0], /finish/);
  assert.throws(() => F.sa2_arm({ script: hex, ack: false }), /confirmation/);
  assert.throws(() => F.sa2_arm({ script: "68 00 49 4C", ack: true }), /count 0/);
  assert.equal(F.sa2_arm({ script: hex, ack: true, label: "t" }).armed, true);
  assert.equal(F.sa2_status().label, "t");
  assert.equal(F.sa2_arm({ script: null }).armed, false);
});

function panel(channel) {
  const { F, tauri } = preview();
  const calls = [];
  const stub = {
    core: { invoke: async (cmd, args) => { calls.push([cmd, args]); if (cmd === "build_channel") return channel; return F[cmd](args || {}); } },
    dialog: tauri.dialog,
  };
  const { ctx, window, document } = makeWindow({ tauri: stub });
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  load(ctx, "nav.js");
  load(ctx, "feat_sa2.js");
  const p = document.getElementById("panel-sa2");
  return { window, p, $: (id) => p.querySelector("#" + id), calls };
}

test("panel: container → candidates → disassembly → key", async () => {
  const { $, p, calls, window } = panel("stable");
  assert.ok(p && !p.placeholder, "feat_sa2.js creates #panel-sa2");
  assert.equal(typeof window.OTSa2.preferred, "function");
  assert.equal(window.OTSa2.preferred([{ decodes: false }, { decodes: true }]), 1);
  assert.equal(window.OTSa2.preferred([]), -1);
  $("sa2Cont").click();
  await tick();
  const load = calls.find(([c]) => c === "sa2_load");
  assert.equal(load[1].container, true);
  assert.equal($("sa2Text").value, "68 03 81 4A 05 87 5A 5A 5A 5A 49 93 00 00 01 00 4C");
  const use = $("sa2Cands").querySelectorAll("button[data-i]");
  assert.equal(use.length, 2, "both candidates listed");
  assert.ok(use[0].classList.contains("primary"), "the decoding candidate is picked");
  assert.ok($("sa2Disasm").querySelectorAll("tbody tr").length >= 7);

  $("sa2Seed").value = "00 00 00 01";
  $("sa2Trace").checked = true;
  $("sa2Run").click();
  await tick();
  const run = calls.filter(([c]) => c === "sa2_inspect").pop()[1];
  assert.deepEqual([run.seed, run.trace, run.traceLimit], ["00 00 00 01", true, 500]);
  assert.match($("sa2Result").innerHTML, /key <b class="mono">00000108<\/b>/);
  assert.ok($("sa2Result").querySelectorAll("tbody tr").length > 5, "trace rows");

  // A file source fills the script directly.
  $("sa2Text").value = "";
  $("sa2File").click();
  await tick();
  assert.match($("sa2Text").value, /^68 03 81/);
  assert.equal($("sa2Cands").innerHTML, "");
});

test("panel: arming is Bench tier and needs the confirmation", async () => {
  const stable = panel("stable");
  await tick();
  assert.equal(stable.$("sa2Live").hidden, true, "hidden on stable");
  assert.ok(!stable.calls.some(([c]) => c === "sa2_status"));

  const { $, calls } = panel("bench");
  await tick();
  assert.equal($("sa2Live").hidden, false, "shown on bench");
  assert.match($("sa2Status").innerHTML, /No SA2 script armed/);
  $("sa2Text").value = "87 5A 5A 5A 5A 4C";
  $("sa2Arm").click();
  await tick();
  assert.match($("sa2Status").innerHTML, /confirmation/);
  $("sa2Ack").checked = true;
  $("sa2Label").value = "my notes";
  $("sa2Arm").click();
  await tick();
  assert.match($("sa2Status").innerHTML, /Armed as <b class="mono">sa2<\/b>: 6 bytes · my notes/);
  $("sa2Disarm").click();
  await tick();
  assert.match($("sa2Status").innerHTML, /No SA2 script armed/);
  assert.deepEqual(calls.filter(([c]) => c === "sa2_arm").map(([, a]) => a.ack), [false, true, false]);
});
