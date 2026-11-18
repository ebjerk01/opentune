// Demo fixtures for the definition-pack picker (pack-picker.js) used by the
// Coding, Adaptation, Service fn, Auto-scan and Live log panels. Preview only:
// packs_list answers with the shipped packs/ folder as bundled by build.py;
// "Load pack…" opens a made-up community pack whose module list addresses the
// virtual car and which has one broken file, so the picker's validation state
// has something to show. Nothing here is from a real vehicle.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const P = window.__PREVIEW__ || {};
  const FILES = (P.vcar && P.vcar.files) || {};
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const clone = (v) => JSON.parse(JSON.stringify(v));
  const INSTALLED = P.packs || [];

  const COMMUNITY_DIR = "demo/community-pack";
  FILES[COMMUNITY_DIR + "/modules/demo-community.json"] = {
    vehicle: "Demo community car (fictional)",
    modules: [
      { name: "Engine", addr: 1, req_id: 0x7e0, resp_id: 0x7e8, protocol: "uds" },
      { name: "Gearbox", addr: 2, req_id: 0x7e1, resp_id: 0x7e9, protocol: "uds" },
      { name: "Seat memory (not fitted)", addr: 0x36, req_id: 0x74c, resp_id: 0x7b6, protocol: "uds" },
    ],
  };
  const COMMUNITY = {
    id: "demo-community", name: "Demo community pack (fictional)", version: "0.1.0", maker: "Demo",
    platform: "demo", vehicles: ["Demo community car (fictional)"], author: "OpenTune preview",
    license: "GPL-3.0-or-later", description: "Preview-only pack loaded from file: one module list and one broken coding file.",
    dir: COMMUNITY_DIR, source: "file", loaded: 1, valid: false,
    notes: [{ file: "coding/seat-memory.json", kind: "coding", message: "parse error: missing field `addressing` at line 1 column 41" }],
    defs: [{ kind: "modules", file: "modules/demo-community.json", path: COMMUNITY_DIR + "/modules/demo-community.json", id: "demo-community", name: "Demo community car (fictional)" }],
  };

  F.packs_list = async () => {
    await wait(60);
    return { root: "packs", packs: clone(INSTALLED), failed: [] };
  };
  // The pack a picked path belongs to: its pack.json, its folder or a file in it.
  F.packs_open = async ({ path }) => {
    await wait(120);
    const p = String(path || "").replace(/\\/g, "/").replace(/\/pack\.json$/, "");
    if (p === COMMUNITY_DIR || p.startsWith(COMMUNITY_DIR + "/")) return clone(COMMUNITY);
    const hit = INSTALLED.find((x) => p === x.dir || p.startsWith(x.dir + "/"));
    if (!hit) throw `${path} is not part of a definition pack — choose a pack's pack.json (see docs/DEFINITION-PACKS.md)`;
    return Object.assign(clone(hit), { source: "file" });
  };
})();
