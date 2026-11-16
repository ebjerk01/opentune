// OpenTune — PUBLIC calibration edition frontend.
// Uses withGlobalTauri: window.__TAURI__.core.invoke and .dialog.open/.save.
// No bundler, no framework.
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
};

// ---- helpers ---------------------------------------------------------------
const $ = (id) => document.getElementById(id);
const el = (tag, cls, html) => { const e = document.createElement(tag); if (cls) e.className = cls; if (html != null) e.innerHTML = html; return e; };
const esc = (s) => String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const baseName = (p) => (p ? p.split(/[\\/]/).pop() : "—");

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

// ---- navigation ------------------------------------------------------------
function showPanel(name) {
  document.querySelectorAll(".panel").forEach((p) => p.classList.toggle("active", p.id === "panel-" + name));
  document.querySelectorAll(".nav-item").forEach((n) => n.classList.toggle("active", n.dataset.panel === name));
}
document.querySelectorAll(".nav-item").forEach((n) => n.addEventListener("click", () => showPanel(n.dataset.panel)));

// Reflect the globally-opened image path into per-panel hints.
function refreshImageHints() {
  const n = state.imagePath ? baseName(state.imagePath) : null;
  $("imageHint").textContent = n ? "open: " + n : "no image opened";
  $("idPathHint").textContent = n ? "image: " + n : "no image opened";
  $("csPathHint").textContent = n ? "image: " + n : "no image opened";
  $("mvImgName").textContent = state.mvImage ? baseName(state.mvImage) : (n ? "uses opened: " + n : "uses opened image");
  $("flTargetName").textContent = state.flTarget ? baseName(state.flTarget) : (n ? "uses opened: " + n : "uses opened image");
}

// =================== IMAGE ===================
async function loadImage(path) {
  if (!path) return;
  try {
    const info = await invoke("image_info", { path });
    state.imagePath = path;
    $("imageMeta").className = "meta-grid";
    $("imageMeta").innerHTML =
      `<div class="k">Name</div><div class="v">${esc(info.name)}</div>` +
      `<div class="k">Size</div><div class="v">${info.size.toLocaleString()} bytes (${(info.size / 1024).toFixed(1)} KiB)</div>` +
      `<div class="k">Path</div><div class="v">${esc(info.path)}</div>`;
    $("hexOffset").value = "0x00000000";
    await renderHex(0);
    refreshImageHints();
    toast("Loaded " + info.name);
  } catch (e) { $("imageMeta").className = ""; $("imageMeta").innerHTML = errBanner(e); }
}
async function renderHex(offset) {
  if (!state.imagePath) return;
  try {
    const r = await invoke("hex_preview", { path: state.imagePath, offset, rows: 16 });
    const lines = r.lines.map((l) => {
      const hx = l.hex.map((b, i) => `<span class="hb">${b}</span>` + (i === 7 ? " " : "")).join(" ");
      return `<span class="ha">${l.addr}</span>  ${hx}  <span class="hc">${esc(l.ascii)}</span>`;
    });
    $("hexView").className = "hexview";
    $("hexView").innerHTML = lines.join("\n") + `\n\n<span class="hc">offset 0x${r.offset.toString(16).padStart(8, "0").toUpperCase()} of ${r.total.toLocaleString()} bytes</span>`;
    $("hexOffset").value = "0x" + r.offset.toString(16).padStart(8, "0").toUpperCase();
    state.hexOffset = r.offset; state.hexTotal = r.total;
  } catch (e) { $("hexView").innerHTML = errBanner(e); }
}
$("btnOpenImage").onclick = async () => { const p = await pickFile("Open flash dump", binFilter); if (p) loadImage(p); };
$("hexPrev").onclick = () => renderHex(Math.max(0, (state.hexOffset || 0) - 256));
$("hexNext").onclick = () => renderHex(Math.min(Math.max(0, (state.hexTotal || 256) - 1), (state.hexOffset || 0) + 256));
$("hexOffset").onchange = () => { const v = parseInt($("hexOffset").value.replace(/^0x/i, ""), 16); if (!isNaN(v)) renderHex(v); };

// =================== IDENTIFY ===================
function defCard(m) {
  if (!m) return `<div class="banner info">No OpenTune definition matched.</div>`;
  return `<div class="banner ok">Matched definition: <b>${esc(m.name)}</b></div>
    <div class="meta-grid">
      <div class="k">id</div><div class="v">${esc(m.id)}</div>
      <div class="k">maker</div><div class="v">${esc(m.maker)} · ${esc(m.family)}</div>
      <div class="k">protocol</div><div class="v">${esc(m.protocol)}</div>
      <div class="k">checksum</div><div class="v">${esc(m.checksum || "—")}</div>
      <div class="k">seed/key</div><div class="v">${esc(m.seedkey || "—")}</div>
    </div>`;
}
function catTable(list) {
  if (!list || !list.length) return `<p class="muted small">No catalog candidates.</p>`;
  const rows = list.map((c) => `<tr><td>${esc(c.make)}</td><td>${esc((c.models || []).slice(0, 4).join(", "))}</td><td>${esc(c.years || "—")}</td><td>${esc(c.family || "—")}</td><td class="mono">${esc(c.source || "—")}</td></tr>`).join("");
  return `<h3>Catalog candidates (${list.length})</h3>
    <table class="tbl"><thead><tr><th>Make</th><th>Models</th><th>Years</th><th>Family</th><th>Source</th></tr></thead><tbody>${rows}</tbody></table>`;
}
$("btnIdentify").onclick = async () => {
  if (!state.imagePath) return toast("Open an image first", true);
  try {
    const r = await invoke("identify", { path: state.imagePath });
    $("idResult").innerHTML =
      `<div class="meta-grid">
        <div class="k">ECU name</div><div class="v">${esc(r.ecu_name || "—")}</div>
        <div class="k">HW #</div><div class="v">${esc(r.hw_number || "—")}</div>
        <div class="k">SW #</div><div class="v">${esc(r.sw_number || "—")}</div>
        <div class="k">VIN</div><div class="v">${esc(r.vin || "—")}</div>
        <div class="k">size</div><div class="v">${r.size.toLocaleString()} bytes</div>
      </div>` + defCard(r.matched) + catTable(r.catalog);
  } catch (e) { $("idResult").innerHTML = errBanner(e); }
};
$("btnMatchId").onclick = async () => {
  try {
    const r = await invoke("match_identity", { hw: $("idHw").value, sw: $("idSw").value });
    $("matchResult").innerHTML = defCard(r.matched) + catTable(r.catalog);
  } catch (e) { $("matchResult").innerHTML = errBanner(e); }
};
document.querySelectorAll("#panel-identify .chip").forEach((c) => c.onclick = () => { $("idHw").value = c.dataset.hw; $("idSw").value = c.dataset.sw; $("btnMatchId").click(); });

// =================== CHECKSUM ===================
function renderChecksum(blocks) {
  if (!blocks.length) { $("csResult").innerHTML = `<div class="banner info">Algorithm reported no protected blocks for this image — check you picked the right family.</div>`; return; }
  const allok = blocks.every((b) => b.ok);
  const rows = blocks.map((b) => `<tr><td>${esc(b.name)}</td><td><span class="pill ${b.ok ? "ok" : "bad"}">${b.ok ? "OK" : "BAD"}</span></td><td class="mono">${esc(b.region)}</td><td class="mono">${esc(b.stored)}</td><td class="mono">${esc(b.computed)}</td></tr>`).join("");
  $("csResult").innerHTML = `<div class="banner ${allok ? "ok" : "bad"}">${allok ? "All blocks OK" : "One or more blocks are BAD — run Fix to correct them."}</div>
    <table class="tbl"><thead><tr><th>Block</th><th>Status</th><th>Region</th><th>Stored</th><th>Computed</th></tr></thead><tbody>${rows}</tbody></table>`;
}
$("btnCsVerify").onclick = async () => {
  if (!state.imagePath) return toast("Open an image first", true);
  try { renderChecksum(await invoke("checksum_verify", { path: state.imagePath, algo: $("csAlgo").value })); }
  catch (e) { $("csResult").innerHTML = errBanner(e); }
};
async function doFix(outPath) {
  if (!state.imagePath) return toast("Open an image first", true);
  try {
    const r = await invoke("checksum_fix", { path: state.imagePath, algo: $("csAlgo").value, outPath: outPath || null });
    const savedElsewhere = r.out_path !== state.imagePath;
    $("csResult").innerHTML = (r.fixes.length
      ? `<div class="banner ok">Applied ${r.fixes.length} fix(es)${savedElsewhere ? " and saved a copy" : " and saved"}.</div><pre class="hexview">${esc(r.fixes.join("\n"))}</pre>`
      : `<div class="banner info">Nothing to fix — checksums already correct.${savedElsewhere ? " (Copy written.)" : ""}</div>`)
      + (savedElsewhere ? `<div class="meta-grid" style="margin-top:8px"><div class="k">saved to</div><div class="v">${esc(r.out_path)}</div></div>` : "");
    // If we wrote a copy, switch to it so re-verify reflects the corrected file.
    if (savedElsewhere) { await loadImage(r.out_path); showPanel("checksum"); }
    else if (state.imagePath) renderHex(state.hexOffset || 0);
  } catch (e) {
    const msg = typeof e === "string" ? e : (e && e.message) || "";
    $("csResult").innerHTML = errBanner(e) + (/permission denied|read-only/i.test(msg)
      ? `<div class="note">The image can't be written in place. Use <b>Fix → Save as…</b> to write a corrected copy you own.</div>` : "");
  }
}
$("btnCsFix").onclick = () => doFix(null);
$("btnCsFixAs").onclick = async () => {
  if (!state.imagePath) return toast("Open an image first", true);
  const def = baseName(state.imagePath).replace(/\.(\w+)$/, "") + "-fixed.bin";
  const out = await pickSave("Save corrected image as", def);
  if (out) doFix(out);
};

// =================== MAPS: findmaps ===================
$("fmStock").onclick = async () => { const p = await pickFile("Stock file", binFilter); if (p) { state.fmStock = p; $("fmStockName").textContent = baseName(p); } };
$("fmTuned").onclick = async () => { const p = await pickFile("Tuned file", binFilter); if (p) { state.fmTuned = p; $("fmTunedName").textContent = baseName(p); } };
$("btnFindmaps").onclick = async () => {
  if (!state.fmStock || !state.fmTuned) return toast("Pick stock + tuned files", true);
  try {
    const regs = await invoke("findmaps", { stock: state.fmStock, tuned: state.fmTuned, gap: +$("fmGap").value || 0, minLen: +$("fmMin").value || 1 });
    if (!regs.length) { $("fmResult").innerHTML = `<div class="banner info">No differing regions found with these settings.</div>`; return; }
    const rows = regs.map((r) => `<tr><td>${r.index}</td><td class="mono">${esc(r.start)}</td><td class="mono">${esc(r.end)}</td><td>${r.len}</td><td class="mono">${esc(r.hint || "—")}</td></tr>`).join("");
    $("fmResult").innerHTML = `<div class="banner ok">${regs.length} candidate region(s).</div>
      <table class="tbl"><thead><tr><th>#</th><th>Start</th><th>End</th><th>Len</th><th>Stride hint</th></tr></thead><tbody>${rows}</tbody></table>`;
  } catch (e) { $("fmResult").innerHTML = errBanner(e); }
};

// =================== MAPS: viewer (read-only) ===================
async function populateMapDefs() {
  try {
    state.defs = await invoke("list_ecudefs");
    const withMaps = state.defs.filter((d) => d.maps > 0);
    const list = withMaps.length ? withMaps : state.defs;
    const fill = (sel) => { sel.innerHTML = ""; list.forEach((d) => { const o = el("option"); o.value = d.id; o.textContent = `${d.name} (${d.maps} maps)`; sel.appendChild(o); }); };
    fill($("mvDef"));
    fill($("cmpDef"));
    await populateMaps();
  } catch (e) { toast("ecudefs: " + e, true); }
}
async function populateMaps() {
  const id = $("mvDef").value; if (!id) return;
  try {
    const maps = await invoke("list_maps", { ecudefId: id });
    const sel = $("mvMap"); sel.innerHTML = "";
    if (!maps.length) { const o = el("option"); o.value = ""; o.textContent = "(no maps defined)"; sel.appendChild(o); return; }
    maps.forEach((m) => { const o = el("option"); o.value = m.name; o.textContent = `${m.name} — ${m.rows}×${m.cols} ${m.data_type} @${m.address}`; sel.appendChild(o); });
  } catch (e) { toast(e, true); }
}
$("mvDef").onchange = populateMaps;
$("mvPick").onclick = async () => { const p = await pickFile("Image for map decode", binFilter); if (p) { state.mvImage = p; refreshImageHints(); } };

function heatColor(t) { // 0..1 -> blue→green→yellow→orange→red
  const stops = [[43, 108, 176], [56, 161, 105], [214, 158, 46], [221, 107, 32], [197, 48, 48]];
  const x = Math.max(0, Math.min(1, t)) * (stops.length - 1);
  const i = Math.floor(x), f = x - i, a = stops[i], b = stops[Math.min(i + 1, stops.length - 1)];
  return `rgb(${Math.round(a[0] + (b[0] - a[0]) * f)},${Math.round(a[1] + (b[1] - a[1]) * f)},${Math.round(a[2] + (b[2] - a[2]) * f)})`;
}
const fmtCell = (v) => (Math.abs(v) >= 100 ? (+v).toFixed(1) : (+v).toFixed(2));
const fmtAxis = (v) => (v == null ? "" : (v % 1 ? (+v).toFixed(1) : String(+v)));

const clone2d = (g) => g.map((r) => r.slice());

$("btnDecodeMap").onclick = async () => {
  const path = state.mvImage || state.imagePath;
  if (!path) return toast("Open an image or pick one", true);
  const id = $("mvDef").value, name = $("mvMap").value;
  if (!id || !name) return toast("Pick a definition and a map", true);
  try {
    const m = await invoke("decode_map", { path, ecudefId: id, mapName: name });
    // Build the editor model from the decode.
    state.ed = {
      path, defId: id, mapName: name,
      rows: m.rows, cols: m.cols, unit: m.unit || "",
      dataType: m.data_type, endian: m.endian, address: m.address,
      factor: m.scaling.factor, offset: m.scaling.offset,
      defMin: m.def_min, defMax: m.def_max,
      notes: m.notes || "",
      xAxis: m.x_axis || { values: [] }, yAxis: m.y_axis || { values: [] },
      orig: clone2d(m.cells), cells: clone2d(m.cells),
      sel: null, anchor: null, drag: false,
      undo: [], redo: [],
    };
    state.edClamped = new Set();
    $("edToolbar").hidden = false;
    edRender();
  } catch (e) { state.ed = null; $("edToolbar").hidden = true; $("mvResult").innerHTML = errBanner(e); }
};

// ---- editor rendering ------------------------------------------------------
function edRange() {
  const ed = state.ed; let lo = Infinity, hi = -Infinity;
  for (const row of ed.cells) for (const v of row) { if (v < lo) lo = v; if (v > hi) hi = v; }
  if (!isFinite(lo)) { lo = 0; hi = 1; }
  if (lo === hi) hi = lo + 1;
  return { lo, hi, span: hi - lo };
}
function edRender() {
  const ed = state.ed; if (!ed) return;
  const { lo, hi, span } = edRange();
  const xv = ed.xAxis.values || [], yv = ed.yAxis.values || [];
  let thead = `<tr><th>${esc(ed.yAxis.name || "")}\\${esc(ed.xAxis.name || "")}</th>`;
  for (let c = 0; c < ed.cols; c++) thead += `<th>${xv[c] != null ? fmtAxis(xv[c]) : c}</th>`;
  thead += "</tr>";
  let body = "";
  for (let r = 0; r < ed.rows; r++) {
    body += `<tr><th>${yv[r] != null ? fmtAxis(yv[r]) : r}</th>`;
    for (let c = 0; c < ed.cols; c++) {
      const v = ed.cells[r][c], t = (v - lo) / span;
      const dirty = v !== ed.orig[r][c] ? " dirty" : "";
      const sel = edInSel(r, c) ? " sel" : "";
      const clamp = state.edClamped && state.edClamped.has(r + "," + c) ? " clamped" : "";
      body += `<td class="cell${dirty}${sel}${clamp}" data-r="${r}" data-c="${c}" style="background:${heatColor(t)}" title="[${r},${c}] was ${fmtCell(ed.orig[r][c])} → ${fmtCell(v)} ${esc(ed.unit)}">${fmtCell(v)}</td>`;
    }
    body += "</tr>";
  }
  const scl = `× ${ed.factor}${ed.offset ? " + " + ed.offset : ""} → ${esc(ed.unit || "raw")}`;
  const bounds = (ed.defMin != null || ed.defMax != null)
    ? `${ed.defMin != null ? fmtCell(ed.defMin) : "−∞"} … ${ed.defMax != null ? fmtCell(ed.defMax) : "+∞"} ${esc(ed.unit)}` : "none";
  $("mvResult").innerHTML =
    `<div class="meta-grid">
      <div class="k">address</div><div class="v">${esc(ed.address)}</div>
      <div class="k">size</div><div class="v">${ed.rows}×${ed.cols} ${esc(ed.dataType)} ${esc(ed.endian)}</div>
      <div class="k">scaling</div><div class="v">${scl}</div>
      <div class="k">def bounds</div><div class="v">${bounds}</div>
    </div>
    ${ed.notes ? `<div class="note">${esc(ed.notes)}</div>` : ""}
    <div class="heatwrap"><table class="heat"><thead>${thead}</thead><tbody>${body}</tbody></table></div>
    <div class="legend"><span>${fmtCell(lo)}</span><span class="bar"></span><span>${fmtCell(hi)} ${esc(ed.unit)}</span> &nbsp; <span>X: ${esc(ed.xAxis.unit || "idx")} · Y: ${esc(ed.yAxis.unit || "idx")}</span></div>`;
  edUpdateToolbar();
}

// ---- selection -------------------------------------------------------------
function edNormSel() {
  const s = state.ed.sel; if (!s) return null;
  return { r0: Math.min(s.r0, s.r1), r1: Math.max(s.r0, s.r1), c0: Math.min(s.c0, s.c1), c1: Math.max(s.c0, s.c1) };
}
function edInSel(r, c) {
  const n = edNormSel(); if (!n) return false;
  return r >= n.r0 && r <= n.r1 && c >= n.c0 && c <= n.c1;
}
function edPaintSelection() {
  document.querySelectorAll("#mvResult td.cell").forEach((td) => {
    const r = +td.dataset.r, c = +td.dataset.c;
    td.classList.toggle("sel", edInSel(r, c));
  });
  edUpdateToolbar();
}
function edUpdateToolbar() {
  const ed = state.ed; if (!ed) return;
  const n = edNormSel();
  $("edSel").textContent = n
    ? `selection: [${n.r0},${n.c0}]–[${n.r1},${n.c1}] (${(n.r1 - n.r0 + 1) * (n.c1 - n.c0 + 1)} cells)`
    : "no selection";
  $("edUndo").disabled = ed.undo.length === 0;
  $("edRedo").disabled = ed.redo.length === 0;
}

// Mouse selection via event delegation on the result container.
const mvEl = $("mvResult");
mvEl.addEventListener("mousedown", (e) => {
  const td = e.target.closest && e.target.closest("td.cell"); if (!td || !state.ed) return;
  if (e.target.classList.contains("cell-input")) return;
  e.preventDefault();
  const r = +td.dataset.r, c = +td.dataset.c;
  if (e.shiftKey && state.ed.anchor) {
    state.ed.sel = { r0: state.ed.anchor.r, c0: state.ed.anchor.c, r1: r, c1: c };
  } else {
    state.ed.anchor = { r, c };
    state.ed.sel = { r0: r, c0: c, r1: r, c1: c };
  }
  state.ed.drag = true;
  edPaintSelection();
});
mvEl.addEventListener("mouseover", (e) => {
  if (!state.ed || !state.ed.drag) return;
  const td = e.target.closest && e.target.closest("td.cell"); if (!td) return;
  state.ed.sel = { r0: state.ed.anchor.r, c0: state.ed.anchor.c, r1: +td.dataset.r, c1: +td.dataset.c };
  edPaintSelection();
});
document.addEventListener("mouseup", () => { if (state.ed) state.ed.drag = false; });

// Double-click a cell to type a value.
mvEl.addEventListener("dblclick", (e) => {
  const td = e.target.closest && e.target.closest("td.cell"); if (!td || !state.ed) return;
  const r = +td.dataset.r, c = +td.dataset.c;
  const cur = state.ed.cells[r][c];
  td.innerHTML = `<input class="cell-input" value="${fmtCell(cur)}" />`;
  const inp = td.querySelector("input"); inp.focus(); inp.select();
  const commit = (save) => {
    if (save) {
      const v = parseFloat(inp.value);
      if (!isNaN(v)) { edPushUndo(); state.edClamped = new Set(); state.ed.cells[r][c] = edClampVal(v, r, c); }
    }
    edRender();
  };
  inp.addEventListener("keydown", (ev) => { if (ev.key === "Enter") commit(true); else if (ev.key === "Escape") commit(false); });
  inp.addEventListener("blur", () => commit(true));
});

// ---- edit operations -------------------------------------------------------
function edClampVal(v, r, c) {
  const ed = state.ed; let out = v;
  if (ed.defMin != null && out < ed.defMin) out = ed.defMin;
  if (ed.defMax != null && out > ed.defMax) out = ed.defMax;
  if (out !== v) state.edClamped.add(r + "," + c);
  return out;
}
function edPushUndo() {
  const ed = state.ed; ed.undo.push(clone2d(ed.cells)); if (ed.undo.length > 60) ed.undo.shift(); ed.redo = [];
}
function edApply(fn) {
  const ed = state.ed; const n = edNormSel();
  if (!n) return toast("Select cells first", true);
  edPushUndo(); state.edClamped = new Set();
  let clamped = 0;
  for (let r = n.r0; r <= n.r1; r++) for (let c = n.c0; c <= n.c1; c++) {
    const nv = edClampVal(fn(ed.cells[r][c], r, c), r, c);
    if (state.edClamped.has(r + "," + c)) clamped++;
    ed.cells[r][c] = nv;
  }
  edRender();
  if (clamped) toast(`${clamped} cell(s) clamped to the definition's bounds`, true);
}
const edOperand = () => { const v = parseFloat($("edVal").value); return isNaN(v) ? 0 : v; };
$("edSet").onclick = () => edApply(() => edOperand());
$("edAdd").onclick = () => edApply((v) => v + edOperand());
$("edSub").onclick = () => edApply((v) => v - edOperand());
$("edMul").onclick = () => edApply((v) => v * (edOperand() / 100));
$("edSmooth").onclick = () => {
  const ed = state.ed, n = edNormSel(); if (!n) return toast("Select cells first", true);
  const src = clone2d(ed.cells);
  edApply((_, r, c) => {
    let sum = 0, cnt = 0;
    for (let dr = -1; dr <= 1; dr++) for (let dc = -1; dc <= 1; dc++) {
      const rr = r + dr, cc = c + dc;
      if (rr >= 0 && rr < ed.rows && cc >= 0 && cc < ed.cols) { sum += src[rr][cc]; cnt++; }
    }
    return sum / cnt;
  });
};
$("edInterp").onclick = () => {
  const ed = state.ed, n = edNormSel(); if (!n) return toast("Select a rectangle first", true);
  if (n.r0 === n.r1 && n.c0 === n.c1) return toast("Interpolate needs more than one cell", true);
  const c00 = ed.cells[n.r0][n.c0], c01 = ed.cells[n.r0][n.c1];
  const c10 = ed.cells[n.r1][n.c0], c11 = ed.cells[n.r1][n.c1];
  const rows = n.r1 - n.r0, cols = n.c1 - n.c0;
  edApply((_, r, c) => {
    const u = rows ? (r - n.r0) / rows : 0, w = cols ? (c - n.c0) / cols : 0;
    const top = c00 + (c01 - c00) * w, bot = c10 + (c11 - c10) * w;
    return top + (bot - top) * u;
  });
};
$("edSelAll").onclick = () => { if (!state.ed) return; state.ed.anchor = { r: 0, c: 0 }; state.ed.sel = { r0: 0, c0: 0, r1: state.ed.rows - 1, c1: state.ed.cols - 1 }; edPaintSelection(); };
$("edSelNone").onclick = () => { if (!state.ed) return; state.ed.sel = null; edPaintSelection(); };
$("edUndo").onclick = () => { const ed = state.ed; if (!ed || !ed.undo.length) return; ed.redo.push(clone2d(ed.cells)); ed.cells = ed.undo.pop(); state.edClamped = new Set(); edRender(); };
$("edRedo").onclick = () => { const ed = state.ed; if (!ed || !ed.redo.length) return; ed.undo.push(clone2d(ed.cells)); ed.cells = ed.redo.pop(); state.edClamped = new Set(); edRender(); };
$("edReset").onclick = () => { const ed = state.ed; if (!ed) return; edPushUndo(); ed.cells = clone2d(ed.orig); state.edClamped = new Set(); edRender(); toast("Reset to decoded values"); };

// ---- write-back ------------------------------------------------------------
async function edDoWrite(outPath) {
  const ed = state.ed; if (!ed) return;
  const dirty = ed.cells.some((row, r) => row.some((v, c) => v !== ed.orig[r][c]));
  if (!dirty && !outPath) return toast("No edits to write", true);
  try {
    const r = await invoke("map_write", {
      path: ed.path, ecudefId: ed.defId, mapName: ed.mapName,
      newCells: ed.cells, outPath: outPath || null, fixChecksum: $("edFixCs").checked,
    });
    let cs = "";
    if (r.checksum && r.checksum.error) cs = `<div class="note">checksum: ${esc(r.checksum.error)}</div>`;
    else if (r.checksum && r.checksum.fixes) cs = r.checksum.fixes.length
      ? `<div class="banner ok">Checksum (${esc(r.checksum_algo || "")}): ${r.checksum.fixes.length} fix(es) applied.</div><pre class="hexview">${esc(r.checksum.fixes.join("\n"))}</pre>`
      : `<div class="banner info">Checksum (${esc(r.checksum_algo || "")}): already correct.</div>`;
    $("mvResult").insertAdjacentHTML("afterbegin",
      `<div class="banner ok">Wrote ${r.cells_changed} changed cell(s) to <b>${esc(baseName(r.out_path))}</b>${r.in_place ? " (in place)" : " (copy)"}.</div>
       <div class="meta-grid"><div class="k">safety backup</div><div class="v">${esc(r.backup)}</div><div class="k">output</div><div class="v">${esc(r.out_path)}</div></div>${cs}`);
    // The written file is now the baseline; re-decode from it so further edits stack cleanly.
    ed.path = r.out_path; ed.orig = clone2d(ed.cells); state.edClamped = new Set();
    if (r.in_place && (ed.path === state.imagePath)) renderHex(state.hexOffset || 0);
    edRender();
    toast("Map written · backup saved");
  } catch (e) {
    const msg = typeof e === "string" ? e : (e && e.message) || "";
    $("mvResult").insertAdjacentHTML("afterbegin", errBanner(e) + (/permission denied|read-only/i.test(msg)
      ? `<div class="note">Can't write in place — use <b>Write → Save as…</b> to write a corrected copy you own. Your original is backed up.</div>` : ""));
  }
}
$("edWrite").onclick = () => edDoWrite(null);
$("edWriteAs").onclick = async () => {
  if (!state.ed) return;
  const def = baseName(state.ed.path).replace(/\.(\w+)$/, "") + "-edit.bin";
  const out = await pickSave("Write edited image as", def);
  if (out) edDoWrite(out);
};

// =================== COMPARE ===================
$("cmpStock").onclick = async () => { const p = await pickFile("Stock image", binFilter); if (p) { state.cmpStock = p; $("cmpStockName").textContent = baseName(p); } };
$("cmpTuned").onclick = async () => { const p = await pickFile("Tuned image", binFilter); if (p) { state.cmpTuned = p; $("cmpTunedName").textContent = baseName(p); } };
$("btnCompare").onclick = async () => {
  const id = $("cmpDef").value;
  if (!id) return toast("Pick a definition", true);
  if (!state.cmpStock || !state.cmpTuned) return toast("Pick stock + tuned images", true);
  $("cmpGrid").innerHTML = "";
  try {
    const r = await invoke("compare_maps", { defId: id, stock: state.cmpStock, tuned: state.cmpTuned });
    const rows = r.maps.map((m) => {
      if (m.out_of_range) return `<tr><td>${esc(m.name)}</td><td>${m.rows}×${m.cols}</td><td colspan="3" class="muted">out of range — skipped</td><td></td></tr>`;
      const unit = m.unit ? " " + esc(m.unit) : "";
      const btn = m.changed ? `<button class="btn sm" data-cmpmap="${esc(m.name)}">grid</button>` : "";
      return `<tr><td>${esc(m.name)}</td><td>${m.rows}×${m.cols}</td>
        <td>${m.changed ? `<span class="pill accent">${m.changed}</span>` : '<span class="pill neutral">0</span>'}</td>
        <td class="mono">${m.changed ? (m.d_max >= 0 ? "+" : "") + fmtCell(m.d_max) + unit : "—"}</td>
        <td class="mono">${m.changed ? (m.d_avg >= 0 ? "+" : "") + fmtCell(m.d_avg) + unit : "—"}</td>
        <td>${btn}</td></tr>`;
    }).join("");
    $("cmpResult").innerHTML =
      `<div class="banner ${r.total_changed ? "ok" : "info"}">${r.changed_maps} of ${r.maps.length} map(s) changed · ${r.total_changed} cell(s) total · ${(r.stock_size / 1024).toFixed(0)} KiB → ${(r.tuned_size / 1024).toFixed(0)} KiB</div>
      <table class="tbl"><thead><tr><th>Map</th><th>Size</th><th>Changed</th><th>Δmax</th><th>Δavg</th><th></th></tr></thead><tbody>${rows}</tbody></table>`;
    $("cmpResult").querySelectorAll("button[data-cmpmap]").forEach((b) => b.onclick = () => showCompareGrid(id, b.dataset.cmpmap));
  } catch (e) { $("cmpResult").innerHTML = errBanner(e); $("cmpGrid").innerHTML = ""; }
};
async function showCompareGrid(id, name) {
  try {
    const r = await invoke("compare_maps", { defId: id, stock: state.cmpStock, tuned: state.cmpTuned, mapName: name });
    const m = r.maps.find((x) => x.name === name);
    if (!m || !m.stock) { $("cmpGrid").innerHTML = `<div class="banner info">No grid for this map.</div>`; return; }
    // Shared colour scale across both grids.
    let lo = Infinity, hi = -Infinity;
    for (const g of [m.stock, m.tuned]) for (const row of g) for (const v of row) { if (v < lo) lo = v; if (v > hi) hi = v; }
    if (!isFinite(lo)) { lo = 0; hi = 1; } if (lo === hi) hi = lo + 1;
    const span = hi - lo;
    const xv = (m.x_axis && m.x_axis.values) || [], yv = (m.y_axis && m.y_axis.values) || [];
    const grid = (cells, other) => {
      let thead = `<tr><th>\\</th>`;
      for (let c = 0; c < m.cols; c++) thead += `<th>${xv[c] != null ? fmtAxis(xv[c]) : c}</th>`;
      thead += "</tr>";
      let body = "";
      for (let r = 0; r < m.rows; r++) {
        body += `<tr><th>${yv[r] != null ? fmtAxis(yv[r]) : r}</th>`;
        for (let c = 0; c < m.cols; c++) {
          const v = cells[r][c], t = (v - lo) / span;
          const ch = other && Math.abs(v - other[r][c]) > 1e-9 ? " changed" : "";
          body += `<td class="cell${ch}" style="background:${heatColor(t)}" title="${fmtCell(v)} ${esc(m.unit || "")}">${fmtCell(v)}</td>`;
        }
        body += "</tr>";
      }
      return `<div class="heatwrap"><table class="heat cmp"><thead>${thead}</thead><tbody>${body}</tbody></table></div>`;
    };
    $("cmpGrid").innerHTML =
      `<h3>${esc(name)} — before → after (changed cells outlined)</h3>
       <div class="cmp-grids">
         <div><h4>Stock</h4>${grid(m.stock, m.tuned)}</div>
         <div><h4>Tuned</h4>${grid(m.tuned, m.stock)}</div>
       </div>
       <div class="legend"><span>${fmtCell(lo)}</span><span class="bar"></span><span>${fmtCell(hi)} ${esc(m.unit || "")}</span></div>`;
  } catch (e) { $("cmpGrid").innerHTML = errBanner(e); }
}

// =================== FLASH ===================
$("flTarget").onclick = async () => { const p = await pickFile("Target image", binFilter); if (p) { state.flTarget = p; refreshImageHints(); } };
$("flData").onclick = async () => { const p = await pickFile("Data file to write", binFilter); if (p) { state.flData = p; $("flDataName").textContent = baseName(p); } };
$("btnFlash").onclick = async () => {
  const path = state.flTarget || state.imagePath;
  if (!path) return toast("Pick a target image", true);
  if (!state.flData) return toast("Pick a data file", true);
  try {
    const r = await invoke("flash", { path, addr: $("flAddr").value, dataPath: state.flData, minMv: +$("flMv").value || 0 });
    $("flResult").innerHTML = `<div class="banner ${r.verified ? "ok" : "info"}">Flash complete${r.verified ? " — read-back verified ✓" : ""}</div>
      <div class="meta-grid">
        <div class="k">region</div><div class="v">${esc(r.region)}</div>
        <div class="k">bytes written</div><div class="v">${r.bytes_written.toLocaleString()}</div>
        <div class="k">battery</div><div class="v">${r.battery_mv != null ? r.battery_mv + " mV" : "—"}</div>
        <div class="k">backup</div><div class="v">${r.backup_len.toLocaleString()} bytes saved</div>
        <div class="k">verified</div><div class="v">${r.verified}</div>
      </div>`;
  } catch (e) { $("flResult").innerHTML = errBanner(e); }
};

// =================== VAULT ===================
async function refreshVault() {
  if (!state.vaultDir) return;
  try {
    const list = await invoke("vault_list", { dir: state.vaultDir });
    if (!list.length) { $("vaultResult").innerHTML = `<div class="banner info">No .bin backups in this folder.</div>`; return; }
    const rows = list.map((v) => `<tr><td class="mono">${esc(v.name)}</td><td>${v.size.toLocaleString()}</td><td>${v.has_meta ? '<span class="pill ok">meta</span>' : '<span class="pill neutral">none</span>'}</td><td><pre class="hexview" style="margin:0;font-size:11px">${esc(v.meta || "")}</pre></td></tr>`).join("");
    $("vaultResult").innerHTML = `<div class="banner ok">${list.length} backup(s).</div>
      <table class="tbl"><thead><tr><th>File</th><th>Bytes</th><th>Sidecar</th><th>Meta</th></tr></thead><tbody>${rows}</tbody></table>`;
  } catch (e) { $("vaultResult").innerHTML = errBanner(e); }
}
$("btnVaultPick").onclick = async () => { const d = await pickDir("Backups folder"); if (d) { state.vaultDir = d; $("vaultDir").textContent = d; refreshVault(); } };
$("btnVaultRefresh").onclick = refreshVault;

// =================== DRIVERS ===================
async function initDrivers() {
  try {
    const list = await invoke("list_drivers");
    const rows = list.map((d) => `<tr><td class="mono">${esc(d.id)}</td><td>${esc(d.name)}</td><td>${d.id === "mock" ? '<span class="pill ok">offline</span>' : '<span class="pill neutral">hardware</span>'}</td></tr>`).join("");
    $("driversResult").innerHTML = `<div class="banner info">${list.length} driver(s) registered. Offline work uses <code>mock</code>.</div>
      <table class="tbl"><thead><tr><th>ID</th><th>Name</th><th>Mode</th></tr></thead><tbody>${rows}</tbody></table>`;
    const sel = $("flDriver"); sel.innerHTML = "";
    list.forEach((d) => { const o = el("option"); o.value = d.id; o.textContent = `${d.name} (${d.id})`; sel.appendChild(o); });
    sel.value = "mock";
  } catch (e) { $("driversResult").innerHTML = errBanner(e); }
}

// =================== GUIDE (offline markdown) ===================
// Minimal, dependency-free Markdown → HTML: headings, bold/italic/code,
// fenced code, lists, tables, blockquotes, hr, paragraphs.
function mdInline(s) {
  let t = esc(s);
  t = t.replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`);
  t = t.replace(/\*\*([^*]+)\*\*/g, (_, c) => `<strong>${c}</strong>`);
  t = t.replace(/(^|[^*])\*([^*\n]+)\*/g, (_, p, c) => `${p}<em>${c}</em>`);
  t = t.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, txt, url) => `<span class="mdlink" title="${esc(url)}">${txt}</span>`);
  return t;
}
function renderMarkdown(md) {
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  let html = "", i = 0;
  let para = [];
  const flushPara = () => { if (para.length) { html += `<p>${mdInline(para.join(" "))}</p>`; } para = []; };
  while (i < lines.length) {
    let line = lines[i];
    if (/^```/.test(line)) {
      flushPara(); i++; let code = [];
      while (i < lines.length && !/^```/.test(lines[i])) { code.push(lines[i]); i++; }
      i++; html += `<pre class="mdcode">${esc(code.join("\n"))}</pre>`; continue;
    }
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|?[\s:-]*-[\s:|-]*$/.test(lines[i + 1])) {
      flushPara();
      const cells = (row) => row.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
      const head = cells(line); i += 2;
      let bodyrows = "";
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) { const c = cells(lines[i]); bodyrows += `<tr>${c.map((x) => `<td>${mdInline(x)}</td>`).join("")}</tr>`; i++; }
      html += `<table class="tbl mdtable"><thead><tr>${head.map((h) => `<th>${mdInline(h)}</th>`).join("")}</tr></thead><tbody>${bodyrows}</tbody></table>`;
      continue;
    }
    let hm = line.match(/^(#{1,4})\s+(.*)$/);
    if (hm) { flushPara(); const lvl = hm[1].length; html += `<h${lvl} class="mdh${lvl}">${mdInline(hm[2])}</h${lvl}>`; i++; continue; }
    if (/^\s*---+\s*$/.test(line)) { flushPara(); html += "<hr/>"; i++; continue; }
    if (/^\s*>\s?/.test(line)) {
      flushPara(); let q = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) { q.push(lines[i].replace(/^\s*>\s?/, "")); i++; }
      html += `<blockquote class="mdquote">${mdInline(q.join(" "))}</blockquote>`; continue;
    }
    if (/^\s*([-*+]|\d+\.)\s+/.test(line)) {
      flushPara();
      const ordered = /^\s*\d+\.\s+/.test(line);
      let items = [];
      while (i < lines.length && /^\s*([-*+]|\d+\.)\s+/.test(lines[i])) { items.push(lines[i].replace(/^\s*([-*+]|\d+\.)\s+/, "")); i++; }
      html += `<${ordered ? "ol" : "ul"} class="mdlist">${items.map((it) => `<li>${mdInline(it)}</li>`).join("")}</${ordered ? "ol" : "ul"}>`;
      continue;
    }
    if (/^\s*$/.test(line)) { flushPara(); i++; continue; }
    para.push(line.trim()); i++;
  }
  flushPara();
  return html;
}
async function loadGuideSection(id) {
  state.guideCur = id;
  document.querySelectorAll("#guideNav .gnav").forEach((b) => b.classList.toggle("active", b.dataset.id === id));
  try {
    const md = await invoke("guide_read", { id });
    $("guideBody").innerHTML = renderMarkdown(md);
    $("guideBody").scrollTop = 0;
  } catch (e) { $("guideBody").innerHTML = errBanner(e); }
}
async function initGuide() {
  try {
    state.guideSections = await invoke("guide_list");
    const nav = $("guideNav"); nav.innerHTML = "";
    if (!state.guideSections.length) { $("guideBody").innerHTML = `<div class="banner info">Guide resources not found in this build.</div>`; return; }
    state.guideSections.forEach((s) => {
      const b = el("button", "gnav"); b.dataset.id = s.id; b.textContent = s.title;
      b.onclick = () => loadGuideSection(s.id);
      nav.appendChild(b);
    });
    await loadGuideSection(state.guideSections[0].id);
  } catch (e) { $("guideBody").innerHTML = errBanner(e); }
}

// =================== BOOT ===================
async function boot() {
  if (!TAURI.core) { document.body.innerHTML = '<p style="padding:2rem">Tauri API unavailable — run inside the OpenTune app.</p>'; return; }
  try {
    const dp = await invoke("data_paths");
    $("dataStatus").innerHTML = `ecudefs: ${dp.ecudefs ? "✓" : "✗"} · catalog: ${dp.catalog ? "✓" : "✗"} · guide: ${dp.guide ? "✓" : "✗"}`;
  } catch (e) { $("dataStatus").textContent = "data resolve failed"; }
  await initDrivers();
  await populateMapDefs();
  await initGuide();
  showPanel("image");
  refreshImageHints();
}
boot();
