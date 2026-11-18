// The browser preview (desktop/preview): every core command the app exposes
// has a demo fixture, so no panel shows "has no demo data yet".
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import dom from "./support/dom.cjs";

const DESKTOP = path.join(dom.UI, "..");
const REPO = path.join(DESKTOP, "..");
const PREVIEW = path.join(DESKTOP, "preview");

// Command names from core_command_list! in desktop/src-tauri/src/lib.rs.
function coreCommands() {
  const src = fs.readFileSync(path.join(DESKTOP, "src-tauri", "src", "lib.rs"), "utf8");
  const m = src.match(/macro_rules! core_command_list \{[\s\S]*?\$emit! \{([\s\S]*?)\}/);
  assert.ok(m, "core_command_list! found");
  return m[1].split(",").map((s) => s.trim().split("::").pop().trim()).filter(Boolean);
}

function fixtures() {
  const profile = JSON.parse(fs.readFileSync(path.join(REPO, "crates", "drivers", "vcar", "profiles", "demo.json"), "utf8"));
  const sandbox = { console, setTimeout, clearTimeout, Date, Math, Promise, JSON };
  sandbox.window = sandbox;
  sandbox.__PREVIEW__ = { vcar: { profile, files: {} }, packs: [], ecudefs: [], guides: [], units: [] };
  const ctx = vm.createContext(sandbox);
  const files = ["mock.js", ...fs.readdirSync(PREVIEW).filter((f) => /^fixtures.*\.js$/.test(f)).sort()];
  for (const f of files) new vm.Script(fs.readFileSync(path.join(PREVIEW, f), "utf8"), { filename: f }).runInContext(ctx);
  return { F: sandbox.__PREVIEW_FIXTURES__, tauri: sandbox.__TAURI__ };
}

test("every core command has a preview fixture", () => {
  const { F } = fixtures();
  const cmds = coreCommands();
  assert.ok(cmds.length > 50 && cmds.includes("packs_list"), `parsed ${cmds.length} commands`);
  const missing = cmds.filter((c) => typeof F[c] !== "function");
  assert.deepEqual(missing, [], "add a fixture in desktop/preview/fixtures-*.js");
});

test("preview pack picker: Load pack… opens the fictional community pack", async () => {
  const { F, tauri } = fixtures();
  const path = await tauri.dialog.open({ filters: [{ name: "Definition pack", extensions: ["json"] }] });
  const pack = await F.packs_open({ path });
  assert.equal(pack.source, "file");
  assert.equal(pack.valid, false);
  assert.equal(pack.defs[0].kind, "modules");
  await assert.rejects(F.packs_open({ path: "demo/elsewhere.json" }), /not part of a definition pack/);
});
