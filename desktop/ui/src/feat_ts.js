// OpenTune — Aftermarket ECU panel (TunerStudio-compatible: Speeduino, rusEFI, MegaSquirt).
// Offline: INI + .msq load/save/diff, table/curve/scalar editing of the working
// tune with INI-limit validation. Online (serial port or "sim"): connect +
// signature check, read, plan write (diff in a confirm dialog), write with a
// backup first and read-back verify, burn toggle, live channel tiles.
// Backend: feat_ts.rs (ts_* commands). Talks to an engine ECU: tier "bench".
"use strict";
(function () {
  const TAURI = window.__TAURI__ || {};
  const invoke = (c, a) => TAURI.core.invoke(c, a);
  const { esc, heatColor, tsFmt, tsCheckValue, seriesPath } = window.OTUtil;
  const msg = (e) => (typeof e === "string" ? e : (e && e.message) || String(e));
  const banner = (cls, html) => `<div class="banner ${cls}">${html}</div>`;
  const base = (p) => (p ? String(p).split(/[\\/]/).pop() : "—");
  const pick = (r) => (Array.isArray(r) ? r[0] : r);
  const INI_F = [{ name: "TunerStudio INI", extensions: ["ini"] }, { name: "All files", extensions: ["*"] }];
  const MSQ_F = [{ name: "Tune (.msq)", extensions: ["msq"] }, { name: "All files", extensions: ["*"] }];

  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-ts";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Aftermarket ECU</h1>
      <p>Speeduino, rusEFI and MegaSquirt through their TunerStudio INI: open and edit tunes offline, then read, write and burn over the serial link.</p>
    </header>
    <div class="small muted mono" id="tsHead" style="margin-bottom:10px">no INI loaded</div>

    <div class="card">
      <h2>Definition and tune (offline)</h2>
      <div class="row">
        <button class="btn" id="tsIniPick" type="button">Open INI…</button>
        <span class="small mono muted" id="tsIniPath">no INI chosen</span>
        <label class="small muted" for="tsSet">#set</label>
        <input class="inp mono" id="tsSet" placeholder="e.g. LAMBDA, mcu_teensy" style="width:190px" />
        <label class="small muted" for="tsUnset">#unset</label>
        <input class="inp mono" id="tsUnset" style="width:120px" />
        <button class="btn primary" id="tsIniLoad" type="button" disabled>Load</button>
      </div>
      <div id="tsIniOut"></div>
      <div class="row" style="margin-top:8px">
        <button class="btn" id="tsMsqOpen" type="button" disabled>Open .msq…</button>
        <button class="btn" id="tsMsqSave" type="button" disabled>Save .msq…</button>
        <button class="btn" id="tsMsqDiff" type="button" disabled>Diff with .msq…</button>
        <label class="confirm" title="Load a tune whose signature differs from the INI's"><input type="checkbox" id="tsForceMsq" /> ignore tune signature</label>
      </div>
      <div id="tsMsqOut"></div>
    </div>

    <div class="card">
      <div class="card-title-row"><h2>Edit</h2>
        <select class="inp" id="tsView" disabled><option value="">tables, curves, settings…</option></select></div>
      <p class="small muted">Click a cell or value to change it. Values outside the INI's limits are refused, never clamped. Changes stay in the working tune until you save or write it.</p>
      <div id="tsEdit"></div>
    </div>

    <div class="card">
      <h2>ECU (online)</h2>
      <div class="row">
        <label class="small muted" for="tsPort">Port</label>
        <input class="inp mono" id="tsPort" placeholder="/dev/ttyACM0, COM3 or sim" style="width:190px" />
        <label class="small muted" for="tsBaud">Baud</label>
        <input class="inp sm mono" id="tsBaud" type="number" value="115200" />
        <select class="inp" id="tsFraming" title="Framing"><option value="auto">framing: from INI</option><option value="envelope">msEnvelope (CRC32)</option><option value="plain">plain</option></select>
        <label class="confirm"><input type="checkbox" id="tsForceSig" /> ignore ECU signature</label>
        <button class="btn primary" id="tsConnect" type="button" disabled>Connect</button>
        <button class="btn" id="tsDisconnect" type="button" disabled>Disconnect</button>
      </div>
      <div id="tsConnOut"></div>
      <div class="row">
        <button class="btn" id="tsRead" type="button" disabled>Read from ECU</button>
        <button class="btn" id="tsPlan" type="button" disabled>Plan write</button>
        <label class="confirm" title="Burn verified pages to permanent storage; off = RAM only, lost at power-off"><input type="checkbox" id="tsBurn" checked /> burn after verify</label>
        <button class="btn warn" id="tsWrite" type="button" disabled>Write to ECU…</button>
      </div>
      <p class="small muted">Write with the engine <b>off</b> and ignition on, on a stable supply. Before writing, OpenTune re-reads the ECU, saves its current tune as a backup (.msq + raw .pages.bin), writes only the changed bytes, reads every page back and burns only if all verified.</p>
      <div id="tsOnlineOut"></div>
      <h3 style="margin-top:14px">Live channels</h3>
      <div class="row">
        <input class="inp mono" id="tsChans" value="rpm, map, coolant, afr, tps, advance, batteryV" style="min-width:280px;flex:1" />
        <button class="btn" id="tsLive" type="button" disabled>Start (5 Hz)</button>
        <span class="pill neutral" id="tsLiveState">idle</span>
      </div>
      <div class="dash-grid" id="tsTiles" style="margin-top:10px"></div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "ts", label: "Aftermarket ECU", icon: "⚒", group: "vehicle", tier: "bench" });

  const $ = (id) => panel.querySelector("#" + id);
  const list = (s) => String(s || "").split(/[,\s]+/).filter(Boolean);
  const S = { iniPath: null, ini: null, values: null, view: "", connected: false, ecuRead: false, timer: null, busy: false };

  function enable() {
    const ini = !!S.ini;
    ["tsMsqOpen", "tsMsqSave", "tsMsqDiff", "tsView", "tsConnect"].forEach((id) => ($(id).disabled = !ini));
    $("tsDisconnect").disabled = !S.connected;
    ["tsRead", "tsLive"].forEach((id) => ($(id).disabled = !S.connected));
    ["tsPlan", "tsWrite"].forEach((id) => ($(id).disabled = !(S.connected && S.ecuRead)));
  }
  async function head() {
    try {
      const s = await invoke("ts_status");
      S.connected = !!s.connected; S.ecuRead = !!s.ecu_read;
      $("tsHead").textContent = s.ini
        ? `INI ${base(s.ini)} (${s.signature || "no signature"}) · working tune: ${s.working || "—"} · ${s.connected ? "connected: " + s.connected : "offline"}${s.ecu_read ? " · ECU tune read" : ""}`
        : "no INI loaded";
    } catch (e) { /* not fatal */ }
    enable();
  }

  // ---- INI ------------------------------------------------------------------
  $("tsIniPick").addEventListener("click", async () => {
    const p = pick(await TAURI.dialog.open({ multiple: false, directory: false, filters: INI_F }));
    if (!p) return;
    S.iniPath = p;
    $("tsIniPath").textContent = base(p);
    $("tsIniLoad").disabled = false;
  });
  $("tsIniLoad").addEventListener("click", async () => {
    stopLive();
    const out = $("tsIniOut");
    try {
      const r = await invoke("ts_load_ini", { path: S.iniPath, set: list($("tsSet").value), unset: list($("tsUnset").value) });
      S.ini = r;
      const w = r.warnings;
      out.innerHTML = `<div class="meta-grid" style="margin-top:10px">
        <div class="k">signature</div><div class="v">${esc(r.signature || "—")}</div>
        <div class="k">framing</div><div class="v">${esc(r.framing)}</div>
        <div class="k">pages</div><div class="v">${r.pages.map((p) => `${p.page}: ${p.size} B${p.burnable ? "" : " (RAM only)"}`).join(", ")}</div>
        <div class="k">contents</div><div class="v">${r.constants} constants, ${r.pc_variables} PC variables, ${r.tables.length} tables, ${r.curves.length} curves, ${r.channels.length} channels</div>
        <div class="k">conditions</div><div class="v">${(r.conditions || []).map(esc).join(", ") || "none"}</div>
        <div class="k">warnings</div><div class="v">${w.invalid} invalid, ${w.reference} reference, ${w.ignored} ignored</div>
      </div>` + (w.invalid ? banner("bad", "Lines that could not be parsed: " + w.first_invalid.map((x) => `line ${x.line} [${esc(x.section)}] ${esc(x.message)}`).join("; ")) : "")
        + (r.setting_groups.length ? `<p class="small muted">Setting groups (use #set): ${r.setting_groups.map((g) => `${esc(g.label)}: ${g.options.map((o) => esc(o.name)).join(" / ")}`).join(" · ")}</p>` : "");
      const sel = $("tsView");
      sel.innerHTML = '<option value="">tables, curves, settings…</option>' +
        '<option value="s:">Settings (scalars and options)</option>' +
        r.tables.map((t) => `<option value="t:${esc(t.id)}">Table — ${esc(t.title || t.id)}</option>`).join("") +
        r.curves.map((c) => `<option value="c:${esc(c.id)}">Curve — ${esc(c.title || c.id)}</option>`).join("");
      $("tsEdit").innerHTML = "";
      $("tsMsqOut").innerHTML = "";
      $("tsOnlineOut").innerHTML = "";
      $("tsConnOut").innerHTML = "";
    } catch (e) { out.innerHTML = banner("bad", esc(msg(e))); }
    head();
  });

  // ---- .msq -----------------------------------------------------------------
  const reportHtml = (r) => {
    const rep = r.report || {};
    let h = `<p class="small">${rep.applied} entries applied${r.entries != null ? ` of ${r.entries}` : ""}${r.base ? ` over the ${esc(r.base)} tune` : ""}.</p>`;
    if ((rep.unknown || []).length) h += `<p class="small muted">Not in this INI: ${rep.unknown.slice(0, 20).map(esc).join(", ")}${rep.unknown.length > 20 ? " …" : ""}</p>`;
    if ((rep.skipped || []).length) h += banner("bad", "Could not apply: " + rep.skipped.map((s) => `${esc(s.name)} (${esc(s.why)})`).join("; "));
    if ((rep.out_of_range || []).length) h += `<p class="small muted">Outside the INI limits: ${rep.out_of_range.map(esc).join(", ")}</p>`;
    if ((rep.notes || []).length) h += `<p class="small muted">${rep.notes.map(esc).join(" · ")}</p>`;
    if ((r.conditions_added || []).length) h += `<p class="small muted">INI re-read with the tune's saved settings: ${r.conditions_added.map(esc).join(", ")}</p>`;
    return h;
  };
  const diffTable = (rows) => rows.length
    ? `<table class="tbl"><thead><tr><th>Constant</th><th>Change</th></tr></thead><tbody>${rows.map((r) => `<tr><td class="mono">${esc(r.name)}</td><td class="mono">${esc(r.change)}</td></tr>`).join("")}</tbody></table>`
    : '<p class="small muted">No differences.</p>';
  $("tsMsqOpen").addEventListener("click", async () => {
    const p = pick(await TAURI.dialog.open({ multiple: false, directory: false, filters: MSQ_F }));
    if (!p) return;
    try {
      const r = await invoke("ts_load_msq", { path: p, force: $("tsForceMsq").checked });
      $("tsMsqOut").innerHTML = banner("ok", `Loaded ${esc(base(p))} (signature ${esc(r.signature || "—")}) as the working tune.`) + reportHtml(r);
      refreshView();
    } catch (e) { $("tsMsqOut").innerHTML = banner("bad", esc(msg(e))); }
    head();
  });
  $("tsMsqSave").addEventListener("click", async () => {
    const p = await TAURI.dialog.save({ defaultPath: "tune.msq", filters: MSQ_F });
    if (!p) return;
    try {
      const r = await invoke("ts_save_msq", { path: p });
      $("tsMsqOut").innerHTML = banner("ok", `Saved ${r.entries} entries to ${esc(base(r.path))}.`) +
        (r.skipped.length ? `<p class="small muted">Not saved: ${r.skipped.map((s) => `${esc(s.name)} (${esc(s.why)})`).join("; ")}</p>` : "");
    } catch (e) { $("tsMsqOut").innerHTML = banner("bad", esc(msg(e))); }
  });
  $("tsMsqDiff").addEventListener("click", async () => {
    const p = pick(await TAURI.dialog.open({ multiple: false, directory: false, filters: MSQ_F }));
    if (!p) return;
    try {
      const r = await invoke("ts_diff_msq", { path: p, force: $("tsForceMsq").checked });
      $("tsMsqOut").innerHTML = `<p class="small"><b>${r.count}</b> constant(s) differ between ${esc(base(p))} and the working tune (${esc(r.direction)}).</p>` + diffTable(r.rows);
    } catch (e) { $("tsMsqOut").innerHTML = banner("bad", esc(msg(e))); }
  });

  // ---- editor ---------------------------------------------------------------
  $("tsView").addEventListener("change", () => { S.view = $("tsView").value; refreshView(); });
  async function refreshView() {
    const out = $("tsEdit");
    if (!S.view) { out.innerHTML = ""; return; }
    const [kind, id] = [S.view.slice(0, 1), S.view.slice(2)];
    try {
      if (kind === "t") out.innerHTML = tableHtml(await invoke("ts_table", { id }));
      else if (kind === "c") out.innerHTML = curveHtml(await invoke("ts_curve", { id }));
      else out.innerHTML = valuesHtml(await invoke("ts_values"));
    } catch (e) { out.innerHTML = banner("bad", esc(msg(e))); }
  }
  function tableHtml(t) {
    S.cur = { kind: "t", name: t.z.name, meta: t.z, cells: t.cells.map((r) => r.slice()), cols: t.cols };
    const flat = [].concat(...t.cells);
    let lo = Math.min(...flat), hi = Math.max(...flat);
    if (lo === hi) hi = lo + 1;
    const xv = (t.x && t.x.values) || [], yv = (t.y && t.y.values) || [];
    const d = t.z.digits;
    let h = `<div class="meta-grid" style="margin-top:6px"><div class="k">table</div><div class="v">${esc(t.title)} (${esc(t.z.name)}, ${t.rows}×${t.cols} ${esc(t.z.units || "")})</div>
      <div class="k">axes</div><div class="v">x ${esc(t.x ? t.x.name + " [" + (t.x.units || "") + "]" : "—")} · y ${esc(t.y ? t.y.name + " [" + (t.y.units || "") + "]" : "—")}</div>
      <div class="k">limits</div><div class="v">${tsFmt(t.z.min, d)} … ${tsFmt(t.z.max, d)}</div></div>`;
    // Rows are drawn top = highest load, like TunerStudio.
    let body = "";
    for (let r = t.rows - 1; r >= 0; r--) {
      body += `<tr><th>${yv[r] != null ? tsFmt(yv[r], t.y.digits) : r}</th>`;
      for (let c = 0; c < t.cols; c++) {
        const v = t.cells[r][c];
        body += `<td class="cell" data-i="${r * t.cols + c}" style="background:${heatColor((v - lo) / (hi - lo))}" title="[${r},${c}] ${tsFmt(v, d)} ${esc(t.z.units || "")}">${tsFmt(v, d)}</td>`;
      }
      body += "</tr>";
    }
    body += `<tr><th>${esc(t.y ? t.y.name : "")}\\${esc(t.x ? t.x.name : "")}</th>${Array.from({ length: t.cols }, (_, c) => `<th>${xv[c] != null ? tsFmt(xv[c], t.x.digits) : c}</th>`).join("")}</tr>`;
    return h + `<div class="heatwrap"><table class="heat ts-heat"><tbody>${body}</tbody></table></div>`;
  }
  function curveHtml(c) {
    const xv = (c.x && c.x.values) || [];
    const y0 = c.ys[0];
    S.cur = { kind: "c", name: y0 ? y0.name : null, meta: y0 && y0.meta, cells: [((y0 && y0.values) || []).slice()], cols: xv.length };
    let h = `<p class="small"><b>${esc(c.title)}</b> — x ${esc(c.x ? c.x.name : "—")} [${esc((c.x && c.x.units) || "")}]` +
      c.ys.map((y) => ` · y ${esc(y.name)} [${esc(y.units || "")}]`).join("") + "</p>";
    if (y0 && y0.values) {
      const p = seriesPath(xv.length ? xv : y0.values.map((_, i) => i), y0.values, 600, 80);
      h += `<div class="hx-chart"><svg class="hx-svg" viewBox="0 0 600 80" preserveAspectRatio="none"><path class="line" d="${p.d}"/></svg></div>`;
    }
    h += `<div class="heatwrap"><table class="heat ts-heat"><tbody><tr><th>${esc(c.x ? c.x.name : "x")}</th>${xv.map((x) => `<th>${tsFmt(x, c.x.digits)}</th>`).join("")}</tr>`;
    c.ys.forEach((y, yi) => {
      const vals = y.values || [];
      let lo = Math.min(...vals), hi = Math.max(...vals);
      if (!(hi > lo)) hi = lo + 1;
      h += `<tr><th>${esc(y.name)}</th>${vals.map((v, i) => `<td class="cell${yi ? " ro" : ""}" ${yi ? "" : `data-i="${i}"`} style="background:${heatColor((v - lo) / (hi - lo))}">${tsFmt(v, y.digits)}</td>`).join("")}</tr>`;
    });
    return h + "</tbody></table></div>" + (c.ys.length > 1 ? '<p class="small muted">Only the first line is editable here.</p>' : "");
  }
  function valuesHtml(r) {
    S.values = r.values;
    const rows = r.values.map((v, i) => {
      const m = v.meta;
      const lim = m.class === "scalar" ? `${tsFmt(m.min, m.digits)} … ${tsFmt(m.max, m.digits)}` : m.class === "bits" ? `${(m.options || []).length} options` : m.class === "string" ? `≤ ${m.len} chars` : "";
      return `<tr data-v="${i}"><td class="mono">${esc(v.name)}${m.pc ? ' <span class="pill neutral">PC</span>' : ""}</td>
        <td class="mono ts-val" tabindex="0">${v.error ? `<span class="muted">${esc(v.error)}</span>` : esc(v.display)}</td><td class="small muted">${esc(lim)}</td></tr>`;
    }).join("");
    return `<input class="inp" id="tsFilter" placeholder="filter…" style="margin:6px 0;width:220px" />
      <div class="cd-wrap"><table class="tbl" id="tsValTbl"><thead><tr><th>Name</th><th>Value</th><th>Limits</th></tr></thead><tbody>${rows}</tbody></table></div>`;
  }
  $("tsEdit").addEventListener("input", (ev) => {
    if (ev.target.id !== "tsFilter") return;
    const q = ev.target.value.toLowerCase();
    panel.querySelectorAll("#tsValTbl tbody tr").forEach((tr) => { tr.hidden = !!q && !tr.textContent.toLowerCase().includes(q); });
  });
  // Inline editors: a cell or a value turns into an input (or a select for options).
  $("tsEdit").addEventListener("click", (ev) => {
    const td = ev.target.closest("td.cell[data-i], td.ts-val");
    if (!td || td.querySelector("input,select")) return;
    if (td.classList.contains("ts-val")) return editValue(td);
    const i = +td.dataset.i, cur = S.cur, cols = cur.cols;
    const old = cur.kind === "t" ? cur.cells[Math.floor(i / cols)][i % cols] : cur.cells[0][i];
    inlineInput(td, tsFmt(old, cur.meta && cur.meta.digits), async (text) => {
      const x = Number(text);
      const err = tsCheckValue(Object.assign({ name: cur.name }, cur.meta), x);
      if (err) throw err;
      const flat = cur.kind === "t" ? [].concat(...cur.cells) : cur.cells[0].slice();
      flat[i] = x;
      await invoke("ts_set", { name: cur.name, value: flat });
    });
  });
  function editValue(td) {
    const v = S.values[+td.closest("tr").dataset.v], m = v.meta;
    if (m.class === "bits") {
      const sel = document.createElement("select");
      sel.className = "inp";
      sel.innerHTML = (m.options || []).map((o, i) => `<option value="${i}" ${o === "INVALID" ? "disabled" : ""} ${v.value && v.value.value === i ? "selected" : ""}>${esc(o)}</option>`).join("");
      td.innerHTML = ""; td.appendChild(sel); sel.focus();
      sel.addEventListener("change", () => commit(td, () => invoke("ts_set", { name: v.name, value: +sel.value })));
      sel.addEventListener("blur", () => setTimeout(refreshView, 150));
      return;
    }
    const now = v.value ? v.value.value : "";
    inlineInput(td, m.class === "scalar" ? tsFmt(now, m.digits) : now, async (text) => {
      if (m.class === "scalar") {
        const x = Number(text);
        const err = tsCheckValue(m, x);
        if (err) throw err;
        await invoke("ts_set", { name: v.name, value: x });
      } else await invoke("ts_set", { name: v.name, value: text });
    });
  }
  function inlineInput(td, initial, save) {
    const inp = document.createElement("input");
    inp.className = "cell-input";
    inp.value = initial;
    td.innerHTML = ""; td.appendChild(inp); inp.focus(); inp.select();
    let done = false;
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") { done = true; commit(td, () => save(inp.value.trim())); }
      if (e.key === "Escape") { done = true; refreshView(); }
    });
    inp.addEventListener("blur", () => { if (!done) refreshView(); });
  }
  async function commit(td, fn) {
    try { await fn(); } catch (e) { window.toast && window.toast(msg(e), true); }
    refreshView();
  }

  // ---- online -----------------------------------------------------------------
  const online = (html) => ($("tsOnlineOut").innerHTML = html);
  $("tsConnect").addEventListener("click", async () => {
    const out = $("tsConnOut");
    out.innerHTML = '<span class="muted small">connecting…</span>';
    try {
      const r = await invoke("ts_connect", { port: $("tsPort").value, baud: +$("tsBaud").value || null, framing: $("tsFraming").value, forceSignature: $("tsForceSig").checked });
      const s = r.signature;
      out.innerHTML = banner(s.matches ? "ok" : "bad", `Connected to ${esc(r.label)} (${esc(r.framing)}). ECU signature <span class="mono">${esc(s.ecu)}</span> ` +
        (s.matches ? "matches the INI." : `does <b>not</b> match the INI's <span class="mono">${esc(s.ini || "—")}</span> (ignored on request).`));
    } catch (e) { out.innerHTML = banner("bad", esc(msg(e))); }
    head();
  });
  $("tsDisconnect").addEventListener("click", async () => {
    stopLive();
    try { await invoke("ts_disconnect"); $("tsConnOut").innerHTML = banner("info", "Disconnected; the port is released."); } catch (e) { $("tsConnOut").innerHTML = banner("bad", esc(msg(e))); }
    head();
  });
  $("tsRead").addEventListener("click", async () => {
    online('<span class="muted small">reading every page…</span>');
    try {
      const r = await invoke("ts_read");
      online(banner("ok", `Read ${r.pages} page(s), ${r.bytes} bytes in ${r.elapsed_s.toFixed(1)} s. This is now the working tune and the base for a write.`));
      refreshView();
    } catch (e) { online(banner("bad", esc(msg(e)))); }
    head();
  });
  const planHtml = (p) => p.empty
    ? banner("info", "The ECU already holds the working tune: nothing to write.")
    : `<p class="small"><b>${p.changed_bytes}</b> byte(s) in page(s) ${p.pages.map((x) => x.page + (x.burnable ? "" : " (RAM only)")).join(", ")}; ${p.constants.length} constant(s):</p>` + diffTable(p.constants);
  $("tsPlan").addEventListener("click", async () => {
    try { online(planHtml(await invoke("ts_plan_write"))); } catch (e) { online(banner("bad", esc(msg(e)))); }
  });
  $("tsWrite").addEventListener("click", async () => {
    let p;
    try { p = await invoke("ts_plan_write"); } catch (e) { return online(banner("bad", esc(msg(e)))); }
    online(planHtml(p));
    if (p.empty) return;
    const burn = $("tsBurn").checked;
    const lines = p.constants.slice(0, 12).map((r) => `  ${r.name}: ${r.change}`).join("\n") + (p.constants.length > 12 ? `\n  … and ${p.constants.length - 12} more` : "");
    const ok = window.confirm(
      `Write ${p.changed_bytes} byte(s) to the ECU on ${p.connected || "the connected port"}?\n\n${lines}\n\n` +
      `The ECU's current tune is saved first (you choose where next). Every page is verified by read-back; ` +
      (burn ? "verified pages are then BURNED to permanent storage." : "nothing is burned: the change is lost at power-off.") +
      "\n\nEngine off, ignition on, stable supply?");
    if (!ok) return online(planHtml(p) + banner("info", "Write cancelled — nothing sent."));
    const d = new Date(), pad = (n) => String(n).padStart(2, "0");
    const stamp = `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
    const backupPath = await TAURI.dialog.save({ title: "Save the ECU's current tune (backup) as…", defaultPath: `ecu-backup-${stamp}.msq`, filters: MSQ_F });
    if (!backupPath) return online(planHtml(p) + banner("info", "No backup location chosen — nothing written."));
    stopLive();
    online('<span class="muted small">re-reading, saving the backup, writing, verifying' + (burn ? ", burning" : "") + "…</span>");
    try {
      const r = await invoke("ts_write", { confirmed: true, burn, backupPath });
      online(r.written
        ? banner("ok", `Wrote ${r.bytes_written} byte(s); verified page(s) ${r.verified_pages.join(", ")}. ` +
            (r.burn ? `Burned page(s) ${r.burned.join(", ") || "—"}${r.not_burnable.length ? `; RAM only: ${r.not_burnable.join(", ")}` : ""}.` : "Not burned: lost at power-off.") +
            (r.crc_checked.length ? ` Page CRC32 OK: ${r.crc_checked.join(", ")}.` : "")) +
          `<p class="small muted">Backup: <span class="mono">${esc(r.backup)}</span> + <span class="mono">${esc(base(r.raw_backup))}</span></p>`
        : banner("info", esc(r.reason)));
    } catch (e) { online(banner("bad", esc(msg(e)))); }
    head();
  });

  // ---- live channels ------------------------------------------------------------
  function stopLive() {
    if (S.timer) { clearInterval(S.timer); S.timer = null; }
    $("tsLive").textContent = "Start (5 Hz)";
    const p = $("tsLiveState"); p.className = "pill neutral"; p.textContent = "idle";
  }
  async function pollOnce() {
    if (S.busy) return; // never overlap polls
    S.busy = true;
    try {
      const r = await invoke("ts_poll", { channels: list($("tsChans").value) });
      $("tsTiles").innerHTML = r.samples.map((s) => `<div class="tile ${s.value == null ? "na" : "live"}" title="${esc(s.note || "")}">
        <div class="t-label"><span>${esc(s.name)}</span><i class="t-dot"></i></div>
        <div><span class="t-val">${s.value == null ? "—" : tsFmt(s.value, Math.abs(s.value) >= 100 ? 0 : 2)}</span><span class="t-unit">${esc(s.units || "")}</span></div>
        <div class="t-foot"><span>${esc(s.note ? s.note.slice(0, 40) : "")}</span><span></span></div></div>`).join("");
    } catch (e) {
      stopLive();
      const p = $("tsLiveState"); p.className = "pill bad"; p.textContent = msg(e);
    } finally { S.busy = false; }
  }
  $("tsLive").addEventListener("click", () => {
    if (S.timer) return stopLive();
    S.timer = setInterval(pollOnce, 200);
    pollOnce();
    $("tsLive").textContent = "Stop";
    const p = $("tsLiveState"); p.className = "pill accent"; p.textContent = "live";
  });
  new MutationObserver(() => { if (S.timer && !panel.classList.contains("active")) stopLive(); })
    .observe(panel, { attributes: true, attributeFilter: ["class"] });

  window.onPanelFirstShow("ts", head);
})();
