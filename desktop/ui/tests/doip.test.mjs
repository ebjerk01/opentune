// DoIP in the desktop UI: the derived `diag` capability the diagnostic panels
// gate on, the sidebar's Discover row for the `doip` driver, and the preview
// fixture's synthetic entities.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";

const { makeWindow, indexScripts, load, UI } = dom;
const tick = async (n = 6) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

// The preview's doip_discover fixture, loaded on its own.
function previewDiscover() {
  const sandbox = { setTimeout: (f) => { f(); return 0; }, Promise, String, Object };
  sandbox.window = sandbox;
  sandbox.__PREVIEW_FIXTURES__ = {};
  const file = path.join(UI, "..", "preview", "fixtures-doip.js");
  new vm.Script(fs.readFileSync(file, "utf8"), { filename: file }).runInContext(vm.createContext(sandbox));
  return sandbox.__PREVIEW_FIXTURES__.doip_discover;
}

function loadApp(invoke) {
  const tauri = {
    core: { invoke: (cmd, args) => (invoke(cmd, args) ?? new Promise(() => {})) },
    dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
    event: { listen: () => new Promise(() => {}) },
  };
  const env = makeWindow({ tauri });
  const main = env.document.createElement("main");
  main.id = "main";
  env.document.body.appendChild(main);
  for (const s of indexScripts()) load(env.ctx, s);
  return env;
}

test("diag is CAN or DoIP; CAN capture still needs can", () => {
  const { window } = loadApp(() => undefined);
  const OT = window.OT;
  assert.equal(OT.deriveCaps({ doip: true, read_memory: true }).diag, true);
  assert.equal(OT.deriveCaps({ can: true }).diag, true);
  assert.equal(OT.deriveCaps({ kline: true }).diag, false);
  assert.equal(OT.deriveCaps(null), null);
  OT.adapter = { connected: true, name: "DoIP" };
  OT.caps = OT.deriveCaps({ doip: true });
  assert.equal(OT.has(["diag"]), true);
  assert.equal(OT.has(["can"]), false);
  assert.ok(OT.CAP_LABELS.diag && OT.CAP_LABELS.doip);
});

test("the diagnostic panels gate on diag, CAN capture on can", () => {
  const src = (f) => fs.readFileSync(path.join(UI, "src", f), "utf8");
  for (const f of ["feat_coding.js", "feat_adaptation.js", "feat_scan.js", "feat_service.js", "feat_logging.js", "feat_obd.js", "feat_obdinfo.js", "feat_dashboard.js"]) {
    assert.match(src(f), /gate\([^)]*\["diag"\]/, f);
    assert.doesNotMatch(src(f), /\["can"\]/, f);
  }
  assert.match(src("feat_candump.js"), /gate\(panel, \["can"\]/);
});

test("preview discovery data is synthetic", async () => {
  const discover = previewDiscover();
  const all = await discover({ port: "" });
  assert.equal(all.dest, "255.255.255.255:13400");
  assert.equal(all.entities.length, 2);
  for (const e of all.entities) {
    assert.match(e.vin, /^WVWZZZ0+$/, "placeholder VIN");
    assert.match(e.ip, /^192\.0\.2\.\d+$/, "RFC 5737 documentation address");
    assert.match(e.eid, /^02:/, "locally administered EID");
    assert.equal(e.spec, `${e.ip}?target=${e.logical_address}`);
  }
  const one = await discover({ port: "192.0.2.11?target=0x0020" });
  assert.equal(one.entities.map((e) => e.logical_address).join(), "0x0020");
});

test("Discover lists the entities and a click fills the port field", async () => {
  const discover = previewDiscover();
  const calls = [];
  const { window, ctx } = loadApp((cmd, args) => {
    if (cmd !== "doip_discover") return undefined;
    calls.push(args);
    return discover(args);
  });
  const $ = (id) => window.document.getElementById(id);
  // The sidebar row only shows for the doip driver.
  $("connDriver").value = "elm327obd";
  vm.runInContext("syncDoipRow()", ctx);
  assert.equal($("connDoipRow").hidden, true);
  $("connDriver").value = "doip";
  vm.runInContext("syncDoipRow()", ctx);
  assert.equal($("connDoipRow").hidden, false);
  assert.match($("connPort").placeholder, /IP/);

  $("connPort").value = "";
  vm.runInContext("doipDiscover()", ctx);
  await tick();
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [{ port: null, vin: null, eid: null, timeoutMs: 2000 }]);
  const box = $("connDoipList");
  const buttons = box.querySelectorAll(".conn-doip-entity");
  assert.equal(buttons.length, 2);
  assert.match(buttons[1].textContent, /0x0020/);
  buttons[1].click();
  assert.equal($("connPort").value, "192.0.2.11?target=0x0020");
  assert.equal(window.OT.conn.port, "192.0.2.11?target=0x0020");
  assert.equal(box.hidden, true);

  // Nothing answering is said plainly.
  $("connPort").value = "192.0.2.99";
  vm.runInContext("doipDiscover()", ctx);
  await tick();
  assert.match(box.textContent, /No DoIP entity answered \(192\.0\.2\.99:13400\)/);
});
