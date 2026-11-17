// OpenTune — calibration panels: Image, Identify, Checksum, Analyze bin.
// Loaded after app-core.js (see its header for the script order).
"use strict";

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
    $("hexView").innerHTML = lines.join("\n") + `\n\n<span class="hc">offset ${hex8(r.offset)} of ${r.total.toLocaleString()} bytes</span>`;
    $("hexOffset").value = hex8(r.offset);
    state.hexOffset = r.offset; state.hexTotal = r.total;
  } catch (e) { $("hexView").innerHTML = errBanner(e); }
}
$("btnOpenImage").onclick = async () => { const p = await pickFile("Open flash dump", binFilter); if (p) loadImage(p); };
$("hexPrev").onclick = () => renderHex(Math.max(0, (state.hexOffset || 0) - 256));
$("hexNext").onclick = () => renderHex(Math.min(Math.max(0, (state.hexTotal || 256) - 1), (state.hexOffset || 0) + 256));
$("hexOffset").onchange = () => { const v = parseHexInput($("hexOffset").value); if (!isNaN(v)) renderHex(v); };

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
// The algorithm selector lists every algorithm the core knows. The static
// options in index.html stay as a fallback; options a plugin overlay added
// (ids the core doesn't know) are kept after the core list.
async function initChecksumAlgos() {
  const sel = $("csAlgo");
  if (!sel) return;
  let algos;
  try { algos = await invoke("checksum_algos"); } catch (e) { return; }
  if (!Array.isArray(algos) || !algos.length) return;
  const keep = sel.value;
  const known = new Set(algos.map((a) => a.id));
  const extra = [...sel.options].filter((o) => !known.has(o.value) && !STATIC_CS_ALGOS.has(o.value));
  sel.innerHTML = algos.map((a) =>
    `<option value="${esc(a.id)}" title="${esc(a.notes)}">${esc(a.id)} — ${esc(a.name)}${a.needs_reference ? " (needs stock reference)" : ""}</option>`).join("");
  extra.forEach((o) => sel.appendChild(o));
  if ([...sel.options].some((o) => o.value === keep)) sel.value = keep;
}
const STATIC_CS_ALGOS = new Set(["edc16", "edc17", "pcr21", "crc32-tail"]);

// Fix writes a NEW file by default; overwriting archives the original first.
async function doFix(outPath) {
  if (!state.imagePath) return toast("Open an image first", true);
  try {
    const r = await invoke("checksum_fix", { path: state.imagePath, algo: $("csAlgo").value, outPath: outPath || null });
    const savedElsewhere = r.out_path !== state.imagePath;
    const where = `<div class="meta-grid" style="margin-top:8px">` +
      (savedElsewhere ? `<div class="k">saved to</div><div class="v">${esc(r.out_path)}</div>` : "") +
      `<div class="k">backup</div><div class="v">${r.backup ? esc(r.backup) + " (the file as it was before this fix)" : "none needed — no existing file was overwritten"}</div></div>`;
    $("csResult").innerHTML = (r.fixes.length
      ? `<div class="banner ok">Applied ${r.fixes.length} fix(es)${savedElsewhere ? " and saved a copy" : " and saved in place"}.</div><pre class="hexview">${esc(r.fixes.join("\n"))}</pre>`
      : `<div class="banner info">Nothing to fix — checksums already correct.${savedElsewhere ? " (Copy written.)" : ""}</div>`)
      + where;
    // If we wrote a copy, switch to it so re-verify reflects the corrected file.
    if (savedElsewhere) { await loadImage(r.out_path); showPanel("checksum"); }
    else if (state.imagePath) renderHex(state.hexOffset || 0);
  } catch (e) {
    const msg = typeof e === "string" ? e : (e && e.message) || "";
    $("csResult").innerHTML = errBanner(e) + (/permission denied|read-only/i.test(msg)
      ? `<div class="note">That file can't be written. Use <b>Fix → Save as…</b> and pick a folder you own.</div>` : "");
  }
}
// Default: Save As a new file next to the original (<name>.fixed.bin). The
// save dialog may also be pointed at the original; the backend then archives
// it to the vault before overwriting.
$("btnCsFix").onclick = async () => {
  if (!state.imagePath) return toast("Open an image first", true);
  const def = state.imagePath.replace(/\.[^.\\/]+$/, "") + ".fixed.bin";
  const out = await pickSave("Save corrected image as", def);
  if (out) doFix(out);
};
$("btnCsFixInPlace").onclick = () => {
  if (!state.imagePath) return toast("Open an image first", true);
  if (!window.confirm(`Overwrite ${baseName(state.imagePath)} with the corrected image?\n\nThe original is archived to the opentune-vault folder beside it first.`)) return;
  doFix(null);
};

// =================== ANALYZE BIN ===================
function recFamily(r) {
  const rc = r.recognition || {}, p = rc.primary;
  if (!p) return `${esc(r.family)}${rc.image_kind && rc.image_kind !== "raw" ? ` <span class="muted small">(${esc(rc.image_kind)} — not a raw image)</span>` : ""}`;
  const tier = p.confidence === "high" ? "ok" : p.confidence === "low" ? "bad" : "accent";
  return `<b>${esc(p.supplier)} ${esc(p.family)}</b>${p.variant ? " · " + esc(p.variant) : ""} <span class="pill ${tier}">${esc(p.confidence)}</span>${rc.contested ? ' <span class="pill bad">contested</span>' : ""}`;
}
function recOthers(r) {
  const o = ((r.recognition || {}).others || []).filter((d) => d.family !== "Unknown");
  const id = r.identity || [];
  return (o.length ? `<div class="k">other claims</div><div class="v">${o.map((d) => `${esc(d.supplier)} ${esc(d.family)}${d.variant ? " " + esc(d.variant) : ""} (${esc(d.confidence)})`).join("<br>")}</div>` : "") +
    (id.length ? `<div class="k">identity (offset)</div><div class="v">${id.map((f) => `${esc(f.kind)}: <code>${esc(f.value)}</code> <span class="muted small">@0x${Number(f.offset).toString(16).toUpperCase()}</span>`).join("<br>")}</div>` : "");
}
async function runAnalyze(path) {
  if (!path) return toast("Open or pick an image first", true);
  $("anResult").innerHTML = `<div class="banner info">Analyzing…</div>`;
  try {
    const r = await invoke("analyze_image", { path });
    const a = r.assignment || {};
    const car = [a.make, a.model, a.engine].filter(Boolean).join(" · ") || "— not determinable offline —";
    const vd = r.vin_decoded, pd = r.part_decoded;
    const csRows = (r.checksum_blocks || []).map((b) => `<tr><td>${esc(b.name)}</td><td><span class="pill ${b.ok ? "ok" : "bad"}">${b.ok ? "OK" : "BAD"}</span></td><td class="mono">${esc(b.region)}</td><td class="mono">${esc(b.stored)}</td><td class="mono">${esc(b.computed)}</td></tr>`).join("");
    const mapRows = (r.maps || []).map((m) => `<tr><td>${esc(m.name)}</td><td>${m.rows}×${m.cols} ${esc(m.data_type)}</td><td class="mono">${esc(m.address)}</td><td class="mono">${esc(m.unit || "")}</td></tr>`).join("");
    $("anResult").innerHTML =
      `<div class="banner ${r.confidence === "High" ? "ok" : "info"}">Assignment: <b>${esc(car)}</b> &nbsp; ${confPill(r.confidence)} <span class="muted small">basis: ${esc(a.basis || "—")}</span></div>
      <div class="meta-grid">
        <div class="k">ECU family</div><div class="v">${recFamily(r)} · ${esc(r.size_class)} · ${(r.size / 1024).toFixed(0)} KiB</div>
        <div class="k">family signals</div><div class="v">${(r.family_how && r.family_how.length) ? r.family_how.map(esc).join("<br>") : "—"}</div>
        ${recOthers(r)}
        <div class="k">VAG part #</div><div class="v">${listOrDash(r.part_numbers)}</div>
        <div class="k">Bosch #</div><div class="v">${listOrDash(r.bosch_numbers)}</div>
        <div class="k">Software #</div><div class="v">${listOrDash(r.sw_numbers)}</div>
        <div class="k">Mercedes #</div><div class="v">${listOrDash(r.mercedes_numbers)}</div>
        <div class="k">VIN</div><div class="v">${esc(r.vin || "—")}${vd ? ` · ${esc(vd.make || "?")}${vd.year ? " · " + vd.year : (vd.year_ambiguous ? " · year ambiguous" : "")}` : ""}</div>
        <div class="k">ASAM/EPK markers</div><div class="v">${r.epk_hits || 0}</div>
      </div>` +
      (pd ? `<h3>Part number</h3><div class="meta-grid">
        <div class="k">kind</div><div class="v">${esc(pd.kind)}${pd.supplier ? " · " + esc(pd.supplier) : ""}</div>
        ${pd.vag_main_group ? `<div class="k">VAG groups</div><div class="v">${esc(pd.vag_type_group || "")} / ${esc(pd.vag_main_group || "")} / ${esc(pd.vag_subgroup || "")} ${pd.vag_version ? "· " + esc(pd.vag_version) : ""}</div>` : ""}
        ${pd.bosch_family ? `<div class="k">Bosch</div><div class="v">${esc(pd.bosch_family)} · ${esc(pd.bosch_consecutive || "")}</div>` : ""}
        ${pd.flags && pd.flags.length ? `<div class="k">flags</div><div class="v">${pd.flags.map(esc).join("; ")}</div>` : ""}
      </div>` : "") +
      (r.catalog_match ? `<div class="banner ok" style="margin-top:12px">Catalog: <b>${esc(r.catalog_match.make)}</b> ${esc((r.catalog_match.models || []).slice(0, 4).join(", "))}${r.catalog_match.source ? ` <span class="muted small">(${esc(r.catalog_match.source)})</span>` : ""}</div>` : `<div class="note" style="margin-top:12px">No vehicle-catalog match — not enough identity to name a car.</div>`) +
      (r.ecudef_match ? `<div class="meta-grid" style="margin-top:8px"><div class="k">matched def</div><div class="v">${esc(r.ecudef_match.name)} [${esc(r.ecudef_match.id)}] · ${esc(r.ecudef_match.maker)} ${esc(r.ecudef_match.family)}</div></div>` : "") +
      (csRows ? `<h3>Checksum blocks (${esc(r.checksum_algo || "")})</h3><table class="tbl"><thead><tr><th>Block</th><th>Status</th><th>Region</th><th>Stored</th><th>Computed</th></tr></thead><tbody>${csRows}</tbody></table>` : `<div class="note">No checksum algorithm for family ${esc(r.family)} — nothing to verify.</div>`) +
      (mapRows ? `<h3>Known maps (${(r.maps || []).length})</h3><table class="tbl"><thead><tr><th>Map</th><th>Size</th><th>Addr</th><th>Unit</th></tr></thead><tbody>${mapRows}</tbody></table>` : "");
    refreshImageHints();
  } catch (e) { $("anResult").innerHTML = errBanner(e); }
}
$("btnAnalyze").onclick = () => runAnalyze(state.imagePath);
$("btnAnalyzePick").onclick = async () => { const p = await pickFile("Analyze flash dump", binFilter); if (p) runAnalyze(p); };
