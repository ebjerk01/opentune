// OpenTune — Actuator tests panel.
// Timed output tests over UDS InputOutputControlByIdentifier (0x2F) on the
// actuator channels an adaptation definition DECLARES: pick the module's
// definition, pick a channel (its options, value range and duration cap come
// from the definition), plan the exact requests, then run a timed test —
// returnControlToECU / resetToDefault / freezeCurrentState /
// shortTermAdjustment with a range-checked value. Control is handed back to
// the ECU automatically at the end, on STOP, on any error, when the panel is
// left, when the adapter disconnects and when the window closes.
// Bench tier: live tests run on alpha / bench builds only; Plan sends nothing
// and works everywhere. A free-form IO DID is behind the Debug gate (alpha /
// bench) with a second confirmation.
// Backend: actuator_list, actuator_plan, actuator_start, actuator_status,
// actuator_stop.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const bad = (e) => `<div class="banner bad">${esc(typeof e === "string" ? e : (e && e.message) || e)}</div>`;
  const POLL_MS = 250;

  const OPT = {
    return_control_to_ecu: "returnControlToECU — hand the output back to the ECU",
    reset_to_default: "resetToDefault — drive the output to its default",
    freeze_current_state: "freezeCurrentState — hold the output where it is",
    short_term_adjustment: "shortTermAdjustment — drive the output to a value",
  };
  const takesControl = (opt) => opt !== "return_control_to_ecu";
  const fmtNum = (n) => (n == null ? "—" : Number.isInteger(n) ? String(n) : String(Math.round(n * 1000) / 1000));

  // ---- pure helpers (unit-tested) -------------------------------------------
  // Check the form against the channel's declaration before anything is sent
  // (the backend checks again). Returns { error } or { value, durationMs }.
  function validate(ch, opt, valueText, durationS, freeform) {
    if (!ch && !freeform) return { error: "Choose an actuator channel." };
    const options = freeform ? Object.keys(OPT) : ch.options || [];
    if (!options.includes(opt)) return { error: `${opt} is not declared for this channel.` };
    const out = { value: null, durationMs: 0 };
    if (takesControl(opt)) {
      const s = Number(String(durationS).trim());
      const max = freeform ? freeform.max : ch.max_duration_ms;
      if (!Number.isFinite(s) || s <= 0) return { error: "Set a test duration in seconds." };
      out.durationMs = Math.round(s * 1000);
      if (out.durationMs > max) return { error: `Duration ${s} s is above this channel's limit of ${max / 1000} s.` };
    }
    if (opt === "short_term_adjustment" && !freeform && ch.value) {
      const t = String(valueText == null ? "" : valueText).trim();
      const v = Number(t);
      if (!t || !Number.isFinite(v)) return { error: `Enter a value (${fmtNum(ch.value.min)}…${fmtNum(ch.value.max)} ${ch.value.unit}).` };
      if (v < ch.value.min || v > ch.value.max) return { error: `${v} ${ch.value.unit} is outside the declared range ${fmtNum(ch.value.min)}…${fmtNum(ch.value.max)} ${ch.value.unit}.` };
      out.value = v;
    }
    return out;
  }

  const outcomeText = {
    completed: "completed", stopped: "stopped early", aborted: "aborted — nothing was driven",
    apply_failed: "control request refused", hold_failed: "link failed during the test",
    control_return_failed: "CONTROL NOT RETURNED",
  };
  function reportHtml(r) {
    if (!r) return "";
    const ok = r.outcome === "completed" || r.outcome === "stopped";
    let h = `<div class="row tight"><span class="pill ${ok ? "ok" : r.safe ? "neutral" : "bad"}">${esc(outcomeText[r.outcome] || r.outcome)}</span>` +
      `<span class="pill neutral">applied ${r.applied ? "yes" : "no"}</span>` +
      `<span class="pill ${r.control_returned ? "ok" : r.safe ? "neutral" : "bad"}">control returned ${r.control_returned ? "yes" : "no"}</span>` +
      `<span class="pill neutral">held ${(r.held_ms / 1000).toFixed(1)} s</span>` +
      (r.status != null ? `<span class="pill neutral">status ${esc(fmtNum(r.status))}${r.status_unit ? " " + esc(r.status_unit) : ""}</span>` : "") +
      (r.security_unlocked ? `<span class="pill neutral">SecurityAccess unlocked</span>` : "") + `</div>`;
    if (r.message) h += `<div class="muted small">${esc(r.message)}</div>`;
    if (!r.safe) h += `<div class="banner bad">⚠ Control was NOT handed back to the ECU (${esc(r.return_attempts)} attempt(s)) — the output may still be forced. Switch the ignition off and on to recover the module, and check the component before driving.</div>`;
    else if (r.applied && !r.control_returned) h += `<div class="banner bad">The module did not acknowledge returnControlToECU, but its control request had been refused, so nothing should be forced. Check the component.</div>`;
    return h;
  }
  function logHtml(ex) {
    if (!ex || !ex.length) return `<div class="muted small">No exchanges yet.</div>`;
    return `<table class="tbl"><thead><tr><th>t</th><th>Phase</th><th class="mono">Request</th><th class="mono">Response</th></tr></thead><tbody>` +
      ex.map((e) => `<tr${e.error ? ' class="bad"' : ""}><td class="mono">${(e.t_ms / 1000).toFixed(2)} s</td><td>${esc(e.phase)}</td><td class="mono">${esc(e.request)}</td>` +
        `<td class="mono">${e.response ? esc(e.response) : ""}${e.error ? ` <span class="pill bad">${esc(e.error)}</span>` : ""}</td></tr>`).join("") +
      `</tbody></table>`;
  }
  function planHtml(p) {
    return `<div class="muted small">${esc(p.channel)} · DID 0x${Number(p.did).toString(16).toUpperCase().padStart(4, "0")} · ${esc(p.option)}` +
      (p.value != null ? ` · ${esc(fmtNum(p.value))} ${esc(p.unit)} (raw ${esc(p.raw)})` : "") +
      (p.duration_ms ? ` · ${(p.duration_ms / 1000).toFixed(1)} s` : "") + `</div>` +
      `<table class="tbl"><thead><tr><th>Phase</th><th class="mono">Request</th><th>What it does</th></tr></thead><tbody>` +
      (p.steps || []).map((s) => `<tr><td>${esc(s.phase)}</td><td class="mono">${esc(s.request)}</td><td>${esc(s.note)}</td></tr>`).join("") +
      `</tbody></table>`;
  }

  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-actuator";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Actuator tests</h1>
      <p>Drive one output of a module for a set time — a fan, a lamp, a solenoid — with UDS InputOutputControlByIdentifier (0x2F). Only channels an adaptation definition <b>declares</b> can be tested, inside their declared range and time limit. Control goes back to the ECU automatically at the end, on <b>STOP</b>, on any error and when you leave the panel.</p>
    </header>

    <div class="card">
      <div id="actPackPick"></div>
      <div class="row">
        <button class="btn primary" id="actList" type="button">List actuator channels</button>
        <label>Timeout ms</label><input class="inp sm" id="actTimeout" value="1000" />
      </div>
      <div id="actDefInfo" class="result"></div>
    </div>

    <div class="card">
      <h2>Channel</h2>
      <div class="row">
        <label>Channel</label><select class="inp wide" id="actChan"></select>
      </div>
      <div id="actChanInfo" class="muted small"></div>
    </div>

    <div class="card warn-card">
      <h2>Test</h2>
      <div class="banner bad">⚠ An actuator test <b>drives a real part</b>. Run it only on a <b>stationary vehicle</b> — parking brake on, gear in park or neutral — with hands, tools and loose clothing clear of the component. The dongle's <b>HS/MS-CAN switch must match the module's bus</b>.</div>
      <div class="row tight">
        <label>Option</label><select class="inp wide" id="actOpt"></select>
      </div>
      <div class="row tight" id="actValueRow" hidden>
        <label>Value</label><input class="inp sm mono" id="actValue" /><span class="muted small" id="actValueHint"></span>
      </div>
      <div class="row tight" id="actDurRow">
        <label>Duration s</label><input class="inp sm" id="actDur" value="3" /><span class="muted small" id="actDurHint"></span>
      </div>
      <div class="row tight" id="actSecRow" hidden>
        <label>Seed/key id</label><input class="inp mono" id="actSeedkey" placeholder="from a plugin, or sa2 — OpenTune ships none" />
      </div>
      <div class="row">
        <button class="btn" id="actPlan" type="button">Plan (send nothing)</button>
      </div>
      <div id="actPlanOut" class="result"></div>

      <div id="actLive" hidden>
        <div class="row tight">
          <label class="confirm"><input type="checkbox" id="actAck" /> The vehicle is stationary and I am clear of the component</label>
        </div>
        <div class="row">
          <button class="btn warn" id="actRun" type="button">Run test</button>
          <button class="btn act-stop" id="actStop" type="button" disabled>STOP</button>
        </div>
        <div class="progress-wrap" id="actProgressWrap" hidden>
          <div class="progress-bar"><div class="progress-fill" id="actProgress"></div></div>
          <div class="muted small" id="actProgressText"></div>
        </div>
      </div>
      <div class="banner info" id="actStableNote" hidden>Live actuator tests are a <b>Bench-tier</b> feature (alpha / bench builds) until they are vehicle-tested. Plan works here and sends nothing.</div>
      <div id="actResult" class="result"></div>
    </div>

    <div class="card" id="actFfCard" hidden>
      <h2>Debug: free-form IO DID</h2>
      <div class="banner bad">⚠ No definition backs a free-form DID: nothing limits the state you send or tells you what the output is. Use it only to bring up a new definition, on a bench or a stationary vehicle. Limited to 5 s; the Target ids must be set.</div>
      <div class="row tight">
        <label class="confirm"><input type="checkbox" id="actFfOn" /> Use this DID instead of a declared channel</label>
      </div>
      <div class="row tight">
        <label>IO DID (hex)</label><input class="inp sm mono" id="actFfDid" placeholder="e.g. 0500" />
        <label>State (hex)</label><input class="inp mono" id="actFfState" placeholder="for shortTermAdjustment" />
        <label class="confirm"><input type="checkbox" id="actFfSec" /> needs SecurityAccess</label>
      </div>
      <div class="row tight">
        <label class="confirm"><input type="checkbox" id="actFfAck" /> I understand nothing checks this DID or state (second confirmation)</label>
      </div>
    </div>

    <div class="card">
      <h2>Exchanges</h2>
      <div id="actLog"><div class="muted small">No test run yet.</div></div>
    </div>
  `;
  const main = document.getElementById("main");
  if (main) {
    const dbg = document.getElementById("panel-debug");
    if (dbg && dbg.parentElement === main) main.insertBefore(panel, dbg);
    else main.appendChild(panel);
  }
  const $ = (id) => panel.querySelector("#" + id);
  window.OTNav.register({ id: "actuator", label: "Actuator tests", icon: "⏻", group: "vehicle", tier: "bench" });
  if (window.OT && typeof window.OT.gate === "function") {
    window.OT.gate(panel, ["diag"], "actuator tests", { disable: ["#actRun"] });
  }

  const st = { defPath: null, def: null, channels: {}, running: false, timer: null, live: false, debug: false };
  const connArgs = () => (window.OT && window.OT.connArgs ? window.OT.connArgs() : { driver: "elm327obd", port: null, baud: null, bus: null, txId: null, rxId: null });
  const timeoutMs = () => {
    const n = parseInt($("actTimeout").value, 10);
    return Number.isFinite(n) && n > 0 ? n : 1000;
  };
  const freeform = () => (st.debug && $("actFfOn").checked
    ? { did: $("actFfDid").value.trim(), state: $("actFfState").value.trim(), security: !!$("actFfSec").checked, max: 5000 }
    : null);
  const selected = () => st.channels[$("actChan").value] || null;

  // ---- channel + option form ----------------------------------------------
  function fillOptions() {
    const ff = freeform();
    const ch = selected();
    const opts = ff ? Object.keys(OPT) : ch ? ch.options : [];
    const prev = $("actOpt").value;
    $("actOpt").innerHTML = opts.map((o) => `<option value="${esc(o)}">${esc(OPT[o] || o)}</option>`).join("");
    // Default to the first option that actually tests something.
    $("actOpt").value = opts.includes(prev) ? prev : opts.find(takesControl) || opts[0] || "";
    refreshForm();
  }
  function refreshForm() {
    const ff = freeform();
    const ch = selected();
    const opt = $("actOpt").value;
    $("actValueRow").hidden = !!ff || !(ch && ch.value && opt === "short_term_adjustment");
    if (ch && ch.value) $("actValueHint").textContent = `${fmtNum(ch.value.min)} … ${fmtNum(ch.value.max)} ${ch.value.unit}`;
    $("actDurRow").hidden = !takesControl(opt);
    const max = ff ? ff.max : ch ? ch.max_duration_ms : 0;
    $("actDurHint").textContent = max ? `at most ${max / 1000} s` : "";
    $("actSecRow").hidden = !(ff ? ff.security : st.def && st.def.requires_security);
    if (!ch) { $("actChanInfo").innerHTML = ""; return; }
    $("actChanInfo").innerHTML =
      `DID <span class="mono">0x${Number(ch.did).toString(16).toUpperCase().padStart(4, "0")}</span>` +
      ` · default state <span class="mono">${esc(ch.default_state || "—")}</span>` +
      (ch.value ? ` · value ${fmtNum(ch.value.min)}…${fmtNum(ch.value.max)} ${esc(ch.value.unit)}` : " · no adjustable value") +
      ` · up to ${ch.max_duration_ms / 1000} s` + (ch.note ? `<br>${esc(ch.note)}` : "");
  }

  const packPick = window.OTPacks.picker({
    panel: "actuator",
    kind: "adaptation",
    filter: { name: "Adaptation def", extensions: ["json"] },
    onChange: (sel) => {
      st.defPath = sel.path;
      st.def = null;
      st.channels = {};
      $("actChan").innerHTML = "";
      $("actDefInfo").innerHTML = "";
      fillOptions();
    },
  });
  $("actPackPick").appendChild(packPick.el);

  $("actList").addEventListener("click", async () => {
    if (!st.defPath) { $("actDefInfo").innerHTML = bad("Choose an adaptation definition (a pack and its definition, or a file) first."); return; }
    try {
      const d = await invoke("actuator_list", { defPath: st.defPath });
      st.def = d;
      st.channels = {};
      (d.channels || []).forEach((c) => { st.channels[c.name] = c; });
      $("actChan").innerHTML = (d.channels || []).map((c) => `<option value="${esc(c.name)}">${esc(c.name)}</option>`).join("");
      $("actChan").value = d.channels && d.channels.length ? d.channels[0].name : "";
      const tgt = d.target ? ` · module <span class="mono">${esc(d.target.tx)}/${esc(d.target.rx)}</span>` : ` · <span class="pill bad">no module address — set the Target ids</span>`;
      const sec = d.requires_security ? ` · <span class="pill accent">SecurityAccess level 0x${Number(d.security_level).toString(16).toUpperCase().padStart(2, "0")}</span>` : "";
      $("actDefInfo").innerHTML =
        `<div><b>${esc(d.name || d.id)}</b> <span class="muted small">${esc(d.module || "")}</span></div>` +
        `<div class="muted small">${(d.channels || []).length} actuator channel(s)${tgt}${sec}</div>` +
        (!(d.channels || []).length ? `<div class="banner info">This definition declares no actuator channels, so there is nothing to test. Only declared channels can be driven.</div>` : "");
      fillOptions();
    } catch (e) {
      $("actDefInfo").innerHTML = bad(e);
    }
  });
  $("actChan").addEventListener("change", fillOptions);
  $("actOpt").addEventListener("change", refreshForm);
  ["actFfOn", "actFfSec"].forEach((id) => $(id).addEventListener("change", fillOptions));

  // The request both Plan and Run send.
  function request() {
    const ff = freeform();
    const ch = selected();
    const opt = $("actOpt").value;
    const v = validate(ch, opt, $("actValue").value, $("actDur").value, ff);
    if (v.error) return v;
    const args = { option: opt, durationMs: v.durationMs, value: v.value };
    if (ff) Object.assign(args, { freeformDid: ff.did, state: ff.state || null, freeformSecurity: ff.security });
    else Object.assign(args, { defPath: st.defPath, channel: ch.name });
    return { args, ch, ff };
  }

  $("actPlan").addEventListener("click", async () => {
    const r = request();
    if (r.error) { $("actPlanOut").innerHTML = bad(r.error); return; }
    try {
      $("actPlanOut").innerHTML = planHtml(await invoke("actuator_plan", r.args));
    } catch (e) {
      $("actPlanOut").innerHTML = bad(e);
    }
  });

  // ---- run / poll / stop --------------------------------------------------
  function setRunning(on) {
    st.running = on;
    $("actRun").disabled = on;
    $("actStop").disabled = !on;
    $("actProgressWrap").hidden = !on;
    if (!on && st.timer) { clearInterval(st.timer); st.timer = null; }
  }
  function render(s) {
    $("actLog").innerHTML = logHtml(s.exchanges);
    if (s.running) {
      const pct = s.duration_ms ? Math.min(100, (100 * s.elapsed_ms) / s.duration_ms) : 100;
      $("actProgress").style.width = pct.toFixed(0) + "%";
      $("actProgressText").textContent = `${s.channel}: ${(s.remaining_ms / 1000).toFixed(1)} s left — control returns to the ECU automatically`;
      return;
    }
    setRunning(false);
    let h = s.error ? bad(s.error) : "";
    if (s.stop_reason && s.report && s.report.stopped) h += `<div class="muted small">Stopped: ${esc(s.stop_reason)}</div>`;
    $("actResult").innerHTML = h + reportHtml(s.report);
  }
  async function poll() {
    try {
      const s = await invoke("actuator_status");
      render(s);
      return s;
    } catch (e) {
      $("actResult").innerHTML = bad(e);
      return null;
    }
  }
  async function stopNow(reason) {
    if (!st.running) return null;
    try {
      const s = await invoke("actuator_stop", { reason });
      render(s);
      return s;
    } catch (e) {
      setRunning(false);
      $("actResult").innerHTML = bad(e);
      return null;
    }
  }

  $("actRun").addEventListener("click", async () => {
    const out = $("actResult");
    if (st.running) return;
    const r = request();
    if (r.error) { out.innerHTML = bad(r.error); return; }
    if (!$("actAck").checked) { out.innerHTML = bad("Tick the confirmation — this drives a real output."); return; }
    $("actAck").checked = false; // a fresh confirmation for every run
    if (r.ff && !$("actFfAck").checked) { out.innerHTML = bad("Free-form DID: tick the second confirmation."); return; }
    const what = r.ff ? `free-form DID ${r.ff.did}` : `“${r.ch.name}”`;
    const dur = r.args.durationMs ? ` for ${(r.args.durationMs / 1000).toFixed(1)} s` : "";
    if (!window.confirm(`Drive ${what} with ${r.args.option}${dur}? Control returns to the ECU automatically at the end or on STOP.`)) {
      out.innerHTML = bad("Cancelled — nothing was sent.");
      return;
    }
    if (r.ff) $("actFfAck").checked = false;
    try {
      out.innerHTML = "";
      await invoke("actuator_start", {
        ...connArgs(),
        ...r.args,
        timeoutMs: timeoutMs(),
        ack: true,
        freeformAck: !!r.ff,
        seedkey: $("actSecRow").hidden ? null : $("actSeedkey").value.trim() || null,
      });
      setRunning(true);
      $("actLog").innerHTML = logHtml([]);
      st.timer = setInterval(poll, POLL_MS);
      poll();
    } catch (e) {
      out.innerHTML = bad(e);
    }
  });
  $("actStop").addEventListener("click", () => stopNow("STOP pressed"));

  // Leaving the panel, losing the adapter or closing the window ends a test.
  const panelHidden = () => (st.running && !panel.classList.contains("active") ? stopNow("panel closed") : null);
  if (typeof MutationObserver === "function") {
    try { new MutationObserver(panelHidden).observe(panel, { attributes: true, attributeFilter: ["class"] }); } catch (e) {}
  }
  if (window.OT && typeof window.OT.onCaps === "function") {
    window.OT.onCaps((adapter) => { if (st.running && !(adapter && adapter.connected)) stopNow("adapter disconnected"); });
  }
  window.addEventListener("beforeunload", () => { if (st.running) invoke("actuator_stop", { reason: "window closed" }).catch(() => {}); });

  // Bench tier: the live controls (and the Debug free-form card) only on alpha / bench.
  window.OTNav.onChannel((ch) => {
    st.live = window.OTNav.benchEnabled();
    st.debug = ch === "alpha" || ch === "bench";
    $("actLive").hidden = !st.live;
    $("actStableNote").hidden = st.live;
    $("actFfCard").hidden = !st.debug;
  });
  fillOptions();

  window.OTActuator = { validate, reportHtml, logHtml, planHtml, poll, stopNow, panelHidden, state: st };
})();
