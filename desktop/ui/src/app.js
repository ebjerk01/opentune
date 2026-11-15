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
    const sel = $("mvDef"); sel.innerHTML = "";
    (withMaps.length ? withMaps : state.defs).forEach((d) => { const o = el("option"); o.value = d.id; o.textContent = `${d.name} (${d.maps} maps)`; sel.appendChild(o); });
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

$("btnDecodeMap").onclick = async () => {
  const path = state.mvImage || state.imagePath;
  if (!path) return toast("Open an image or pick one", true);
  const id = $("mvDef").value, name = $("mvMap").value;
  if (!id || !name) return toast("Pick a definition and a map", true);
  try {
    const m = await invoke("decode_map", { path, ecudefId: id, mapName: name });
    const lo = m.min != null ? m.min : 0, hi = m.max != null ? m.max : 1, span = (hi - lo) || 1;
    const xv = m.x_axis.values || [], yv = m.y_axis.values || [];
    let thead = `<tr><th>${esc(m.y_axis.name || "")}\\${esc(m.x_axis.name || "")}</th>`;
    for (let c = 0; c < m.cols; c++) thead += `<th>${xv[c] != null ? fmtAxis(xv[c]) : c}</th>`;
    thead += "</tr>";
    let body = "";
    for (let r = 0; r < m.rows; r++) {
      body += `<tr><th>${yv[r] != null ? fmtAxis(yv[r]) : r}</th>`;
      for (let c = 0; c < m.cols; c++) {
        const v = m.cells[r][c], t = (v - lo) / span;
        body += `<td class="cell" style="background:${heatColor(t)}" title="[${r},${c}] ${fmtCell(v)} ${esc(m.unit || "")}">${fmtCell(v)}</td>`;
      }
      body += "</tr>";
    }
    const scl = `× ${m.scaling.factor}${m.scaling.offset ? " + " + m.scaling.offset : ""} → ${esc(m.unit || "raw")}`;
    $("mvResult").innerHTML =
      `<div class="meta-grid">
        <div class="k">address</div><div class="v">${esc(m.address)}</div>
        <div class="k">size</div><div class="v">${m.rows}×${m.cols} ${esc(m.data_type)} ${esc(m.endian)}</div>
        <div class="k">scaling</div><div class="v">${scl}</div>
        <div class="k">range</div><div class="v">${fmtCell(lo)} … ${fmtCell(hi)} ${esc(m.unit || "")}</div>
      </div>
      ${m.notes ? `<div class="note">${esc(m.notes)}</div>` : ""}
      <div class="heatwrap"><table class="heat"><thead>${thead}</thead><tbody>${body}</tbody></table></div>
      <div class="legend"><span>${fmtCell(lo)}</span><span class="bar"></span><span>${fmtCell(hi)} ${esc(m.unit || "")}</span> &nbsp; <span>X: ${esc(m.x_axis.unit || "idx")} · Y: ${esc(m.y_axis.unit || "idx")}</span></div>`;
  } catch (e) { $("mvResult").innerHTML = errBanner(e); }
};

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
