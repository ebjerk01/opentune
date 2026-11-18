// OpenTune — maps: findmaps diff, map viewer / editor and write-back, Compare,
// the 3-D surface / curve view and XDF/A2L import + region promotion.
// Loaded after app-calibration.js (see app-core.js for the script order).
"use strict";

// =================== MAPS: findmaps ===================
$("fmStock").onclick = async () => { const p = await pickFile("Stock file", binFilter); if (p) { state.fmStock = p; $("fmStockName").textContent = baseName(p); } };
$("fmTuned").onclick = async () => { const p = await pickFile("Tuned file", binFilter); if (p) { state.fmTuned = p; $("fmTunedName").textContent = baseName(p); } };
$("btnFindmaps").onclick = async () => {
  if (!state.fmStock || !state.fmTuned) return toast("Pick stock + tuned files", true);
  try {
    const regs = await invoke("findmaps", { stock: state.fmStock, tuned: state.fmTuned, gap: +$("fmGap").value || 0, minLen: +$("fmMin").value || 1 });
    if (!regs.length) { $("fmResult").innerHTML = `<div class="banner info">No differing regions found with these settings.</div>`; return; }
    const rows = regs.map((r) => `<tr><td>${r.index}</td><td class="mono">${esc(r.start)}</td><td class="mono">${esc(r.end)}</td><td>${r.len}</td><td class="mono">${esc(r.hint || "—")}</td><td><button class="btn sm" data-mk-start="${esc(r.start)}" data-mk-len="${r.len}">Make map</button></td></tr>`).join("");
    $("fmResult").innerHTML = `<div class="banner ok">${regs.length} candidate region(s). Use <b>Make map</b> to promote one into the editor.</div>
      <table class="tbl"><thead><tr><th>#</th><th>Start</th><th>End</th><th>Len</th><th>Stride hint</th><th></th></tr></thead><tbody>${rows}</tbody></table>`;
    $("fmResult").querySelectorAll("button[data-mk-start]").forEach((b) => b.onclick = () => prefillPromote(b.dataset.mkStart, +b.dataset.mkLen));
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
    return true;
  } catch (e) { toast("ecudefs: " + e, true); return false; }
}
// The definition lists load once, off the startup path: when Maps / Compare is
// first shown, when a project needs them, or in idle time after boot.
let defsReady = null;
function ensureDefs() {
  if (!defsReady) defsReady = populateMapDefs().then((ok) => { if (!ok) defsReady = null; });
  return defsReady;
}
onPanelFirstShow("maps", ensureDefs);
onPanelFirstShow("compare", ensureDefs);
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


// Build the shared editor model from a decode result. `source` is either
// { defId, mapName } for a bundled definition, or { adhoc: <MapDef JSON> } for
// an imported/promoted map (written back via mapdef_write).
function buildEditorModel(path, m, source) {
  state.ed = {
    path,
    defId: source.defId || null,
    mapName: source.mapName || m.name,
    adhoc: source.adhoc || null,
    rows: m.rows, cols: m.cols, unit: m.unit || "",
    dataType: m.data_type, endian: m.endian, address: m.address,
    factor: m.scaling.factor, offset: m.scaling.offset,
    defMin: m.def_min, defMax: m.def_max,
    notes: m.notes || "",
    xAxis: m.x_axis || { values: [] }, yAxis: m.y_axis || { values: [] },
    orig: clone2d(m.cells), cells: clone2d(m.cells),
    sel: null, anchor: null, drag: false,
    undo: [], redo: [],
    diff: false, // diff-overlay toggle: highlight cells changed vs `orig`
  };
  state.edClamped = new Set();
  $("edToolbar").hidden = false;
  $("viewToggle").hidden = false;
  setView("2d");
  edRender();
}

$("btnDecodeMap").onclick = async () => {
  const path = state.mvImage || state.imagePath;
  if (!path) return toast("Open an image or pick one", true);
  const id = $("mvDef").value, name = $("mvMap").value;
  if (!id || !name) return toast("Pick a definition and a map", true);
  try {
    const m = await invoke("decode_map", { path, ecudefId: id, mapName: name });
    buildEditorModel(path, m, { defId: id, mapName: name });
  } catch (e) { state.ed = null; $("edToolbar").hidden = true; $("viewToggle").hidden = true; $("mvResult").innerHTML = errBanner(e); }
};

// Open an ad-hoc MapDef (imported or promoted) in the editor.
async function openMapDef(path, mapdef) {
  try {
    const m = await invoke("decode_mapdef", { path, map: mapdef });
    buildEditorModel(path, m, { adhoc: m.mapdef });
    showPanel("maps");
    $("mvResult").scrollIntoView({ behavior: "smooth", block: "nearest" });
    toast("Opened " + (mapdef.name || "map") + " in the editor");
  } catch (e) { $("mvResult").innerHTML = errBanner(e); }
}

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
  let diffUp = 0, diffDown = 0;
  for (let r = 0; r < ed.rows; r++) {
    body += `<tr><th>${yv[r] != null ? fmtAxis(yv[r]) : r}</th>`;
    for (let c = 0; c < ed.cols; c++) {
      const v = ed.cells[r][c], t = (v - lo) / span;
      const d = v - ed.orig[r][c];
      const changed = Math.abs(d) > 1e-9;
      if (changed) { if (d > 0) diffUp++; else diffDown++; }
      // In diff-overlay mode a changed cell is tinted by direction (+/−);
      // otherwise the existing accent "dirty" ring marks any edit.
      const mark = changed ? (ed.diff ? (d > 0 ? " diff-up" : " diff-down") : " dirty") : "";
      const sel = edInSel(r, c) ? " sel" : "";
      const clamp = state.edClamped && state.edClamped.has(r + "," + c) ? " clamped" : "";
      const dtip = changed ? `[${r},${c}] was ${fmtCell(ed.orig[r][c])} → ${fmtCell(v)} (${d > 0 ? "+" : ""}${fmtCell(d)}) ${esc(ed.unit)}`
                           : `[${r},${c}] ${fmtCell(v)} ${esc(ed.unit)} (unchanged)`;
      body += `<td class="cell${mark}${sel}${clamp}" data-r="${r}" data-c="${c}" style="background:${heatColor(t)}" title="${dtip}">${fmtCell(v)}</td>`;
    }
    body += "</tr>";
  }
  ed._diffUp = diffUp; ed._diffDown = diffDown;
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
    <div class="legend"><span>${fmtCell(lo)}</span><span class="bar"></span><span>${fmtCell(hi)} ${esc(ed.unit)}</span> &nbsp; <span>X: ${esc(ed.xAxis.unit || "idx")} · Y: ${esc(ed.yAxis.unit || "idx")}</span></div>` +
    (ed.diff
      ? `<div class="legend difflegend"><span class="dsw up"></span> increased ${diffUp} <span class="dsw down"></span> decreased ${diffDown} &nbsp; <span class="muted">${diffUp + diffDown} of ${ed.rows * ed.cols} cell(s) changed vs the decoded baseline</span></div>`
      : "");
  edUpdateToolbar();
  if (state.view === "3d") drawPlot();
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
  const db = $("edDiff"); if (db) db.classList.toggle("active", !!ed.diff);
  const dc = $("edDiffCount");
  if (dc) {
    const changed = (ed._diffUp || 0) + (ed._diffDown || 0);
    dc.textContent = ed.diff ? `${changed} changed (▲${ed._diffUp || 0} / ▼${ed._diffDown || 0})` : (changed ? `${changed} edited cell(s)` : "");
  }
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
$("edDiff").onclick = () => { const ed = state.ed; if (!ed) return; ed.diff = !ed.diff; edRender(); };
$("edAddProject").onclick = () => { if (captureCurrentEdit(false)) { showPanel("project"); } };

// ---- write-back ------------------------------------------------------------
async function edDoWrite(outPath) {
  const ed = state.ed; if (!ed) return;
  const dirty = ed.cells.some((row, r) => row.some((v, c) => v !== ed.orig[r][c]));
  if (!dirty && !outPath) return toast("No edits to write", true);
  try {
    const r = ed.adhoc
      ? await invoke("mapdef_write", {
          path: ed.path, map: ed.adhoc,
          newCells: ed.cells, outPath: outPath || null,
          fixChecksum: $("edFixCs").checked, checksumAlgo: $("csAlgo") ? $("csAlgo").value : null,
        })
      : await invoke("map_write", {
          path: ed.path, ecudefId: ed.defId, mapName: ed.mapName,
          newCells: ed.cells, outPath: outPath || null, fixChecksum: $("edFixCs").checked,
        });
    let cs = "";
    // Which checksum family was used and where it came from (the matched
    // definition, or the Checksum panel as a fallback for imported maps).
    const csWho = esc(r.checksum_algo || "") + (r.checksum_source ? `, from ${esc(r.checksum_source)}` : "");
    if (r.checksum && r.checksum.error) cs = `<div class="note">checksum${r.checksum_source ? " (" + csWho + ")" : ""}: ${esc(r.checksum.error)}</div>`;
    else if (r.checksum && r.checksum.fixes) cs = r.checksum.fixes.length
      ? `<div class="banner ok">Checksum (${csWho}): ${r.checksum.fixes.length} fix(es) applied.</div><pre class="hexview">${esc(r.checksum.fixes.join("\n"))}</pre>`
      : `<div class="banner info">Checksum (${csWho}): already correct.</div>`;
    const summary =
      `<div class="banner ok">Wrote ${r.cells_changed} changed cell(s) to <b>${esc(baseName(r.out_path))}</b>${r.in_place ? " (in place)" : " (copy)"}.</div>
       <div class="meta-grid"><div class="k">safety backup</div><div class="v">${esc(r.backup)}</div><div class="k">output</div><div class="v">${esc(r.out_path)}</div></div>${cs}`;
    // The written file is now the baseline; re-decode from it so further edits stack cleanly.
    ed.path = r.out_path; ed.orig = clone2d(ed.cells); state.edClamped = new Set();
    if (r.in_place && (ed.path === state.imagePath)) renderHex(state.hexOffset || 0);
    // Re-render first: edRender() rewrites #mvResult, so the summary goes in after it.
    edRender();
    $("mvResult").insertAdjacentHTML("afterbegin", summary);
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

// =================== 3D SURFACE / CURVE VIEW ===================
// Self-contained canvas renderer — no external/CDN libraries. A 2-D map draws
// as a rotatable isometric surface coloured by the existing heat ramp; a 1-D
// curve (rows==1 or cols==1) draws as a line graph. Read-only visualization.
const plot = { az: -0.7, el: 0.5, drag: false, lx: 0, ly: 0 };

function setView(v) {
  state.view = v;
  $("btn2d").classList.toggle("active", v === "2d");
  $("btn3d").classList.toggle("active", v === "3d");
  $("mvResult").hidden = v === "3d";
  $("mvPlot").hidden = v !== "3d";
  const ed = state.ed;
  $("viewHint").textContent = ed ? (ed.rows === 1 || ed.cols === 1 ? "1-D curve" : "2-D surface · drag to orbit") : "";
  if (v === "3d") drawPlot();
}
const vt = $("viewToggle");
if (vt) { $("btn2d").onclick = () => setView("2d"); $("btn3d").onclick = () => setView("3d"); }

function cssVar(name, fb) { const v = getComputedStyle(document.body).getPropertyValue(name).trim(); return v || fb; }

function drawPlot() {
  const ed = state.ed, cv = $("plot3d"); if (!ed || !cv) return;
  const ctx = cv.getContext("2d"); const W = cv.width, H = cv.height;
  ctx.clearRect(0, 0, W, H);
  const ink = cssVar("--ink", "#1b1f27"), soft = cssVar("--ink-soft", "#596073");
  const { lo, hi, span } = edRange();
  if (ed.rows === 1 || ed.cols === 1) { drawCurve(ctx, W, H, ed, lo, hi, span, ink, soft); return; }
  drawSurface(ctx, W, H, ed, lo, hi, span, ink, soft);
}

function drawSurface(ctx, W, H, ed, lo, hi, span, ink, soft) {
  const rows = ed.rows, cols = ed.cols;
  const az = plot.az, el = plot.el;
  const ca = Math.cos(az), sa = Math.sin(az), ce = Math.cos(el), se = Math.sin(el);
  const scale = Math.min(W, H) * 0.34, cx = W / 2, cy = H * 0.58, height = 0.75;
  const proj = (gx, gy, gz) => {
    const x = (cols > 1 ? gx / (cols - 1) : 0.5) * 2 - 1;
    const y = (rows > 1 ? gy / (rows - 1) : 0.5) * 2 - 1;
    const z = ((gz - lo) / span) * 2 * height - height;
    const x1 = x * ca - y * sa, y1 = x * sa + y * ca;
    const y2 = y1 * ce - z * se, depth = y1 * se + z * ce;
    return { sx: cx + x1 * scale, sy: cy - y2 * scale, depth };
  };
  // Vertices.
  const V = [];
  for (let r = 0; r < rows; r++) { V[r] = []; for (let c = 0; c < cols; c++) V[r][c] = proj(c, r, ed.cells[r][c]); }
  // Quads with average depth for painter's sort.
  const quads = [];
  for (let r = 0; r < rows - 1; r++) for (let c = 0; c < cols - 1; c++) {
    const p = [V[r][c], V[r][c + 1], V[r + 1][c + 1], V[r + 1][c]];
    const avg = (ed.cells[r][c] + ed.cells[r][c + 1] + ed.cells[r + 1][c + 1] + ed.cells[r + 1][c]) / 4;
    quads.push({ p, t: (avg - lo) / span, depth: (p[0].depth + p[1].depth + p[2].depth + p[3].depth) / 4 });
  }
  quads.sort((a, b) => a.depth - b.depth);
  ctx.lineJoin = "round";
  for (const q of quads) {
    ctx.beginPath(); ctx.moveTo(q.p[0].sx, q.p[0].sy);
    for (let k = 1; k < 4; k++) ctx.lineTo(q.p[k].sx, q.p[k].sy);
    ctx.closePath();
    ctx.fillStyle = heatColor(q.t); ctx.globalAlpha = 0.92; ctx.fill();
    ctx.globalAlpha = 1; ctx.strokeStyle = "rgba(0,0,0,0.18)"; ctx.lineWidth = 0.5; ctx.stroke();
  }
  // Axis captions.
  ctx.fillStyle = soft; ctx.font = "11px ui-monospace, monospace"; ctx.textAlign = "center";
  ctx.fillText((ed.xAxis.name || "X") + (ed.xAxis.unit ? " (" + ed.xAxis.unit + ")" : ""), W / 2, H - 8);
  ctx.save(); ctx.translate(14, H / 2); ctx.rotate(-Math.PI / 2);
  ctx.fillText((ed.yAxis.name || "Y") + (ed.yAxis.unit ? " (" + ed.yAxis.unit + ")" : ""), 0, 0); ctx.restore();
  ctx.textAlign = "left"; ctx.fillStyle = ink;
  ctx.fillText(`${ed.mapName}  ·  ${fmtCell(lo)}…${fmtCell(hi)} ${ed.unit || ""}`, 12, 18);
}

function drawCurve(ctx, W, H, ed, lo, hi, span, ink, soft) {
  const flat = ed.rows === 1 ? ed.cells[0] : ed.cells.map((r) => r[0]);
  const ax = ed.rows === 1 ? ed.xAxis : ed.yAxis;
  const n = flat.length; const padL = 54, padR = 20, padT = 24, padB = 38;
  const gw = W - padL - padR, gh = H - padT - padB;
  const xf = (i) => padL + (n > 1 ? i / (n - 1) : 0.5) * gw;
  const yf = (v) => padT + gh - ((v - lo) / span) * gh;
  // Grid + axes.
  ctx.strokeStyle = cssVar("--line", "#e2e5ea"); ctx.lineWidth = 1; ctx.fillStyle = soft; ctx.font = "10px ui-monospace, monospace";
  for (let g = 0; g <= 4; g++) { const y = padT + (gh * g) / 4; ctx.beginPath(); ctx.moveTo(padL, y); ctx.lineTo(W - padR, y); ctx.stroke(); ctx.textAlign = "right"; ctx.fillText(fmtCell(hi - (span * g) / 4), padL - 6, y + 3); }
  // Area + line.
  ctx.beginPath(); flat.forEach((v, i) => { const x = xf(i), y = yf(v); i ? ctx.lineTo(x, y) : ctx.moveTo(x, y); });
  ctx.lineTo(xf(n - 1), padT + gh); ctx.lineTo(xf(0), padT + gh); ctx.closePath();
  ctx.fillStyle = "rgba(232,113,26,0.12)"; ctx.fill();
  ctx.beginPath(); flat.forEach((v, i) => { const x = xf(i), y = yf(v); i ? ctx.lineTo(x, y) : ctx.moveTo(x, y); });
  ctx.strokeStyle = cssVar("--accent", "#e8711a"); ctx.lineWidth = 2; ctx.stroke();
  flat.forEach((v, i) => { const x = xf(i), y = yf(v); ctx.beginPath(); ctx.arc(x, y, 2.4, 0, 7); ctx.fillStyle = heatColor((v - lo) / span); ctx.fill(); });
  // X labels (breakpoints if present).
  ctx.fillStyle = soft; ctx.textAlign = "center";
  const xv = (ax && ax.values) || [];
  const step = Math.ceil(n / 8);
  for (let i = 0; i < n; i += step) ctx.fillText(xv[i] != null ? fmtAxis(xv[i]) : String(i), xf(i), H - 20);
  ctx.fillStyle = ink; ctx.textAlign = "left"; ctx.font = "11px ui-monospace, monospace";
  ctx.fillText(`${ed.mapName} (${ax && ax.unit ? ax.unit : "idx"} → ${ed.unit || "value"})`, padL, 16);
}

(() => {
  const cv = $("plot3d"); if (!cv) return;
  cv.addEventListener("mousedown", (e) => { plot.drag = true; plot.lx = e.clientX; plot.ly = e.clientY; });
  window.addEventListener("mousemove", (e) => {
    if (!plot.drag) return;
    plot.az += (e.clientX - plot.lx) * 0.01; plot.lx = e.clientX;
    plot.el = Math.max(0.05, Math.min(1.45, plot.el + (e.clientY - plot.ly) * 0.008)); plot.ly = e.clientY;
    if (state.view === "3d") drawPlot();
  });
  window.addEventListener("mouseup", () => { plot.drag = false; });
})();

// =================== IMPORT XDF/A2L + PROMOTE ===================
const defFilter = [
  { name: "Definitions (XDF / A2L)", extensions: ["xdf", "a2l"] },
  { name: "All files", extensions: ["*"] },
];
$("btnImportDef").onclick = async () => { const p = await pickFile("Import XDF / A2L", defFilter); if (p) { state.impFile = p; $("impFileName").textContent = baseName(p); } };
$("btnDoImport").onclick = async () => {
  if (!state.impFile) return toast("Pick an XDF or A2L first", true);
  const target = state.mvImage || state.imagePath;
  try {
    const r = await invoke("import_definitions", { path: state.impFile, base: $("impBase").value || null });
    state.impMaps = r.maps || [];
    const head = r.kind === "a2l"
      ? `A2L · project ${esc(r.project || "?")} · module ${esc(r.module || "?")}${r.base != null ? " · base 0x" + (r.base >>> 0).toString(16).toUpperCase() : ""}`
      : `XDF · ${esc(r.title || "(untitled)")} · base offset 0x${(r.base_offset >>> 0).toString(16).toUpperCase()}`;
    if (!state.impMaps.length) { $("impResult").innerHTML = `<div class="banner info">${head} — no importable maps.</div>`; return; }
    const rows = state.impMaps.map((m, i) => `<tr><td>${esc(m.name)}</td><td>${m.rows}×${m.cols} ${esc(m.data_type)}</td><td class="mono">${esc(m.address_hex)}</td><td>${esc(m.shape)}</td><td class="mono">${esc(m.unit || "")}</td><td><button class="btn sm" data-imp="${i}"${target ? "" : " disabled title='open an image first'"}>Open</button></td></tr>`).join("");
    $("impResult").innerHTML = `<div class="banner ok">${head} — ${state.impMaps.length} map(s).</div>
      <table class="tbl"><thead><tr><th>Map</th><th>Size</th><th>Addr</th><th>Shape</th><th>Unit</th><th></th></tr></thead><tbody>${rows}</tbody></table>
      ${target ? "" : `<div class="note">Open a flash dump (Image panel) to decode these maps into the editor.</div>`}`;
    $("impResult").querySelectorAll("button[data-imp]").forEach((b) => b.onclick = () => { const p = state.mvImage || state.imagePath; if (!p) return toast("Open an image first", true); openMapDef(p, state.impMaps[+b.dataset.imp]); });
  } catch (e) { $("impResult").innerHTML = errBanner(e); }
};

function prefillPromote(start, len) {
  $("promoteForm").hidden = false;
  $("pmStart").value = start; $("pmLen").value = len;
  // Guess square-ish u16 dims from the length.
  const cells = Math.floor(len / 2);
  let cols = 16; for (const w of [16, 12, 10, 8]) if (cells % w === 0) { cols = w; break; }
  $("pmCols").value = cols; $("pmRows").value = Math.max(1, Math.floor(cells / cols));
  $("promoteForm").scrollIntoView({ behavior: "smooth", block: "nearest" });
  toast("Fill in dims/scaling, then Make map");
}
$("btnPromote").onclick = async () => {
  const path = state.mvImage || state.imagePath;
  if (!path) return toast("Open an image first", true);
  try {
    const mapdef = await invoke("promote_region", {
      path, start: $("pmStart").value, len: +$("pmLen").value || null,
      name: $("pmName").value, rows: +$("pmRows").value || 1, cols: +$("pmCols").value || 1,
      dataType: $("pmType").value, endian: $("pmEndian").value,
      factor: parseFloat($("pmFactor").value) || 1, offset: parseFloat($("pmOffset").value) || 0,
    });
    await openMapDef(path, mapdef);
  } catch (e) { $("impResult").innerHTML = errBanner(e); toast(typeof e === "string" ? e : "promote failed", true); }
};
