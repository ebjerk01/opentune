// OpenTune — Live datalogging panel (frontend).
//
// Self-contained: injects its own nav item + panel into the existing shell,
// reuses the global adapter connection (window.OT) and the app's CSS classes.
// The live loop runs here: on Start a setInterval calls the stateless backend
// command `log_poll_once` every N ms, each tick opening+reading+closing the
// adapter, and the table updates in place. Rows are accumulated for CSV export.
"use strict";
(function () {
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"]/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c])
    );
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "—");

  // ---- panel state ---------------------------------------------------------
  const S = {
    defPath: null,
    params: [], // [{id,name,units}]
    timer: null, // setInterval handle while live
    rows: [], // accumulated samples for CSV: {ts,paramId,name,raw,value,units,note}
    liveCells: {}, // paramId -> { value, units, note } <td> elements (in-place update)
    started: 0, // ticks since Start (for the status line)
  };

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
        <div class="row">
          <button class="btn" id="logPick">Choose logger.xml…</button>
          <span class="muted small" id="logPathHint">no definition loaded</span>
          <button class="btn primary" id="logLoad">Load</button>
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

    // Wire controls.
    sec.querySelector("#logPick").addEventListener("click", pick);
    sec.querySelector("#logLoad").addEventListener("click", load);
    sec.querySelector("#logSelAll").addEventListener("click", () => setAll(true));
    sec.querySelector("#logSelNone").addEventListener("click", () => setAll(false));
    sec.querySelector("#logStart").addEventListener("click", start);
    sec.querySelector("#logStop").addEventListener("click", stop);
    sec.querySelector("#logSaveCsv").addEventListener("click", saveCsv);

    // Gate the panel on CAN capability (live datalogging needs a bus adapter).
    if (window.OT && typeof window.OT.gate === "function") {
      window.OT.gate(sec, ["can"], "live datalogging", { disable: ["#logLoad", "#logStart"] });
    }
  }

  // ---- definition picking + loading ---------------------------------------
  async function pick() {
    try {
      const r = await T().dialog.open({
        multiple: false,
        directory: false,
        filters: [{ name: "logger.xml", extensions: ["xml"] }],
      });
      const path = Array.isArray(r) ? r[0] : r;
      if (!path) return;
      S.defPath = path;
      const hint = document.getElementById("logPathHint");
      if (hint) hint.textContent = baseName(path);
    } catch (e) {
      status("pick failed: " + msg(e), true);
    }
  }

  async function load() {
    if (!S.defPath) return status("choose a logger.xml first", true);
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

  // ---- live loop -----------------------------------------------------------
  function start() {
    if (S.timer) return; // already live
    const ids = selectedIds();
    if (!S.defPath) return status("choose a logger.xml first", true);

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

  function stop() {
    if (S.timer) {
      clearInterval(S.timer);
      S.timer = null;
    }
    document.getElementById("logStart").disabled = false;
    document.getElementById("logStop").disabled = true;
    document.getElementById("logSaveCsv").disabled = S.rows.length === 0;
    document.getElementById("logInterval").disabled = false;
    status(`stopped — ${S.rows.length} row(s) captured`);
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
        let shown = "—";
        if (s.value != null) shown = fmtNum(s.value);
        else if (s.raw != null) shown = String(s.raw);
        cell.value.textContent = shown;
        if (s.units) cell.units.textContent = s.units;
        cell.note.textContent = s.note || "";
      }
      S.rows.push({
        ts,
        paramId: s.paramId,
        name: s.name,
        raw: s.raw == null ? "" : s.raw,
        value: s.value == null ? "" : s.value,
        units: s.units || "",
        note: s.note || "",
      });
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
    const head = ["ts", "param_id", "name", "raw", "value", "units", "note"];
    const q = (v) => {
      const s = String(v == null ? "" : v);
      return /[",\n]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
    };
    const lines = [head.join(",")];
    S.rows.forEach((r) => {
      lines.push([r.ts, r.paramId, r.name, r.raw, r.value, r.units, r.note].map(q).join(","));
    });
    const csv = lines.join("\n") + "\n";
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

  // Mount after the DOM (and app.js) are ready.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", mount);
  } else {
    mount();
  }
})();
