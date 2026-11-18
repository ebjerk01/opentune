// OpenTune — Service functions panel (self-contained).
//
// Declarative service procedures (DPF regen, service / oil reset, SAS cal, EPB
// service mode, …) from a ServiceScript JSON, run over the live adapter.
//   • Plan  — always safe: validates + lists the planned steps, sends nothing.
//   • Run   — gated behind a confirm checkbox: executes the steps in order and
//             stops on the first failure.
//
// Injects its own nav item + panel so no other file needs editing. Uses the
// shared OT connection/capability framework (window.OT) and window.showPanel.
"use strict";
(function () {
  const TAURI = window.__TAURI__ || {};
  const invoke = (c, a) => TAURI.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "—");
  const toast = (m, bad) => { try { if (window.toast) window.toast(m, bad); } catch (_) {} };

  let scriptPath = null;

  // ---- rendering -----------------------------------------------------------
  const PILL = { ok: "ok", completed: "ok", failed: "bad", planned: "neutral", skipped: "neutral" };
  const errHtml = (e) =>
    `<p class="pill bad">error</p><p class="mono small">${esc(typeof e === "string" ? e : (e && e.message) || e)}</p>`;

  function outcomeLine(rep, isRun) {
    const o = rep.outcome || {};
    const kind = o.kind || "planned";
    const pill = `<span class="pill ${PILL[kind] || "neutral"}">${esc(kind)}${o.kind === "failed" && o.at != null ? " @ step " + o.at : ""}</span>`;
    const sec = rep.security_unlocked ? ` <span class="pill ok">security unlocked</span>` : "";
    const title = isRun ? "Run" : "Plan";
    const name = rep.name || rep.script_id || "";
    return `<p>${esc(title)}: <b>${esc(name)}</b> <span class="mono small muted">${esc(rep.script_id || "")}</span> — ${pill}${sec}</p>`;
  }

  function stepsTable(steps) {
    if (!steps || !steps.length) return `<p class="muted small">No steps.</p>`;
    const rows = steps
      .map((s) => {
        const io = [];
        if (s.request) io.push(`→ ${esc(s.request)}`);
        if (s.response) io.push(`← ${esc(s.response)}`);
        const detail = `<span class="small">${esc(s.detail || "")}</span>` + (io.length ? `<br><span class="mono small muted">${io.join("  ")}</span>` : "");
        return `<tr>
          <td class="mono" style="padding:3px 8px;text-align:right">${esc(s.index)}</td>
          <td class="mono" style="padding:3px 8px">${esc(s.op)}</td>
          <td style="padding:3px 8px"><span class="pill ${PILL[s.status] || "neutral"}">${esc(s.status)}</span></td>
          <td style="padding:3px 8px">${detail}</td>
        </tr>`;
      })
      .join("");
    return `<table class="mono" style="width:100%;border-collapse:collapse;font-size:12px">
      <thead><tr>
        <th style="padding:3px 8px;text-align:right">#</th>
        <th style="padding:3px 8px;text-align:left">Op</th>
        <th style="padding:3px 8px;text-align:left">Status</th>
        <th style="padding:3px 8px;text-align:left">Detail</th>
      </tr></thead><tbody>${rows}</tbody></table>`;
  }

  function renderReport(rep, isRun) {
    let html = outcomeLine(rep, isRun);
    if (rep.description) html += `<p class="muted small">${esc(rep.description)}</p>`;
    if (rep.note) html += `<p class="muted small">${esc(rep.note)}</p>`;
    else if (rep.needs_key_provider && !rep.seedkey_available) html += `<p class="muted small">This script needs UDS SecurityAccess — it can be planned but not executed here (no OEM seed/key provider shipped).</p>`;
    html += stepsTable(rep.steps);
    return html;
  }

  // ---- panel ---------------------------------------------------------------
  const PANEL_HTML = `
    <header class="panel-head">
      <h1>Service functions</h1>
      <p>Run declarative service procedures — DPF forced regeneration, service / oil-interval reset, steering-angle (SAS) calibration, EPB service mode — from a ServiceScript JSON over the connected adapter. Plan previews every UDS step and sends nothing; Run executes them in order and stops on the first failure.</p>
    </header>
    <div class="card">
      <h2>Script</h2>
      <div id="svcPackPick"></div>
      <div class="row">
        <label>Per-request timeout</label>
        <input class="inp" id="svcTimeout" value="2000" style="max-width:110px" />
        <span class="muted small">ms</span>
      </div>
      <div class="row">
        <button class="btn primary" id="svcPlan">Plan</button>
        <label class="small"><input type="checkbox" id="svcAck" /> I understand Run sends real writes / actions to the module</label>
        <button class="btn warn" id="svcRun" disabled>Run</button>
      </div>
      <p class="muted small">Plan is always safe: it validates the script and lists the planned steps without opening the adapter or sending anything. Run is gated behind the confirm box above.</p>
      <p class="muted small">Scripts that need UDS SecurityAccess can be <b>planned</b> but not <b>executed</b> here — OpenTune ships no OEM seed/key provider. Make sure the adapter's HS/MS-CAN switch matches the target module's bus.</p>
      <div id="svcResult" class="result"></div>
    </div>`;

  function build() {
    const nav = document.getElementById("nav");
    if (nav) window.OTNav.register({ id: "service", label: "Service fn", icon: "🛠", group: "vehicle", tier: "bench" });
    const main = document.getElementById("main");
    if (main && !document.getElementById("panel-service")) {
      const sec = document.createElement("section");
      sec.className = "panel";
      sec.id = "panel-service";
      sec.innerHTML = PANEL_HTML;
      main.appendChild(sec);
      wire(sec);
    }
  }

  function wire(sec) {
    const q = (id) => sec.querySelector(id);
    const svcResult = q("#svcResult");
    const ackCb = q("#svcAck");
    const planBtn = q("#svcPlan");
    const runBtn = q("#svcRun");

    const capsOk = () => (window.OT && window.OT.has ? window.OT.has(["diag"]) : true);
    const syncRun = () => { runBtn.disabled = !(capsOk() && ackCb.checked); };
    // Plan sends nothing and never opens the adapter: it is never caps-gated.
    const syncPlan = () => { planBtn.disabled = false; };

    // Capability gate: banner + disable Run when the adapter can't do CAN.
    if (window.OT && window.OT.gate) {
      window.OT.gate(sec, ["diag"], "running service functions", { disable: ["#svcRun"] });
    }
    // Combine the gate with the ack checkbox for Run; re-run on every caps change.
    if (window.OT && window.OT.onCaps) window.OT.onCaps(() => { syncPlan(); syncRun(); });
    ackCb.addEventListener("change", syncRun);
    syncRun();

    // The ServiceScript comes from a definition pack (or a loose file).
    const packPick = window.OTPacks.picker({
      panel: "service",
      kind: "services",
      filter: { name: "Service script", extensions: ["json"] },
      onChange: (sel) => { scriptPath = sel.path; svcResult.innerHTML = ""; },
    });
    q("#svcPackPick").appendChild(packPick.el);

    async function execute(cmd, isRun) {
      if (!scriptPath) { svcResult.innerHTML = errHtml("Choose a service script (a pack and its script, or a file) first."); return; }
      const timeoutMs = parseInt(q("#svcTimeout").value, 10) || 2000;
      const conn = window.OT && window.OT.connArgs ? window.OT.connArgs() : {};
      const args = Object.assign({}, conn, { scriptPath, timeoutMs });
      if (isRun) args.ack = true;
      planBtn.disabled = true; runBtn.disabled = true;
      svcResult.innerHTML = `<p class="muted small">${isRun ? "Running" : "Planning"} ${esc(baseName(scriptPath))}…</p>`;
      try {
        const rep = await invoke(cmd, args);
        svcResult.innerHTML = renderReport(rep, isRun);
        toast(isRun ? "Service run finished" : "Plan ready");
      } catch (e) {
        svcResult.innerHTML = errHtml(e);
        toast(isRun ? "Service run failed" : "Plan failed", true);
      } finally {
        syncPlan(); syncRun();
      }
    }

    planBtn.addEventListener("click", () => execute("service_plan", false));
    runBtn.addEventListener("click", () => execute("service_run", true));
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", build);
  else build();
})();
