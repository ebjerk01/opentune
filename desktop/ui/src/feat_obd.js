// OpenTune — Live OBD-II dashboard feature.
// Self-contained: injects its own sidebar nav item and panel, then polls the
// backend `obd_live` command on an interval and shows RPM / coolant / speed /
// battery as big readouts. Reuses the global connection (window.OT.connArgs) and
// gates itself on the adapter's CAN capability (window.OT.gate). No new CSS.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const el = (tag, cls, html) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (html != null) e.innerHTML = html;
    return e;
  };

  let timer = null;

  function build() {
    const nav = document.getElementById("nav");
    const main = document.getElementById("main");
    if (!nav || !main || document.getElementById("panel-obd")) return;

    // --- sidebar nav item ---
    window.OTNav.register({ id: "obd", label: "OBD live", icon: "📟", group: "vehicle", tier: "bench" });

    // --- panel ---
    const panel = el("section", "panel");
    panel.id = "panel-obd";
    panel.innerHTML = `
      <header class="panel-head">
        <h1>OBD live</h1>
        <p>Real-time engine values over generic OBD-II (mode 01) via the ELM327 serial adapter.</p>
      </header>
      <div class="card">
        <p class="muted small">
          Needs the vehicle <b>running / ignition on</b> and the dongle's HS/MS switch on the
          <b>HS-CAN</b> side — powertrain live data (RPM, coolant, speed) is on HS-CAN. With no car
          connected the live values read <span class="mono">—</span>; battery voltage still reads.
        </p>
        <div class="row">
          <button class="btn primary" id="obdStart">Start</button>
          <button class="btn" id="obdStop" disabled>Stop</button>
          <label>Interval</label>
          <input class="inp mono" id="obdInterval" type="number" min="100" step="100" value="500" style="width:90px" />
          <span class="muted small">ms</span>
          <span class="pill neutral" id="obdStatus">idle</span>
        </div>
      </div>
      <div class="card">
        <div class="result">
          <div class="row">
            <div style="flex:1 1 120px">
              <div class="muted small">RPM</div>
              <div class="mono" id="obdRpm" style="font-size:34px;font-weight:700">—</div>
            </div>
            <div style="flex:1 1 120px">
              <div class="muted small">Coolant °C</div>
              <div class="mono" id="obdCoolant" style="font-size:34px;font-weight:700">—</div>
            </div>
            <div style="flex:1 1 120px">
              <div class="muted small">Speed km/h</div>
              <div class="mono" id="obdSpeed" style="font-size:34px;font-weight:700">—</div>
            </div>
            <div style="flex:1 1 120px">
              <div class="muted small">Battery V</div>
              <div class="mono" id="obdBatt" style="font-size:34px;font-weight:700">—</div>
            </div>
          </div>
        </div>
      </div>`;
    main.appendChild(panel);

    const startBtn = panel.querySelector("#obdStart");
    const stopBtn = panel.querySelector("#obdStop");
    const intervalInp = panel.querySelector("#obdInterval");
    const statusPill = panel.querySelector("#obdStatus");
    const rpmEl = panel.querySelector("#obdRpm");
    const coolEl = panel.querySelector("#obdCoolant");
    const spdEl = panel.querySelector("#obdSpeed");
    const battEl = panel.querySelector("#obdBatt");

    const DASH = "—";
    function render(r) {
      r = r || {};
      rpmEl.textContent = r.rpm != null ? Math.round(r.rpm) : DASH;
      coolEl.textContent = r.coolant_c != null ? r.coolant_c : DASH;
      spdEl.textContent = r.speed_kph != null ? r.speed_kph : DASH;
      battEl.textContent = r.battery_mv != null ? (r.battery_mv / 1000).toFixed(1) : DASH;
    }

    let inFlight = false;
    async function poll() {
      if (inFlight) return; // skip overlapping ticks (a slow/no-car read must not stack opens)
      inFlight = true;
      try {
        const r = await invoke("obd_live", { ...window.OT.connArgs() });
        render(r);
        statusPill.className = "pill ok";
        statusPill.textContent = "live";
      } catch (e) {
        // Open/handshake failed (no adapter / bad port): blank the readouts and
        // surface the reason, but keep polling in case the adapter comes back.
        render(null);
        statusPill.className = "pill bad";
        statusPill.textContent = typeof e === "string" ? e : (e && e.message) || "error";
      } finally {
        inFlight = false;
      }
    }

    function stop() {
      if (timer) { clearInterval(timer); timer = null; }
      startBtn.disabled = false;
      stopBtn.disabled = true;
      statusPill.className = "pill neutral";
      statusPill.textContent = "idle";
    }

    function start() {
      if (timer) return;
      let ms = parseInt(intervalInp.value, 10);
      if (!Number.isFinite(ms) || ms < 100) ms = 500;
      startBtn.disabled = true;
      stopBtn.disabled = false;
      statusPill.className = "pill accent";
      statusPill.textContent = "starting…";
      poll();
      timer = setInterval(poll, ms);
    }

    startBtn.addEventListener("click", start);
    stopBtn.addEventListener("click", stop);

    // Capability gate: live OBD needs a CAN-capable adapter. Disables Start (and
    // re-enables it) as the adapter changes; stops polling if the cap is lost.
    window.OT.gate(panel, ["can"], "live OBD data", { disable: ["#obdStart"] });
    window.OT.onCaps(() => { if (!window.OT.has(["can"])) stop(); });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", build);
  } else {
    build();
  }
})();
