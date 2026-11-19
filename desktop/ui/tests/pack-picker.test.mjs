// Definition-pack picker (src/pack-picker.js): pure helpers, the picker's
// choose / restore / load-from-file flow on the DOM stub, and storage that is
// blocked or broken.
import test from "node:test";
import assert from "node:assert/strict";
import dom from "./support/dom.cjs";

const { makeWindow, load } = dom;
const tick = async (n = 6) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

const def = (kind, dir, file, id, name) => ({ kind, file, path: `${dir}/${file}`, id, name });
const DEMO = {
  id: "vcar-demo", name: "Virtual car", version: "1.0.0", license: "GPL-3.0-or-later", dir: "/p/vcar-demo",
  source: "installed", loaded: 3, valid: true, notes: [],
  defs: [
    def("coding", "/p/vcar-demo", "coding/engine.json", "vcar-engine", "Engine"),
    def("coding", "/p/vcar-demo", "coding/abs.json", "vcar-abs", "ABS"),
    def("modules", "/p/vcar-demo", "modules/vcar-demo.json", "vcar-demo", "Virtual car"),
  ],
};
const OBD = {
  id: "generic-obd2", name: "Generic OBD-II", version: "1.1.0", dir: "/p/generic-obd2", source: "installed",
  loaded: 1, valid: true, notes: [], defs: [def("loggers", "/p/generic-obd2", "loggers/obd2-mode01.json", "obd2-mode01", "")],
};
const EXT = {
  id: "community", name: "Community", version: "0.1.0", dir: "/home/u/community", source: "file", loaded: 1, valid: false,
  notes: [{ file: "coding/broken.json", kind: "coding", message: "parse error: missing field `addressing`" }],
  defs: [def("coding", "/home/u/community", "coding/bcm.json", "bcm", "Body <BCM>")],
};

function memStorage(init) {
  const m = new Map(Object.entries(init || {}));
  return { m, getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k) };
}

function setup({ storage, packs = [DEMO, OBD], dialog = null, listError = null, external = { [EXT.dir]: EXT } } = {}) {
  const calls = [];
  const tauri = {
    core: {
      invoke: async (cmd, args) => {
        calls.push([cmd, args]);
        if (cmd === "packs_list") {
          if (listError) throw listError;
          return { root: "/p", packs: JSON.parse(JSON.stringify(packs)), failed: [] };
        }
        if (cmd === "packs_open") {
          const dir = String(args.path).replace(/\/pack\.json$/, "");
          const hit = external[dir] || packs.find((p) => p.dir === dir);
          if (!hit) throw `${args.path} is not part of a definition pack`;
          return Object.assign(JSON.parse(JSON.stringify(hit)), { source: "file" });
        }
        throw `unexpected ${cmd}`;
      },
    },
    dialog: { open: async (o) => (typeof dialog === "function" ? dialog(o) : dialog) },
  };
  const { ctx, window } = makeWindow({ tauri });
  if (storage !== undefined) Object.defineProperty(window, "localStorage", storage);
  load(ctx, "pack-picker.js");
  return { window, P: window.OTPacks, calls };
}

const selectedOption = (sel) => sel.children.find((o) => "selected" in o.attributes);

test("pure helpers: packs offered per kind, labels and validation state", () => {
  const { P } = setup();
  assert.deepEqual(P.packsFor([DEMO, OBD, EXT], "coding").map((p) => p.id), ["vcar-demo", "community"]);
  assert.deepEqual(P.packsFor([DEMO, OBD], "loggers").map((p) => p.id), ["generic-obd2"]);
  assert.deepEqual(P.packsFor([DEMO, OBD], "adaptation"), []);
  assert.equal(P.packLabel(DEMO), "Virtual car v1.0.0");
  assert.equal(P.packLabel(EXT), "Community v0.1.0 (from file)");
  assert.equal(P.defLabel(DEMO.defs[0]), "Engine — engine.json");
  assert.equal(P.defLabel(OBD.defs[0]), "obd2-mode01 — obd2-mode01.json");
  assert.deepEqual(JSON.parse(JSON.stringify(P.packState(DEMO))), { level: "ok", text: "valid · 3 definitions" });
  assert.equal(P.packState(EXT).level, "warn");
  assert.equal(P.packState(Object.assign({}, EXT, { loaded: 0 })).level, "bad");
  assert.ok(P.samePath("C:\\packs\\a.json", "C:/packs/a.json"));
  assert.equal(P.mergePack([DEMO, EXT], Object.assign({}, EXT, { version: "0.2.0" })).length, 2);
  assert.equal(P.locate([DEMO], "/p/vcar-demo/coding/abs.json", "coding").def.id, "vcar-abs");
  assert.equal(P.locate([DEMO], "/p/vcar-demo/coding/abs.json", "modules").pack, null);
});

test("lists packs with the kind, picks a definition and remembers it per panel", async () => {
  const store = memStorage();
  const { P, window } = setup({ storage: { value: store } });
  const seen = [];
  const pp = P.picker({ panel: "coding", kind: "coding", onChange: (s, how) => seen.push([s.path, how.user]) });
  const packSel = pp.el.querySelector('[data-pp="pack"]');
  assert.match(packSel.innerHTML, /loading packs/);
  await tick();
  // Only the pack that carries coding definitions is offered.
  assert.match(packSel.innerHTML, /Virtual car v1\.0\.0/);
  assert.doesNotMatch(packSel.innerHTML, /Generic OBD/);
  pp.choosePack("/p/vcar-demo");
  assert.equal(pp.get().path, null, "two coding defs: the user chooses");
  const defSel = pp.el.querySelector('[data-pp="def"]');
  assert.match(defSel.innerHTML, /choose a coding definition/);
  pp.chooseDef("/p/vcar-demo/coding/abs.json");
  assert.equal(pp.get().path, "/p/vcar-demo/coding/abs.json");
  assert.equal(pp.get().def.id, "vcar-abs");
  assert.equal(selectedOption(defSel).value, "/p/vcar-demo/coding/abs.json");
  const info = pp.el.querySelector('[data-pp="info"]').innerHTML;
  assert.match(info, /pill ok/);
  assert.match(info, /vcar-demo/);
  assert.match(info, /v1\.0\.0/);
  assert.match(info, /coding\/abs\.json/);
  assert.deepEqual(seen.at(-1), ["/p/vcar-demo/coding/abs.json", true]);
  assert.deepEqual(JSON.parse(store.m.get("opentune.packpicker.coding")), { packDir: "/p/vcar-demo", path: "/p/vcar-demo/coding/abs.json" });

  // A second panel of another kind keeps its own choice; one def is auto-taken.
  const scan = P.picker({ panel: "scan", kind: "modules" });
  await tick();
  scan.choosePack("/p/vcar-demo");
  assert.equal(scan.get().path, "/p/vcar-demo/modules/vcar-demo.json");
  assert.ok(store.m.has("opentune.packpicker.scan"));
  assert.equal(JSON.parse(store.m.get("opentune.packpicker.coding")).path, "/p/vcar-demo/coding/abs.json");
  void window;
});

test("restores the remembered choice when the panel loads", async () => {
  const store = memStorage({
    "opentune.packpicker.coding": JSON.stringify({ packDir: "/p/vcar-demo", path: "/p/vcar-demo/coding/engine.json" }),
  });
  const { P } = setup({ storage: { value: store } });
  const seen = [];
  const pp = P.picker({ panel: "coding", kind: "coding", onChange: (s, how) => seen.push([s.path, how.user]) });
  await tick();
  assert.equal(pp.get().path, "/p/vcar-demo/coding/engine.json");
  assert.equal(pp.get().pack.id, "vcar-demo");
  assert.deepEqual(seen, [["/p/vcar-demo/coding/engine.json", false]], "restore reports user: false");
});

test("a remembered definition that vanished leaves the pack chosen and no path", async () => {
  const store = memStorage({
    "opentune.packpicker.coding": JSON.stringify({ packDir: "/p/gone", path: "/p/gone/coding/x.json" }),
  });
  const { P } = setup({ storage: { value: store } });
  const pp = P.picker({ panel: "coding", kind: "coding" });
  await tick();
  // Not in any pack any more: kept as a loose file (the engine reports if it is missing).
  assert.equal(pp.get().pack, null);
  assert.equal(pp.get().path, "/p/gone/coding/x.json");
  assert.match(pp.el.querySelector('[data-pp="info"]').innerHTML, /not from a pack/);
});

test("Load pack… opens a pack from disk, shows its skipped files and offers it everywhere", async () => {
  const store = memStorage();
  const { P, calls } = setup({ storage: { value: store }, dialog: "/home/u/community/pack.json" });
  const a = P.picker({ panel: "coding", kind: "coding" });
  const b = P.picker({ panel: "adaptation", kind: "adaptation" });
  await tick();
  a.el.querySelector('[data-pp="loadPack"]').click();
  await tick();
  assert.ok(calls.some(([c, x]) => c === "packs_open" && x.path === "/home/u/community/pack.json"));
  assert.equal(a.get().pack.id, "community");
  assert.equal(a.get().path, "/home/u/community/coding/bcm.json", "its one coding def is taken");
  const info = a.el.querySelector('[data-pp="info"]').innerHTML;
  assert.match(info, /1 loaded · 1 skipped/);
  assert.match(info, /coding\/broken\.json/);
  assert.match(info, /loaded from file/);
  assert.doesNotMatch(a.el.querySelector('[data-pp="def"]').innerHTML, /<BCM>/, "labels are escaped");
  assert.deepEqual(JSON.parse(store.m.get("opentune.packpicker.extra")), ["/home/u/community"]);
  // The adaptation picker has no adaptation def there, so it is not offered.
  assert.doesNotMatch(b.el.querySelector('[data-pp="pack"]').innerHTML, /Community/);

  // Next start: the remembered external pack is reopened and listed again.
  const again = setup({ storage: { value: store } });
  const c = again.P.picker({ panel: "coding", kind: "coding" });
  await tick();
  assert.match(c.el.querySelector('[data-pp="pack"]').innerHTML, /Community v0\.1\.0 \(from file\)/);
});

test("a remembered external pack that is gone is forgotten", async () => {
  const store = memStorage({ "opentune.packpicker.extra": JSON.stringify(["/gone/pack", EXT.dir]) });
  const { P } = setup({ storage: { value: store } });
  await P.library();
  assert.deepEqual(JSON.parse(store.m.get("opentune.packpicker.extra")), [EXT.dir]);
});

test("Load pack… errors are shown in the picker", async () => {
  const { P } = setup({ dialog: "/tmp/not-a-pack.json" });
  const pp = P.picker({ panel: "service", kind: "services" });
  await tick();
  pp.el.querySelector('[data-pp="loadPack"]').click();
  await tick();
  assert.match(pp.el.querySelector('[data-pp="info"]').innerHTML, /Load pack: \/tmp\/not-a-pack\.json is not part of a definition pack/);
  assert.equal(pp.get().path, null);
});

test("File… selects the pack a file belongs to, or keeps a loose file", async () => {
  let next = "/p/vcar-demo/coding/engine.json";
  const opened = [];
  const { P } = setup({ dialog: (o) => { opened.push(o); return next; } });
  const pp = P.picker({ panel: "coding", kind: "coding", filter: { name: "Coding def", extensions: ["json"] } });
  await tick();
  pp.el.querySelector('[data-pp="file"]').click();
  await tick();
  assert.equal(opened[0].filters[0].name, "Coding def", "the panel's own filter");
  assert.equal(pp.get().pack.id, "vcar-demo");
  next = "/elsewhere/my-coding.json";
  pp.el.querySelector('[data-pp="file"]').click();
  await tick();
  assert.equal(pp.get().pack, null);
  assert.equal(pp.get().path, "/elsewhere/my-coding.json");
  assert.match(pp.el.querySelector('[data-pp="info"]').innerHTML, /my-coding\.json/);
});

test("works when storage is blocked or throws", async () => {
  const blocked = { get() { throw new Error("SecurityError: storage disabled"); } };
  const { P } = setup({ storage: blocked });
  const pp = P.picker({ panel: "coding", kind: "coding" });
  await tick();
  pp.choosePack("/p/vcar-demo");
  pp.chooseDef("/p/vcar-demo/coding/engine.json");
  assert.equal(pp.get().path, "/p/vcar-demo/coding/engine.json");

  const throwing = { value: { getItem() { throw new Error("x"); }, setItem() { throw new Error("quota"); }, removeItem() { throw new Error("x"); } } };
  const t = setup({ storage: throwing });
  const pq = t.P.picker({ panel: "coding", kind: "coding" });
  await tick();
  pq.chooseDef("/p/vcar-demo/coding/engine.json"); // no pack chosen yet: nothing
  assert.equal(pq.get().path, null);
  pq.choosePath("/p/vcar-demo/coding/engine.json");
  assert.equal(pq.get().def.id, "vcar-engine");

  const junk = memStorage({ "opentune.packpicker.coding": "{not json", "opentune.packpicker.extra": "\"str\"" });
  const j = setup({ storage: { value: junk } });
  const pj = j.P.picker({ panel: "coding", kind: "coding" });
  await tick();
  assert.equal(pj.get().path, null, "unparseable memory is ignored");
});

test("each pack-consuming panel mounts one picker and follows its choice", async () => {
  const store = memStorage();
  const invoked = [];
  const tauri = {
    core: {
      invoke: async (cmd, args) => {
        invoked.push([cmd, args]);
        if (cmd === "packs_list") return { root: "/p", packs: JSON.parse(JSON.stringify([DEMO, OBD])), failed: [] };
        return new Promise(() => {}); // engine commands never answer here
      },
    },
    dialog: { open: async () => null, save: async () => null },
  };
  const { ctx, window, document } = makeWindow({ strictIds: true, tauri });
  Object.defineProperty(window, "localStorage", { value: store });
  for (const id of ["nav", "main"]) {
    const el = document.createElement(id === "main" ? "main" : "div");
    el.id = id;
    document.body.appendChild(el);
  }
  window.OTNav = { register() {} };
  window.OT = { gate() {}, onCaps() {}, has: () => true, connArgs: () => ({ driver: "vcar" }) };
  for (const s of ["util.js", "pack-picker.js", "feat_coding.js", "feat_scan.js", "feat_adaptation.js", "feat_service.js", "livelog-core.js", "feat_logging.js"]) load(ctx, s);
  const kinds = { coding: "coding", scan: "modules", adaptation: "adaptation", service: "services", logging: "loggers" };
  for (const [panel, kind] of Object.entries(kinds)) {
    const sec = document.getElementById("panel-" + panel);
    assert.ok(sec, `#panel-${panel}`);
    const pickers = sec.querySelectorAll(".pack-picker");
    assert.equal(pickers.length, 1, `one pack picker in #panel-${panel}`);
    assert.equal(pickers[0].dataset.kind, kind);
  }
  await tick();
  assert.equal(invoked.filter(([c]) => c === "packs_list").length, 1, "the pack list is shared by all panels");

  // Choosing the module list in Auto-scan's picker is what Scan runs on.
  const scanSel = document.getElementById("panel-scan").querySelector('[data-pp="pack"]');
  scanSel.value = "/p/vcar-demo";
  scanSel.dispatch("change");
  document.getElementById("scanRun").click();
  await tick();
  const run = invoked.find(([c]) => c === "autoscan_run");
  assert.ok(run, "autoscan_run invoked");
  assert.equal(run[1].modulesPath, "/p/vcar-demo/modules/vcar-demo.json");
  assert.equal(JSON.parse(store.m.get("opentune.packpicker.scan")).packDir, "/p/vcar-demo");

  // Coding reads the definition chosen in its picker.
  const cod = document.getElementById("panel-coding");
  cod.querySelector('[data-pp="pack"]').value = "/p/vcar-demo";
  cod.querySelector('[data-pp="pack"]').dispatch("change");
  cod.querySelector('[data-pp="def"]').value = "/p/vcar-demo/coding/engine.json";
  cod.querySelector('[data-pp="def"]').dispatch("change");
  document.getElementById("codRead").click();
  await tick();
  const read = invoked.find(([c]) => c === "coding_read");
  assert.equal(read && read[1].defPath, "/p/vcar-demo/coding/engine.json");
});

test("a failing packs_list still leaves File… and says why", async () => {
  const { P } = setup({ listError: "packs resources not found" });
  const pp = P.picker({ panel: "logging", kind: "loggers" });
  await tick();
  const sel = pp.el.querySelector('[data-pp="pack"]');
  assert.match(sel.innerHTML, /no pack has a logger definition/);
  assert.equal(sel.disabled, true);
  assert.match(pp.el.querySelector('[data-pp="info"]').innerHTML, /packs resources not found/);
});
