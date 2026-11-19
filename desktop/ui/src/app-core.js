// OpenTune — PUBLIC calibration edition frontend: shared state, DOM helpers,
// file pickers and panel navigation. Uses withGlobalTauri:
// window.__TAURI__.core.invoke and .dialog.open/.save. No bundler, no framework.
//
// The app is split into plain scripts that share one global scope, loaded in
// this order by index.html (after util.js, ot-caps.js and nav.js):
//   app-core.js         state, $/el/toast/errBanner, pickers, showPanel, onPanelFirstShow
//   app-calibration.js  Image, Identify, Checksum, Analyze bin
//   app-maps.js         findmaps, map viewer/editor, Compare, 3-D view, XDF/A2L import
//   app-tools.js        Flash, Vault, Drivers, Guide, Utilities, Project, About & updates
//   app-boot.js         adapter status, Debug panel, connection bar, boot()
// Top-level code in one file may only use names from itself or earlier files;
// functions run later (handlers, boot) may use any of them.
"use strict";

const TAURI = window.__TAURI__ || {};
const invoke = (cmd, args) => TAURI.core.invoke(cmd, args);
const dialog = () => TAURI.dialog;

const state = {
  imagePath: null,
  hexOffset: 0,
  hexTotal: 0,
  fmStock: null,
  fmTuned: null,
  mvImage: null,
  flTarget: null,
  flData: null,
  vaultDir: null,
  defs: [],
  guideSections: [],
  guideCur: null,
  ed: null,       // active map-editor model
  edClamped: null, // Set of "r,c" cells clamped to a def bound on last op
  cmpStock: null,
  cmpTuned: null,
  view: "2d",
  impFile: null,
  impMaps: [],
  // Tuning project (.otproj): base image + per-map edits, not a copy of the bin.
  project: { path: null, name: "", notes: "", baseImagePath: "", created: null, imageStatus: null, edits: [] },
};

// ---- helpers ---------------------------------------------------------------
// Pure helpers (escaping, formatting, Markdown, …) come from util.js.
const {
  esc, baseName, hex8, parseHexInput, fmtCell, fmtAxis, clone2d, heatColor, splitList,
  confPill, listOrDash, imgBadge, editKey, cellsChanged, renderMarkdown,
} = window.OTUtil;
const $ = (id) => document.getElementById(id);
const el = (tag, cls, html) => { const e = document.createElement(tag); if (cls) e.className = cls; if (html != null) e.innerHTML = html; return e; };

let toastTimer;
function toast(msg, bad) {
  const t = $("toast");
  t.textContent = msg; t.className = "toast show" + (bad ? " bad" : "");
  clearTimeout(toastTimer); toastTimer = setTimeout(() => (t.className = "toast"), 2600);
}
function errBanner(e) { return `<div class="banner bad">${esc(typeof e === "string" ? e : (e && e.message) || e)}</div>`; }

async function pickFile(title, filters) {
  const opts = { title, multiple: false, directory: false };
  if (filters) opts.filters = filters;
  const r = await dialog().open(opts);
  return Array.isArray(r) ? r[0] : r;
}
async function pickDir(title) {
  const r = await dialog().open({ title, directory: true, multiple: false });
  return Array.isArray(r) ? r[0] : r;
}
async function pickSave(title, defaultPath) {
  return await dialog().save({ title, defaultPath, filters: [{ name: "Flash / dump", extensions: ["bin"] }] });
}
const binFilter = [
  { name: "Flash / dump", extensions: ["bin", "ori", "frf", "dat", "hex", "eep"] },
  { name: "All files", extensions: ["*"] },
];

// Startup trace mark: printed by the backend with OPENTUNE_TRACE_STARTUP=1, a
// no-op otherwise. Fire-and-forget; never throws.
function startupMark(label) {
  try {
    const ms = typeof performance !== "undefined" ? Math.round(performance.now()) : 0;
    const p = TAURI.core && TAURI.core.invoke("startup_mark", { label: `${label} (page +${ms} ms)` });
    if (p && p.catch) p.catch(() => {});
  } catch (e) { /* not in Tauri */ }
}
startupMark("app-core.js running");

// ---- navigation ------------------------------------------------------------
// Panels do their expensive work (backend loads, big renders) the first time
// they are shown, not at startup: onPanelFirstShow(id, fn) runs fn then (or
// right away when the panel has already been shown).
const panelShown = new Set();
const panelFirstShow = {};
function onPanelFirstShow(name, fn) {
  if (panelShown.has(name)) { try { fn(); } catch (e) {} return; }
  (panelFirstShow[name] = panelFirstShow[name] || []).push(fn);
}
window.onPanelFirstShow = onPanelFirstShow;
// #main is the content pane's own scroll container (the sidebar scrolls on its
// own): each panel keeps its scroll position across switches.
const panelScroll = {};
let currentPanel = null;
function showPanel(name) {
  const main = document.getElementById("main");
  const switching = currentPanel !== name;
  if (main && switching && currentPanel) panelScroll[currentPanel] = main.scrollTop;
  document.querySelectorAll(".panel").forEach((p) => p.classList.toggle("active", p.id === "panel-" + name));
  document.querySelectorAll(".nav-item").forEach((n) => n.classList.toggle("active", n.dataset.panel === name));
  if (main && switching) main.scrollTop = panelScroll[name] || 0;
  currentPanel = name;
  if (switching) {
    // An open custom listbox (listbox.js) belongs to the old view; the render
    // check (render-check.js, OPENTUNE_PERF) measures each panel's scrolling.
    if (window.OTListbox) window.OTListbox.closeAll();
    if (window.OTRender) window.OTRender.panelSwitched(name);
  }
  if (window.OTNav && window.OTNav.activated) window.OTNav.activated(name);
  // Counted for an embedder's automatic diagnostics summary (diagup.js).
  if (switching && window.OTDiagUp) window.OTDiagUp.panelOpened(name);
  if (!panelShown.has(name)) {
    panelShown.add(name);
    (panelFirstShow[name] || []).splice(0).forEach((fn) => { try { fn(); } catch (e) {} });
  }
}
document.querySelectorAll(".nav-item").forEach((n) => n.addEventListener("click", () => {
  showPanel(n.dataset.panel);
  // Re-read version + busy state each time the About panel is opened.
  if (n.dataset.panel === "about" && typeof refreshUpdateStatus === "function") refreshUpdateStatus();
}));

// Reflect the globally-opened image path into per-panel hints.
function refreshImageHints() {
  const n = state.imagePath ? baseName(state.imagePath) : null;
  $("imageHint").textContent = n ? "open: " + n : "no image opened";
  $("idPathHint").textContent = n ? "image: " + n : "no image opened";
  if ($("anPathHint")) $("anPathHint").textContent = n ? "uses opened: " + n : "no image opened";
  $("csPathHint").textContent = n ? "image: " + n : "no image opened";
  $("mvImgName").textContent = state.mvImage ? baseName(state.mvImage) : (n ? "uses opened: " + n : "uses opened image");
  $("flTargetName").textContent = state.flTarget ? baseName(state.flTarget) : (n ? "uses opened: " + n : "uses opened image");
}
