// OpenTune — Haldex AWD panel (OpenHaldex status + mode, Gen4 read, CAN-log decode).
// Backend: feat_haldex.rs (haldex_modes / _status / _set_mode / _gen4_read /
// _decode_log / _export_csv), a thin shell over opentune-haldex (docs/HALDEX.md).
// Talks to the car, so it stays tier "bench" until vehicle-tested.
"use strict";
(function () {
  const TAURI = window.__TAURI__ || {};
  const invoke = (c, a) => TAURI.core.invoke(c, a);
  const { esc, seriesPath } = window.OTUtil;
  const msg = (e) => (typeof e === "string" ? e : (e && e.message) || String(e));
  const banner = (cls, html) => `<div class="banner ${cls}">${html}</div>`;

  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-haldex";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Haldex AWD</h1>
      <p>OpenHaldex controller status and drive mode, a read-only Haldex Gen4 read, and an offline decoder for drivetrain-CAN captures.</p>
    </header>

    <div class="card">
      <div class="card-title-row"><h2>OpenHaldex</h2>
        <label class="confirm"><input type="checkbox" id="hxSim" /> Simulated unit</label></div>
      <p class="small muted">Reads the controller's supplier DIDs on <span class="mono">70F / 779</span> (lock <span class="mono">FDA0</span>, slip <span class="mono">FDA1</span>).
        From the OBD port this only reaches the unit where the gateway routes 0x70F to the drivetrain CAN (MQB); on PQ cars connect to the drivetrain CAN.</p>
      <div class="row">
        <button class="btn" id="hxRead" type="button">Read status</button>
        <button class="btn primary" id="hxLive" type="button">Live (2 Hz)</button>
        <span class="pill neutral" id="hxState">idle</span>
      </div>
      <div class="dash-grid" id="hxTiles" style="margin-top:10px"></div>
      <h3 style="margin-top:14px">Drive mode</h3>
      <p class="small muted">Changing the mode changes how the car drives: only with the car stationary or in a safe place, and know how to return to <b>Stock</b>. Each change asks for confirmation and shows the mode the unit reports back.</p>
      <div class="seg" id="hxModes" role="group" aria-label="OpenHaldex mode"></div>
      <div id="hxModeOut" style="margin-top:10px"></div>
    </div>

    <div class="card">
      <div class="card-title-row"><h2>Haldex Gen4 read (read-only)</h2>
        <label class="confirm"><input type="checkbox" id="hxG4Sim" checked /> Simulated unit</label></div>
      ${banner("info", "The Gen4 (0AY 907 554 C, PQ35/PQ46) speaks KWP2000 over <b>VW TP2.0</b>. A real read needs a raw-CAN adapter (SocketCAN, slcan/CANable or gs_usb/candleLight), which opens the TP2.0 channel to address 0x0A; the ELM327 drivers cannot and are <b>refused</b> with that reason. The simulated unit runs the full TP2.0 + KWP2000 stack with synthetic data.")}
      <div class="row">
        <label class="small muted" for="hxGroups">Groups</label>
        <input class="inp mono" id="hxGroups" value="1, 3, 125, 126" style="width:150px" />
        <label class="confirm"><input type="checkbox" id="hxDtcs" checked /> Stored codes</label>
        <button class="btn primary" id="hxG4" type="button">Read Gen4</button>
      </div>
      <div id="hxG4Out"></div>
    </div>

    <div class="card">
      <h2>CAN log decode</h2>
      <p class="small muted">Open a candump log, OpenTune CSV or SavvyCAN CSV capture of the drivetrain CAN (e.g. from <b>CAN capture</b>). Offline.</p>
      <div class="row">
        <button class="btn" id="hxPick" type="button">Open log…</button>
        <span class="small mono muted" id="hxPath">no log chosen</span>
        <select class="inp" id="hxBus" title="Frame set">
          <option value="auto">auto (PQ + MQB)</option>
          <option value="pq">PQ (Gen1/2/4)</option>
          <option value="mqb">MQB (Gen5)</option>
        </select>
        <button class="btn primary" id="hxDecode" type="button" disabled>Decode</button>
        <button class="btn" id="hxExport" type="button" disabled title="Write the series as a Live-log CSV the Log viewer opens">Export CSV for Log viewer…</button>
      </div>
      <div id="hxLogOut"></div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "haldex", label: "Haldex AWD", icon: "⚙", group: "vehicle", tier: "bench" });

  const $ = (id) => panel.querySelector("#" + id);
  const sim = () => $("hxSim").checked;
  const conn = () => (window.OT && window.OT.connArgs ? window.OT.connArgs() : { driver: "mock" });
  const state = (cls, t) => { const p = $("hxState"); p.className = "pill " + cls; p.textContent = t; };

  // ---- OpenHaldex status --------------------------------------------------
  let timer = null, inFlight = false, modes = [];
  function tile(label, val, unit, cls) {
    return `<div class="tile ${cls || "live"}"><div class="t-label"><span>${esc(label)}</span><i class="t-dot"></i></div>
      <div><span class="t-val">${esc(val)}</span><span class="t-unit">${esc(unit || "")}</span></div></div>`;
  }
  const slipTxt = (v) => (v == null ? "—" : String(v));
  function showStatus(r) {
    const l = r.lock || {}, s = r.slip || {};
    const m = modes.find((x) => x.mode === l.mode);
    $("hxTiles").innerHTML =
      tile("Mode", m ? m.mode : l.mode || `#${l.mode_index}`, "") +
      tile("Commanded lock", l.commanded_pct ?? "—", "%") +
      tile("Engagement (raw)", l.engagement_raw ?? "—", "") +
      tile("Slip FL / FR", `${slipTxt(s.fl)} / ${slipTxt(s.fr)}`, "", r.slip ? "live" : "na") +
      tile("Slip RL / RR", `${slipTxt(s.rl)} / ${slipTxt(s.rr)}`, "", r.slip ? "live" : "na");
    panel.querySelectorAll("#hxModes button").forEach((b) => b.classList.toggle("on", b.dataset.m === l.mode));
  }
  async function readStatus() {
    if (inFlight) return; // never overlap polls
    inFlight = true;
    try {
      const r = await invoke("haldex_status", { ...conn(), sim: sim(), timeoutMs: 500 });
      showStatus(r);
      if (!timer) state("ok", r.sim ? "read (simulated)" : "read");
    } catch (e) {
      state("bad", msg(e));
    } finally {
      inFlight = false;
    }
  }
  function stopLive() {
    if (timer) { clearInterval(timer); timer = null; }
    $("hxLive").textContent = "Live (2 Hz)";
  }
  $("hxRead").addEventListener("click", readStatus);
  $("hxLive").addEventListener("click", () => {
    if (timer) { stopLive(); return state("neutral", "stopped"); }
    timer = setInterval(readStatus, 500);
    readStatus();
    $("hxLive").textContent = "Stop";
    state("accent", sim() ? "live (simulated)" : "live");
  });
  // Stop polling when the panel is hidden (frees the adapter for other panels).
  new MutationObserver(() => { if (timer && !panel.classList.contains("active")) { stopLive(); state("neutral", "stopped"); } })
    .observe(panel, { attributes: true, attributeFilter: ["class"] });

  // ---- mode change ----------------------------------------------------------
  function drawModes() {
    $("hxModes").innerHTML = modes
      .map((m) => `<button type="button" data-m="${esc(m.mode)}" title="${esc(m.label)}">${esc(m.mode)}</button>`).join("");
  }
  $("hxModes").addEventListener("click", async (ev) => {
    const b = ev.target.closest("button[data-m]");
    if (!b) return;
    const m = modes.find((x) => x.mode === b.dataset.m);
    const req = `2E FD A2 ${String(m.index).padStart(2, "0")}`;
    const ok = window.confirm(
      `Change the OpenHaldex mode to ${m.label}?\n\n` +
      `Sends ${req} to 0x70F${sim() ? " (simulated unit)" : ""}. This changes how the car drives — ` +
      "only with the car stationary or in a safe place. You can return to Stock the same way.");
    if (!ok) return;
    const out = $("hxModeOut");
    out.innerHTML = '<span class="muted small">changing mode…</span>';
    try {
      const r = await invoke("haldex_set_mode", { ...conn(), mode: m.mode, confirm: true, sim: sim(), timeoutMs: 1000 });
      out.innerHTML = banner("ok", `The unit now reports mode <b>${esc(r.mode)}</b>, commanded lock ${esc(r.lock.commanded_pct)} % (read back after <span class="mono">${esc(r.request)}</span>).`);
      showStatus({ lock: r.lock, slip: null });
    } catch (e) {
      out.innerHTML = banner("bad", esc(msg(e)));
    }
  });

  // ---- Gen4 -----------------------------------------------------------------
  $("hxG4").addEventListener("click", async () => {
    const out = $("hxG4Out");
    const groups = $("hxGroups").value.split(/[,\s]+/).filter(Boolean).map(Number).filter((n) => n >= 0 && n <= 255);
    out.innerHTML = '<span class="muted small">reading…</span>';
    try {
      const r = await invoke("haldex_gen4_read", { ...conn(), sim: $("hxG4Sim").checked, groups, readDtcs: $("hxDtcs").checked });
      out.innerHTML = gen4Html(r);
    } catch (e) {
      out.innerHTML = banner("bad", esc(msg(e)));
    }
  });
  function gen4Html(r) {
    const rep = r.report || {}, id = rep.identity;
    let h = r.sim ? banner("info", "Simulated Gen4 unit — synthetic data, not read from a car.") : "";
    if (rep.session_error) h += banner("bad", "Session: " + esc(rep.session_error));
    h += id
      ? `<div class="meta-grid"><div class="k">part number</div><div class="v">${esc(id.part_number)}</div>
         <div class="k">software</div><div class="v">${esc(id.sw_version)}</div>
         <div class="k">component</div><div class="v">${esc(id.component)}</div>
         <div class="k">generation</div><div class="v">${esc(id.generation)}</div></div>`
      : banner("bad", "No identification: " + esc(rep.identity_error || "no answer"));
    if (rep.dtcs) h += `<p class="small"><b>Stored codes:</b> ${rep.dtcs.length ? rep.dtcs.map((d) => `<span class="mono">${String(d.vag_number).padStart(5, "0")}</span>`).join(", ") : "none"} <span class="muted">(no fault-text decode yet)</span></p>`;
    else if (rep.dtc_error) h += `<p class="small muted">Codes: ${esc(rep.dtc_error)}</p>`;
    const rows = (rep.groups || []).flatMap((g) => (g.error
      ? [`<tr><td class="mono">${g.group}</td><td colspan="3" class="muted">${esc(g.error)}</td></tr>`]
      : g.values.map((v, i) => `<tr><td class="mono">${i ? "" : g.group}</td><td>${esc(v.label || "—")}</td>
          <td class="mono">${v.value == null ? `raw ${v.formula.toString(16)}/${v.a}/${v.b}` : esc(+v.value.toFixed(3))}</td><td>${esc(v.unit || "")}</td></tr>`)));
    if (rows.length) h += `<table class="tbl"><thead><tr><th>Group</th><th>Value (community labels)</th><th>Reading</th><th>Unit</th></tr></thead><tbody>${rows.join("")}</tbody></table>`;
    return h;
  }

  // ---- CAN log decode -------------------------------------------------------
  let logPath = null;
  $("hxPick").addEventListener("click", async () => {
    try {
      const r = await TAURI.dialog.open({ multiple: false, directory: false, filters: [{ name: "CAN log", extensions: ["log", "csv", "txt"] }, { name: "All files", extensions: ["*"] }] });
      const p = Array.isArray(r) ? r[0] : r;
      if (!p) return;
      logPath = p;
      $("hxPath").textContent = String(p).split(/[\\/]/).pop();
      $("hxDecode").disabled = false;
      $("hxExport").disabled = false;
    } catch (e) { $("hxLogOut").innerHTML = banner("bad", esc(msg(e))); }
  });
  $("hxDecode").addEventListener("click", async () => {
    const out = $("hxLogOut");
    out.innerHTML = '<span class="muted small">decoding…</span>';
    try {
      out.innerHTML = logHtml(await invoke("haldex_decode_log", { path: logPath, bus: $("hxBus").value }));
    } catch (e) { out.innerHTML = banner("bad", esc(msg(e))); }
  });
  $("hxExport").addEventListener("click", async () => {
    try {
      const base = String(logPath).replace(/\.[^.\\/]*$/, "");
      const outPath = await TAURI.dialog.save({ defaultPath: base + ".haldex.csv", filters: [{ name: "Live-log CSV", extensions: ["csv"] }] });
      if (!outPath) return;
      const r = await invoke("haldex_export_csv", { path: logPath, bus: $("hxBus").value, out: outPath });
      window.toast && window.toast(`wrote ${r.rows} rows — open it in the Log viewer`);
    } catch (e) { $("hxLogOut").insertAdjacentHTML("afterbegin", banner("bad", esc(msg(e)))); }
  });

  const rng = (r, u) => (r ? `${+r.min.toFixed(1)} … ${+r.max.toFixed(1)} ${u} (last ${+r.last.toFixed(1)}, ${r.samples} samples)` : "—");
  function chart(s) {
    const W = 600, H = 70, p = seriesPath(s.t, s.v, W, H);
    const span = s.t.length ? s.t[s.t.length - 1] - s.t[0] : 0;
    return `<div class="hx-chart"><div class="t-label"><span>${esc(s.name)} <span class="muted">${esc(s.units)}</span></span>
        <span class="mono">${p.lo == null ? "" : `${+p.lo.toFixed(1)} … ${+p.hi.toFixed(1)}`}</span></div>
      <svg class="hx-svg" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-label="${esc(s.name)}"><path class="line" d="${p.d}"/></svg>
      <div class="t-foot"><span>0 s</span><span>${span.toFixed(1)} s</span></div></div>`;
  }
  function logHtml(r) {
    const s = r.summary || {};
    const kinds = Object.entries(s.per_kind || {}).map(([k, n]) => `${esc(k)} ${n}`).join(", ") || "none";
    let h = `<div class="meta-grid" style="margin-top:10px">
      <div class="k">frames</div><div class="v">${s.frames} (${s.decoded} Haldex-related${r.bad_lines ? `, ${r.bad_lines} unreadable lines` : ""})</div>
      <div class="k">kinds</div><div class="v">${kinds}</div>
      <div class="k">lock request</div><div class="v">${rng(s.lock_request_pct, "%")}</div>
      <div class="k">lock actual</div><div class="v">${rng(s.lock_actual_pct, "%")}</div>
      <div class="k">Gen4 temperature</div><div class="v">${rng(s.gen4_temp_c, "°C")}</div>
      <div class="k">max front-rear Δ</div><div class="v">${s.max_front_rear_delta_kmh == null ? "—" : s.max_front_rear_delta_kmh.toFixed(1) + " km/h"}</div>
      <div class="k">flags seen</div><div class="v">${(s.flags_seen || []).map(esc).join(", ") || "none"}${s.fault_stored_seen ? " · fault stored" : ""}${s.mqb_fault_reaction ? " · MQB fault reaction" : ""}</div>
      <div class="k">OpenHaldex</div><div class="v">${(s.openhaldex_modes || []).map(esc).join(", ") || "—"}</div>
    </div>`;
    if (!s.decoded) return h + banner("info", "No Haldex-related frames in this log. Check the bus (drivetrain CAN) and the frame set.");
    h += `<div class="hx-charts">${(r.series || []).map(chart).join("")}</div>
      <p class="small muted">Byte meanings: opendbc vw_pq/vw_mqb (MIT) and OpenHaldex-C6 (MIT, community confidence for the Gen4 temperature and lever scale). Clutch actual is in the DBC's Nm/min scale.</p>`;
    const rows = (r.rows || []).map((x) => {
      const d = Object.assign({}, x.decoded), kind = d.frame; delete d.frame;
      const txt = Object.entries(d).map(([k, v]) => `${k}=${typeof v === "object" && v !== null ? JSON.stringify(v) : v}`).join(" ");
      return `<tr><td class="mono">${x.t == null ? "—" : x.t.toFixed(3)}</td><td class="mono">${esc(x.id)}</td><td>${esc(kind)}</td><td class="mono small">${esc(txt)}</td></tr>`;
    }).join("");
    h += `<details><summary class="small">Decoded frames (first ${(r.rows || []).length} of ${r.rows_total})</summary>
      <div class="cd-wrap"><table class="tbl"><thead><tr><th>t (s)</th><th>ID</th><th>Frame</th><th>Fields</th></tr></thead><tbody>${rows}</tbody></table></div></details>`;
    return h;
  }

  window.onPanelFirstShow("haldex", async () => {
    try { modes = await invoke("haldex_modes"); drawModes(); } catch (e) { $("hxModeOut").innerHTML = banner("bad", esc(msg(e))); }
  });
})();
