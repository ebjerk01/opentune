// OpenTune — Flash, Vault, Drivers, Guide, Utilities, Project (.otproj) and
// About & updates. Loaded after app-maps.js (see app-core.js).
"use strict";

// =================== FLASH ===================
$("flTarget").onclick = async () => { const p = await pickFile("Target image", binFilter); if (p) { state.flTarget = p; refreshImageHints(); } };
$("flData").onclick = async () => { const p = await pickFile("Data file to write", binFilter); if (p) { state.flData = p; $("flDataName").textContent = baseName(p); } };
$("btnFlash").onclick = async () => {
  const path = state.flTarget || state.imagePath;
  if (!path) return toast("Pick a target image", true);
  if (!state.flData) return toast("Pick a data file", true);
  try {
    const r = await invoke("flash", { path, addr: $("flAddr").value, dataPath: state.flData });
    $("flResult").innerHTML = `<div class="banner ${r.verified ? "ok" : "info"}">Write to file complete${r.verified ? " — read-back verified ✓" : ""}</div>
      <div class="meta-grid">
        <div class="k">target</div><div class="v">${esc(path)}</div>
        <div class="k">region</div><div class="v">${esc(r.region)}</div>
        <div class="k">bytes written</div><div class="v">${r.bytes_written.toLocaleString()}</div>
        <div class="k">voltage gate</div><div class="v">${esc(r.voltage_gate || "—")}</div>
        <div class="k">backup</div><div class="v">${esc(r.backup)} (${r.backup_len.toLocaleString()} bytes, the whole file before the write)</div>
        <div class="k">verified</div><div class="v">${r.verified}</div>
      </div>`;
    if (path === state.imagePath) renderHex(state.hexOffset || 0);
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
// One list_drivers call shared by the Drivers panel, the Debug tab and the
// sidebar connection bar.
let driversP = null;
function driverList() {
  if (!driversP) driversP = invoke("list_drivers").catch((e) => { driversP = null; throw e; });
  return driversP;
}
async function initDrivers() {
  try {
    const list = await driverList();
    const rows = list.map((d) => `<tr><td class="mono">${esc(d.id)}</td><td>${esc(d.name)}</td><td>${d.id === "mock" ? '<span class="pill ok">offline</span>' : '<span class="pill neutral">hardware</span>'}</td></tr>`).join("");
    $("driversResult").innerHTML = `<div class="banner info">${list.length} driver(s) registered. Offline work uses <code>mock</code>.</div>
      <table class="tbl"><thead><tr><th>ID</th><th>Name</th><th>Mode</th></tr></thead><tbody>${rows}</tbody></table>`;
  } catch (e) { $("driversResult").innerHTML = errBanner(e); }
}

// =================== GUIDE (offline markdown) ===================
// Pages are rendered by OTUtil.renderMarkdown (util.js).
async function loadGuideSection(id) {
  state.guideCur = id;
  document.querySelectorAll("#guideNav .gnav").forEach((b) => b.classList.toggle("active", b.dataset.id === id));
  try {
    const md = await invoke("guide_read", { id });
    $("guideBody").innerHTML = renderMarkdown(md);
    $("guideBody").scrollTop = 0;
  } catch (e) { $("guideBody").innerHTML = errBanner(e); }
}
// In-guide links: #/guide/<id> switches section; external links open outside.
$("guideBody").addEventListener("click", (e) => {
  const a = e.target.closest && e.target.closest("a[data-guide]");
  if (!a) return;
  e.preventDefault();
  if (state.guideSections.some((s) => s.id === a.dataset.guide)) loadGuideSection(a.dataset.guide);
  else toast("No guide page " + a.dataset.guide, true);
});
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
    // A panel's "Open full guide" may already have picked a page: keep it.
    if (state.guideCur) document.querySelectorAll("#guideNav .gnav").forEach((b) => b.classList.toggle("active", b.dataset.id === state.guideCur));
    else await loadGuideSection(state.guideSections[0].id);
  } catch (e) { $("guideBody").innerHTML = errBanner(e); }
}
// The guide loads when its panel is first opened.
onPanelFirstShow("guide", initGuide);

// =================== UTILITIES (VIN / part / DTC / seed-key) ===================
$("btnVin").onclick = async () => {
  try {
    const v = await invoke("vin_decode", { vin: $("vinInput").value });
    const my = v.model_year;
    $("vinResult").innerHTML = `<div class="banner ok">VIN <b>${esc(v.vin)}</b></div>
      <div class="meta-grid">
        <div class="k">manufacturer</div><div class="v">${esc(v.manufacturer || "unknown WMI")}</div>
        <div class="k">country</div><div class="v">${esc(v.country || "—")}</div>
        <div class="k">WMI / VDS / VIS</div><div class="v">${esc(v.wmi)} · ${esc(v.vds)} · ${esc(v.vis)}</div>
        <div class="k">check digit</div><div class="v">${esc(v.check_digit)} · ${v.check_digit_valid == null ? "n/a (non-NA)" : (v.check_digit_valid ? "valid ✓" : "INVALID ✗")}</div>
        <div class="k">model year</div><div class="v">${my.resolved != null ? my.resolved : "ambiguous"} <span class="muted small">(code ${esc(my.code)}${my.candidates && my.candidates.length ? " → " + my.candidates.join(" / ") : ""})</span></div>
        <div class="k">plant</div><div class="v">${esc(v.plant_code)}</div>
        <div class="k">serial</div><div class="v">${esc(v.serial)}</div>
        ${v.flags && v.flags.length ? `<div class="k">flags</div><div class="v">${v.flags.map(esc).join("; ")}</div>` : ""}
      </div>`;
  } catch (e) { $("vinResult").innerHTML = errBanner(e); }
};
$("btnPart").onclick = async () => {
  try {
    const p = await invoke("part_decode", { part: $("partInput").value });
    $("partResult").innerHTML = `<div class="banner ok"><b>${esc(p.normalized)}</b> · ${esc(p.kind)}${p.supplier ? " · " + esc(p.supplier) : ""}</div>
      <div class="meta-grid">
        ${p.vag_main_group ? `<div class="k">VAG type/main/sub</div><div class="v">${esc(p.vag_type_group || "")} / ${esc(p.vag_main_group || "")} / ${esc(p.vag_subgroup || "")}</div><div class="k">version</div><div class="v">${esc(p.vag_version || "—")}</div>` : ""}
        ${p.bosch_family ? `<div class="k">Bosch family</div><div class="v">${esc(p.bosch_family)}</div><div class="k">consecutive</div><div class="v">${esc(p.bosch_consecutive || "—")}</div>` : ""}
        ${p.flags && p.flags.length ? `<div class="k">flags</div><div class="v">${p.flags.map(esc).join("; ")}</div>` : ""}
      </div>`;
  } catch (e) { $("partResult").innerHTML = errBanner(e); }
};
$("btnDtc").onclick = async () => {
  try {
    const d = await invoke("dtc_lookup", { code: $("dtcInput").value });
    $("dtcResult").innerHTML = `<div class="banner ok"><b>${esc(d.code)}</b></div>
      <div class="meta-grid"><div class="k">description</div><div class="v" style="font-family:inherit">${esc(d.description)}</div><div class="k">system</div><div class="v" style="font-family:inherit">${esc(d.system)}</div></div>`;
  } catch (e) { $("dtcResult").innerHTML = errBanner(e); }
};
async function initSeedKey() {
  try {
    const algos = await invoke("seedkey_algos");
    const sel = $("skAlgo"); sel.innerHTML = "";
    algos.forEach((a) => { const o = el("option"); o.value = a.id; o.textContent = a.name; o.dataset.params = (a.params || []).join(","); o.dataset.desc = a.desc; sel.appendChild(o); });
    sel.onchange = skReflect; skReflect();
  } catch (e) { $("skResult").innerHTML = errBanner(e); }
}
function skReflect() {
  const o = $("skAlgo").selectedOptions[0]; if (!o) return;
  const params = (o.dataset.params || "").split(",").filter(Boolean);
  $("skMaskRow").hidden = !params.includes("mask");
  $("skAddRow").hidden = !params.includes("add");
  $("skDesc").textContent = o.dataset.desc || "";
}
$("btnSeedKey").onclick = async () => {
  try {
    const r = await invoke("seedkey_compute", { seed: $("skSeed").value, algo: $("skAlgo").value, mask: $("skMask").value || null, add: $("skAdd").value || null });
    const src = r.source === "plugin" ? ` <span class="pill accent">plugin algorithm</span>` : "";
    $("skResult").innerHTML = `<div class="banner ok">key: <b class="mono">${esc(r.key)}</b>${src}</div><div class="note">${esc(r.note)}</div>`;
  } catch (e) { $("skResult").innerHTML = errBanner(e); }
};

// =================== PROJECT (.otproj) ===================
// A tuning project records *what you changed* against a base image — the base
// image path + fingerprint, the selected definition/checksum, and one edit per
// map (edited grid + decoded baseline). It never copies the whole .bin.

const projFilter = [
  { name: "OpenTune project", extensions: ["otproj"] },
  { name: "All files", extensions: ["*"] },
];
async function pickSaveProject(title, defaultPath) {
  return await dialog().save({ title, defaultPath, filters: [{ name: "OpenTune project", extensions: ["otproj"] }] });
}

// --- recent list (per-viewer convenience; safe to be empty) -----------------
const RECENT_KEY = "opentune.recentProjects";
function getRecent() { try { return JSON.parse(localStorage.getItem(RECENT_KEY) || "[]"); } catch { return []; } }
function pushRecent(path) {
  try {
    let l = getRecent().filter((p) => p !== path);
    l.unshift(path); l = l.slice(0, 8);
    localStorage.setItem(RECENT_KEY, JSON.stringify(l));
  } catch { /* private window / blocked storage — recents just won't persist */ }
  renderRecent();
}
function renderRecent() {
  const host = $("prjRecent"); if (!host) return;
  const l = getRecent();
  if (!l.length) { host.innerHTML = `<p class="muted small">No recent projects.</p>`; return; }
  host.innerHTML = l.map((p) => `<button class="recent-item" data-recent="${esc(p)}"><span>🗂</span> <span class="rp">${esc(p)}</span></button>`).join("");
  host.querySelectorAll("button[data-recent]").forEach((b) => (b.onclick = () => doOpenProject(b.dataset.recent)));
}

// --- edit capture (editKey / cellsChanged live in util.js) ------------------
// Capture the map currently open in the editor into the project. Returns true
// when something was stored. `silent` suppresses the "nothing to capture" toast.
function captureCurrentEdit(silent) {
  const ed = state.ed;
  if (!ed) { if (!silent) toast("No map open in the editor", true); return false; }
  const changed = ed.cells.some((row, r) => row.some((v, c) => Math.abs(v - ed.orig[r][c]) > 1e-9));
  if (!changed) { if (!silent) toast("This map has no edits to capture", true); return false; }
  const rec = {
    kind: ed.adhoc ? "adhoc" : "def",
    defId: ed.defId || null,
    mapName: ed.mapName,
    mapdef: ed.adhoc || null,
    cells: clone2d(ed.cells),
    baseline: clone2d(ed.orig),
    notes: "",
    rows: ed.rows, cols: ed.cols,
    dataType: ed.dataType, unit: ed.unit || "",
  };
  rec.key = editKey(rec);
  const i = state.project.edits.findIndex((e) => e.key === rec.key);
  if (i >= 0) { rec.notes = state.project.edits[i].notes || ""; state.project.edits[i] = rec; }
  else state.project.edits.push(rec);
  // Adopt the editor's image as the project base image if none chosen yet.
  if (!state.project.baseImagePath) state.project.baseImagePath = ed.path;
  renderProject();
  if (!silent) toast(`Captured "${rec.mapName}" (${cellsChanged(rec.cells, rec.baseline)} changed cell(s))`);
  return true;
}

// --- build / save -----------------------------------------------------------
function buildProjectPayload() {
  return {
    format: "opentune-project",
    version: 1,
    name: state.project.name || "",
    notes: state.project.notes || "",
    ecudef_id: ($("mvDef") && $("mvDef").value) || null,
    checksum_algo: ($("csAlgo") && $("csAlgo").value) || null,
    base_image: { path: state.project.baseImagePath || "", size: 0, hash: "" },
    edits: state.project.edits.map((e) => ({
      kind: e.kind,
      def_id: e.defId || null,
      map_name: e.mapName || "",
      mapdef: e.mapdef || null,
      cells: e.cells,
      baseline: e.baseline,
      notes: e.notes || "",
    })),
    created: state.project.created || new Date().toISOString(),
    saved: new Date().toISOString(),
  };
}
async function saveProject(saveAs) {
  captureCurrentEdit(true); // fold in the editor's current map, if any
  if (!state.project.edits.length) return toast("Nothing to save yet — edit a map and capture it", true);
  let path = state.project.path;
  if (saveAs || !path) {
    const base = (state.project.name || "tune").trim().replace(/[^\w.-]+/g, "_") || "tune";
    path = await pickSaveProject("Save tuning project as", base + ".otproj");
    if (!path) return;
  }
  try {
    const r = await invoke("project_save", { path, project: buildProjectPayload() });
    state.project.path = r.path;
    state.project.imageStatus = "match"; // just fingerprinted the current image
    pushRecent(r.path);
    renderProject();
    toast("Project saved: " + baseName(r.path));
  } catch (e) {
    const msg = typeof e === "string" ? e : (e && e.message) || "";
    $("prjMeta").innerHTML = errBanner(e) + (/permission denied|read-only/i.test(msg)
      ? `<div class="note">Can't write there — use <b>Save as…</b> to pick a folder you own.</div>` : "");
    toast("Save failed", true);
  }
}

// --- open -------------------------------------------------------------------
async function doOpenProject(path) {
  if (!path) return;
  try {
    const r = await invoke("project_open", { path });
    const p = r.project || {};
    state.project = {
      path: r.path,
      name: p.name || "",
      notes: p.notes || "",
      baseImagePath: (p.base_image && p.base_image.path) || "",
      created: p.created || null,
      imageStatus: r.image_status,
      edits: (p.edits || []).map((e) => {
        const rec = {
          kind: e.kind || (e.mapdef ? "adhoc" : "def"),
          defId: e.def_id || null,
          mapName: e.map_name || "",
          mapdef: e.mapdef || null,
          cells: e.cells || [],
          baseline: e.baseline || [],
          notes: e.notes || "",
          rows: (e.cells && e.cells.length) || 0,
          cols: (e.cells && e.cells[0] && e.cells[0].length) || 0,
          dataType: (e.mapdef && e.mapdef.data_type) || "",
          unit: (e.mapdef && e.mapdef.unit) || "",
        };
        rec.key = editKey(rec);
        return rec;
      }),
    };
    // Restore the selected definition + checksum so the editor lines up.
    if (p.ecudef_id && $("mvDef")) { await ensureDefs(); $("mvDef").value = p.ecudef_id; if ($("cmpDef")) $("cmpDef").value = p.ecudef_id; await populateMaps(); }
    if (p.checksum_algo && $("csAlgo")) $("csAlgo").value = p.checksum_algo;
    pushRecent(r.path);
    renderProject();
    showPanel("project");
    const warn = r.image_status === "mismatch" ? "base image CHANGED since save" : r.image_status === "missing" ? "base image not found" : "";
    toast("Opened " + baseName(r.path) + (warn ? " — " + warn : ""), !!warn);
  } catch (e) {
    $("prjMeta").innerHTML = errBanner(e);
    toast(typeof e === "string" ? e : "open failed", true);
  }
}

// Restore one stored edit into the editor: re-decode the map from the base image
// (fresh baseline), then lay the saved values on top.
async function openProjectEdit(rec) {
  const path = state.project.baseImagePath || state.mvImage || state.imagePath;
  if (!path) return toast("Set the project's base image first", true);
  try {
    let m;
    if (rec.kind === "adhoc" && rec.mapdef) {
      m = await invoke("decode_mapdef", { path, map: rec.mapdef });
      buildEditorModel(path, m, { adhoc: m.mapdef });
    } else {
      m = await invoke("decode_map", { path, ecudefId: rec.defId, mapName: rec.mapName });
      buildEditorModel(path, m, { defId: rec.defId, mapName: rec.mapName });
    }
    const ed = state.ed;
    if (rec.cells.length === ed.rows && rec.cells.every((row) => row.length === ed.cols)) {
      ed.cells = clone2d(rec.cells);
      ed.diff = true; // show what the project changed, straight away
      edRender();
      toast(`Restored "${rec.mapName}" (${cellsChanged(rec.cells, ed.orig)} changed cell(s))`);
    } else {
      toast("Stored edit dimensions don't match the decoded map — showing decoded values only", true);
    }
    showPanel("maps");
    $("mvResult").scrollIntoView({ behavior: "smooth", block: "nearest" });
  } catch (e) { toast(typeof e === "string" ? e : "could not open edit", true); showPanel("project"); }
}

// --- render -----------------------------------------------------------------
function renderProject() {
  const P = state.project;
  if ($("prjName").value !== P.name) $("prjName").value = P.name || "";
  if ($("prjNotes").value !== P.notes) $("prjNotes").value = P.notes || "";
  $("prjPathHint").textContent = P.path ? baseName(P.path) : "unsaved";
  $("prjBaseHint").textContent = P.baseImagePath ? P.baseImagePath : "no base image";
  $("prjImgStatus").innerHTML = imgBadge(P.imageStatus || (P.baseImagePath ? null : "no_image"));

  const meta = [];
  if (P.path) meta.push(`<div class="k">file</div><div class="v">${esc(P.path)}</div>`);
  meta.push(`<div class="k">edits</div><div class="v">${P.edits.length} map(s)</div>`);
  if (P.created) meta.push(`<div class="k">created</div><div class="v">${esc(P.created)}</div>`);
  $("prjMeta").innerHTML = `<div class="meta-grid">${meta.join("")}</div>` +
    (P.imageStatus === "mismatch" ? `<div class="banner bad" style="margin-top:10px">The base image has changed since this project was saved. Restoring an edit will lay your saved values onto a <b>different</b> stock — review each map before writing.</div>` : "") +
    (P.imageStatus === "missing" ? `<div class="banner bad" style="margin-top:10px">The referenced base image could not be found. Point <b>Base image…</b> at the dump before restoring edits.</div>` : "");

  const host = $("prjEdits");
  if (!P.edits.length) { host.innerHTML = `<p class="muted small">No edits captured. Open a map in the editor, change it, then <b>Add to project</b> (or <b>Capture current map</b>).</p>`; return; }
  const rows = P.edits.map((e, i) => {
    const n = cellsChanged(e.cells, e.baseline);
    const kind = e.kind === "adhoc" ? "ad-hoc" : esc(e.defId || "def");
    return `<tr>
      <td>${esc(e.mapName)}</td>
      <td>${e.rows}×${e.cols} ${esc(e.dataType || "")}</td>
      <td class="mono">${kind}</td>
      <td>${n ? `<span class="pill accent">${n}</span>` : `<span class="pill neutral">0</span>`}</td>
      <td><input class="prj-note-inp" data-note="${i}" value="${esc(e.notes || "")}" placeholder="notes" /></td>
      <td><button class="btn sm" data-open="${i}">Open</button> <button class="btn sm" data-del="${i}">✕</button></td>
    </tr>`;
  }).join("");
  host.innerHTML = `<table class="tbl"><thead><tr><th>Map</th><th>Size</th><th>From</th><th>Changed</th><th>Notes</th><th></th></tr></thead><tbody>${rows}</tbody></table>`;
  host.querySelectorAll("button[data-open]").forEach((b) => (b.onclick = () => openProjectEdit(P.edits[+b.dataset.open])));
  host.querySelectorAll("button[data-del]").forEach((b) => (b.onclick = () => { P.edits.splice(+b.dataset.del, 1); renderProject(); }));
  host.querySelectorAll("input[data-note]").forEach((inp) => (inp.onchange = () => { const e = P.edits[+inp.dataset.note]; if (e) e.notes = inp.value; }));
}

function newProject() {
  state.project = { path: null, name: "", notes: "", baseImagePath: state.imagePath || "", created: new Date().toISOString(), imageStatus: state.imagePath ? null : "no_image", edits: [] };
  renderProject();
  toast("New project");
}

// --- bindings ---------------------------------------------------------------
$("prjNew").onclick = newProject;
$("prjOpen").onclick = async () => { const p = await pickFile("Open tuning project", projFilter); if (p) doOpenProject(p); };
$("prjSave").onclick = () => saveProject(false);
$("prjSaveAs").onclick = () => saveProject(true);
$("prjCapture").onclick = () => captureCurrentEdit(false);
$("prjBaseImg").onclick = async () => { const p = await pickFile("Base image for project", binFilter); if (p) { state.project.baseImagePath = p; state.project.imageStatus = null; renderProject(); } };
$("prjName").oninput = () => { state.project.name = $("prjName").value; };
$("prjNotes").oninput = () => { state.project.notes = $("prjNotes").value; };

// =================== ABOUT & UPDATES ===================
// The Tauri updater plugin + endpoint + minisign pubkey live in tauri.conf.json;
// these handlers are the invocation layer. Nothing downloads until the user
// clicks, the signature is verified by the backend before install, and the
// backend refuses to start an update while a flash / map write-back is running.
const upd = { available: false, installing: false, info: null };

async function refreshUpdateStatus() {
  try {
    const s = await invoke("update_status");
    if ($("abVersion")) $("abVersion").textContent = "v" + s.current_version;
    const busy = !!s.busy;
    // While a long op holds the guard, the backend will refuse an update; reflect
    // that in the UI instead of letting the user hit a wall.
    const blocked = busy || upd.installing;
    if ($("btnUpdCheck")) $("btnUpdCheck").disabled = blocked;
    if ($("btnUpdInstall")) $("btnUpdInstall").disabled = blocked;
    $("abBusyHint").textContent = busy ? `blocked — "${s.busy_op}" is running` : "";
  } catch { /* not in Tauri / command missing — leave the panel inert */ }
}

async function checkForUpdates() {
  $("abResult").innerHTML = `<div class="banner info">Checking for updates…</div>`;
  $("btnUpdInstall").hidden = true;
  $("btnUpdRestart").hidden = true;
  upd.available = false; upd.info = null;
  try {
    const r = await invoke("update_check");
    if (r.available) {
      upd.available = true; upd.info = r;
      const notes = r.notes ? `<div class="note" style="margin-top:10px">${esc(r.notes)}</div>` : "";
      $("abResult").innerHTML = `<div class="banner ok">Update available: <b>v${esc(r.version)}</b> (you have v${esc(r.current_version)}).</div>${notes}`;
      $("btnUpdInstall").hidden = false;
    } else {
      $("abResult").innerHTML = `<div class="banner ok">You are up to date (v${esc(r.current_version)}).</div>`;
    }
  } catch (e) {
    $("abResult").innerHTML = `<div class="banner bad">Update check failed — ${esc(typeof e === "string" ? e : (e && e.message) || e)}<br><span class="muted small">This is expected offline; try again when you are online.</span></div>`;
  }
  refreshUpdateStatus();
}

function setProgress(downloaded, total) {
  $("abProgWrap").hidden = false;
  const fill = $("abProgFill");
  if (total && total > 0) {
    const pct = Math.min(100, Math.round((downloaded / total) * 100));
    fill.classList.remove("indeterminate");
    fill.style.width = pct + "%";
    $("abProgText").textContent = `${(downloaded / 1048576).toFixed(1)} / ${(total / 1048576).toFixed(1)} MiB (${pct}%)`;
  } else {
    // No content-length yet — show an indeterminate sweep.
    fill.classList.add("indeterminate");
    $("abProgText").textContent = `${(downloaded / 1048576).toFixed(1)} MiB downloaded…`;
  }
}

async function installUpdate() {
  if (!upd.available || upd.installing) return;
  // Confirm before replacing the running app.
  try {
    const ok = await dialog().confirm(
      `Download and install v${upd.info.version}? OpenTune will need to restart to finish. Your dumps, projects and backups are not affected.`,
      { title: "Install update", kind: "info" }
    );
    if (!ok) return;
  } catch { /* confirm unavailable — proceed, the backend still guards */ }

  upd.installing = true;
  $("btnUpdInstall").disabled = true;
  $("btnUpdCheck").disabled = true;
  $("abResult").innerHTML = `<div class="banner info">Downloading & verifying the signed update…</div>`;
  setProgress(0, 0);

  let unlisten = null;
  try {
    if (TAURI.event && TAURI.event.listen) {
      unlisten = await TAURI.event.listen("update://progress", (ev) => {
        const p = ev.payload || {};
        setProgress(p.downloaded || 0, p.total || 0);
      });
    }
    const r = await invoke("update_install");
    if (r.installed) {
      $("abProgWrap").hidden = true;
      $("abResult").innerHTML = `<div class="banner ok">Update v${esc(r.version)} installed and signature-verified. Restart to apply it.</div>`;
      $("btnUpdInstall").hidden = true;
      $("btnUpdRestart").hidden = false;
      toast("Update installed — restart to apply");
    } else if (r.up_to_date) {
      $("abProgWrap").hidden = true;
      $("abResult").innerHTML = `<div class="banner ok">Already up to date.</div>`;
      $("btnUpdInstall").hidden = true;
    }
  } catch (e) {
    // Bad signature, interrupted download, refused-while-busy, etc. Nothing was
    // installed; the running app is untouched.
    $("abProgWrap").hidden = true;
    $("abResult").innerHTML = `<div class="banner bad">Update failed — ${esc(typeof e === "string" ? e : (e && e.message) || e)}<br><span class="muted small">Nothing was installed; your current version is unchanged.</span></div>`;
    toast("Update failed", true);
  } finally {
    if (unlisten) { try { unlisten(); } catch {} }
    upd.installing = false;
    $("btnUpdCheck").disabled = false;
    refreshUpdateStatus();
  }
}

async function initUpdates() {
  if ($("btnUpdCheck")) $("btnUpdCheck").onclick = checkForUpdates;
  if ($("btnUpdInstall")) $("btnUpdInstall").onclick = installUpdate;
  if ($("btnUpdRestart")) $("btnUpdRestart").onclick = async () => {
    try { await invoke("update_relaunch"); } catch (e) { toast("Restart failed: " + e, true); }
  };
  await refreshUpdateStatus();
}
