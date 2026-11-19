// OpenTune — definition-pack picker shared by the vehicle panels.
// Loaded after app-boot.js and before every feat_*.js script.
//
// The coding / adaptation / service / auto-scan / live-log engines and the
// Controllers panel each run on one definition file. A panel mounts one picker for the kind it needs:
//
//   const pp = OTPacks.picker({ panel: "coding", kind: "coding",
//     filter: { name: "Coding def", extensions: ["json"] },
//     onChange: (sel) => { defPath = sel.path; } });
//   someCard.appendChild(pp.el);
//
// It lists the installed packs (`packs_list`) that carry that kind, then the
// definitions in the chosen pack, and shows the pack's id, version and
// validation state (files the loader skipped, and why). "Load pack…" opens a
// pack from anywhere on disk (`packs_open`); such packs are remembered and
// offered in every panel. "File…" still takes a loose definition file. The
// choice is remembered per panel in localStorage (wrapped in try/catch: the
// picker works the same when storage is blocked).
//
// sel = { path, packDir, pack, def } — `pack` / `def` are null for a loose file;
// `path` is null when nothing is chosen.
"use strict";
(function () {
  const KEY = "opentune.packpicker.";
  const MAX_EXTRA = 8;
  const KIND_LABEL = {
    coding: "coding definition", adaptation: "adaptation definition", services: "service script",
    loggers: "logger definition", modules: "module list", ecudefs: "ECU definition",
    controllers: "controller definition",
  };
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "");
  const samePath = (a, b) => !!a && !!b && String(a).replace(/\\/g, "/") === String(b).replace(/\\/g, "/");
  const errText = (e) => String(e && e.message ? e.message : e);

  // ---- storage (per viewer; never required) --------------------------------
  function storage() {
    try { return window.localStorage || null; } catch (_) { return null; }
  }
  function recall(key, fallback) {
    try {
      const s = storage();
      const raw = s && s.getItem(KEY + key);
      return raw ? JSON.parse(raw) : fallback;
    } catch (_) { return fallback; }
  }
  function remember(key, value) {
    try {
      const s = storage();
      if (!s) return;
      if (value == null) s.removeItem(KEY + key);
      else s.setItem(KEY + key, JSON.stringify(value));
    } catch (_) { /* storage full or blocked: the picker just forgets */ }
  }

  // ---- pure helpers (unit-tested) -----------------------------------------
  const defsOf = (pack, kind) => ((pack && pack.defs) || []).filter((d) => d.kind === kind);
  const notesOf = (pack, kind) => ((pack && pack.notes) || []).filter((n) => !kind || n.kind === kind);
  // Packs worth offering for `kind`: those with a definition of it, or with a
  // skipped file of it (so a broken pack shows up with its reason).
  const packsFor = (packs, kind) => (packs || []).filter((p) => defsOf(p, kind).length || notesOf(p, kind).length);
  function packLabel(p) {
    const v = p.version ? ` v${p.version}` : "";
    return `${p.name || p.id}${v}${p.source === "file" ? " (from file)" : ""}`;
  }
  function defLabel(d) {
    const file = baseName(d.file);
    return d.name && d.name !== d.id ? `${d.name} — ${file}` : `${d.id || file} — ${file}`;
  }
  // Validation state of a pack: { level: ok|warn|bad, text }.
  function packState(p) {
    const loaded = p.loaded || 0;
    const skipped = (p.notes || []).length;
    if (!skipped) return { level: "ok", text: `valid · ${loaded} definition${loaded === 1 ? "" : "s"}` };
    if (!loaded) return { level: "bad", text: `nothing loaded · ${skipped} file${skipped === 1 ? "" : "s"} skipped` };
    return { level: "warn", text: `${loaded} loaded · ${skipped} skipped` };
  }
  // Merge a pack into a list, replacing one from the same folder.
  function mergePack(list, pack) {
    const out = (list || []).filter((p) => !samePath(p.dir, pack.dir));
    out.push(pack);
    return out;
  }
  // The pack + definition a path belongs to (a loose file gives nulls).
  function locate(packs, path, kind) {
    for (const p of packs || []) {
      const d = defsOf(p, kind).find((x) => samePath(x.path, path));
      if (d) return { pack: p, def: d };
    }
    return { pack: null, def: null };
  }

  // ---- the shared library: installed packs + packs loaded from file --------
  let libPromise = null;
  let libReady = false;
  let lib = { root: null, packs: [], failed: [], error: null };
  const pickers = new Set();
  const notify = () => pickers.forEach((p) => { try { p.refresh(); } catch (_) {} });

  async function load() {
    let next;
    try {
      const r = (await invoke("packs_list")) || {};
      next = { root: r.root || null, packs: r.packs || [], failed: r.failed || [], error: null };
    } catch (e) {
      next = { root: null, packs: [], failed: [], error: errText(e) };
    }
    const extra = recall("extra", []);
    const kept = [];
    for (const dir of Array.isArray(extra) ? extra : []) {
      if (next.packs.some((p) => samePath(p.dir, dir))) continue;
      try {
        next.packs = mergePack(next.packs, await invoke("packs_open", { path: dir }));
        kept.push(dir);
      } catch (_) { /* moved or deleted since: forget it */ }
    }
    if (kept.length !== (Array.isArray(extra) ? extra.length : 0)) remember("extra", kept.length ? kept : null);
    lib = next;
    libReady = true;
    notify();
    return lib;
  }

  // The library, loaded once and shared by every picker (`force` re-reads).
  function library(force) {
    if (force || !libPromise) libPromise = load();
    return libPromise;
  }

  // Load a pack from disk and offer it in every panel from now on.
  async function openPack(path) {
    await library();
    const pack = await invoke("packs_open", { path });
    // Re-opening an installed pack refreshes it; it stays "installed".
    const prev = lib.packs.find((p) => samePath(p.dir, pack.dir));
    if (prev && prev.source === "installed") pack.source = "installed";
    lib = Object.assign({}, lib, { packs: mergePack(lib.packs, pack) });
    if (pack.source === "file") {
      const extra = (recall("extra", []) || []).filter((d) => !samePath(d, pack.dir));
      remember("extra", [pack.dir].concat(extra).slice(0, MAX_EXTRA));
    }
    notify();
    return pack;
  }

  // ---- the picker ----------------------------------------------------------
  function picker(o) {
    const kind = o.kind;
    const what = KIND_LABEL[kind] || "definition";
    const filter = o.filter || { name: "Definition", extensions: ["json"] };
    const onChange = typeof o.onChange === "function" ? o.onChange : () => {};
    const el = document.createElement("div");
    el.className = "pack-picker";
    el.dataset.kind = kind;
    el.innerHTML = `
      <div class="row tight">
        <label>Pack</label>
        <select class="inp" data-pp="pack" title="Installed definition packs that carry a ${esc(what)}"><option value="">loading packs…</option></select>
        <select class="inp wide" data-pp="def" title="The ${esc(what)} to use"></select>
        <button class="btn sm" type="button" data-pp="loadPack" title="Open a definition pack (its pack.json) from disk">Load pack…</button>
        <button class="btn sm" type="button" data-pp="file" title="Use a single ${esc(what)} file that is not in a pack">File…</button>
      </div>
      <div class="small muted" data-pp="info"></div>`;
    const q = (k) => el.querySelector(`[data-pp="${k}"]`);
    const packSel = q("pack");
    const defSel = q("def");
    const info = q("info");

    // Current selection; restored from storage once the library is in.
    let sel = { path: null, packDir: null, pack: null, def: null };
    let message = "";
    const saved = recall(o.panel, null);
    let restoring = saved && typeof saved === "object" ? saved : null;

    const currentPack = () => lib.packs.find((p) => samePath(p.dir, sel.packDir)) || null;

    function set(next, user) {
      const changed = !samePath(next.path, sel.path) || !samePath(next.packDir, sel.packDir);
      sel = next;
      message = "";
      remember(o.panel, sel.path || sel.packDir ? { packDir: sel.packDir, path: sel.path } : null);
      render();
      if (changed || user) onChange(Object.assign({}, sel), { user: !!user });
    }

    function choosePack(dir, user) {
      const p = lib.packs.find((x) => samePath(x.dir, dir)) || null;
      if (!p) return set({ path: null, packDir: null, pack: null, def: null }, user);
      const defs = defsOf(p, kind);
      const d = defs.length === 1 ? defs[0] : null; // one choice: take it
      set({ path: d ? d.path : null, packDir: p.dir, pack: p, def: d }, user);
    }

    function chooseDef(path, user) {
      const p = currentPack();
      const d = p ? defsOf(p, kind).find((x) => samePath(x.path, path)) : null;
      set({ path: d ? d.path : null, packDir: p ? p.dir : null, pack: p, def: d || null }, user);
    }

    // A path from "File…" (or a restore): select its pack when it is in one.
    function choosePath(path, user) {
      const hit = locate(lib.packs, path, kind);
      if (hit.pack) set({ path: hit.def.path, packDir: hit.pack.dir, pack: hit.pack, def: hit.def }, user);
      else set({ path, packDir: null, pack: null, def: null }, user);
    }

    function render() {
      const offered = packsFor(lib.packs, kind);
      const p = currentPack();
      if (p && !offered.includes(p)) offered.push(p);
      let opts;
      if (!libReady) opts = `<option value="">loading packs…</option>`;
      else if (!offered.length) opts = `<option value="">no pack has a ${esc(what)}</option>`;
      else opts = `<option value="">— choose a pack —</option>` +
        offered.map((x) => `<option value="${esc(x.dir)}"${samePath(x.dir, sel.packDir) ? " selected" : ""}>${esc(packLabel(x))}</option>`).join("");
      packSel.innerHTML = opts;
      packSel.value = p ? p.dir : "";
      packSel.disabled = !offered.length;

      const defs = defsOf(p, kind);
      if (!p) defSel.innerHTML = sel.path ? `<option value="">${esc(baseName(sel.path))}</option>` : `<option value="">—</option>`;
      else if (!defs.length) defSel.innerHTML = `<option value="">no valid ${esc(what)} in this pack</option>`;
      else defSel.innerHTML = (sel.def ? "" : `<option value="">— choose a ${esc(what)} —</option>`) +
        defs.map((d) => `<option value="${esc(d.path)}"${samePath(d.path, sel.path) ? " selected" : ""}>${esc(defLabel(d))}</option>`).join("");
      defSel.value = sel.def ? sel.def.path : "";
      defSel.disabled = !p || !defs.length;

      info.innerHTML = infoHtml(p);
    }

    function infoHtml(p) {
      const msg = message ? `<div class="mono">${esc(message)}</div>` : "";
      if (!p) {
        if (sel.path) return `<span class="pill neutral">file</span> <span class="mono" title="${esc(sel.path)}">${esc(baseName(sel.path))}</span> — not from a pack; checked when it is used.` + msg;
        const why = lib.error ? ` Packs could not be listed: <span class="mono">${esc(lib.error)}</span>` : "";
        return `Choose a pack and a ${esc(what)}, load a pack from disk, or use a single file.${why}` + msg;
      }
      const st = packState(p);
      const cls = st.level === "ok" ? "ok" : st.level === "bad" ? "bad" : "neutral";
      const meta = [p.license, p.source === "file" ? "loaded from file" : "installed"].filter(Boolean).map(esc).join(" · ");
      const notes = notesOf(p);
      const details = notes.length
        ? `<details${notesOf(p, kind).length ? " open" : ""}><summary>Skipped file${notes.length === 1 ? "" : "s"}</summary>` +
          notes.map((n) => `<div><span class="mono">${esc(n.file)}</span> — ${esc(n.message)}</div>`).join("") + `</details>`
        : "";
      const def = sel.def ? ` · <span class="mono" title="${esc(sel.def.path)}">${esc(sel.def.file)}</span>` : "";
      return `<span class="pill ${cls}">${esc(st.text)}</span> <span class="mono" title="${esc(p.dir)}">${esc(p.id)}</span>` +
        `${p.version ? ` · v${esc(p.version)}` : ""}${def}${meta ? ` · ${meta}` : ""}` + details + msg;
    }

    function refresh() {
      // Keep the selection pointing at the (possibly reloaded) pack object.
      if (restoring) {
        const r = restoring;
        restoring = null;
        if (r.path) return choosePath(r.path, false);
        if (r.packDir) return choosePack(r.packDir, false);
      }
      if (sel.packDir) {
        const p = currentPack();
        const d = p && sel.path ? defsOf(p, kind).find((x) => samePath(x.path, sel.path)) : null;
        sel = Object.assign({}, sel, { pack: p, def: d || null, path: p ? (d ? d.path : null) : sel.path, packDir: p ? p.dir : null });
      }
      render();
    }

    async function dialogPath(opts) {
      const r = await T().dialog.open(Object.assign({ multiple: false, directory: false, defaultPath: (window.OT && window.OT.packsDir) || undefined }, opts));
      return Array.isArray(r) ? r[0] : r;
    }

    packSel.addEventListener("change", () => choosePack(packSel.value, true));
    defSel.addEventListener("change", () => chooseDef(defSel.value, true));
    q("loadPack").addEventListener("click", async () => {
      try {
        const path = await dialogPath({ title: "Load a definition pack (choose its pack.json)", filters: [{ name: "Definition pack", extensions: ["json"] }] });
        if (!path) return;
        const pack = await openPack(path);
        choosePack(pack.dir, true);
      } catch (e) {
        message = "Load pack: " + errText(e);
        render();
      }
    });
    q("file").addEventListener("click", async () => {
      try {
        const path = await dialogPath({ filters: [filter] });
        if (path) { await library(); choosePath(path, true); }
      } catch (e) {
        message = "File: " + errText(e);
        render();
      }
    });

    const api = {
      el, kind, refresh,
      get: () => Object.assign({}, sel),
      choosePack: (dir) => choosePack(dir, true),
      chooseDef: (path) => chooseDef(path, true),
      choosePath: (path) => choosePath(path, true),
      clear: () => set({ path: null, packDir: null, pack: null, def: null }, true),
    };
    pickers.add(api);
    render();
    library().then(() => refresh());
    return api;
  }

  window.OTPacks = {
    picker, library, openPack, KIND_LABEL,
    // pure helpers, exported for the unit tests
    defsOf, packsFor, packLabel, defLabel, packState, mergePack, locate, samePath,
    _reset() { libPromise = null; libReady = false; lib = { root: null, packs: [], failed: [], error: null }; pickers.clear(); },
  };
})();
