// OpenTune — Log viewer + map overlay (frontend). Offline.
//
// Open a CSV datalog, plot channels against time (wheel = zoom at the cursor,
// drag = pan, double-click = reset), read every channel at the cursor, see
// per-channel statistics for the visible window, and overlay the log on a
// calibration map: which cells the engine actually used, for how long, and the
// range of a chosen channel in each cell.
// Backend: log_open, log_series (min/max-decimated per pixel), log_sample_at,
// log_def_maps, log_map_hits, plus the core list_ecudefs.
"use strict";
(function () {
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "—");
  const msg = (e) => (typeof e === "string" ? e : (e && e.message) || String(e));
  const PALETTE = 8; // .lv-c1 … .lv-c8 in styles.css
  const SVGNS = "http://www.w3.org/2000/svg";

  const V = {
    path: null,
    sum: null, // log_open result
    sel: [], // plotted channel names
    color: {}, // name -> 1..8
    full: { t0: 0, t1: 1 },
    view: { t0: 0, t1: 1 },
    data: null, // log_series result
    mode: "lanes",
    seq: 0,
    busy: false,
    again: false,
    drag: null,
    cursorT: null,
    // overlay
    defs: [],
    defPath: null,
    maps: [],
    imagePath: null,
    ovMode: "nearest",
    metric: "hits",
    hits: null,
  };

  // ---- panel ---------------------------------------------------------------
  const sec = document.createElement("section");
  sec.className = "panel wide";
  sec.id = "panel-logview";
  sec.innerHTML = `
    <header class="panel-head">
      <h1>Log viewer</h1>
      <p>Open a CSV datalog, plot it, and overlay it on a calibration map to see which cells the
         engine actually used. Fully offline; nothing is written.</p>
    </header>

    <div class="card">
      <h2>Log</h2>
      <div class="row">
        <button class="btn primary" id="lvOpen" type="button">Open log…</button>
        <span class="muted small mono" id="lvPathHint">no log opened</span>
        <button class="btn sm" id="lvDrive" type="button" disabled title="Play this log back in the Drive view">▶ Drive view</button>
        <label for="lvUnit">Time unit</label>
        <select class="inp sm" id="lvUnit" title="Override the detected time unit">
          <option value="auto">auto</option><option value="s">seconds</option><option value="ms">milliseconds</option>
          <option value="us">microseconds</option><option value="min">minutes</option><option value="clock">hh:mm:ss</option>
          <option value="index">sample index</option>
        </select>
      </div>
      <div id="lvSummary" class="result"></div>
    </div>

    <div class="card" id="lvPlotCard" hidden>
      <div class="card-title-row">
        <h2>Channels</h2>
        <div class="row tight" style="margin:0">
          <div class="seg" role="group" aria-label="Plot layout">
            <button type="button" data-m="lanes">Lanes</button><button type="button" data-m="overlay">Overlay</button>
          </div>
          <button class="btn sm" id="lvZoomIn" type="button" title="Zoom in">+</button>
          <button class="btn sm" id="lvZoomOut" type="button" title="Zoom out">−</button>
          <button class="btn sm" id="lvReset" type="button">Whole log</button>
        </div>
      </div>
      <div class="lv-picker" id="lvPicker"></div>
      <div class="lv-plotwrap" id="lvPlotWrap">
        <svg class="lv-svg" id="lvSvg" role="img" aria-label="Log plot"></svg>
      </div>
      <div class="lv-status small muted" id="lvView"></div>
      <div class="lv-readout" id="lvReadout"><span class="muted small">Move the mouse over the plot to read every channel.</span></div>
    </div>

    <div class="card" id="lvStatsCard" hidden>
      <h2>Statistics <span class="muted small" id="lvStatsScope"></span></h2>
      <div class="lv-tw" id="lvStats"></div>
    </div>

    <div class="card" id="lvOvCard" hidden>
      <h2>Map overlay</h2>
      <p class="small muted" style="margin-top:0">Places every sample on the map's breakpoints. Units are compared,
        never converted: a mismatch is refused unless you accept it.</p>
      <div class="row">
        <label for="lvDef">Definition</label>
        <select class="inp" id="lvDef"></select>
        <button class="btn sm" id="lvDefFile" type="button">Definition file…</button>
        <label for="lvMap">Map</label>
        <select class="inp wide" id="lvMap"></select>
      </div>
      <div class="row">
        <button class="btn sm" id="lvImg" type="button">Image…</button>
        <span class="muted small mono" id="lvImgHint">no image</span>
      </div>
      <div class="row">
        <label for="lvX" id="lvXLbl">X channel</label>
        <select class="inp" id="lvX"></select>
        <label for="lvY" id="lvYLbl">Y channel</label>
        <select class="inp" id="lvY"></select>
        <label for="lvVal">Value range of</label>
        <select class="inp" id="lvVal"></select>
      </div>
      <div class="row">
        <div class="seg" role="group" aria-label="Binning" id="lvOvMode">
          <button type="button" data-m="nearest" title="Whole sample to the nearest cell">Nearest</button><button type="button" data-m="bilinear" title="Split by interpolation weight">Bilinear</button>
        </div>
        <label class="chk"><input type="checkbox" id="lvWindow" /> Visible window only</label>
        <label class="chk"><input type="checkbox" id="lvAccept" /> Accept unit mismatch (values as logged)</label>
        <button class="btn primary" id="lvRun" type="button">Overlay</button>
      </div>
      <div id="lvOvResult" class="result"></div>
    </div>`;
  document.getElementById("main").appendChild(sec);
  window.OTNav.register({ id: "logview", label: "Log viewer", icon: "⌇", group: "calibration", tier: "public" });

  const $ = (id) => sec.querySelector("#" + id);
  const svg = $("lvSvg");

  // ---- formatting ----------------------------------------------------------
  function fmt(v, dp) {
    if (v == null || !isFinite(v)) return "—";
    const a = Math.abs(v);
    if (dp != null) return (+v).toFixed(dp);
    if (a !== 0 && (a >= 1e6 || a < 1e-3)) return (+v).toExponential(2);
    if (Number.isInteger(v)) return String(v);
    return (+v).toFixed(a >= 100 ? 1 : a >= 10 ? 2 : 3);
  }
  const tUnit = () => (V.sum && V.sum.time.label) || "s";
  function fmtT(t) {
    if (t == null) return "—";
    if (tUnit() === "samples") return "#" + Math.round(t);
    return (+t).toFixed(Math.abs(V.view.t1 - V.view.t0) < 10 ? 3 : 2) + " s";
  }
  function heatColor(t) {
    // Same ramp as the Maps panel (OTUtil.heatColor) so both grids read alike.
    const stops = [[43, 108, 176], [56, 161, 105], [214, 158, 46], [221, 107, 32], [197, 48, 48]];
    const x = Math.max(0, Math.min(1, t)) * (stops.length - 1);
    const i = Math.floor(x), f = x - i, a = stops[i], b = stops[Math.min(i + 1, stops.length - 1)];
    return `rgb(${Math.round(a[0] + (b[0] - a[0]) * f)},${Math.round(a[1] + (b[1] - a[1]) * f)},${Math.round(a[2] + (b[2] - a[2]) * f)})`;
  }
  const sw = (name) => `<span class="lv-sw lv-c${V.color[name] || 8}"></span>`;

  // ---- opening ---------------------------------------------------------------
  async function pickLog() {
    try {
      const r = await T().dialog.open({ multiple: false, directory: false, filters: [{ name: "CSV log", extensions: ["csv", "txt", "tsv", "log"] }] });
      const p = Array.isArray(r) ? r[0] : r;
      if (!p) return;
      V.path = p;
      await openLog(true);
    } catch (e) {
      $("lvSummary").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
    }
  }

  async function openLog(fresh) {
    if (!V.path) return;
    $("lvPathHint").textContent = baseName(V.path);
    try {
      const s = await invoke("log_open", { path: V.path, timeUnit: $("lvUnit").value });
      V.sum = s;
      s.channels.forEach((c, i) => (V.color[c.name] = (i % PALETTE) + 1));
      if (fresh || !V.sel.every((n) => s.channels.some((c) => c.name === n))) {
        V.sel = s.channels.slice(0, 3).map((c) => c.name);
      }
      V.full = { t0: s.t0 || 0, t1: s.t1 || 0 };
      if (V.full.t1 <= V.full.t0) V.full.t1 = V.full.t0 + 1;
      V.view = Object.assign({}, V.full);
      renderSummary();
      renderPicker();
      fillChannelSelects();
      $("lvPlotCard").hidden = false;
      $("lvStatsCard").hidden = false;
      $("lvOvCard").hidden = false;
      $("lvDrive").disabled = !window.OTDriveView;
      V.hits = null;
      $("lvOvResult").innerHTML = "";
      if (!V.defs.length) loadDefs();
      fetchSeries();
    } catch (e) {
      $("lvSummary").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
      $("lvPlotCard").hidden = $("lvStatsCard").hidden = $("lvOvCard").hidden = true;
    }
  }

  function renderSummary() {
    const s = V.sum;
    const fmtName = s.format === "opentune_long" ? "OpenTune Live log" : "CSV (one column per channel)";
    const src = { header: "from the header", format: "fixed by the format", override: "chosen", inferred: "inferred — check it", none: "no time column" }[s.time.source] || s.time.source;
    const rate = s.rate_hz ? `${fmt(s.rate_hz)} Hz` : "—";
    $("lvSummary").innerHTML = `
      <div class="meta-grid">
        <div class="k">format</div><div class="v">${esc(fmtName)} · delimiter ${esc(s.delimiter)}${s.decimal_comma ? " · decimal comma" : ""}${s.units_row ? " · units row" : ""}</div>
        <div class="k">samples</div><div class="v">${s.rows} · ${s.channels.length} channel(s)</div>
        <div class="k">time</div><div class="v">${esc(s.time.column || "(sample index)")} · ${esc(s.time.unit)} (${esc(src)})</div>
        <div class="k">duration</div><div class="v">${tUnit() === "samples" ? s.rows + " samples" : fmt(s.duration) + " s"} · ${rate}${s.time.epoch_start ? " · started " + esc(new Date(s.time.epoch_start * 1000).toISOString().replace("T", " ").slice(0, 19)) + " UTC" : ""}</div>
      </div>
      ${(s.notes || []).map((n) => `<div class="note">${esc(n)}</div>`).join("")}`;
  }

  // ---- channel picker ---------------------------------------------------------
  function renderPicker() {
    $("lvPicker").innerHTML = V.sum.channels
      .map((c) => `<label class="chk lv-pick"><input type="checkbox" value="${esc(c.name)}" ${V.sel.includes(c.name) ? "checked" : ""}/>${sw(c.name)}<span>${esc(c.name)}</span>${c.unit ? `<span class="muted">${esc(c.unit)}</span>` : ""}</label>`)
      .join("");
    $("lvPicker").querySelectorAll("input").forEach((ck) =>
      ck.addEventListener("change", () => {
        V.sel = Array.from($("lvPicker").querySelectorAll("input:checked")).map((x) => x.value);
        fetchSeries();
      })
    );
  }

  // ---- series fetch (latest view wins) ------------------------------------------
  async function fetchSeries() {
    if (!V.path) return;
    if (V.busy) { V.again = true; return; }
    V.busy = true;
    const seq = ++V.seq;
    try {
      const w = Math.max(200, Math.round(plotWidth()));
      const r = await invoke("log_series", { path: V.path, channels: V.sel, t0: V.view.t0, t1: V.view.t1, buckets: w });
      if (seq === V.seq) {
        V.data = r;
        draw();
        renderStats(r.stats);
      }
    } catch (e) {
      $("lvView").innerHTML = `<span class="lv-err">${esc(msg(e))}</span>`;
    } finally {
      V.busy = false;
      if (V.again) { V.again = false; fetchSeries(); }
    }
  }
  let fetchTimer = null;
  const fetchSoon = () => { clearTimeout(fetchTimer); fetchTimer = setTimeout(fetchSeries, 40); };

  // ---- plot -----------------------------------------------------------------
  const PAD = { l: 10, r: 10, t: 10, b: 24 };
  const plotWidth = () => $("lvPlotWrap").clientWidth || 800;
  function geom() {
    const W = Math.max(200, plotWidth());
    const n = Math.max(1, V.sel.length);
    const H = V.mode === "lanes" ? Math.max(260, PAD.t + PAD.b + n * 74) : 320;
    return { W, H, pw: W - PAD.l - PAD.r, ph: H - PAD.t - PAD.b };
  }
  const xOf = (t, g) => PAD.l + ((t - V.view.t0) / (V.view.t1 - V.view.t0)) * g.pw;
  const tOf = (x, g) => V.view.t0 + ((x - PAD.l) / g.pw) * (V.view.t1 - V.view.t0);

  function niceStep(span, n) {
    const raw = span / n, p = Math.pow(10, Math.floor(Math.log10(raw))), f = raw / p;
    return (f < 1.5 ? 1 : f < 3.5 ? 2 : f < 7.5 ? 5 : 10) * p;
  }

  function el(name, attrs, text) {
    const e = document.createElementNS(SVGNS, name);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    if (text != null) e.textContent = text;
    return e;
  }

  function draw() {
    const g = geom();
    svg.setAttribute("width", g.W);
    svg.setAttribute("height", g.H);
    svg.setAttribute("viewBox", `0 0 ${g.W} ${g.H}`);
    svg.textContent = "";
    const span = V.view.t1 - V.view.t0;

    // time grid
    const step = niceStep(span, Math.max(2, Math.round(g.pw / 110)));
    for (let t = Math.ceil(V.view.t0 / step) * step; t <= V.view.t1 + 1e-12; t += step) {
      const x = xOf(t, g);
      svg.appendChild(el("line", { class: "lv-grid", x1: x, x2: x, y1: PAD.t, y2: PAD.t + g.ph }));
      const lbl = tUnit() === "samples" ? fmt(Math.round(t)) : fmt(+t.toPrecision(10));
      svg.appendChild(el("text", { class: "lv-tick", x, y: g.H - 7, "text-anchor": "middle" }, lbl));
    }
    svg.appendChild(el("text", { class: "lv-tick", x: g.W - PAD.r, y: g.H - 7, "text-anchor": "end" }, tUnit()));

    const series = (V.data && V.data.series) || [];
    const n = Math.max(1, series.length);
    series.forEach((s, i) => {
      let lo = Infinity, hi = -Infinity;
      for (const v of s.v) if (v != null) { if (v < lo) lo = v; if (v > hi) hi = v; }
      if (!isFinite(lo)) { lo = 0; hi = 1; }
      if (lo === hi) { lo -= 1; hi += 1; }
      const band = V.mode === "lanes" ? { top: PAD.t + (i * g.ph) / n, h: g.ph / n } : { top: PAD.t, h: g.ph };
      const pad = band.h * 0.08;
      const yOf = (v) => band.top + pad + (1 - (v - lo) / (hi - lo)) * (band.h - 2 * pad);
      if (V.mode === "lanes") {
        if (i > 0) svg.appendChild(el("line", { class: "lv-lane", x1: PAD.l, x2: PAD.l + g.pw, y1: band.top, y2: band.top }));
        svg.appendChild(el("text", { class: `lv-lbl lv-f${V.color[s.name] || 8}`, x: PAD.l + 4, y: band.top + 13 }, `${s.name}${s.unit ? " [" + s.unit + "]" : ""}`));
        svg.appendChild(el("text", { class: "lv-tick", x: PAD.l + g.pw - 2, y: band.top + 12, "text-anchor": "end" }, fmt(hi)));
        svg.appendChild(el("text", { class: "lv-tick", x: PAD.l + g.pw - 2, y: band.top + band.h - 4, "text-anchor": "end" }, fmt(lo)));
      }
      let d = "", pen = false;
      for (let k = 0; k < s.t.length; k++) {
        const v = s.v[k];
        if (v == null) { pen = false; continue; }
        d += (pen ? "L" : "M") + xOf(s.t[k], g).toFixed(1) + " " + yOf(v).toFixed(1);
        pen = true;
      }
      if (d) svg.appendChild(el("path", { class: `lv-line lv-s${V.color[s.name] || 8}`, d }));
    });
    if (!series.length) {
      svg.appendChild(el("text", { class: "lv-tick", x: g.W / 2, y: g.H / 2, "text-anchor": "middle" }, "Tick one or more channels to plot them."));
    }
    svg.appendChild(el("rect", { class: "lv-frame", x: PAD.l, y: PAD.t, width: g.pw, height: g.ph }));
    const cur = el("line", { class: "lv-cursor", id: "lvCursor", x1: -10, x2: -10, y1: PAD.t, y2: PAD.t + g.ph });
    svg.appendChild(cur);
    if (V.cursorT != null) placeCursor(V.cursorT);

    const d = V.data;
    $("lvView").textContent = d
      ? `${fmtT(V.view.t0)} … ${fmtT(V.view.t1)} · ${d.points_in_range} sample(s) in view${d.decimated ? ` · drawn as min/max of ${d.buckets} buckets` : ""} · wheel = zoom, drag = pan, double-click = whole log`
      : "";
  }

  function placeCursor(t) {
    const c = svg.querySelector("#lvCursor");
    if (!c) return;
    const g = geom();
    const x = t == null ? -10 : xOf(t, g);
    c.setAttribute("x1", x);
    c.setAttribute("x2", x);
  }

  // ---- zoom / pan -----------------------------------------------------------
  function setView(t0, t1) {
    const full = V.full.t1 - V.full.t0;
    const minSpan = Math.max(full * 1e-6, ((V.sum && V.sum.median_dt) || 0) * 4, 1e-9);
    let span = Math.min(full, Math.max(minSpan, t1 - t0));
    let a = (t0 + t1) / 2 - span / 2;
    a = Math.max(V.full.t0, Math.min(V.full.t1 - span, a));
    V.view = { t0: a, t1: a + span };
    draw();
    fetchSoon();
  }
  function zoomAt(t, f) {
    setView(t - (t - V.view.t0) * f, t + (V.view.t1 - t) * f);
  }
  const svgX = (ev) => ev.clientX - svg.getBoundingClientRect().left;

  svg.addEventListener("wheel", (ev) => {
    if (!V.data) return;
    ev.preventDefault();
    zoomAt(tOf(svgX(ev), geom()), ev.deltaY > 0 ? 1.25 : 0.8);
  }, { passive: false });
  svg.addEventListener("mousedown", (ev) => {
    if (ev.button !== 0 || !V.data) return;
    V.drag = { x: svgX(ev), view: Object.assign({}, V.view) };
    svg.classList.add("dragging");
  });
  window.addEventListener("mousemove", (ev) => {
    if (!V.drag) return;
    const g = geom();
    const dt = ((svgX(ev) - V.drag.x) / g.pw) * (V.drag.view.t1 - V.drag.view.t0);
    setView(V.drag.view.t0 - dt, V.drag.view.t1 - dt);
  });
  window.addEventListener("mouseup", () => {
    if (V.drag) { V.drag = null; svg.classList.remove("dragging"); }
  });
  svg.addEventListener("dblclick", () => setView(V.full.t0, V.full.t1));

  // ---- cursor readout (exact values, one request per animation frame) ---------
  let readPending = false, readT = null;
  svg.addEventListener("mousemove", (ev) => {
    if (!V.data || V.drag) return;
    const g = geom();
    const x = svgX(ev);
    if (x < PAD.l || x > PAD.l + g.pw) return;
    V.cursorT = tOf(x, g);
    placeCursor(V.cursorT);
    readT = V.cursorT;
    if (readPending) return;
    readPending = true;
    requestAnimationFrame(async () => {
      const t = readT;
      try {
        const r = await invoke("log_sample_at", { path: V.path, t });
        renderReadout(r);
      } catch (e) { /* the log may have been replaced meanwhile */ }
      readPending = false;
    });
  });
  svg.addEventListener("mouseleave", () => { V.cursorT = null; placeCursor(null); });

  function renderReadout(r) {
    const rows = r.values
      .slice()
      .sort((a, b) => (V.sel.includes(b.name) ? 1 : 0) - (V.sel.includes(a.name) ? 1 : 0))
      .map((c) => `<div class="lv-ro${V.sel.includes(c.name) ? " on" : ""}">${sw(c.name)}<span class="lv-ro-n">${esc(c.name)}</span><span class="lv-ro-v mono">${fmt(c.value)}</span><span class="muted small">${esc(c.unit)}</span></div>`)
      .join("");
    $("lvReadout").innerHTML = `<div class="lv-ro-t small"><b>${fmtT(r.t)}</b> <span class="muted">sample #${r.index}</span></div><div class="lv-ro-grid">${rows}</div>`;
  }

  // ---- statistics --------------------------------------------------------
  function renderStats(stats) {
    const whole = Math.abs(V.view.t0 - V.full.t0) < 1e-12 && Math.abs(V.view.t1 - V.full.t1) < 1e-12;
    $("lvStatsScope").textContent = whole ? "— whole log" : `— visible window ${fmtT(V.view.t0)} … ${fmtT(V.view.t1)}`;
    const body = (stats || [])
      .map((c) => `<tr><td>${sw(c.name)} ${esc(c.name)}</td><td>${esc(c.unit)}</td><td class="mono num">${fmt(c.min)}</td><td class="mono num">${fmt(c.max)}</td><td class="mono num">${fmt(c.mean)}</td><td class="mono num">${fmt(c.std)}</td><td class="mono num">${c.count}</td><td class="mono num${c.invalid ? " lv-warn" : ""}">${c.invalid}</td><td class="mono num">${c.t_max != null ? fmtT(c.t_max) : "—"}</td></tr>`)
      .join("");
    $("lvStats").innerHTML = `<table class="tbl lv-stats"><thead><tr><th>Channel</th><th>Unit</th><th class="num">Min</th><th class="num">Max</th><th class="num">Mean</th><th class="num">Std dev</th><th class="num">Samples</th><th class="num">Missing</th><th class="num">Max at</th></tr></thead><tbody>${body}</tbody></table>`;
  }

  // ---- map overlay --------------------------------------------------------
  async function loadDefs() {
    try {
      V.defs = (await invoke("list_ecudefs")) || [];
    } catch (e) {
      V.defs = [];
    }
    fillDefSelect();
  }
  function fillDefSelect() {
    const sel = $("lvDef");
    const opts = ['<option value="">(choose)</option>'];
    if (V.defPath) opts.push(`<option value="__file__">File: ${esc(baseName(V.defPath))}</option>`);
    V.defs.filter((d) => d.maps > 0).forEach((d) => opts.push(`<option value="${esc(d.id)}">${esc(d.name)} (${d.maps} map${d.maps === 1 ? "" : "s"})</option>`));
    const keep = sel.value;
    sel.innerHTML = opts.join("");
    if (keep && Array.from(sel.options).some((o) => o.value === keep)) sel.value = keep;
  }
  function defArgs() {
    const v = $("lvDef").value;
    return v === "__file__" ? { ecudefId: null, ecudefPath: V.defPath } : { ecudefId: v || null, ecudefPath: null };
  }
  async function loadMaps() {
    const a = defArgs();
    V.maps = [];
    $("lvMap").innerHTML = "";
    if (!a.ecudefId && !a.ecudefPath) return;
    try {
      const r = await invoke("log_def_maps", a);
      V.maps = (r.maps || []).filter((m) => m.rows * m.cols > 1);
      $("lvMap").innerHTML = V.maps.length
        ? V.maps.map((m, i) => `<option value="${i}">${esc(m.name)} — ${m.rows}×${m.cols}</option>`).join("")
        : '<option value="">(no maps with axes)</option>';
      onMap();
    } catch (e) {
      $("lvOvResult").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
    }
  }
  const curMap = () => V.maps[+$("lvMap").value];

  // Unit spellings of the same unit (display hint only; the backend decides).
  const UNIT = { "1/min": "rpm", "u/min": "rpm", "min-1": "rpm", "mg/hub": "mg/stroke", "mg/str": "mg/stroke", mbar: "hpa", "°c": "degc" };
  const normU = (u) => { const s = String(u || "").toLowerCase().replace(/\s+/g, "").replace(/^[[(]|[\])]$/g, ""); return UNIT[s] || s; };
  function guessChannel(axis) {
    if (!V.sum) return "";
    const words = String(axis.name || "").toLowerCase().split(/[^a-z0-9]+/).filter((w) => w.length > 2);
    let best = "", score = 0;
    V.sum.channels.forEach((c) => {
      let s = 0;
      if (axis.unit && normU(c.unit) === normU(axis.unit)) s += 2;
      const n = c.name.toLowerCase();
      words.forEach((w) => { if (n.includes(w)) s += 1; });
      if (s > score) { score = s; best = c.name; }
    });
    return best;
  }
  function axisText(a) {
    return `${a.name || "axis"}${a.unit ? ", " + a.unit : ""}`;
  }
  function onMap() {
    const m = curMap();
    if (!m) return;
    $("lvXLbl").textContent = `X channel (${axisText(m.x_axis)})`;
    $("lvYLbl").textContent = m.rows > 1 ? `Y channel (${axisText(m.y_axis)})` : "Y channel (not used: 1-row curve)";
    $("lvY").disabled = m.rows <= 1;
    const gx = guessChannel(m.x_axis), gy = m.rows > 1 ? guessChannel(m.y_axis) : "";
    if (gx) $("lvX").value = gx;
    if (gy) $("lvY").value = gy;
    updateImgHint();
  }
  function fillChannelSelects() {
    const opts = V.sum.channels.map((c) => `<option value="${esc(c.name)}">${esc(c.name)}${c.unit ? " [" + esc(c.unit) + "]" : ""}</option>`).join("");
    ["lvX", "lvY"].forEach((id) => { const k = $(id).value; $(id).innerHTML = opts; if (k) $(id).value = k; });
    const k = $("lvVal").value;
    $("lvVal").innerHTML = '<option value="">(none)</option>' + opts;
    if (k) $("lvVal").value = k;
    onMap();
  }
  function imagePath() {
    if (V.imagePath) return V.imagePath;
    // Fall back to the image opened in the Image panel (app-core.js global state).
    try { if (typeof state !== "undefined" && state && state.imagePath) return state.imagePath; } catch (e) {}
    return null;
  }
  function updateImgHint() {
    const p = imagePath(), m = curMap();
    const need = m && m.needs_image;
    $("lvImgHint").innerHTML = p
      ? esc(baseName(p)) + (V.imagePath ? "" : ' <span class="muted">(from Image panel)</span>')
      : need ? '<span class="lv-warn">this map\'s axes are stored in the image — choose the .bin it belongs to</span>' : "no image (axes are inline; map values not shown)";
  }

  async function runOverlay() {
    const m = curMap();
    if (!m) return ovError("Choose a definition and a map.");
    const win = $("lvWindow").checked;
    try {
      $("lvRun").disabled = true;
      const r = await invoke("log_map_hits", {
        path: V.path,
        xChannel: $("lvX").value,
        yChannel: m.rows > 1 ? $("lvY").value : null,
        ...defArgs(),
        mapName: m.name,
        imagePath: imagePath(),
        mode: V.ovMode,
        valueChannel: $("lvVal").value || null,
        acceptUnits: $("lvAccept").checked,
        t0: win ? V.view.t0 : null,
        t1: win ? V.view.t1 : null,
      });
      V.hits = r;
      if (V.metric === "value" && !r.values) V.metric = "hits";
      renderOverlay();
    } catch (e) {
      ovError(msg(e));
    } finally {
      $("lvRun").disabled = false;
    }
  }
  function ovError(t) {
    V.hits = null;
    $("lvOvResult").innerHTML = `<div class="banner bad">${esc(t)}</div>`;
  }

  function cellMetric(h, r, c) {
    if (V.metric === "time") return h.time[r][c];
    if (V.metric === "value") return h.values ? h.values.mean[r][c] : null;
    return h.hits[r][c];
  }

  function renderOverlay() {
    const h = V.hits;
    if (!h) return;
    const mp = h.map, xs = mp.x_axis.values || [], ys = mp.y_axis.values || [];
    const cells = mp.cells;
    const showMap = $("lvShowMap") ? $("lvShowMap").checked : !!cells;
    const o = h.outside;
    const u = h.units;
    const uLine = (k, x) => (x ? `<span class="lv-u ${x.check}">${k}: ${esc(x.axis || "axis")} ← ${esc(x.channel)} (${esc(x.channel_unit || "no unit")} / ${esc(x.axis_unit || "no unit")}: ${x.check === "match" ? "units match" : x.check === "mismatch" ? "MISMATCH, not converted" : "unit unknown"})</span>` : "");

    // metric range
    let lo = Infinity, hi = -Infinity;
    for (let r = 0; r < h.rows; r++) for (let c = 0; c < h.cols; c++) {
      const v = cellMetric(h, r, c);
      if (v != null && (V.metric === "value" || v > 0)) { lo = Math.min(lo, v); hi = Math.max(hi, v); }
    }
    if (V.metric !== "value") lo = 0;
    if (!isFinite(hi)) { lo = 0; hi = 1; }
    const span = hi - lo || 1;
    const metricFmt = (v) => (v == null ? "" : V.metric === "hits" ? (h.mode === "bilinear" ? fmt(v, v < 10 ? 1 : 0) : String(Math.round(v))) : V.metric === "time" ? fmt(v, 1) : fmt(v));

    let thead = `<tr><th>${esc(mp.y_axis.name || "")}\\${esc(mp.x_axis.name || "")}</th>`;
    for (let c = 0; c < h.cols; c++) thead += `<th>${xs[c] != null ? fmt(xs[c]) : c}</th>`;
    thead += "</tr>";
    let body = "";
    for (let r = 0; r < h.rows; r++) {
      body += `<tr><th>${h.rows > 1 && ys[r] != null ? fmt(ys[r]) : ""}</th>`;
      for (let c = 0; c < h.cols; c++) {
        const v = cellMetric(h, r, c);
        const used = h.hits[r][c] > 0;
        const mv = cells && cells[r] ? cells[r][c] : null;
        const tip = [
          `[${r},${c}]`,
          `hits ${fmt(h.hits[r][c])}`,
          `time ${fmt(h.time[r][c], 2)} ${h.time_label}`,
          h.values && h.values.mean[r][c] != null ? `${h.value_channel.name} ${fmt(h.values.min[r][c])} … ${fmt(h.values.max[r][c])} (mean ${fmt(h.values.mean[r][c])}) ${h.value_channel.unit}` : null,
          mv != null ? `map ${fmt(mv)} ${mp.unit || ""}` : null,
        ].filter(Boolean).join(" · ");
        const text = showMap && mv != null ? fmt(mv) : used ? metricFmt(v) : "";
        if (!used || v == null) body += `<td class="cell lv-zero" title="${esc(tip)}">${text}</td>`;
        else body += `<td class="cell" style="background:${heatColor((v - lo) / span)}" title="${esc(tip)}">${text}</td>`;
      }
      body += "</tr>";
    }
    const metricName = { hits: h.mode === "bilinear" ? "hits (bilinear weight)" : "hits", time: `time in cell (${h.time_label})`, value: h.value_channel ? `mean ${h.value_channel.name} (${h.value_channel.unit || "as logged"})` : "value" }[V.metric];
    $("lvOvResult").innerHTML = `
      ${(h.warnings || []).map((w) => `<div class="banner info">${esc(w)}</div>`).join("")}
      ${mp.cells_error ? `<div class="note">Map values not shown: ${esc(mp.cells_error)}</div>` : ""}
      <div class="lv-ovsum small">
        <b>${h.samples_used}</b> of ${h.samples_total} sample(s) placed · ${h.samples_skipped} skipped (missing X/Y) ·
        <span class="${o.any ? "lv-warn" : ""}">${o.any} outside the axes</span>${o.any ? ` (X low ${o.x_low}, X high ${o.x_high}, Y low ${o.y_low}, Y high ${o.y_high}; counted in the edge cell, as the ECU clamps)` : ""}
      </div>
      <div class="lv-ovsum small">${uLine("X", u.x)} ${uLine("Y", u.y)}</div>
      <div class="row" style="margin-top:10px">
        <div class="seg" role="group" aria-label="Colour by" id="lvMetric">
          <button type="button" data-m="hits">Hits</button><button type="button" data-m="time">Time</button>${h.values ? '<button type="button" data-m="value">Value</button>' : ""}
        </div>
        ${cells ? `<label class="chk"><input type="checkbox" id="lvShowMap" ${showMap ? "checked" : ""}/> Show map values (${esc(mp.unit || "raw")})</label>` : ""}
        <button class="btn sm" id="lvOpenMaps" type="button" title="Switch to Maps with this definition and map selected (and this image, decoded)">Open in Maps</button>
      </div>
      <div class="heatwrap"><table class="heat lv-heat"><thead>${thead}</thead><tbody>${body}</tbody></table></div>
      <div class="legend"><span>${fmt(lo)}</span><span class="bar"></span><span>${fmt(hi)}</span> &nbsp; <span>${esc(metricName)} · empty cells were never used</span></div>`;
    segOn($("lvMetric"), V.metric);
    $("lvMetric").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => { V.metric = b.dataset.m; renderOverlay(); }));
    if ($("lvShowMap")) $("lvShowMap").addEventListener("change", renderOverlay);
    $("lvOpenMaps").addEventListener("click", openInMapsPanel);
  }

  // "Open in Maps": the same definition + map (and image) in the Maps panel.
  async function openInMapsPanel() {
    const t = window.OTUtil.mapsTarget($("lvDef").value, curMap(), imagePath());
    if (t.error) return toastMsg(t.error, true);
    if (typeof window.openInMaps !== "function") return toastMsg("the Maps panel is not available", true);
    try {
      await window.openInMaps(t);
    } catch (e) {
      toastMsg("Open in Maps: " + msg(e), true);
    }
  }
  function toastMsg(t, bad) {
    if (typeof window.toast === "function") window.toast(t, bad);
    else ovError(t);
  }

  // ---- wiring -------------------------------------------------------------
  function segOn(seg, v) {
    if (seg) seg.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.dataset.m === v));
  }
  $("lvOpen").addEventListener("click", pickLog);
  $("lvDrive").addEventListener("click", () => { if (V.path && window.OTDriveView) window.OTDriveView.openLog(V.path); });
  $("lvUnit").addEventListener("change", () => openLog(false));
  const layoutSeg = sec.querySelector('#lvPlotCard .seg');
  segOn(layoutSeg, V.mode);
  layoutSeg.querySelectorAll("button").forEach((b) => b.addEventListener("click", () => { V.mode = b.dataset.m; segOn(layoutSeg, V.mode); draw(); }));
  $("lvZoomIn").addEventListener("click", () => zoomAt((V.view.t0 + V.view.t1) / 2, 0.5));
  $("lvZoomOut").addEventListener("click", () => zoomAt((V.view.t0 + V.view.t1) / 2, 2));
  $("lvReset").addEventListener("click", () => setView(V.full.t0, V.full.t1));
  $("lvDef").addEventListener("change", loadMaps);
  $("lvMap").addEventListener("change", onMap);
  $("lvDefFile").addEventListener("click", async () => {
    const r = await T().dialog.open({ multiple: false, directory: false, filters: [{ name: "Definition", extensions: ["json"] }] });
    const p = Array.isArray(r) ? r[0] : r;
    if (!p) return;
    V.defPath = p;
    fillDefSelect();
    $("lvDef").value = "__file__";
    loadMaps();
  });
  $("lvImg").addEventListener("click", async () => {
    const r = await T().dialog.open({ multiple: false, directory: false, filters: [{ name: "Flash image", extensions: ["bin", "BIN", "ori", "mod"] }, { name: "All files", extensions: ["*"] }] });
    const p = Array.isArray(r) ? r[0] : r;
    if (!p) return;
    V.imagePath = p;
    updateImgHint();
  });
  segOn($("lvOvMode"), V.ovMode);
  $("lvOvMode").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => { V.ovMode = b.dataset.m; segOn($("lvOvMode"), V.ovMode); if (V.hits) runOverlay(); }));
  $("lvRun").addEventListener("click", runOverlay);

  // Redraw at the new width when the panel is shown or the window resizes.
  if (window.ResizeObserver) {
    let lastW = 0;
    new ResizeObserver(() => {
      const w = plotWidth();
      if (V.data && w && Math.abs(w - lastW) > 2) { lastW = w; draw(); fetchSoon(); }
    }).observe($("lvPlotWrap"));
  }
})();
