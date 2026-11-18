// OpenTune — Inspection (OBD-II readiness, DTCs, freeze frame, vehicle info).
// What an emissions / periodic inspection reads over generic OBD-II: the MIL
// and stored-code count, readiness monitors, stored / pending / permanent DTCs,
// the freeze frame and the calibration IDs + CVNs (mode 09).
// Backend: obd_inspect (one command; `sections` picks what to read). Shares the
// global adapter connection (window.OT) and the app-wide adapter lock.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const store = {
    get(k, d) { try { const v = localStorage.getItem("ot.insp." + k); return v == null ? d : JSON.parse(v); } catch (e) { return d; } },
    set(k, v) { try { localStorage.setItem("ot.insp." + k, JSON.stringify(v)); } catch (e) {} },
  };
  const SECTIONS = { readiness: "readiness", dtcs: "dtcs", freeze: "freeze_frame", info: "info" };
  const STATE_TEXT = { complete: "complete", incomplete: "incomplete", not_supported: "not supported" };
  const CYCLE_TEXT = { complete: "complete", incomplete: "incomplete", not_supported: "disabled" };
  const LABEL = { engine: "Engine", transmission: "Transmission", other: "Module" };

  let ecus = {}; // tx -> merged ecu entry
  let order = [];
  let current = null;
  let silent = [];
  let busy = false;

  // ---- styles (panel-scoped) -----------------------------------------------
  const css = document.createElement("style");
  css.textContent = `
    #panel-inspection .insp-ecus { margin-bottom: 12px; }
    #panel-inspection .insp-sum { display: grid; grid-template-columns: auto 1fr 1fr 1fr; gap: 16px; align-items: center; }
    #panel-inspection .mil { display: flex; align-items: center; gap: 12px; }
    #panel-inspection .mil-lamp { width: 54px; height: 54px; border-radius: 50%; display: grid; place-items: center;
      background: var(--line); color: var(--muted); border: 2px solid var(--card-border); }
    #panel-inspection .mil-lamp svg { width: 32px; height: 32px; }
    #panel-inspection .mil.on .mil-lamp { background: radial-gradient(circle at 40% 35%, #ffd27a, #f29d0b 60%, #b26b00);
      color: #3b2400; border-color: #f29d0b; box-shadow: 0 0 18px rgba(242, 157, 11, .55); }
    #panel-inspection .mil-txt b { display: block; font-size: 14px; }
    #panel-inspection .stat .k { font-size: 11px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--ink-soft); }
    #panel-inspection .stat .v { font: 700 28px var(--mono); font-variant-numeric: tabular-nums; }
    #panel-inspection .stat .v.sm { font-size: 16px; }
    #panel-inspection .stat .v.bad { color: var(--bad); }
    #panel-inspection .stat .v.ok { color: var(--good); }
    #panel-inspection .mon-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(190px, 1fr)); gap: 10px; }
    #panel-inspection .mon { border: 1px solid var(--card-border); border-radius: 10px; padding: 9px 12px; display: flex; flex-direction: column; gap: 5px; background: var(--card); }
    #panel-inspection .mon.na { opacity: .55; }
    #panel-inspection .mon .mn { font-weight: 600; font-size: 13px; }
    #panel-inspection .mon .mc { font-size: 11px; color: var(--muted); }
    #panel-inspection .st { display: inline-block; padding: 2px 9px; border-radius: 20px; font-size: 11px; font-weight: 700; align-self: flex-start; }
    #panel-inspection .st.complete { background: var(--good-bg); color: var(--good); }
    #panel-inspection .st.incomplete { background: var(--accent-soft); color: var(--accent); }
    #panel-inspection .st.not_supported { background: rgba(127, 127, 127, .15); color: var(--ink-soft); }
    #panel-inspection .dtc-cols { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 14px; }
    #panel-inspection .dtc { display: flex; gap: 10px; padding: 6px 0; border-bottom: 1px solid var(--line); font-size: 12.5px; }
    #panel-inspection .dtc:last-child { border-bottom: 0; }
    #panel-inspection .dtc .code { font: 700 13px var(--mono); color: var(--bad); min-width: 52px; }
    #panel-inspection .dtc-col.pending .code { color: var(--warn); }
    #panel-inspection .kv { display: grid; grid-template-columns: 140px 1fr; gap: 6px 14px; font-size: 13px; }
    #panel-inspection .kv .k { color: var(--ink-soft); }
    #panel-inspection .cvn-mod { color: var(--warn); font-weight: 700; }
    @media (max-width: 720px) { #panel-inspection .insp-sum { grid-template-columns: 1fr 1fr; } }`;
  document.head.appendChild(css);

  // ---- build ---------------------------------------------------------------
  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-inspection";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Inspection</h1>
      <p>What an emissions inspection reads over OBD-II: check-engine lamp, readiness monitors, trouble codes, freeze frame and calibration IDs with their verification numbers.</p>
    </header>
    <div class="card">
      <div class="row">
        <button class="btn primary" id="inspAll" type="button">Read all</button>
        <button class="btn sm" data-sec="readiness" type="button">Readiness</button>
        <button class="btn sm" data-sec="dtcs" type="button">Codes</button>
        <button class="btn sm" data-sec="freeze" type="button">Freeze frame</button>
        <button class="btn sm" data-sec="info" type="button">Vehicle info</button>
        <label class="chk small"><input type="checkbox" id="inspAllEcus"> All OBD modules (7E0–7E7)</label>
        <span class="pill neutral" id="inspStatus">idle</span>
      </div>
      <p class="small muted" id="inspTarget">Ignition on. Reads the engine ECU unless a target is set in the sidebar or “All OBD modules” is ticked.</p>
    </div>
    <div class="seg insp-ecus" id="inspEcus" role="group" aria-label="Module" hidden></div>
    <div id="inspBody"></div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "inspection", label: "Inspection", icon: "✓", group: "vehicle", tier: "bench" });

  const $ = (id) => panel.querySelector("#" + id);
  const status = (cls, text) => { const p = $("inspStatus"); p.className = "pill " + cls; p.textContent = text; };
  $("inspAllEcus").checked = !!store.get("allEcus", false);

  const milSvg = `<svg viewBox="0 0 32 32" aria-hidden="true" fill="currentColor"><path d="M9 11h3V9h8v2h2l2 2h2v-2h2v9h-2v-2h-2v3l-3 3H12l-2-3H8v2H5v-8h3v-3z"/></svg>`;

  // ---- rendering -----------------------------------------------------------
  function err(e, key) { return e && e.errors && e.errors[key]; }
  function errBox(msg) { return `<div class="banner bad">${esc(msg)}</div>`; }
  function fmtVal(v) {
    if (v == null) return "—";
    if (Array.isArray(v)) return v.map((x) => (x == null ? "—" : fmtVal(x))).join(" / ");
    return Math.abs(v - Math.round(v)) < 1e-9 ? String(Math.round(v)) : v.toFixed(Math.abs(v) < 10 ? 2 : 1);
  }

  function summary(e) {
    const r = e.readiness && e.readiness.status;
    if (!r) return err(e, "readiness") ? errBox("Readiness: " + err(e, "readiness")) : "";
    const inc = r.monitors.filter((m) => m.state === "incomplete");
    const ready = inc.length === 0;
    return `<div class="card insp-sum">
      <div class="mil ${r.mil ? "on" : ""}"><div class="mil-lamp" title="Malfunction indicator lamp">${milSvg}</div>
        <div class="mil-txt"><b>Check-engine lamp ${r.mil ? "ON" : "off"}</b><span class="small muted">MIL commanded by the ECU</span></div></div>
      <div class="stat"><div class="k">Stored codes</div><div class="v ${r.dtc_count ? "bad" : "ok"}">${r.dtc_count}</div></div>
      <div class="stat"><div class="k">Readiness</div><div class="v sm ${ready ? "ok" : "bad"}">${ready ? "All complete" : inc.length + " not ready"}</div></div>
      <div class="stat"><div class="k">Engine</div><div class="v sm">${r.ignition === "compression" ? "Diesel (compression)" : "Petrol (spark)"}</div></div>
    </div>`;
  }

  function readiness(e) {
    const rd = e.readiness;
    if (!rd) return "";
    const cyc = {};
    ((rd.this_cycle && rd.this_cycle.monitors) || []).forEach((m) => { cyc[m.key] = m.state; });
    const tiles = rd.status.monitors.map((m) => `
      <div class="mon ${m.state === "not_supported" ? "na" : ""}">
        <span class="mn">${esc(m.name)}</span>
        <span class="st ${m.state}">${STATE_TEXT[m.state] || m.state}</span>
        <span class="mc">${m.continuous ? "continuous · " : ""}this drive cycle: ${cyc[m.key] ? CYCLE_TEXT[cyc[m.key]] : "—"}</span>
      </div>`).join("");
    return `<div class="card"><h2>Readiness monitors</h2>
      <div class="mon-grid">${tiles}</div>
      <p class="small muted">“Since clear” status from PID 01; the line under each monitor is PID 41 (this drive cycle).
      ${rd.this_cycle ? "" : esc(rd.this_cycle_error || "")}</p></div>`;
  }

  function dtcCol(title, cls, list, error) {
    const body = error ? `<p class="small muted">Not available: ${esc(error)}</p>`
      : !list || !list.length ? `<p class="small muted">None</p>`
      : list.map((d) => `<div class="dtc"><span class="code">${esc(d.code)}</span><span>${esc(d.description)}</span></div>`).join("");
    return `<div class="dtc-col ${cls}"><h3>${title}${list && list.length ? ` (${list.length})` : ""}</h3>${body}</div>`;
  }

  function dtcs(e) {
    const d = e.dtcs;
    if (!d) return "";
    const er = d.errors || {};
    return `<div class="card"><h2>Trouble codes</h2><div class="dtc-cols">
      ${dtcCol("Stored (03)", "stored", d.stored, er.stored)}
      ${dtcCol("Pending (07)", "pending", d.pending, er.pending)}
      ${dtcCol("Permanent (0A)", "permanent", d.permanent, er.permanent)}
    </div><p class="small muted">Permanent codes cannot be cleared with a scan tool: the ECU erases them itself once the monitor has passed again.</p></div>`;
  }

  function freeze(e) {
    const f = e.freeze_frame;
    if (!f) return err(e, "freeze") ? `<div class="card"><h2>Freeze frame</h2><p class="small muted">${esc(err(e, "freeze"))}</p></div>` : "";
    if (!f.stored) return `<div class="card"><h2>Freeze frame</h2><p class="small muted">No freeze frame stored.</p></div>`;
    const head = f.dtc ? `Stored by <b class="mono">${esc(f.dtc.code)}</b> — ${esc(f.dtc.description)}` : "Trigger code not reported";
    const rows = f.values.map((v) => `<tr><td class="mono">${v.pid.toString(16).toUpperCase().padStart(2, "0")}</td>
      <td>${esc(v.name)}</td><td class="mono">${v.value != null ? esc(fmtVal(v.value)) : v.error ? "—" : "raw " + esc(v.raw)}</td>
      <td>${esc(v.unit)}${v.error ? ` <span class="small muted">${esc(v.error)}</span>` : ""}</td></tr>`).join("");
    const errs = Object.keys(f.errors || {}).map((k) => `${k}: ${f.errors[k]}`).join("; ");
    return `<div class="card"><h2>Freeze frame ${f.frame}</h2><p class="small">${head}</p>
      <table class="tbl"><thead><tr><th>PID</th><th>Parameter</th><th>Value</th><th>Unit</th></tr></thead><tbody>${rows}</tbody></table>
      ${errs ? `<p class="small muted">${esc(errs)}</p>` : ""}</div>`;
  }

  function info(e) {
    const i = e.info;
    if (!i) return "";
    const n = Math.max(i.calids.length, i.cvns.length);
    const rows = [];
    for (let k = 0; k < n; k++) {
      rows.push(`<tr><td>${k + 1}</td><td class="mono">${esc(i.calids[k] || "—")}</td><td class="mono">${esc(i.cvns[k] || "—")}</td></tr>`);
    }
    const errs = Object.keys(i.errors || {}).map((k) => `${k}: ${i.errors[k]}`).join("; ");
    return `<div class="card"><h2>Vehicle information</h2>
      <div class="kv">
        <div class="k">VIN</div><div class="mono">${esc(i.vin || "—")}</div>
        <div class="k">ECU name</div><div>${esc(i.ecu_name || "—")}</div>
        <div class="k">Mode 09 items</div><div class="mono small">${i.supported.length ? i.supported.map((p) => p.toString(16).toUpperCase().padStart(2, "0")).join(" ") : "—"}</div>
      </div>
      <h3>Calibration</h3>
      ${n ? `<table class="tbl"><thead><tr><th>#</th><th>Calibration ID (CALID)</th><th>Verification number (CVN)</th></tr></thead><tbody>${rows.join("")}</tbody></table>`
          : `<p class="small muted">No calibration IDs reported.</p>`}
      <div class="note">The <b>CVN</b> is a checksum the ECU computes over the calibration named by the CALID next to it. Compare it with the stock CVN for that CALID (inspection databases, the manufacturer, a stock read): <span class="cvn-mod">a CVN that differs from the stock value indicates a modified calibration.</span></div>
      ${errs ? `<p class="small muted">${esc(errs)}</p>` : ""}</div>`;
  }

  function render() {
    const tabs = $("inspEcus");
    tabs.hidden = order.length < 2;
    tabs.innerHTML = order.map((tx) => {
      const e = ecus[tx];
      return `<button type="button" data-tx="${esc(tx)}" class="${tx === current ? "on" : ""}">${esc(LABEL[e.label] || "Module")} ${esc(e.addr.rx)}</button>`;
    }).join("");
    tabs.querySelectorAll("button").forEach((b) => b.addEventListener("click", () => { current = b.dataset.tx; render(); }));
    const e = ecus[current];
    const body = $("inspBody");
    if (!e) {
      body.innerHTML = silent.length ? `<p class="small muted">No OBD module answered (asked ${silent.length}).</p>` : "";
      return;
    }
    $("inspTarget").textContent = `Module ${window.OT && window.OT.targetText ? window.OT.targetText(e.addr) : e.addr.tx + "/" + e.addr.rx}` +
      (silent.length ? ` · no answer from ${silent.join(", ")}` : "");
    if (!e.responded) {
      body.innerHTML = errBox("No response from this module: " + (err(e, "module") || "check ignition, bus and adapter."));
      return;
    }
    body.innerHTML = summary(e) + readiness(e) + dtcs(e) + freeze(e) + info(e);
  }

  // ---- data ----------------------------------------------------------------
  function merge(res, secs) {
    const keys = secs.length ? secs : Object.keys(SECTIONS);
    const fresh = [];
    (res.ecus || []).forEach((n) => {
      const tx = n.addr.tx;
      fresh.push(tx);
      const old = ecus[tx] || { errors: {} };
      const m = Object.assign({}, old, { addr: n.addr, label: n.label, responded: n.responded });
      m.errors = Object.assign({}, old.errors || {});
      keys.forEach((k) => {
        m[SECTIONS[k]] = n[SECTIONS[k]];
        delete m.errors[k];
      });
      delete m.errors.module;
      Object.assign(m.errors, n.errors || {});
      ecus[tx] = m;
    });
    if (fresh.length) {
      order = Array.from(new Set(order.concat(fresh))).sort();
      if (!current || !ecus[current]) current = fresh[0];
    }
    silent = res.silent || [];
  }

  async function read(secs) {
    if (busy) return;
    busy = true;
    panel.querySelectorAll("button.btn").forEach((b) => { b.dataset.wasDisabled = b.disabled ? "1" : ""; b.disabled = true; });
    const all = $("inspAllEcus").checked;
    status("accent", all ? "asking every OBD module…" : "reading…");
    try {
      const args = { ...window.OT.connArgs(), sections: secs, allEcus: all, frame: 0 };
      const res = await invoke("obd_inspect", args);
      if (all || !secs.length) { ecus = {}; order = []; current = null; }
      merge(res, secs);
      render();
      const n = (res.ecus || []).length;
      status(n ? "ok" : "bad", n ? `read ${n} module${n > 1 ? "s" : ""}` : "no answer");
    } catch (e) {
      const m = typeof e === "string" ? e : (e && e.message) || "error";
      status(/busy/i.test(m) ? "accent" : "bad", m);
    } finally {
      busy = false;
      panel.querySelectorAll("button.btn").forEach((b) => { b.disabled = b.dataset.wasDisabled === "1"; });
    }
  }

  // ---- wiring --------------------------------------------------------------
  $("inspAll").addEventListener("click", () => read([]));
  panel.querySelectorAll("button[data-sec]").forEach((b) => b.addEventListener("click", () => read([b.dataset.sec])));
  $("inspAllEcus").addEventListener("change", (e) => store.set("allEcus", e.target.checked));
  if (window.OT && window.OT.gate) {
    window.OT.gate(panel, ["diag"], "the inspection readout", { disable: ["#inspAll", "button[data-sec]"] });
  }
  render();
})();
