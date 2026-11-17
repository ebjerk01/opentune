// Auto-debug wizard — a guided, capability-speced self-test for the connected
// adapter. Steps through each test, says what it's doing, prompts for the
// physical car action where one is needed, runs the check, and records a report
// (every raw exchange also lands in the Debug session log for analysis).
// Self-contained: no backend changes — orchestrates probe_adapter / obd_live /
// debug_raw_request. Alpha panel. The report can be saved to a local JSON file.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>]/g, (m) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[m]));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const conn = () => (window.OT ? window.OT.connArgs() : { driver: "elm327obd" });

  // --- low-level helpers ----------------------------------------------------
  async function raw(hex, timeoutMs) {
    const r = await invoke("debug_raw_request", { ...conn(), request: hex, timeoutMs: timeoutMs || 1500 });
    return r; // hex string, or throws
  }
  function asAscii(hexstr) {
    const b = hexstr.split(/\s+/).filter(Boolean).map((x) => parseInt(x, 16));
    const a = b.filter((x) => x >= 0x20 && x < 0x7f).map((x) => String.fromCharCode(x)).join("");
    return a.length >= 4 ? a : null;
  }

  // --- test catalogue (capability-speced) -----------------------------------
  // ok: true=pass, false=fail, null=informational/manual.
  const TESTS = [
    { id: "connect", title: "Adapter handshake", need: [],
      car: "Plug the adapter into the OBD-II port and turn the ignition ON (engine off).",
      doing: "Opening the adapter and reading its firmware + battery voltage.",
      run: async () => {
        const st = window.OT ? await window.OT.probe() : null;
        if (!st || !st.connected) return { ok: false, detail: "no adapter — check the cable, port and driver/port in the Adapter bar" };
        const v = st.battery_mv != null ? (st.battery_mv / 1000).toFixed(1) + "V" : "?";
        return { ok: true, detail: `${st.name || st.driver} ${st.firmware || ""} · ${v}`.trim() };
      } },
    { id: "voltage", title: "Battery / car power", need: [],
      doing: "Checking the OBD port shows ~12 V (car power present).",
      run: async () => {
        const st = window.OT && window.OT.adapter;
        const mv = st && st.battery_mv;
        if (mv == null) return { ok: null, detail: "this adapter can't read voltage" };
        if (mv < 11000) return { ok: false, detail: `${(mv / 1000).toFixed(1)}V — low/no car power. Ignition on?` };
        return { ok: true, detail: `${(mv / 1000).toFixed(1)}V` };
      } },
    { id: "pids", title: "OBD-II supported PIDs", need: ["can"],
      doing: "Querying mode 01 PID 00 (which live PIDs the ECU supports).",
      run: async () => { const r = await raw("0100"); return { ok: r.replace(/\s/g, "").startsWith("4100"), detail: r }; } },
    { id: "vin", title: "Read VIN", need: ["can"],
      doing: "Querying mode 09 PID 02 (vehicle identification number).",
      run: async () => { const r = await raw("0902", 2500); const a = asAscii(r); return { ok: !r.replace(/\s/g, "").startsWith("7F"), detail: a ? `"${a}"  (${r})` : r }; } },
    { id: "dtc", title: "Stored fault codes", need: ["can"],
      doing: "Querying mode 03 (stored diagnostic trouble codes).",
      run: async () => { const r = await raw("03"); const ok = r.replace(/\s/g, "").startsWith("43"); return { ok: ok || null, detail: ok ? `DTC frame: ${r}` : r }; } },
    { id: "start", title: "Engine start → RPM", need: ["can"],
      car: "Start the engine and let it idle.",
      doing: "Reading live RPM (PID 010C) to confirm the engine is running.",
      run: async () => { const d = await invoke("obd_live", conn()); if (d.rpm == null) return { ok: false, detail: "no RPM — engine running? correct CAN bus (powertrain = HS)?" }; return { ok: d.rpm > 300, detail: `RPM ${Math.round(d.rpm)}` }; } },
    { id: "rev", title: "Throttle response", need: ["can"],
      car: "Gently rev the engine toward ~2000 rpm, then release.",
      doing: "Sampling RPM ~8x over 3 s to see it rise and fall.",
      run: async () => {
        let mn = 1e9, mx = 0;
        for (let i = 0; i < 8; i++) { const d = await invoke("obd_live", conn()); if (d.rpm != null) { mn = Math.min(mn, d.rpm); mx = Math.max(mx, d.rpm); } await sleep(400); }
        if (mx === 0) return { ok: false, detail: "no RPM samples" };
        return { ok: (mx - mn) > 300, detail: `RPM ${Math.round(mn)}–${Math.round(mx)}` };
      } },
    { id: "coolant", title: "Coolant temperature", need: ["can"],
      doing: "Reading coolant temp (PID 0105).",
      run: async () => { const d = await invoke("obd_live", conn()); if (d.coolant_c == null) return { ok: null, detail: "not reported" }; return { ok: true, detail: `${d.coolant_c} °C` }; } },
    { id: "lights", title: "Exterior lights (bus activity)", need: ["can"],
      car: "Turn the headlights ON, then OFF.",
      doing: "Exercises the body/comfort CAN. On an ELM327 this is observational; on a raw-CAN or HEX-class adapter the traffic is visible.",
      run: async () => ({ ok: null, detail: "action performed — note it; captured for analysis" }) },
    { id: "volume", title: "Infotainment input", need: ["can"],
      car: "Press Volume-Up on the radio a few times.",
      doing: "Exercises the infotainment/comfort bus (observational on ELM327).",
      run: async () => ({ ok: null, detail: "action performed — note it; captured for analysis" }) },
    { id: "kline", title: "K-line interface", need: ["kline"],
      car: "Ignition ON.",
      doing: "Your adapter supports K-line. A full K-line (KWP) module probe needs a module address — use the Coding / Adaptation panels with a definition.",
      run: async () => ({ ok: null, detail: "K-line capable — ready for KWP1281/KWP2000 modules" }) },
    { id: "flash", title: "Flash capability", need: ["write_memory"],
      doing: "Your adapter can write memory (J2534-class). This step only confirms the capability — no write is performed.",
      run: async () => ({ ok: true, detail: "memory-write capable (flashing allowed in the Flash panel)" }) },
    { id: "stop", title: "Engine stop", need: ["can"],
      car: "Turn the engine OFF (ignition may stay on).",
      doing: "Confirming RPM drops to 0.",
      run: async () => { const d = await invoke("obd_live", conn()); return { ok: d.rpm == null || d.rpm < 300, detail: `RPM ${d.rpm == null ? "—" : Math.round(d.rpm)}` }; } },
  ];

  // --- UI shell -------------------------------------------------------------
  window.OTNav.register({ id: "autodebug", label: "Auto-debug", icon: "🧪", group: "vehicle", tier: "bench" });

  const sec = document.createElement("section");
  sec.className = "panel";
  sec.id = "panel-autodebug";
  sec.innerHTML =
    '<header class="panel-head"><h1>Auto-debug</h1><p>A guided self-test tailored to your adapter. It tells you what it is doing and what to do in the car; the full run is logged for analysis and can be saved to a local JSON file.</p></header>' +
    '<div class="card"><h2>Vehicle under test</h2>' +
    '  <div class="row"><input id="adMake" class="inp" placeholder="Make (e.g. VW)"><input id="adModel" class="inp" placeholder="Model (e.g. Passat CC)"><input id="adYear" class="inp" placeholder="Year" style="max-width:7ch"></div>' +
    '  <div class="row"><input id="adEngine" class="inp" placeholder="Engine / code (e.g. 2.0 TDI CBAB)"><input id="adNotes" class="inp" placeholder="Notes (optional)" style="flex:2"></div>' +
    "</div>" +
    '<div class="card">' +
    '  <div class="row"><button class="btn primary" id="adRun">Start auto-debug</button>' +
    '    <button class="btn" id="adSave" disabled>Save report…</button>' +
    '    <span id="adAdapter" class="muted small"></span></div>' +
    '  <div id="adStep" class="result"></div>' +
    '  <div id="adRows" class="result"></div>' +
    "</div>" +
    '<div class="card"><h2>Completed runs</h2><div id="adHistory" class="result"></div></div>';
  document.getElementById("main").appendChild(sec);

  const results = [];
  let planned = [];

  function mark(ok) { return ok === true ? '<span class="pill ok">PASS</span>' : ok === false ? '<span class="pill bad">FAIL</span>' : '<span class="pill neutral">INFO</span>'; }
  function renderRows() {
    $("adRows").innerHTML = results.length
      ? '<table class="tbl"><thead><tr><th>Test</th><th>Result</th><th>Detail</th></tr></thead><tbody>' +
        results.map((r) => `<tr><td>${esc(r.title)}</td><td>${mark(r.ok)}</td><td class="mono small">${esc(r.detail)}</td></tr>`).join("") +
        "</tbody></table>"
      : "";
  }

  function waitForContinue(car) {
    return new Promise((resolve) => {
      $("adStep").innerHTML =
        `<div class="banner info"><b>Do this in the car:</b> ${esc(car)}</div>` +
        '<div class="row"><button class="btn primary" id="adCont">Done — run test</button><button class="btn" id="adSkip">Skip</button></div>';
      $("adCont").onclick = () => resolve(true);
      $("adSkip").onclick = () => resolve(false);
    });
  }

  async function runPlan() {
    results.length = 0; renderRows();
    $("adSave").disabled = true;
    // Refresh capabilities first.
    const st = window.OT ? await window.OT.probe() : null;
    const who = st && st.connected ? `${st.name || st.driver} ${st.firmware || ""}`.trim() : "no adapter";
    $("adAdapter").textContent = "adapter: " + who;
    planned = TESTS.filter((t) => !t.need.length || (window.OT && window.OT.has(t.need)));
    for (let i = 0; i < planned.length; i++) {
      const t = planned[i];
      $("adStep").innerHTML = `<div class="banner"><b>Step ${i + 1}/${planned.length}: ${esc(t.title)}</b><br><span class="muted small">${esc(t.doing)}</span></div>`;
      if (t.car) {
        const go = await waitForContinue(t.car);
        if (!go) { results.push({ title: t.title, ok: null, detail: "skipped" }); renderRows(); continue; }
      } else {
        await sleep(500);
      }
      let r;
      try { r = await t.run(); } catch (e) { r = { ok: false, detail: String(e) }; }
      results.push({ title: t.title, ok: r.ok, detail: r.detail });
      renderRows();
    }
    const pass = results.filter((r) => r.ok === true).length;
    const fail = results.filter((r) => r.ok === false).length;
    const info = results.filter((r) => r.ok === null).length;
    $("adStep").innerHTML = `<div class="banner ${fail ? "bad" : "ok"}">Done — ${pass} passed, ${fail} failed, ${info} info, across ${planned.length} capability-matched tests.</div>`;
    $("adSave").disabled = false;
    pushHistory({ when: new Date().toISOString(), vehicle: vehicleLabel(), pass, fail, info, total: planned.length });
  }

  // --- vehicle + report -----------------------------------------------------
  function vehicle() {
    return {
      make: $("adMake").value.trim(),
      model: $("adModel").value.trim(),
      year: $("adYear").value.trim(),
      engine: $("adEngine").value.trim(),
      notes: $("adNotes").value.trim(),
    };
  }
  function vehicleLabel() {
    const v = vehicle();
    return [v.make, v.model, v.year, v.engine].filter(Boolean).join(" ") || "unknown vehicle";
  }
  function buildReport() {
    const ad = window.OT && window.OT.adapter;
    return {
      tool: "opentune auto-debug",
      when: new Date().toISOString(),
      vehicle: vehicle(),
      adapter: ad && ad.connected ? { driver: ad.driver, name: ad.name, firmware: ad.firmware, battery_mv: ad.battery_mv, capabilities: ad.capabilities } : null,
      results: results.map((r) => ({ test: r.title, result: r.ok === true ? "pass" : r.ok === false ? "fail" : "info", detail: r.detail })),
    };
  }

  // --- run history (local) --------------------------------------------------
  const HKEY = "ot_autodebug_history";
  function loadHistory() { try { return JSON.parse(localStorage.getItem(HKEY) || "[]"); } catch (e) { return []; } }
  function pushHistory(h) {
    try { const a = loadHistory(); a.unshift(h); localStorage.setItem(HKEY, JSON.stringify(a.slice(0, 50))); } catch (e) {}
    renderHistory();
  }
  function renderHistory() {
    const a = loadHistory();
    $("adHistory").innerHTML = a.length
      ? '<table class="tbl"><thead><tr><th>When</th><th>Vehicle</th><th>Pass</th><th>Fail</th><th>Info</th></tr></thead><tbody>' +
        a.map((h) => `<tr><td class="mono small">${esc((h.when || "").replace("T", " ").slice(0, 16))}</td><td>${esc(h.vehicle)}</td><td>${h.pass}</td><td>${h.fail}</td><td>${h.info}</td></tr>`).join("") +
        "</tbody></table>"
      : '<div class="muted small">No runs yet.</div>';
  }

  $("adRun").addEventListener("click", runPlan);
  $("adSave").addEventListener("click", () => {
    const blob = new Blob([JSON.stringify(buildReport(), null, 2)], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `opentune-autodebug-${(vehicleLabel().replace(/[^a-z0-9]+/gi, "-") || "run").toLowerCase()}.json`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  });
  renderHistory();
})();
