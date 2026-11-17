// OpenTune — live Dashboard (OBD-II mode 01).
// Two hero gauges (RPM, boost) and a grid of value tiles with sparklines and
// warn/critical colouring. Only PIDs the ECU reports as supported are polled.
// Backend: obd_pid_table (offline), obd_supported, obd_read. Shares the global
// adapter connection (window.OT) and the app-wide adapter lock.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const store = {
    get(k, d) { try { const v = localStorage.getItem("ot.dash." + k); return v == null ? d : JSON.parse(v); } catch (e) { return d; } },
    set(k, v) { try { localStorage.setItem("ot.dash." + k, JSON.stringify(v)); } catch (e) {} },
  };

  const PRESETS = {
    diesel: ["engine_load", "coolant_temp", "intake_air_temp", "intake_map", "maf", "fuel_rail_pressure", "egt_b1", "accel_pedal_d", "speed", "module_voltage", "oil_temp", "commanded_egr"],
    petrol: ["engine_load", "coolant_temp", "intake_air_temp", "intake_map", "throttle_pos", "timing_advance", "stft_b1", "ltft_b1", "maf", "speed", "module_voltage", "oil_temp"],
  };
  const REDLINE = { diesel: [5000, 4500], petrol: [7000, 6400] };
  const BOOST = { id: "boost_kpa", key: "boost_kpa", name: "Boost", unit: "kPa", min: -20, max: 250, warn: null, crit: null, group: "air", derived: true, needs: ["intake_map", "baro_pressure"] };
  const HIST = 90;

  let table = [];
  let byKey = {};
  let supported = null; // Set of keys the ECU reports, null before Start
  let timer = null;
  let inFlight = false;
  let preset = store.get("preset", "diesel");
  let custom = store.get("custom", null);
  let pUnit = store.get("punit", "bar");
  const hist = {};
  const mm = {};
  const sensors = {}; // key -> sorted sensor indexes that have reported a value (EGT)

  // ---- units ---------------------------------------------------------------
  function shown(key, v) {
    const t = byKey[key] || {};
    if (v == null || Number.isNaN(v)) return { v: null, unit: t.unit || "" };
    if (t.unit === "kPa" && pUnit === "bar") return { v: v / 100, unit: "bar", dp: 2 };
    const dp = t.unit === "V" || t.unit === "λ" ? 2 : t.unit === "g/s" || t.unit === "%" ? 1 : 0;
    return { v, unit: t.unit || "", dp };
  }
  const fmt = (s) => (s.v == null ? "—" : s.v.toFixed(s.dp || 0));
  function level(key, v) {
    const t = byKey[key] || {};
    if (v == null) return "";
    if (t.crit != null && v >= t.crit) return "crit";
    if (t.warn != null && v >= t.warn) return "warn";
    return "";
  }

  // ---- selection -----------------------------------------------------------
  function selection() {
    let keys = preset === "custom" && custom ? custom : preset === "all" ? table.map((t) => t.key) : PRESETS[preset] || PRESETS.diesel;
    keys = keys.filter((k) => k !== "rpm" && k !== "boost_kpa" && byKey[k]);
    return keys;
  }
  function pidsFor(keys) {
    const need = new Set(["rpm", ...keys]);
    if (byKey.boost_kpa) BOOST.needs.forEach((k) => need.add(k));
    const pids = new Set();
    need.forEach((k) => { const t = byKey[k]; if (t && t.pid != null && (!supported || supported.has(k))) pids.add(t.pid); });
    return Array.from(pids).sort((a, b) => a - b);
  }

  // ---- gauge ---------------------------------------------------------------
  const A0 = 150, SWEEP = 240, R = 78, C = 100;
  function pt(deg) { const r = (deg * Math.PI) / 180; return [C + R * Math.cos(r), C + R * Math.sin(r)]; }
  function arc(f0, f1) {
    const a = A0 + SWEEP * Math.max(0, Math.min(1, f0)), b = A0 + SWEEP * Math.max(0, Math.min(1, f1));
    if (b - a < 0.01) return "";
    const [x0, y0] = pt(a), [x1, y1] = pt(b);
    return `M${x0.toFixed(2)} ${y0.toFixed(2)} A${R} ${R} 0 ${b - a > 180 ? 1 : 0} 1 ${x1.toFixed(2)} ${y1.toFixed(2)}`;
  }
  function gaugeSvg(id, lo, hi, red, ticks, tickFmt) {
    const tk = ticks.map((v) => {
      const f = (v - lo) / (hi - lo), [x, y] = pt(A0 + SWEEP * f);
      const [lx, ly] = [C + (x - C) * 1.24, C + (y - C) * 1.24];
      return `<text class="g-tick" x="${lx.toFixed(1)}" y="${(ly + 3).toFixed(1)}" text-anchor="middle">${tickFmt(v)}</text>`;
    }).join("");
    const redArc = red != null ? `<path class="g-red" d="${arc((red - lo) / (hi - lo), 1)}"/>` : "";
    return `<svg viewBox="0 0 200 172" class="g-svg" role="img" aria-labelledby="${id}Lbl">
      <path class="g-track" d="${arc(0, 1)}"/>${redArc}
      <path class="g-val" id="${id}Arc" d=""/>${tk}
      <text class="g-num" id="${id}Num" x="100" y="108" text-anchor="middle">—</text>
      <text class="g-unit" id="${id}Unit" x="100" y="128" text-anchor="middle"></text>
    </svg>`;
  }

  // ---- build ---------------------------------------------------------------
  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-dashboard";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Dashboard</h1>
      <p>Live engine values over OBD-II. Pick a preset or your own set; only values this ECU reports are polled.</p>
    </header>
    <div class="card dash-bar">
      <div class="row">
        <button class="btn primary" id="dashStart" type="button">Start</button>
        <button class="btn" id="dashStop" type="button" disabled>Stop</button>
        <label class="small muted" for="dashPreset">Values</label>
        <select class="inp" id="dashPreset">
          <option value="diesel">Diesel set</option>
          <option value="petrol">Petrol set</option>
          <option value="all">Everything supported</option>
          <option value="custom">My selection</option>
        </select>
        <label class="small muted" for="dashRate">Every</label>
        <select class="inp sm" id="dashRate"><option>250</option><option selected>500</option><option>1000</option></select>
        <span class="small muted">ms</span>
        <div class="seg" role="group" aria-label="Pressure unit">
          <button type="button" data-u="bar">bar</button><button type="button" data-u="kPa">kPa</button>
        </div>
        <button class="btn sm" id="dashPick" type="button">Choose values…</button>
        <span class="pill neutral" id="dashStatus">idle</span>
      </div>
      <div id="dashPicker" class="pid-picker" hidden></div>
    </div>
    <div class="dash-hero">
      <div class="gauge" id="gRpmBox"><div class="g-label" id="gRpmLbl">Engine speed</div><div id="gRpm"></div></div>
      <div class="gauge" id="gBoostBox"><div class="g-label" id="gBoostLbl">Boost</div><div id="gBoost"></div></div>
    </div>
    <div class="dash-grid" id="dashGrid"></div>
    <p class="small muted" id="dashMissing"></p>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "dashboard", label: "Dashboard", icon: "◔", group: "vehicle", tier: "bench" });

  const $ = (id) => panel.querySelector("#" + id);
  const status = (cls, text) => { const p = $("dashStatus"); p.className = "pill " + cls; p.textContent = text; };

  function drawGauges() {
    const [max, red] = REDLINE[preset === "petrol" ? "petrol" : "diesel"];
    const step = max / 5;
    $("gRpm").innerHTML = gaugeSvg("gR", 0, max, red, [0, step, 2 * step, 3 * step, 4 * step, max], (v) => (v / 1000).toFixed(0) + "k");
    const bar = pUnit === "bar";
    $("gBoost").innerHTML = gaugeSvg("gB", bar ? -0.2 : -20, bar ? 2.5 : 250, null, bar ? [0, 0.5, 1, 1.5, 2, 2.5] : [0, 50, 100, 150, 200, 250], (v) => (bar ? v.toFixed(1) : v));
    setGauge("gR", hist.rpm ? hist.rpm[hist.rpm.length - 1] : null, 0, max, "rpm", 0, red);
    const b = hist.boost_kpa ? hist.boost_kpa[hist.boost_kpa.length - 1] : null;
    setGauge("gB", b == null ? null : bar ? b / 100 : b, bar ? -0.2 : -20, bar ? 2.5 : 250, bar ? "bar" : "kPa", bar ? 2 : 0, null);
  }
  function setGauge(id, v, lo, hi, unit, dp, red) {
    const a = panel.querySelector("#" + id + "Arc");
    if (!a) return;
    const f = v == null ? 0 : (v - lo) / (hi - lo);
    const z = lo < 0 ? (0 - lo) / (hi - lo) : 0; // arcs start at zero, so vacuum draws backwards
    a.setAttribute("d", v == null ? "" : arc(Math.min(z, f), Math.max(z, f)));
    a.classList.toggle("hot", red != null && v != null && v >= red);
    panel.querySelector("#" + id + "Num").textContent = v == null ? "—" : v.toFixed(dp);
    panel.querySelector("#" + id + "Unit").textContent = unit;
  }

  function tileKeys() {
    const out = [];
    selection().forEach((k) => {
      if (sensors[k] && sensors[k].length) sensors[k].forEach((i) => out.push(k + "#" + i));
      else out.push(k);
    });
    return out;
  }
  function tileName(tk) {
    const [k, i] = tk.split("#");
    const t = byKey[k];
    return i == null ? t.name : `${t.name} S${Number(i) + 1}`;
  }

  function drawTiles() {
    const grid = $("dashGrid");
    grid.innerHTML = "";
    tileKeys().forEach((tk) => {
      const k = tk.split("#")[0];
      const off = supported && !supported.has(k);
      const d = document.createElement("div");
      d.className = "tile" + (off ? " na" : "");
      d.dataset.k = tk;
      d.innerHTML = `
        <div class="t-label"><span>${esc(tileName(tk))}</span><i class="t-dot"></i></div>
        <div><span class="t-val">—</span><span class="t-unit">${esc(shown(k, 0).unit)}</span></div>
        <svg class="spark" viewBox="0 0 100 30" preserveAspectRatio="none" aria-hidden="true"><path class="area" d=""/><path class="line" d=""/></svg>
        <div class="t-foot"><span>${off ? "not reported" : "min —"}</span><span>${off ? "" : "max —"}</span></div>`;
      grid.appendChild(d);
      if (!off) paintTile(d, tk);
    });
    const missing = supported ? selection().filter((k) => !supported.has(k)).map((k) => byKey[k].name) : [];
    $("dashMissing").textContent = missing.length ? "Not reported by this ECU: " + missing.join(", ") + "." : "";
  }

  function paintTile(d, tk) {
    const k = tk.split("#")[0];
    const h = hist[tk] || [];
    const last = h.length ? h[h.length - 1] : null;
    const s = shown(k, last);
    d.querySelector(".t-val").textContent = fmt(s);
    d.querySelector(".t-unit").textContent = s.unit;
    d.className = "tile " + (last == null ? "" : "live ") + level(k, last);
    const m = mm[tk];
    if (m) {
      const f = d.querySelectorAll(".t-foot span");
      f[0].textContent = "min " + fmt(shown(k, m[0]));
      f[1].textContent = "max " + fmt(shown(k, m[1]));
    }
    const pts = h.filter((v) => v != null);
    if (pts.length > 1) {
      let lo = Math.min(...pts), hi = Math.max(...pts);
      if (hi - lo < 1e-9) { lo -= 1; hi += 1; }
      const xs = (i) => (i / (HIST - 1)) * 100, ys = (v) => 28 - ((v - lo) / (hi - lo)) * 26;
      const off = HIST - h.length;
      let line = "";
      h.forEach((v, i) => { if (v != null) line += (line ? "L" : "M") + xs(i + off).toFixed(1) + " " + ys(v).toFixed(1); });
      d.querySelector(".spark .line").setAttribute("d", line);
      d.querySelector(".spark .area").setAttribute("d", line + `L${xs(HIST - 1).toFixed(1)} 30L${xs(off + h.findIndex((v) => v != null)).toFixed(1)} 30Z`);
    }
  }

  function drawPicker() {
    const box = $("dashPicker");
    const chosen = new Set(selection());
    const groups = {};
    table.filter((t) => t.key !== "rpm" && t.key !== "boost_kpa").forEach((t) => { (groups[t.group || "other"] = groups[t.group || "other"] || []).push(t); });
    box.innerHTML = Object.keys(groups).map((g) => `
      <fieldset class="pp-group"><legend>${esc(g)}</legend>${groups[g].map((t) => `
        <label class="pp"><input type="checkbox" value="${esc(t.key)}" ${chosen.has(t.key) ? "checked" : ""} ${supported && !supported.has(t.key) ? "disabled" : ""}>
        <span>${esc(t.name)}</span><span class="muted mono">${t.pid != null ? "01 " + t.pid.toString(16).toUpperCase().padStart(2, "0") : "calc"}</span></label>`).join("")}
      </fieldset>`).join("");
    box.querySelectorAll("input").forEach((cb) => cb.addEventListener("change", () => {
      custom = Array.from(box.querySelectorAll("input:checked")).map((c) => c.value);
      preset = "custom";
      $("dashPreset").value = "custom";
      store.set("custom", custom);
      store.set("preset", preset);
      drawTiles();
    }));
  }

  // ---- data ----------------------------------------------------------------
  function push(tk, v) {
    const h = (hist[tk] = hist[tk] || []);
    h.push(v);
    if (h.length > HIST) h.shift();
    if (v != null) { const m = (mm[tk] = mm[tk] || [v, v]); m[0] = Math.min(m[0], v); m[1] = Math.max(m[1], v); }
  }
  function ingest(r) {
    const vals = (r && r.values) || {};
    let reshape = false;
    Object.keys(vals).forEach((k) => {
      const v = vals[k];
      if (Array.isArray(v)) {
        // Backends report fixed sensor slots with null for absent sensors; tile
        // only the sensors that have ever answered.
        const seen = (sensors[k] = sensors[k] || []);
        v.forEach((x, i) => {
          if (x == null) return;
          if (!seen.includes(i)) { seen.push(i); seen.sort((a, b) => a - b); reshape = true; }
          push(k + "#" + i, x);
        });
      } else push(k, v);
    });
    if (reshape) drawTiles();
    const bar = pUnit === "bar";
    const [max, red] = REDLINE[preset === "petrol" ? "petrol" : "diesel"];
    setGauge("gR", vals.rpm != null ? vals.rpm : null, 0, max, "rpm", 0, red);
    const b = vals.boost_kpa;
    setGauge("gB", b == null ? null : bar ? b / 100 : b, bar ? -0.2 : -20, bar ? 2.5 : 250, bar ? "bar" : "kPa", bar ? 2 : 0, null);
    panel.querySelectorAll(".tile").forEach((d) => { if (!d.classList.contains("na")) paintTile(d, d.dataset.k); });
  }

  async function poll() {
    if (inFlight) return;
    inFlight = true;
    try {
      ingest(await invoke("obd_read", { ...window.OT.connArgs(), pids: pidsFor(selection()) }));
      status("ok", "live");
    } catch (e) {
      const m = typeof e === "string" ? e : (e && e.message) || "error";
      status(/busy/i.test(m) ? "accent" : "bad", /busy/i.test(m) ? "adapter busy — retrying" : m);
    } finally {
      inFlight = false;
    }
  }

  async function start() {
    if (timer) return;
    $("dashStart").disabled = true;
    status("accent", "reading supported values…");
    try {
      const s = await invoke("obd_supported", { ...window.OT.connArgs() });
      const pids = new Set(s.pids || []);
      supported = new Set(table.filter((t) => t.pid != null && pids.has(t.pid)).map((t) => t.key));
      if (BOOST.needs.every((k) => supported.has(k))) supported.add("boost_kpa");
    } catch (e) {
      $("dashStart").disabled = false;
      status("bad", typeof e === "string" ? e : "could not read supported values");
      return;
    }
    $("gBoostBox").classList.toggle("na", !supported.has("boost_kpa"));
    drawTiles();
    if (!$("dashPicker").hidden) drawPicker();
    $("dashStop").disabled = false;
    poll();
    timer = setInterval(poll, parseInt($("dashRate").value, 10) || 500);
  }
  function stop() {
    if (timer) clearInterval(timer);
    timer = null;
    $("dashStart").disabled = false;
    $("dashStop").disabled = true;
    status("neutral", "idle");
  }

  // ---- wiring --------------------------------------------------------------
  $("dashPreset").value = preset;
  panel.querySelectorAll(".seg button").forEach((b) => {
    b.classList.toggle("on", b.dataset.u === pUnit);
    b.addEventListener("click", () => {
      pUnit = b.dataset.u;
      store.set("punit", pUnit);
      panel.querySelectorAll(".seg button").forEach((x) => x.classList.toggle("on", x === b));
      drawGauges();
      panel.querySelectorAll(".tile").forEach((d) => { if (!d.classList.contains("na")) paintTile(d, d.dataset.k); });
    });
  });
  $("dashPreset").addEventListener("change", (e) => {
    preset = e.target.value;
    store.set("preset", preset);
    drawGauges();
    drawTiles();
    if (!$("dashPicker").hidden) drawPicker();
  });
  $("dashRate").addEventListener("change", () => { if (timer) { stop(); start(); } });
  $("dashPick").addEventListener("click", () => { const p = $("dashPicker"); p.hidden = !p.hidden; if (!p.hidden) drawPicker(); });
  $("dashStart").addEventListener("click", start);
  $("dashStop").addEventListener("click", stop);
  if (window.OT && window.OT.gate) {
    window.OT.gate(panel, ["can"], "the dashboard", { disable: ["#dashStart"] });
    window.OT.onCaps(() => { if (!window.OT.has(["can"])) stop(); });
  }

  (async function init() {
    try { table = await invoke("obd_pid_table"); } catch (e) { table = []; }
    table = table.concat([BOOST]);
    byKey = {};
    table.forEach((t) => { byKey[t.key] = t; });
    drawGauges();
    drawTiles();
  })();
})();
