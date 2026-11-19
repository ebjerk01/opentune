// OpenTune — "Check my car": the guided wizard for a non-expert.
// Six steps over engines the app already has — Connect (the sidebar adapter
// box + capability probe, battery voltage), Identify (VIN and engine-computer
// identity: OBD mode 09, UDS 22 F190 fallback), Scan (OBD readiness, MIL and
// codes from every OBD address, plus the full module list through Auto-scan),
// Explain (plain language from the DTC database; never a cause), Live check (a
// short mode-01 snapshot rated with the Dashboard's own warn / crit marks) and
// Report (HTML / Markdown to save, or print).
// READ-ONLY: it never clears codes or writes anything; the Explain step only
// links to the Auto-scan panel, which keeps its own confirmation for clearing.
// Every step can be skipped or retried; errors are plain language with the raw
// detail one click away. Logic lives in wizard-core.js (node-tested).
// Backend (existing commands only): probe_adapter (via OT.probe), obd_inspect,
// obd_supported, vin_decode, autoscan_run, dtc_lookup, obd_pid_table, obd_read.
// Bench tier until vehicle-tested.
"use strict";
(function () {
  const C = window.OTWizardCore;
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const LEVEL_PILL = { ok: "ok", warn: "accent", crit: "bad", bad: "bad", info: "neutral", unknown: "neutral", "": "neutral" };
  const LEVEL_WORD = { ok: "OK", warn: "warning", crit: "critical", bad: "attention", info: "info", unknown: "unknown", "": "—" };
  const STATUS_ICON = { pending: "○", running: "…", done: "✓", failed: "!", skipped: "–" };

  // What to do in the car before each step, and the Run button's label.
  const INTRO = {
    connect: { lead: "Plug the adapter into the car's OBD socket (under the dashboard, driver's side on most cars) and switch the ignition on. The engine can stay off.",
      run: "Check the adapter" },
    identify: { lead: "Reads the vehicle identification number (VIN) and the engine computer's name and calibration IDs. Nothing is changed.", run: "Read identity" },
    scan: { lead: "Reads the check-engine lamp, the readiness monitors and the trouble codes from every OBD module. With the vehicle's module list chosen below it also asks every module in the list (Auto-scan). Read-only: no code is cleared.",
      run: "Scan the car" },
    explain: { lead: "What the scan found, in plain words. Descriptions come from OpenTune's trouble-code table; the wizard does not guess causes.", run: "Explain again" },
    live: { lead: "Start the engine and let it idle with the car parked. The wizard takes a short snapshot (about three seconds) of coolant temperature, engine speed, fuel trims and voltage.",
      run: "Take the snapshot" },
    report: { lead: "A summary of every step. Save it as HTML or Markdown, or print it.", run: "Rebuild the report" },
  };

  // ---- styles (panel-scoped) -----------------------------------------------
  const css = document.createElement("style");
  css.textContent = `
    #panel-wizard .wiz-steps { display: flex; flex-wrap: wrap; gap: 6px; }
    #panel-wizard .wiz-step { display: flex; align-items: center; gap: 6px; border: 1px solid var(--card-border); background: var(--card);
      color: var(--ink); border-radius: 20px; padding: 5px 12px; font: inherit; font-size: 13px; cursor: pointer; }
    #panel-wizard .wiz-step[disabled] { opacity: .5; cursor: default; }
    #panel-wizard .wiz-step .n { font: 700 12px var(--mono); color: var(--ink-soft); }
    #panel-wizard .wiz-step .i { font-weight: 700; }
    #panel-wizard .wiz-step.current { border-color: var(--accent); box-shadow: 0 0 0 2px var(--accent-soft); }
    #panel-wizard .wiz-step.done .i { color: var(--good); }
    #panel-wizard .wiz-step.failed .i { color: var(--bad); }
    #panel-wizard .wiz-step.skipped .i, #panel-wizard .wiz-step.pending .i { color: var(--muted); }
    #panel-wizard .wiz-lead { margin: 4px 0 10px; }
    #panel-wizard .wiz-nav { justify-content: space-between; margin-top: 12px; }
    #panel-wizard .wiz-items { display: flex; flex-direction: column; gap: 8px; }
    #panel-wizard .wiz-item { border: 1px solid var(--card-border); border-radius: 10px; padding: 9px 12px; background: var(--card); }
    #panel-wizard .wiz-item .t { font-weight: 600; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
    #panel-wizard .wiz-item ul { margin: 6px 0 0 18px; padding: 0; }
    #panel-wizard .wiz-kv { display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; font-size: 13px; }
    #panel-wizard .wiz-kv .k { color: var(--ink-soft); }
    #panel-wizard .wiz-report { border: 1px solid var(--card-border); border-radius: 10px; padding: 12px 16px; background: var(--card); max-height: 60vh; overflow: auto; }
    #panel-wizard .wiz-report h1 { font-size: 18px; margin: 0 0 4px; }
    #panel-wizard .wiz-report h2 { font-size: 14px; margin: 16px 0 6px; }
    #panel-wizard .wiz-report .st { font-size: 11px; margin-left: 6px; color: var(--ink-soft); }
    #panel-wizard .wiz-report dl { display: grid; grid-template-columns: max-content 1fr; gap: 3px 12px; margin: 4px 0; font-size: 13px; }
    #panel-wizard .wiz-report dt { color: var(--ink-soft); }
    #panel-wizard .wiz-report dd { margin: 0; }
    #panel-wizard .wiz-report table { border-collapse: collapse; width: 100%; font-size: 12.5px; }
    #panel-wizard .wiz-report th, #panel-wizard .wiz-report td { border: 1px solid var(--line); padding: 3px 7px; text-align: left; vertical-align: top; }
    #panel-wizard .wiz-report .note { border-left: 3px solid var(--accent); padding: 6px 10px; margin: 8px 0; }
    @media (max-width: 720px) { #panel-wizard .wiz-kv { grid-template-columns: 1fr; } }
    @media print {
      body.wiz-printing * { visibility: hidden !important; }
      body.wiz-printing #wizReport, body.wiz-printing #wizReport * { visibility: visible !important; }
      body.wiz-printing #wizReport { position: absolute; left: 0; top: 0; width: 100%; max-height: none; overflow: visible; border: 0; }
    }`;
  document.head.appendChild(css);

  // ---- build ---------------------------------------------------------------
  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-wizard";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Check my car</h1>
      <p>A guided, read-only check: connect, identify the car, scan for trouble codes, read what they mean, take a short live snapshot and save a report. Nothing is written to the car and no codes are cleared. Any step can be skipped or retried.</p>
    </header>
    <div class="card"><div class="wiz-steps" id="wizSteps"></div></div>
    <div class="card">
      <h2 id="wizTitle"></h2>
      <p class="wiz-lead" id="wizLead"></p>
      <div id="wizControls"></div>
      <div id="wizPackBox" hidden></div>
      <div id="wizResult" class="result"></div>
      <div class="row wiz-nav">
        <button class="btn" id="wizBack" type="button">← Back</button>
        <span class="row tight">
          <button class="btn" id="wizSkip" type="button">Skip this step</button>
          <button class="btn primary" id="wizRun" type="button">Run</button>
          <button class="btn" id="wizNext" type="button">Next →</button>
        </span>
      </div>
    </div>`;
  const main = document.getElementById("main");
  if (main) {
    const dbg = document.getElementById("panel-debug");
    if (dbg && dbg.parentElement === main) main.insertBefore(panel, dbg);
    else main.appendChild(panel);
  }
  const $ = (id) => panel.querySelector("#" + id);
  window.OTNav.register({ id: "wizard", label: "Check my car", icon: "✔", group: "vehicle", tier: "bench" });

  let run = C.newRun();
  const st = { modulesPath: null, cancel: false };
  const connArgs = () => (window.OT && window.OT.connArgs ? window.OT.connArgs() : { driver: "elm327obd", port: null, baud: null, bus: "hs", txId: null, rxId: null });
  const isSim = () => !!C.SIM_DRIVERS[connArgs().driver];
  const stepId = () => C.STEPS[run.current].id;

  // The vehicle's module list for the full scan (optional; the shared picker).
  const packPick = window.OTPacks.picker({
    panel: "wizard",
    kind: "modules",
    filter: { name: "Modules", extensions: ["json"] },
    onChange: (sel) => { st.modulesPath = sel.path; },
  });
  $("wizPackBox").innerHTML = `<div class="small muted">Full module scan (optional): the vehicle's module list.</div>`;
  $("wizPackBox").appendChild(packPick.el);

  // ---- rendering ------------------------------------------------------------
  const pill = (level, text) => `<span class="pill ${LEVEL_PILL[level] || "neutral"}">${esc(text)}</span>`;
  const kv = (rows) => `<div class="wiz-kv">${rows.map(([k, v]) => `<span class="k">${esc(k)}</span><span>${v}</span>`).join("")}</div>`;
  function errorHtml(err) {
    if (!err) return "";
    return `<div class="banner bad">${esc(err.text)}</div>` +
      (err.raw ? `<details class="small"><summary>Technical detail</summary><pre class="mono">${esc(err.raw)}</pre></details>` : "");
  }

  function renderSteps() {
    $("wizSteps").innerHTML = C.STEPS.map((s, i) => {
      const status = run.steps[s.id].status;
      const cls = `wiz-step ${status}${i === run.current ? " current" : ""}`;
      return `<button class="${cls}" type="button" data-i="${i}"${C.reachable(run, i) && !C.busy(run) ? "" : " disabled"} title="${esc(s.title)}: ${esc(status)}">` +
        `<span class="n">${i + 1}</span> ${esc(s.title)} <span class="i">${STATUS_ICON[status] || ""}</span></button>`;
    }).join("");
    $("wizSteps").querySelectorAll(".wiz-step").forEach((b) => b.addEventListener("click", () => {
      C.goTo(run, Number(b.dataset.i));
      render();
      autoRun();
    }));
  }

  function controlsHtml(id) {
    if (id === "connect") {
      const c = connArgs();
      const ad = window.OT && window.OT.adapter;
      const now = isSim() ? C.SIM_DRIVERS[c.driver] : `${c.driver}${c.port ? " on " + c.port : ""}${c.bus ? " · " + c.bus.toUpperCase() : ""}`;
      return `<div class="small">Adapter: <b>${esc(now)}</b>${ad && ad.connected && !isSim() ? ` · last seen: ${esc(ad.name || ad.driver)}` : ""}</div>` +
        `<div class="small muted">Choose a different adapter in the sidebar <b>Adapter</b> box (driver, port, HS/MS), or try the wizard without a car:</div>` +
        `<div class="row tight"><button class="btn sm" type="button" id="wizUseVcar">Use the virtual car</button>` +
        `<button class="btn sm" type="button" id="wizUseSim">Use the OBD simulator</button></div>`;
    }
    if (id === "explain" && run.steps.scan.data && run.steps.scan.data.summary && run.steps.scan.data.summary.dtcs.length) {
      return `<div class="banner info">Clearing codes is not part of this check. After a repair, codes can be cleared in the <b>Auto-scan</b> panel, which asks for confirmation first. <button class="btn sm" type="button" id="wizOpenScan">Open Auto-scan…</button></div>`;
    }
    if (id === "report" && run.steps.report.status === "done") {
      return `<div class="row tight"><button class="btn" type="button" id="wizSaveHtml">Save HTML…</button>` +
        `<button class="btn" type="button" id="wizSaveMd">Save Markdown…</button>` +
        `<button class="btn" type="button" id="wizPrint">Print…</button>` +
        `<button class="btn" type="button" id="wizRestart">Start a new check</button></div>`;
    }
    return "";
  }

  function resultHtml(id) {
    const s = run.steps[id];
    if (s.status === "running") return `<div class="muted small" id="wizProgress">Working…</div>`;
    if (s.status === "failed") return errorHtml(s.error) + `<div class="muted small">Press <b>Retry</b> after checking the above, or <b>Skip this step</b>.</div>`;
    if (s.status === "skipped") return `<div class="muted small">Skipped. Press <b>Run</b> to do it after all.</div>`;
    if (s.status !== "done") return "";
    const d = s.data;
    if (id === "connect") {
      return kv([
        ["Adapter", esc(`${d.name}${d.firmware ? " " + d.firmware : ""}`) + (d.sim ? " " + pill("info", "offline") : " " + pill("ok", "connected"))],
        ["Battery", pill(d.voltage.level, LEVEL_WORD[d.voltage.level]) + " " + esc(d.voltage.text)],
      ]);
    }
    if (id === "identify") {
      const rows = [["VIN", d.vin ? `<span class="mono">${esc(d.vin)}</span> <span class="muted small">(${esc(d.vinSource)})</span>` : `<span class="muted">not reported by the car</span>`]];
      if (d.decoded && d.decoded.manufacturer) rows.push(["Manufacturer", esc(d.decoded.manufacturer) + ' <span class="muted small">(from the VIN)</span>']);
      if (d.decoded && d.decoded.modelYear) rows.push(["Model year", esc(d.decoded.modelYear) + ' <span class="muted small">(from the VIN)</span>']);
      if (d.ecuName) rows.push(["Engine computer", esc(d.ecuName)]);
      if (d.calids.length) rows.push(["Calibration IDs", `<span class="mono">${esc(d.calids.join(", "))}</span>`]);
      if (d.cvns.length) rows.push(["Verification numbers", `<span class="mono">${esc(d.cvns.join(", "))}</span>`]);
      if (d.target) rows.push(["Asked", `<span class="mono">${esc(d.target)}</span>`]);
      const notes = Object.keys(d.errors || {}).map((k) => `${k}: ${d.errors[k]}`);
      return kv(rows) + (notes.length ? `<details class="small"><summary>Not read</summary><pre class="mono">${esc(notes.join("\n"))}</pre></details>` : "");
    }
    if (id === "scan") {
      const sm = d.summary;
      const rows = [
        ["Check-engine lamp", sm.mil == null ? pill("info", "not read") : sm.mil ? pill("bad", "ON") : pill("ok", "off")],
        ["Trouble codes", sm.dtcs.length ? pill("bad", String(sm.dtcs.length)) + " " + esc(sm.dtcs.map((x) => x.code).join(", ")) : pill("ok", "none")],
        ["OBD modules", esc(sm.obdModules.join(", ") || "none answered")],
      ];
      if (sm.readiness) rows.push(["Monitors not finished", sm.readiness.incomplete.length ? pill("warn", String(sm.readiness.incomplete.length)) + " " + esc(sm.readiness.incomplete.map((m) => m.name).join(", ")) : pill("ok", "none")]);
      if (sm.modules) rows.push(["Module list", esc(`${sm.modules.vehicle || ""}: ${sm.modules.present.length} of ${sm.modules.total} answered`)]);
      else if (d.modulesNote) rows.push(["Module list", `<span class="muted">${esc(d.modulesNote)}</span>`]);
      return kv(rows) + (d.problems || []).map((p) => `<div class="small"><b>${esc(p.what)}:</b></div>` + errorHtml(p)).join("") +
        `<div class="muted small">Next: <b>Explain</b> says what this means.</div>`;
    }
    if (id === "explain") {
      return `<div class="wiz-items">` + (d.items || []).map((it) =>
        `<div class="wiz-item"><div class="t">${pill(it.level, LEVEL_WORD[it.level])} ${esc(it.title)}</div>` +
        `<div class="small">${esc(it.text)}</div>` +
        (it.system ? `<div class="small muted">System: ${esc(it.system)}</div>` : "") +
        (it.seen && it.seen.length ? `<ul class="small">${it.seen.map((x) => `<li>${esc(x)}</li>`).join("")}</ul>` : "") +
        (it.list && it.list.length ? `<ul class="small">${it.list.map((x) => `<li>${esc(x)}</li>`).join("")}</ul>` : "") +
        (it.extra ? `<div class="small muted">${esc(it.extra)}</div>` : "") + `</div>`).join("") + `</div>`;
    }
    if (id === "live") {
      const sm = d.summary;
      const fmt = (v, unit) => (v == null ? "—" : (unit === "V" ? v.toFixed(2) : unit === "%" ? v.toFixed(1) : v.toFixed(0)) + " " + unit);
      return `<table class="tbl"><thead><tr><th>Value</th><th>Last</th><th>Min – max</th><th>Rating</th><th>Note</th></tr></thead><tbody>` +
        sm.rows.map((r) => `<tr><td>${esc(r.name)}</td><td class="mono">${esc(fmt(r.last, r.unit))}</td>` +
          `<td class="mono">${r.last == null ? "—" : esc(fmt(r.min, r.unit) + " – " + fmt(r.max, r.unit))}</td>` +
          `<td>${r.level ? pill(r.level, LEVEL_WORD[r.level]) : "—"}</td><td class="small">${esc(r.note || "")}</td></tr>`).join("") +
        `</tbody></table>` + kv([
        ["Battery", pill(sm.battery.level, LEVEL_WORD[sm.battery.level]) + " " + esc(sm.battery.text)],
        ["Idle", esc(sm.idle.text)],
        ["Readings", esc(String(sm.samples))],
      ]);
    }
    if (id === "report") return `<div class="wiz-report" id="wizReport">${d.html}</div>`;
    return "";
  }

  function render() {
    const id = stepId();
    const meta = C.STEPS[run.current];
    const s = run.steps[id];
    const intro = INTRO[id];
    const busy = C.busy(run);
    renderSteps();
    $("wizTitle").textContent = `Step ${run.current + 1} of ${C.STEPS.length}: ${meta.title}`;
    $("wizLead").textContent = intro.lead;
    $("wizControls").innerHTML = controlsHtml(id);
    $("wizPackBox").hidden = id !== "scan";
    $("wizResult").innerHTML = resultHtml(id);
    $("wizRun").textContent = s.status === "failed" ? "Retry" : s.status === "done" && meta.io ? "Run again" : intro.run;
    $("wizRun").disabled = busy;
    $("wizBack").disabled = busy || run.current === 0;
    $("wizSkip").disabled = busy || s.status === "done" || s.status === "skipped" || id === "report";
    $("wizNext").disabled = !C.canNext(run);
    wireControls(id);
  }

  function wireControls(id) {
    const on = (bid, fn) => { const b = $(bid); if (b) b.addEventListener("click", fn); };
    on("wizUseVcar", () => useConn({ driver: "vcar", port: "demo", bus: "hs" }));
    on("wizUseSim", () => useConn({ driver: "obdsim", port: "", bus: "hs" }));
    on("wizOpenScan", () => window.showPanel && window.showPanel("scan"));
    on("wizSaveHtml", () => save("html"));
    on("wizSaveMd", () => save("md"));
    on("wizPrint", printReport);
    on("wizRestart", () => { run = C.newRun(); render(); });
    if (id === "scan") packPick.refresh && packPick.refresh();
  }

  // Point the shared connection (and the sidebar Adapter box, when it lists
  // the driver) at an offline vehicle.
  function useConn(patch) {
    if (!window.OT || !window.OT.setConn) return;
    window.OT.setConn(Object.assign({ txId: "", rxId: "" }, patch));
    const sel = document.getElementById("connDriver");
    if (sel && sel.querySelector && sel.querySelector(`option[value="${patch.driver}"]`)) {
      sel.value = patch.driver;
      const port = document.getElementById("connPort");
      if (port) port.value = patch.port || "";
    }
    if (run.steps.connect.status !== "running") run.steps.connect.status = "pending";
    // The virtual car's own module list, unless one is already chosen.
    if (patch.driver === "vcar" && !st.modulesPath && window.OTPacks.library) {
      window.OTPacks.library().then((lib) => {
        const p = ((lib && lib.packs) || []).find((x) => x.id === "vcar-demo");
        if (p && !st.modulesPath) packPick.choosePack(p.dir);
      }).catch(() => {});
    }
    render();
  }

  // ---- step runners (existing commands only; all read-only) -------------------
  async function doConnect() {
    if (isSim()) return C.connectResult(null, connArgs());
    const probe = window.OT && window.OT.probe ? await window.OT.probe() : await invoke("probe_adapter", connArgs());
    return C.connectResult(probe, connArgs());
  }

  async function doIdentify() {
    const insp = await invoke("obd_inspect", { ...connArgs(), sections: ["info"], allEcus: false, frame: null });
    const e = (insp && insp.ecus && insp.ecus[0]) || null;
    let sup = null;
    if (!(e && e.info && e.info.vin)) {
      try { sup = await invoke("obd_supported", connArgs()); } catch (_) { sup = null; }
    }
    const vin = (e && e.info && e.info.vin) || (sup && sup.vin) || null;
    let decoded = null;
    if (vin) { try { decoded = await invoke("vin_decode", { vin }); } catch (_) { decoded = null; } }
    const id = C.identity(insp, sup, decoded);
    if (!id.responded && !id.vin) throw (e && e.errors && (e.errors.module || Object.values(e.errors)[0])) || "no response: the engine computer did not answer";
    return id;
  }

  async function doScan() {
    const out = { obd: null, modules: null, problems: [], modulesNote: null, summary: null };
    try {
      out.obd = await invoke("obd_inspect", { ...connArgs(), sections: ["readiness", "dtcs"], allEcus: true, frame: null });
    } catch (e) {
      out.problems.push(Object.assign({ what: "OBD scan" }, C.errorInfo(e)));
    }
    if (isSim()) out.modulesNote = "The OBD simulator only answers OBD requests, so the full module scan does not apply.";
    else if (!st.modulesPath) out.modulesNote = "No module list was chosen, so only the OBD modules were asked.";
    else {
      try {
        out.modules = await invoke("autoscan_run", { ...connArgs(), modulesPath: st.modulesPath, timeoutMs: 2000 });
      } catch (e) {
        out.problems.push(Object.assign({ what: "Module scan" }, C.errorInfo(e)));
      }
    }
    out.summary = C.scanSummary(out);
    const answered = out.summary.obdModules.length + (out.modules ? out.modules.presentCount || 0 : 0);
    if (!answered) throw out.problems.length ? out.problems[0].raw : "no response: no module answered (OBD addresses 7E0–7E7 silent)";
    return out;
  }

  async function doExplain() {
    const sc = run.steps.scan.status === "done" ? run.steps.scan.data : null;
    const lookups = {};
    if (sc) {
      for (const d of sc.summary.dtcs) {
        try { lookups[d.code] = await invoke("dtc_lookup", { code: d.code }); } catch (_) { /* keep the scan's own description */ }
      }
    }
    return { items: C.explain(sc ? sc.summary : null, lookups, { modulesNote: sc && sc.modulesNote }), lookups };
  }

  async function doLive() {
    const table = await invoke("obd_pid_table");
    const sup = await invoke("obd_supported", connArgs());
    const pids = C.livePids(table, sup && sup.pids);
    if (!pids.length) throw "the engine computer reports none of the values this check reads (coolant, engine speed, fuel trims, voltage)";
    const samples = [];
    let last = null;
    st.cancel = false;
    for (let i = 0; i < C.LIVE_SAMPLES && !st.cancel; i++) {
      try {
        samples.push(await invoke("obd_read", { ...connArgs(), pids }));
      } catch (e) {
        last = e;
      }
      const p = $("wizProgress");
      if (p) p.textContent = `Reading ${i + 1} of ${C.LIVE_SAMPLES}…`;
      if (i < C.LIVE_SAMPLES - 1 && !st.cancel) await sleep(C.LIVE_GAP_MS);
    }
    if (!samples.length) throw last || "no reading arrived";
    return { pids, summary: C.liveSummary(samples, table), stopped: st.cancel };
  }

  function doReport() {
    const meta = { app: "OpenTune" };
    return { html: C.reportHtml(run, meta, false) };
  }

  const RUNNERS = { connect: doConnect, identify: doIdentify, scan: doScan, explain: doExplain, live: doLive, report: doReport };

  async function runStep(id) {
    if (C.busy(run)) return;
    const mine = run;
    C.begin(run, id);
    render();
    try {
      const data = await RUNNERS[id]();
      if (mine !== run) return; // a new check was started meanwhile
      C.succeed(run, id, data);
    } catch (e) {
      if (mine !== run) return;
      C.failStep(run, id, e);
    }
    render();
  }

  // Explain and the report need no car: they fill in on arrival.
  function autoRun() {
    const id = stepId();
    const s = run.steps[id];
    if (!C.STEPS[run.current].io && (s.status === "pending" || id === "report") && !C.busy(run)) return runStep(id);
    return null;
  }

  // ---- save / print -----------------------------------------------------------
  function save(kind) {
    const text = kind === "md" ? C.reportMarkdown(run, { app: "OpenTune" }) : C.reportHtml(run, { app: "OpenTune" }, true);
    const blob = new Blob([text], { type: kind === "md" ? "text/markdown" : "text/html" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = C.reportFileName(run, kind);
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }
  function printReport() {
    document.body.classList.add("wiz-printing");
    try { window.print(); } finally { setTimeout(() => document.body.classList.remove("wiz-printing"), 500); }
  }

  // ---- wiring -------------------------------------------------------------------
  $("wizRun").addEventListener("click", () => runStep(stepId()));
  $("wizSkip").addEventListener("click", () => {
    C.skip(run, stepId());
    C.next(run);
    render();
    autoRun();
  });
  $("wizNext").addEventListener("click", () => {
    C.next(run);
    render();
    autoRun();
  });
  $("wizBack").addEventListener("click", () => {
    C.back(run);
    render();
  });
  // Leaving the panel ends a live snapshot early (what was read is kept).
  if (typeof MutationObserver === "function") {
    try {
      new MutationObserver(() => { if (!panel.classList.contains("active")) st.cancel = true; })
        .observe(panel, { attributes: true, attributeFilter: ["class"] });
    } catch (e) {}
  }
  if (window.OT && typeof window.OT.onCaps === "function") {
    window.OT.onCaps(() => { if (stepId() === "connect" && !C.busy(run)) render(); });
  }
  render();

  window.OTWizard = { get run() { return run; }, state: st, render, runStep, save, reportText: (kind) => (kind === "md" ? C.reportMarkdown(run, { app: "OpenTune" }) : C.reportHtml(run, { app: "OpenTune" }, true)) };
})();
