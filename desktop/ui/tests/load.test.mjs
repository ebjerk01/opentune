// Load every UI script in index.html order on a DOM stub and check the globals
// other scripts (feature panels, plugin overlays) rely on. Catches a split
// that uses a name before the script defining it has loaded.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";

const { makeWindow, indexScripts, load, UI } = dom;

function loadAll() {
  const env = makeWindow();
  const main = env.document.createElement("main");
  main.id = "main";
  env.document.body.appendChild(main);
  const scripts = indexScripts();
  for (const s of scripts) load(env.ctx, s);
  return { ...env, scripts };
}

test("index.html lists util.js first and every referenced script exists", () => {
  const scripts = indexScripts();
  assert.equal(scripts[0], "util.js");
  assert.equal(scripts[scripts.length - 1], "extensions.js", "extensions load last");
  for (const s of scripts) assert.ok(fs.existsSync(path.join(UI, "src", s)), `missing src/${s}`);
  const app = scripts.filter((s) => s.startsWith("app-"));
  assert.deepEqual(app, ["app-core.js", "app-calibration.js", "app-maps.js", "app-tools.js", "app-boot.js"]);
  assert.ok(scripts.indexOf("nav.js") < scripts.indexOf("app-core.js"));
  assert.ok(scripts.indexOf("app-boot.js") < scripts.indexOf("feat_coding.js"));
});

test("all scripts load in order and expose the shared globals", () => {
  const { ctx, window } = loadAll();
  // window properties used by feature scripts and plugin overlays
  for (const fn of ["showPanel", "onPanelFirstShow", "toast", "loadGuideSection"]) {
    assert.equal(typeof window[fn], "function", `window.${fn}`);
  }
  for (const fn of ["connArgs", "gate", "probe", "setConn", "onCaps"]) assert.equal(typeof window.OT[fn], "function", `OT.${fn}`);
  assert.equal(typeof window.OT.conn, "object");
  assert.equal(window.OT.conn.driver, "elm327obd");
  assert.equal(typeof window.OTNav.register, "function");
  assert.equal(typeof window.OTHelp.add, "function");
  assert.equal(typeof window.OTUtil.renderMarkdown, "function");
  assert.ok(Array.isArray(window.OT_EXTENSIONS));
  // bare names shared between the app scripts (global lexical scope)
  const kinds = vm.runInContext(
    "[typeof $, typeof el, typeof esc, typeof invoke, typeof errBanner, typeof pickFile, typeof state, typeof startupMark, typeof baseName]",
    ctx,
  );
  assert.deepEqual(Array.from(kinds), ["function", "function", "function", "function", "function", "function", "object", "function", "function"]);
  // connArgs keeps its call shape
  assert.deepEqual(JSON.parse(JSON.stringify(window.OT.connArgs())), {
    driver: "elm327obd", port: null, baud: null, bus: "hs", txId: null, rxId: null,
  });
});

test("showPanel runs first-show hooks once", () => {
  const { window } = loadAll();
  let n = 0;
  window.onPanelFirstShow("zz-test", () => n++);
  window.showPanel("zz-test");
  window.showPanel("zz-test");
  assert.equal(n, 1);
  // Registered after the first show: runs right away.
  window.onPanelFirstShow("zz-test", () => n++);
  assert.equal(n, 2);
});

test("panels and ids plugins extend are present", () => {
  const { document } = loadAll();
  const html = fs.readFileSync(path.join(UI, "index.html"), "utf8");
  for (const id of ["main", "csAlgo", "skAlgo", "nav", "toast"]) assert.match(html, new RegExp(`id="${id}"`), `#${id} in index.html`);
  const auto = document.getElementById("panel-autodebug");
  assert.ok(auto && !auto.placeholder, "feat_autodebug.js creates #panel-autodebug");
});
