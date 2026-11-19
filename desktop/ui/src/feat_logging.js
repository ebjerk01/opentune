// OpenTune — Live datalogging panel (frontend).
//
// Self-contained: injects its own nav item + panel into the existing shell,
// reuses the global adapter connection (window.OT) and the app's CSS classes.
// The live loop runs here: on Start a setInterval calls the stateless backend
// command `log_poll_once` every N ms (the hub reuses the opened adapter), and
// the table updates in place. Rows are accumulated for CSV export.
//
// Fast (0x2C), an SSM transport or Reconnect: Start opens a live session
// instead (`log_start`). Fast packs the channels into dynamically defined DIDs
// where the ECU allows it and otherwise polls (the badge shows which, and
// why); SSM (K-line or CAN, ECU or TCU) identifies the module with BF, logs
// the parameters its capability bits allow and lists what was left out and
// why; Reconnect re-opens the adapter after a dropped link. Ticks go to
// `log_tick` (never overlapping) and Stop calls `log_stop`, which clears the
// DIDs. The session holds the adapter, so it is stopped when the panel is
// hidden, the adapter disappears or the window closes. Link events (lost,
// reconnecting, reconnected) arrive as marker rows (param_id "link"): they go
// into the CSV, the link pill and the Drive view. Pure logic: livelog-core.js.
"use strict";
(function () {
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"]/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c])
    );

  // ---- panel state ---------------------------------------------------------
  const S = {
    defPath: null,
    params: [], // [{id,name,units}]
    timer: null, // setInterval handle while live
    rows: [], // accumulated samples for CSV: {ts,paramId,name,raw,value,units,note,text}
    liveCells: {}, // paramId -> { value, units, note } <td> elements (in-place update)
    started: 0, // ticks since Start (for the status line)
    fast: false, // the running stream is a log_start session
  };
  const { logModeBadge, fmtLogRate, liveLogRow, liveLogCsv, liveShown, vehicleSpeedKmh } = window.OTUtil;
  const L = window.OTLiveLog;
  const $ = (id) => document.getElementById(id);
  // The capability the panel needs; regate() swaps the contents per transport
  // (the gate reads it on every refresh).
  const needs = ["diag"];
  let gate = null;

  // ---- build the nav item + panel -----------------------------------------
  function mount() {
    const nav = document.getElementById("nav");
    const main = document.getElementById("main");
    if (!nav || !main || document.getElementById("panel-logging")) return;

    window.OTNav.register({ id: "logging", label: "Live log", icon: "📈", group: "vehicle", tier: "bench" });

    const sec = document.createElement("section");
    sec.className = "panel";
    sec.id = "panel-logging";
    sec.innerHTML = `
      <header class="panel-head">
        <h1>Live log</h1>
        <p>Live datalogging from a RomRaider-style <span class="mono">logger.xml</span>.
           Pick a definition, choose parameters, and stream values from the ECU.</p>
      </header>

      <div class="card">
        <h2>Definition</h2>
        <div id="logPackPick"></div>
        <div class="row">
          <button class="btn primary" id="logLoad">Load</button>
          <span class="muted small">a logger definition from a pack, or a RomRaider <span class="mono">logger.xml</span> via File…</span>
        </div>
        <div id="logParamsBox" class="result" hidden>
          <div class="row">
            <span class="muted small" id="logParamCount">—</span>
            <button class="btn" id="logSelAll" type="button">Select all</button>
            <button class="btn" id="logSelNone" type="button">Select none</button>
          </div>
          <div id="logParams"></div>
        </div>
      </div>

      <div class="card">
        <h2>Live stream</h2>
        <div class="row">
          <label for="logInterval">Interval (ms)</label>
          <input class="inp" id="logInterval" type="number" min="50" step="50" value="500" style="max-width:7rem" />
          <button class="btn primary" id="logStart">Start</button>
          <button class="btn" id="logStop" disabled>Stop</button>
          <button class="btn" id="logSaveCsv" disabled>Save CSV</button>
          <span class="pill" id="logStatus">idle</span>
        </div>
        <div class="row">
          <label class="small muted" for="logTransport">Transport</label>
          <select class="inp sm" id="logTransport" title="What Start speaks to the module">
            ${L.TRANSPORTS.map((t) => `<option value="${t.id}">${esc(t.label)}</option>`).join("")}
          </select>
          <span id="logSsmOpts" hidden>
            <label class="small muted" for="logSsmTarget">Module</label>
            <select class="inp sm" id="logSsmTarget">
              <option value="ecu">ECU (engine)</option>
              <option value="tcu">TCU (transmission)</option>
            </select>
            <label class="confirm" title="A synthetic SSM module, no adapter needed (ssmsim)">
              <input type="checkbox" id="logSsmSim" /> Simulated module</label>
          </span>
          <label class="confirm" title="Hold one session on the adapter; when nothing answers for 5 ticks (or the adapter resets) close and re-open it with a growing pause (0.5–10 s), re-define the dynamic DIDs and carry on. Gives up after 5 minutes down, or after 3 re-opens in a row with no answer. Off: the session stops when the link is lost.">
            <input type="checkbox" id="logReconnect" /> Reconnect if the link drops</label>
          <span class="pill" id="logLink" hidden>—</span>
        </div>
        <div id="logSsmInfo" class="small" hidden></div>
        <div class="row">
          <label class="confirm" title="UDS 0x2C DynamicallyDefineDataIdentifier: one request per record instead of one per channel">
            <input type="checkbox" id="logFast" /> Fast (0x2C)</label>
          <label class="small muted" for="logDdid">Dynamic DID</label>
          <input class="inp sm mono" id="logDdid" value="F200" title="First DDDID, 0xF200–0xF3FF" />
          <label class="small muted" for="logMaxRec">Max record</label>
          <input class="inp sm mono" id="logMaxRec" type="number" min="1" max="255" value="32" title="Bytes per dynamic DID (ECU limits vary)" />
          <label class="small muted" for="logSess">Session</label>
          <input class="inp sm mono" id="logSess" placeholder="keep" title="Diagnostic session to enter before defining, e.g. 03 (blank = leave as is)" />
          <span class="pill neutral" id="logMode" hidden>—</span>
          <span class="small muted mono" id="logRate"></span>
        </div>
        <p class="muted small" id="logModeWhy" hidden></p>
        <p class="muted small">
          Polling speed depends on the adapter — an ELM327 is slow, so expect only
          ~1–3 parameters/sec. The dongle's HS/MS-CAN switch must match the module's
          bus. UDS 0x22 DIDs, OBD-II mode-01 PIDs and 0x23 memory reads are driven; Subaru
          SSM needs a K-line cable (kkl, kline-bridge) or a CAN adapter. Other parameters
          are read back with a note.
        </p>
        <div id="logResult" class="result">
          <span class="muted small">Load a definition, select parameters, then press Start.</span>
        </div>
      </div>
    `;
    main.appendChild(sec);
    // Optional GPS source (feat_gps.js): its gps_* rows join the CSV.
    if (window.OTGps) window.OTGps.mount(sec, { id: "logGps", speedError: true });

    // Wire controls. The logger definition comes from a definition pack, or a
    // loose logger.xml / LogDef JSON through the picker's File… button.
    const packPick = window.OTPacks.picker({
      panel: "logging",
      kind: "loggers",
      filter: { name: "Logger definition", extensions: ["xml", "json"] },
      onChange: (sel) => {
        S.defPath = sel.path;
        const box = document.getElementById("logParamsBox");
        if (box && !S.timer) box.hidden = true;
      },
    });
    sec.querySelector("#logPackPick").appendChild(packPick.el);
    sec.querySelector("#logLoad").addEventListener("click", load);
    sec.querySelector("#logSelAll").addEventListener("click", () => setAll(true));
    sec.querySelector("#logSelNone").addEventListener("click", () => setAll(false));
    sec.querySelector("#logStart").addEventListener("click", start);
    sec.querySelector("#logStop").addEventListener("click", () => stop());
    sec.querySelector("#logSaveCsv").addEventListener("click", saveCsv);
    sec.querySelector("#logFast").addEventListener("change", fastOpts);
    ["logTransport", "logSsmSim"].forEach((id) => sec.querySelector("#" + id).addEventListener("change", transportOpts));
    fastOpts();

    // A fast session holds the adapter: stop it when the panel is hidden, the
    // adapter goes away or the window closes (the backend also stops a
    // session that stops ticking).
    new MutationObserver(() => {
      if (S.fast && S.timer && !sec.classList.contains("active")) stop("panel closed");
    }).observe(sec, { attributes: true, attributeFilter: ["class"] });
    if (window.OT && typeof window.OT.onCaps === "function") {
      window.OT.onCaps((st) => { if (S.fast && S.timer && !(st && st.connected)) stop("adapter disconnected"); });
    }
    window.addEventListener("beforeunload", () => { if (S.fast && S.timer) invoke("log_stop").catch(() => {}); });

    // Gate the panel on the transport's capability (a diagnostic link, or a
    // raw K-line for SSM); the SSM simulator needs no adapter.
    if (window.OT && typeof window.OT.gate === "function") {
      gate = window.OT.gate(sec, needs, "live datalogging", {}) || null;
      if (typeof window.OT.onCaps === "function") window.OT.onCaps(() => regate());
    }
    transportOpts();
  }

  // ---- transport / options ----------------------------------------------------
  function opts() {
    return {
      transport: $("logTransport").value || "uds",
      target: $("logSsmTarget").value,
      sim: $("logSsmSim").checked,
      reconnect: $("logReconnect").checked,
      fast: $("logFast").checked,
    };
  }

  function transportOpts() {
    const o = opts();
    const ssm = L.isSsm(o.transport);
    $("logSsmOpts").hidden = !ssm;
    // SSM always batches its reads: Fast (0x2C) is UDS only.
    if (ssm) $("logFast").checked = false;
    $("logFast").disabled = ssm || !!S.timer;
    fastOpts();
    regate();
  }

  // Start (and Load) are allowed when the adapter can do the transport, or
  // the SSM simulator is chosen (offline).
  function regate() {
    const o = opts();
    needs.splice(0, needs.length, ...L.needsFor(o.transport, o.sim));
    if (gate && typeof gate.refresh === "function") gate.refresh();
    const sim = L.isSsm(o.transport) && o.sim;
    const banner = document.querySelector("#panel-logging .cap-banner");
    if (sim && banner) banner.hidden = true;
    const ok = sim || !window.OT || typeof window.OT.has !== "function" || window.OT.has(needs);
    for (const id of ["logLoad", "logStart"]) {
      const b = $(id);
      if (!b) continue;
      b.disabled = !ok || (id === "logStart" && !!S.timer);
      b.classList.toggle("cap-disabled", !ok);
    }
    ["logTransport", "logSsmTarget", "logSsmSim", "logReconnect"].forEach((id) => { if ($(id)) $(id).disabled = !!S.timer; });
  }

  // The link pill: lost / reconnecting attempt k / reconnected (and why).
  function showLink(link, markers) {
    const pill = $("logLink");
    if (!pill) return;
    const b = L.linkBadge(link, markers);
    pill.hidden = !b;
    if (b) { pill.className = "pill " + b.cls; pill.textContent = b.text; pill.title = b.title || ""; }
  }

  // SSM: the module's identification and what was left out, with reasons.
  function showSsm(ssm) {
    const box = $("logSsmInfo");
    const s = L.ssmSummary(ssm);
    box.hidden = !s;
    if (!s) { box.innerHTML = ""; return; }
    const notes = s.notes.length
      ? `<details><summary>Left out (${s.notes.length} reason(s))</summary><ul>${s.notes.map((n) => `<li>${esc(n)}</li>`).join("")}</ul></details>`
      : "";
    box.innerHTML = `<div><b>SSM</b> ${esc(s.head)} · ${esc(s.logged)}</div>` +
      (s.notLogged ? `<div class="muted">${esc(s.notLogged)}</div>` : "") + notes;
  }

  // ---- definition loading ---------------------------------------------------
  async function load() {
    if (!S.defPath) return status("choose a logger definition first", true);
    const box = document.getElementById("logParamsBox");
    const list = document.getElementById("logParams");
    try {
      const res = await invoke("log_params", { defPath: S.defPath });
      S.params = (res && res.params) || [];
      list.innerHTML = "";
      if (!S.params.length) {
        list.innerHTML = '<span class="muted small">No parameters in this definition.</span>';
      } else {
        S.params.forEach((p) => {
          const id = "logp_" + cssId(p.id);
          const row = document.createElement("label");
          row.className = "row";
          row.style.cursor = "pointer";
          row.innerHTML =
            `<input type="checkbox" class="logParamCk" value="${esc(p.id)}" id="${id}" />` +
            `<span>${esc(p.name || p.id)}</span>` +
            (p.units ? ` <span class="chip">${esc(p.units)}</span>` : "") +
            ` <span class="muted small mono">${esc(p.id)}</span>`;
          list.appendChild(row);
        });
      }
      const cnt = document.getElementById("logParamCount");
      if (cnt) cnt.textContent = `${S.params.length} parameter(s)`;
      box.hidden = false;
      status(`loaded ${S.params.length} parameter(s)`);
    } catch (e) {
      box.hidden = true;
      status("load failed: " + msg(e), true);
    }
  }

  function setAll(on) {
    document.querySelectorAll(".logParamCk").forEach((c) => (c.checked = on));
  }

  function selectedIds() {
    return Array.from(document.querySelectorAll(".logParamCk"))
      .filter((c) => c.checked)
      .map((c) => c.value);
  }

  function fastOpts() {
    const on = document.getElementById("logFast").checked;
    ["logDdid", "logMaxRec", "logSess"].forEach((id) => (document.getElementById(id).disabled = !on || !!S.timer));
  }

  function showMode(st, rate) {
    const pill = document.getElementById("logMode");
    const why = document.getElementById("logModeWhy");
    const b = logModeBadge(st);
    pill.hidden = !b;
    if (b) { pill.className = "pill " + b.cls; pill.textContent = b.text; pill.title = b.title; }
    const reason = st && st.mode === "polling" ? st.reason : null;
    why.hidden = !reason && !(st && st.polled && st.polled.length && st.mode === "fast-2c");
    why.textContent = reason ? "Polling: " + reason
      : st && st.polled && st.polled.length ? "Polled alongside the dynamic DIDs: " + st.polled.join(", ") : "";
    document.getElementById("logRate").textContent = rate ? fmtLogRate(rate) : "";
  }

  // ---- live loop -----------------------------------------------------------
  async function start() {
    if (S.timer) return; // already live
    const ids = selectedIds();
    if (!S.defPath) return status("choose a logger.xml first", true);
    if (L.usesSession(opts())) return startFast(ids);

    const intMs = Math.max(50, parseInt(document.getElementById("logInterval").value, 10) || 500);
    S.rows = [];
    S.started = 0;
    buildTable(L.tableParams(null, S.params, ids));
    showSsm(null);
    showLink(null, []);

    document.getElementById("logStart").disabled = true;
    document.getElementById("logStop").disabled = false;
    document.getElementById("logSaveCsv").disabled = true;
    document.getElementById("logInterval").disabled = true;

    let inFlight = false;
    const tick = async () => {
      if (inFlight) return; // skip if the adapter is slower than the interval
      inFlight = true;
      try {
        const res = await invoke("log_poll_once", {
          ...window.OT.connArgs(),
          defPath: S.defPath,
          ids,
          timeoutMs: 1000,
        });
        applySamples(res);
      } catch (e) {
        status("poll error: " + msg(e), true);
        if (window.OTDrive) window.OTDrive.feed.emit({ type: "error", source: "livelog", message: msg(e), busy: /busy/i.test(msg(e)) });
      } finally {
        inFlight = false;
      }
    };
    S.timer = setInterval(tick, intMs);
    regate();
    tick(); // fire immediately, don't wait a full interval
    status(`live @ ${intMs} ms`);
  }

  // A live session on the adapter (Fast, SSM or Reconnect), ticked without
  // overlap.
  async function startFast(ids) {
    const intMs = Math.max(20, parseInt(document.getElementById("logInterval").value, 10) || 500);
    const maxRec = parseInt(document.getElementById("logMaxRec").value, 10);
    const o = opts();
    document.getElementById("logStart").disabled = true;
    status(L.isSsm(o.transport) ? "starting SSM session (BF identification)…"
      : o.fast ? "starting fast session (probing DIDs, defining 0x2C records)…" : "starting live session…");
    let res;
    try {
      res = await invoke("log_start", {
        ...window.OT.connArgs(),
        defPath: S.defPath,
        ids,
        timeoutMs: 1000,
        fast: o.fast,
        dynamicDid: document.getElementById("logDdid").value || null,
        maxRecord: isFinite(maxRec) ? maxRec : null,
        fastSession: document.getElementById("logSess").value || null,
        ...L.startArgs(o),
      });
    } catch (e) {
      regate();
      return status("start failed: " + msg(e), true);
    }
    S.rows = [];
    S.started = 0;
    S.fast = true;
    buildTable(L.tableParams(res.params, S.params, ids));
    showMode(res.status, null);
    showSsm(res.ssm);
    showLink(res.link, []);
    document.getElementById("logStop").disabled = false;
    document.getElementById("logSaveCsv").disabled = true;
    document.getElementById("logInterval").disabled = true;
    document.getElementById("logFast").disabled = true;
    fastOpts();

    let inFlight = false;
    const tick = async () => {
      if (inFlight || !S.timer) return; // never overlap ticks
      inFlight = true;
      try {
        const r = await invoke("log_tick");
        applySamples(r);
        showMode(r.status, r.rate);
        if (r.ended && !r.running) stop(null, r.ended);
      } catch (e) {
        const m = msg(e);
        if (window.OTDrive) window.OTDrive.feed.emit({ type: "error", source: "livelog", message: m, busy: /busy/i.test(m) });
        if (/ended|no live log session/.test(m)) stop(null, m);
        else status("tick error: " + m, true);
      } finally {
        inFlight = false;
      }
    };
    S.timer = setInterval(tick, intMs);
    tick();
    regate();
    status(`live (session) @ ${intMs} ms`);
  }

  async function stop(why, endedMsg) {
    const wasFast = S.fast;
    if (S.timer && window.OTDrive) window.OTDrive.feed.emit({ type: "stop", source: "livelog" });
    if (S.timer) {
      clearInterval(S.timer);
      S.timer = null;
    }
    S.fast = false;
    document.getElementById("logStop").disabled = true;
    document.getElementById("logInterval").disabled = false;
    transportOpts();
    if (wasFast) {
      try {
        const r = await invoke("log_stop");
        if (r && r.status) showMode(r.status, r.rate);
        // Link markers no tick picked up (e.g. the one that ended it).
        const events = (r && r.events) || [];
        const ts = Date.now();
        events.forEach((s) => S.rows.push(liveLogRow(ts, s)));
        if (r && r.link) showLink(r.link, events);
        const ended = endedMsg || (r && r.ended);
        document.getElementById("logSaveCsv").disabled = S.rows.length === 0;
        if (ended) return status(`stopped — ${ended}`, true);
      } catch (e) {
        if (endedMsg) return status(`stopped — ${endedMsg}`, true);
      }
    }
    document.getElementById("logSaveCsv").disabled = S.rows.length === 0;
    status(`stopped${typeof why === "string" && why ? " (" + why + ")" : ""} — ${S.rows.length} row(s) captured`);
  }

  // Build the live table once per Start; cells are then updated in place.
  // `show`: the channels to list ({ id, name, units }).
  function buildTable(show) {
    S.liveCells = {};
    const res = document.getElementById("logResult");
    if (!show.length) {
      res.innerHTML = '<span class="muted small">No parameters selected — nothing to stream.</span>';
      return;
    }
    const tbl = document.createElement("table");
    tbl.className = "mono";
    tbl.style.width = "100%";
    tbl.innerHTML =
      "<thead><tr>" +
      "<th style='text-align:left'>Parameter</th>" +
      "<th style='text-align:right'>Value</th>" +
      "<th style='text-align:left'>Units</th>" +
      "<th style='text-align:left'>Note</th>" +
      "</tr></thead>";
    const tb = document.createElement("tbody");
    show.forEach((p) => {
      const tr = document.createElement("tr");
      const tdName = document.createElement("td");
      tdName.textContent = p.name || p.id;
      const tdVal = document.createElement("td");
      tdVal.style.textAlign = "right";
      tdVal.textContent = "—";
      const tdUnits = document.createElement("td");
      tdUnits.textContent = p.units || "";
      const tdNote = document.createElement("td");
      tdNote.className = "muted small";
      tr.appendChild(tdName);
      tr.appendChild(tdVal);
      tr.appendChild(tdUnits);
      tr.appendChild(tdNote);
      tb.appendChild(tr);
      S.liveCells[p.id] = { value: tdVal, units: tdUnits, note: tdNote };
    });
    tbl.appendChild(tb);
    res.innerHTML = "";
    res.appendChild(tbl);
  }

  // Apply one poll result: update cells in place and accumulate CSV rows.
  function applySamples(res) {
    const samples = (res && res.samples) || [];
    const ts = (res && res.ts) || Date.now();
    S.started += 1;
    samples.forEach((s) => {
      const cell = S.liveCells[s.paramId];
      if (cell) {
        // Text channels (identification DIDs) show their decoded text.
        cell.value.textContent = liveShown(s, fmtNum);
        cell.value.style.textAlign = s.text != null ? "left" : "right";
        cell.value.classList.toggle("live-text", s.text != null);
        if (s.units) cell.units.textContent = s.units;
        cell.note.textContent = s.note || "";
      }
      S.rows.push(liveLogRow(ts, s));
    });
    // GPS card open: hand it the vehicle speed and log its latest channels.
    const gps = window.OTGps && window.OTGps.isOpen() ? window.OTGps.samples() : [];
    if (window.OTGps && window.OTGps.isOpen()) {
      window.OTGps.setVehicleKmh(vehicleSpeedKmh(samples));
      gps.forEach((g) => S.rows.push(liveLogRow(ts, g)));
    }
    // The link: pill, and the Drive view gets a frame only while it answers
    // (an error with the link state otherwise), never a dead link as live.
    const markers = L.linkMarkers(samples);
    showLink(res && res.link, markers);
    if (window.OTDrive) {
      const D = window.OTDrive;
      window.OTDrive.feed.emit(L.driveEvent({ ...res, ts, samples: samples.concat(gps) }, (ch) => D.samplesFrame(ch)));
    }
    const down = L.linkText(res && res.link, markers);
    status(`live — ${S.started} tick(s), ${S.rows.length} row(s)` + (down ? " · " + down : ""), !!down);
  }

  function fmtNum(v) {
    if (typeof v !== "number" || !isFinite(v)) return String(v);
    return Number.isInteger(v) ? String(v) : v.toFixed(3).replace(/\.?0+$/, "");
  }

  // ---- CSV export ----------------------------------------------------------
  function saveCsv() {
    if (!S.rows.length) return status("nothing captured yet", true);
    const csv = liveLogCsv(S.rows);
    const blob = new Blob([csv], { type: "text/csv" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "opentune-log-" + stamp() + ".csv";
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    setTimeout(() => URL.revokeObjectURL(url), 2000);
    status(`saved ${S.rows.length} row(s) to CSV`);
  }

  // ---- small helpers -------------------------------------------------------
  function status(text, bad) {
    const p = document.getElementById("logStatus");
    if (!p) return;
    p.textContent = text;
    p.classList.toggle("bad", !!bad);
  }
  function msg(e) {
    return typeof e === "string" ? e : (e && e.message) || String(e);
  }
  function cssId(s) {
    return String(s).replace(/[^A-Za-z0-9_-]/g, "_");
  }
  function stamp() {
    const d = new Date();
    const p = (n) => String(n).padStart(2, "0");
    return (
      d.getFullYear() +
      p(d.getMonth() + 1) +
      p(d.getDate()) +
      "-" +
      p(d.getHours()) +
      p(d.getMinutes()) +
      p(d.getSeconds())
    );
  }

  // Mount after the DOM (and the app scripts) are ready.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
