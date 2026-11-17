// OpenTune — full-vehicle Auto-scan + DTCs (frontend).
//
// Self-contained feature: injects its own nav button + panel at load, drives the
// `autoscan_run` / `autoscan_clear` backend commands over the shared adapter
// connection (window.OT), and renders a per-module presence / identity / DTC
// report. Loaded after app.js and ot-caps.js, so window.OT and window.showPanel
// already exist. Uses only existing CSS classes — adds no styles of its own.
"use strict";
(function () {
  const TAURI = window.__TAURI__ || {};
  const invoke = (cmd, args) => TAURI.core.invoke(cmd, args);
  const dialog = () => TAURI.dialog;

  // Minimal local helpers (kept self-contained; does not lean on app.js scope).
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]),
    );
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "");

  // Panel-local state: the chosen VehicleModules JSON path.
  const state = { modulesPath: null };

  // ---- inject nav button ---------------------------------------------------
  window.OTNav.register({ id: "scan", label: "Auto-scan", icon: "🔎", group: "vehicle", tier: "bench" });

  // ---- inject panel --------------------------------------------------------
  const sec = document.createElement("section");
  sec.className = "panel";
  sec.id = "panel-scan";
  sec.innerHTML = `
    <header class="panel-head">
      <h1>Auto-scan</h1>
      <p>Walk a vehicle's full module list (ForScan / VCDS style): probe every
         module for presence, read its identity, and read its stored diagnostic
         trouble codes. The scan is read-only — it never writes to a module.</p>
    </header>
    <div class="card">
      <div class="row">
        <button class="btn" id="scanPick">Choose modules JSON…</button>
        <span id="scanFile" class="muted small mono">— no module list chosen</span>
      </div>
      <div class="row">
        <button class="btn primary" id="scanRun">Scan vehicle</button>
        <button class="btn warn" id="scanClear">Clear all DTCs</button>
      </div>
      <p class="muted small">The module list is data: one JSON file per vehicle
         (<span class="mono">vehicle</span> + <span class="mono">modules[]</span>
         with CAN <span class="mono">req_id</span>/<span class="mono">resp_id</span>).
         Make sure the dongle's physical <b>HS/MS-CAN hardware switch</b> matches the
         bus the modules live on — a module on the wrong bus simply shows as absent.</p>
      <div id="scanResult" class="result"></div>
      <div id="scanClearResult" class="result"></div>
    </div>`;
  const main = document.getElementById("main");
  if (main) main.appendChild(sec);

  const $ = (id) => sec.querySelector("#" + id);

  // Gate the live actions on a CAN-capable, connected adapter.
  if (window.OT && window.OT.gate) {
    window.OT.gate(sec, ["can"], "vehicle scan", { disable: ["#scanRun", "#scanClear"] });
  }

  // ---- rendering -----------------------------------------------------------
  function identityCell(m) {
    if (!m.present) return '<span class="muted">—</span>';
    if (!m.identity) return '<span class="muted">no identity</span>';
    const id = m.identity;
    const parts = [];
    if (id.vin) parts.push(`VIN <span class="mono">${esc(id.vin)}</span>`);
    (id.dids || []).forEach((d) => {
      const v = d.text != null ? d.text : d.raw;
      parts.push(`${esc(d.label)} <span class="mono">${esc(v)}</span>`);
    });
    return parts.length ? parts.join("<br>") : '<span class="muted">—</span>';
  }

  function presentPill(m) {
    return m.present
      ? '<span class="pill ok">present</span>'
      : '<span class="pill neutral">absent</span>';
  }

  function renderScan(r) {
    const rows = (r.modules || [])
      .map((m) => {
        const dtcPill =
          m.dtcCount > 0
            ? `<span class="pill bad">${m.dtcCount}</span>`
            : m.present
            ? '<span class="pill ok">0</span>'
            : '<span class="muted">—</span>';
        const err = m.error
          ? `<span class="mono small">${esc(m.error)}</span>`
          : '<span class="muted">—</span>';
        const addr = m.target ? ` <span class="muted small mono">${esc(m.target.tx)}/${esc(m.target.rx)}</span>` : "";
        return `<tr>
          <td>${esc(m.name)}${addr}</td>
          <td>${presentPill(m)}</td>
          <td class="small">${identityCell(m)}</td>
          <td>${dtcPill}</td>
          <td>${err}</td>
        </tr>`;
      })
      .join("");

    // Flat DTC detail list across all modules that reported codes.
    const dtcBlocks = (r.modules || [])
      .filter((m) => m.dtcs && m.dtcs.length)
      .map((m) => {
        const drows = m.dtcs
          .map(
            (d) =>
              `<tr>
                 <td class="mono">${esc(d.code)}</td>
                 <td>${esc(d.description)}</td>
                 <td class="mono">${esc(d.system)}</td>
                 <td class="mono">${d.status != null ? esc(d.status) : "—"}</td>
               </tr>`,
          )
          .join("");
        return `<div class="small muted" style="margin-top:10px">${esc(m.name)}</div>
          <table class="tbl">
            <thead><tr><th>Code</th><th>Description</th><th>Sys</th><th>Status</th></tr></thead>
            <tbody>${drows}</tbody>
          </table>`;
      })
      .join("");

    const summary = `<div class="row small muted">
        Vehicle <b>${esc(r.vehicle)}</b> · ${r.presentCount}/${r.moduleCount} modules present ·
        <b>${r.dtcTotal}</b> stored DTC(s)</div>`;

    $("scanResult").innerHTML =
      summary +
      `<table class="tbl">
        <thead><tr><th>Module</th><th>Present</th><th>VIN / identity</th><th>#DTCs</th><th>Error</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>` +
      dtcBlocks;
  }

  function renderClear(r) {
    const rows = (r.results || [])
      .map((x) => {
        const pill = x.cleared
          ? '<span class="pill ok">cleared</span>'
          : '<span class="pill neutral">skipped</span>';
        const err = x.error
          ? `<span class="mono small">${esc(x.error)}</span>`
          : '<span class="muted">—</span>';
        return `<tr><td>${esc(x.name)}</td><td>${pill}</td><td>${err}</td></tr>`;
      })
      .join("");
    $("scanClearResult").innerHTML =
      `<div class="row small muted">Cleared <b>${r.clearedCount}</b> present module(s) on
         <b>${esc(r.vehicle)}</b>.</div>` +
      `<table class="tbl">
        <thead><tr><th>Module</th><th>Result</th><th>Error</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>`;
  }

  function errBanner(e) {
    return `<div class="result"><span class="pill bad">error</span>
      <span class="mono small">${esc(e && e.message ? e.message : e)}</span></div>`;
  }

  // ---- wiring --------------------------------------------------------------
  $("scanPick").addEventListener("click", async () => {
    try {
      const r = await dialog().open({ filters: [{ name: "Modules", extensions: ["json"] }], defaultPath: (window.OT && window.OT.packsDir) || undefined });
      if (!r) return;
      state.modulesPath = Array.isArray(r) ? r[0] : r;
      $("scanFile").textContent = baseName(state.modulesPath);
    } catch (e) {
      $("scanResult").innerHTML = errBanner(e);
    }
  });

  $("scanRun").addEventListener("click", async () => {
    if (!state.modulesPath) {
      $("scanResult").innerHTML =
        '<div class="muted small">Choose a vehicle module list (JSON) first.</div>';
      return;
    }
    $("scanClearResult").innerHTML = "";
    $("scanResult").innerHTML = '<div class="muted small">Scanning vehicle…</div>';
    try {
      const r = await invoke("autoscan_run", {
        ...window.OT.connArgs(),
        modulesPath: state.modulesPath,
        timeoutMs: 2000,
      });
      renderScan(r);
    } catch (e) {
      $("scanResult").innerHTML = errBanner(e);
    }
  });

  $("scanClear").addEventListener("click", async () => {
    if (!state.modulesPath) {
      $("scanClearResult").innerHTML =
        '<div class="muted small">Choose a vehicle module list (JSON) first.</div>';
      return;
    }
    if (!window.confirm("Clear stored DTCs on every present module in this list? Each module is addressed on its own CAN ids.")) return;
    $("scanClearResult").innerHTML = '<div class="muted small">Clearing DTCs on present modules…</div>';
    try {
      const r = await invoke("autoscan_clear", {
        ...window.OT.connArgs(),
        modulesPath: state.modulesPath,
        timeoutMs: 2000,
      });
      renderClear(r);
    } catch (e) {
      $("scanClearResult").innerHTML = errBanner(e);
    }
  });
})();
