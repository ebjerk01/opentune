// OpenTune — Live datalogging panel (frontend).
//
// Self-contained: injects its own nav item + panel into the existing shell,
// reuses the global adapter connection (window.OT) and the app's CSS classes.
// The live loop runs here: on Start a setInterval calls the stateless backend
// command `log_poll_once` every N ms (the hub reuses the opened adapter), and
// the table updates in place. Rows are accumulated for CSV export.
//
// Fast (0x2C) toggle: Start opens a live session instead (`log_start`), which
// packs the channels into dynamically defined DIDs where the ECU allows it and
// otherwise polls (the badge shows which, and why). Ticks go to `log_tick`
// (never overlapping) and Stop calls `log_stop`, which clears the DIDs. The
// session holds the adapter, so it is stopped when the panel is hidden, the
// adapter disappears or the window closes.
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
  const { logModeBadge, fmtLogRate, liveLogRow, liveLogCsv, liveShown } = window.OTUtil;

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
          bus. Only UDS (service 0x22 / DID) parameters are driven today; others are
          read back with a note.
        </p>
        <div id="logResult" class="result">
          <span class="muted small">Load a definition, select parameters, then press Start.</span>
        </div>
      </div>
    `;
    main.appendChild(sec);

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

    // Gate the panel on CAN capability (live datalogging needs a bus adapter).
    if (window.OT && typeof window.OT.gate === "function") {
      window.OT.gate(sec, ["diag"], "live datalogging", { disable: ["#logLoad", "#logStart"] });
    }
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
    if (document.getElementById("logFast").checked) return startFast(ids);

    const intMs = Math.max(50, parseInt(document.getElementById("logInterval").value, 10) || 500);
    S.rows = [];
    S.started = 0;
    buildTable(ids);

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
      } finally {
        inFlight = false;
      }
    };
    S.timer = setInterval(tick, intMs);
    tick(); // fire immediately, don't wait a full interval
    status(`live @ ${intMs} ms`);
  }

  // Fast mode: one live session on the adapter, ticked without overlap.
  async function startFast(ids) {
    const intMs = Math.max(20, parseInt(document.getElementById("logInterval").value, 10) || 500);
    const maxRec = parseInt(document.getElementById("logMaxRec").value, 10);
    document.getElementById("logStart").disabled = true;
    status("starting fast session (probing DIDs, defining 0x2C records)…");
    let res;
    try {
      res = await invoke("log_start", {
        ...window.OT.connArgs(),
        defPath: S.defPath,
        ids,
        timeoutMs: 1000,
        fast: true,
        dynamicDid: document.getElementById("logDdid").value || null,
        maxRecord: isFinite(maxRec) ? maxRec : null,
        fastSession: document.getElementById("logSess").value || null,
      });
    } catch (e) {
      document.getElementById("logStart").disabled = false;
      return status("start failed: " + msg(e), true);
    }
    S.rows = [];
    S.started = 0;
    S.fast = true;
    buildTable(ids);
    showMode(res.status, null);
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
      } catch (e) {
        const m = msg(e);
        if (/ended|no live log session/.test(m)) stop(null, m);
        else status("tick error: " + m, true);
      } finally {
        inFlight = false;
      }
    };
    S.timer = setInterval(tick, intMs);
    tick();
    status(`live (session) @ ${intMs} ms`);
  }

  async function stop(why, endedMsg) {
    const wasFast = S.fast;
    if (S.timer) {
      clearInterval(S.timer);
      S.timer = null;
    }
    S.fast = false;
    document.getElementById("logStart").disabled = false;
    document.getElementById("logStop").disabled = true;
    document.getElementById("logSaveCsv").disabled = S.rows.length === 0;
    document.getElementById("logInterval").disabled = false;
    document.getElementById("logFast").disabled = false;
    fastOpts();
    if (wasFast) {
      try {
        const r = await invoke("log_stop");
        if (r && r.status) showMode(r.status, r.rate);
        const ended = endedMsg || (r && r.ended);
        if (ended) return status(`stopped — ${ended}`, true);
      } catch (e) {
        if (endedMsg) return status(`stopped — ${endedMsg}`, true);
      }
    }
    status(`stopped${typeof why === "string" && why ? " (" + why + ")" : ""} — ${S.rows.length} row(s) captured`);
  }

  // Build the live table once per Start; cells are then updated in place.
  function buildTable(ids) {
    S.liveCells = {};
    const res = document.getElementById("logResult");
    const show = ids.length ? S.params.filter((p) => ids.includes(p.id)) : S.params;
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
    status(`live — ${S.started} tick(s), ${S.rows.length} row(s)`);
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
