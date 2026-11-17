// OpenTune — CAN capture: record all bus traffic to a file.
// Passive capture (listen-only where the adapter supports it) in candump log,
// CSV or Vector ASC format, with live counters and a per-ID table whose changed
// bytes are highlighted. Backend: candump_start / candump_status / candump_stop.
"use strict";
(function () {
  const TAURI = window.__TAURI__ || {};
  const invoke = (c, a) => TAURI.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const EXT = { candump: "log", csv: "csv", asc: "asc" };

  let path = null;
  let timer = null;
  let busy = false;
  let prev = {}; // id -> last hex, for change highlighting
  let sortBy = "id";

  const panel = document.createElement("section");
  panel.className = "panel";
  panel.id = "panel-candump";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>CAN capture</h1>
      <p>Record every frame on the bus to a file for later analysis. The adapter only listens; nothing is sent to the car.</p>
    </header>
    <div class="card">
      <div class="row">
        <label class="small muted" for="cdFormat">Format</label>
        <select class="inp" id="cdFormat">
          <option value="candump">candump log (.log)</option>
          <option value="csv">CSV (.csv)</option>
          <option value="asc">Vector ASC (.asc)</option>
        </select>
        <button class="btn" id="cdPick" type="button">Save to…</button>
        <span class="small mono muted" id="cdPath">no file chosen</span>
      </div>
      <div class="row">
        <label class="small muted" for="cdDur">Stop after</label>
        <input class="inp sm mono" id="cdDur" type="number" min="0" step="10" value="0">
        <span class="small muted">s (0 = until you stop)</span>
        <label class="small muted" for="cdFilter">Only IDs</label>
        <input class="inp mono" id="cdFilter" placeholder="all — or e.g. 7E8, 280" style="width:180px">
      </div>
      <div class="row">
        <button class="btn primary" id="cdStart" type="button">Start capture</button>
        <button class="btn" id="cdStop" type="button" disabled>Stop</button>
        <span class="pill neutral" id="cdStatus">idle</span>
      </div>
      <p class="small muted">Match the adapter's bus and speed to the bus you want (HS-CAN 500 kbit/s for powertrain on most cars).
        ELM327 adapters capture in monitor mode and can drop frames on a busy bus; those are counted under <b>Dropped</b>.</p>
      <div id="cdDone"></div>
    </div>
    <div class="cd-stats" id="cdStats"></div>
    <div class="card">
      <div class="card-title-row">
        <h2>Traffic by ID</h2>
        <div class="seg" role="group" aria-label="Sort">
          <button type="button" data-s="id" class="on">ID</button><button type="button" data-s="hz">Rate</button><button type="button" data-s="count">Count</button>
        </div>
      </div>
      <div class="cd-wrap"><table class="tbl cd-tbl">
        <thead><tr><th class="mono">ID</th><th>Count</th><th>Rate</th><th>DLC</th><th>Last data</th></tr></thead>
        <tbody id="cdRows"><tr><td colspan="5" class="muted">Start a capture to see live traffic per CAN ID.</td></tr></tbody>
      </table></div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "candump", label: "CAN capture", icon: "≡", group: "vehicle", tier: "bench" });

  const $ = (id) => panel.querySelector("#" + id);
  const status = (cls, t) => { const p = $("cdStatus"); p.className = "pill " + cls; p.textContent = t; };
  const idHex = (r) => r.id.toString(16).toUpperCase().padStart(r.ext ? 8 : 3, "0");
  const mmss = (s) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;
  const kb = (b) => (b >= 1048576 ? (b / 1048576).toFixed(1) + " MB" : (b / 1024).toFixed(1) + " KB");

  function stats(st) {
    const cells = [
      ["Frames", (st.frames || 0).toLocaleString()],
      ["Frames / s", Math.round(st.fps || 0).toLocaleString()],
      ["IDs seen", (st.ids || []).length],
      ["Written", kb(st.bytes || 0)],
      ["Elapsed", mmss(st.elapsed_s || 0)],
      ["Dropped", (st.dropped || 0).toLocaleString()],
    ];
    $("cdStats").innerHTML = cells.map(([k, v], i) =>
      `<div class="stat${i === 5 && st.dropped ? " warn" : ""}"><div class="s-k">${k}</div><div class="s-v">${v}</div></div>`).join("");
  }
  stats({});

  function bytes(hex, old) {
    const b = (hex || "").match(/../g) || [];
    const o = (old || "").match(/../g) || [];
    return b.map((x, i) => `<span class="byte${old != null && o[i] !== x ? " chg" : ""}">${x}</span>`).join("");
  }
  function rows(ids) {
    if (!ids || !ids.length) return;
    const maxHz = Math.max(...ids.map((r) => r.hz || 0), 1);
    const list = ids.slice().sort((a, b) => (sortBy === "id" ? a.id - b.id : (b[sortBy] || 0) - (a[sortBy] || 0)));
    $("cdRows").innerHTML = list.map((r) => {
      const k = idHex(r);
      const html = `<tr><td class="mono">${k}${r.ext ? ' <span class="muted">x</span>' : ""}</td>
        <td class="mono">${(r.count || 0).toLocaleString()}</td>
        <td><div class="cd-rate"><span class="mono">${(r.hz || 0).toFixed(r.hz < 10 ? 1 : 0)} Hz</span><i style="width:${((r.hz || 0) / maxHz) * 100}%"></i></div></td>
        <td class="mono">${r.dlc}</td><td class="mono cd-data">${bytes(r.last, prev[k])}</td></tr>`;
      prev[k] = r.last;
      return html;
    }).join("");
  }

  async function tick() {
    if (busy) return;
    busy = true;
    try {
      const st = await invoke("candump_status");
      stats(st);
      rows(st.ids);
      if (st.error) status("bad", st.error);
      else if (st.running) status("ok", "capturing");
      if (!st.running) finished(st);
    } catch (e) {
      status("bad", String(e));
    } finally {
      busy = false;
    }
  }
  function finished(st) {
    if (timer) clearInterval(timer);
    timer = null;
    $("cdStart").disabled = false;
    $("cdStop").disabled = true;
    if (st && st.frames != null && !st.error) {
      status("neutral", "stopped");
      $("cdDone").innerHTML = `<div class="banner ok">Saved ${(st.frames || 0).toLocaleString()} frames (${kb(st.bytes || 0)}) to <span class="mono">${esc(st.path || path || "")}</span>${st.dropped ? ` — ${st.dropped} dropped by the adapter` : ""}.</div>`;
    }
  }

  function stamp() {
    const d = new Date(), p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
  }
  async function pick() {
    const fmt = $("cdFormat").value;
    try {
      const r = await TAURI.dialog.save({ title: "Save CAN capture", defaultPath: `can-${stamp()}.${EXT[fmt]}`, filters: [{ name: fmt, extensions: [EXT[fmt]] }] });
      if (r) { path = r; $("cdPath").textContent = r; }
    } catch (e) { status("bad", String(e)); }
  }

  async function start() {
    if (!path) await pick();
    if (!path) return;
    const filter = $("cdFilter").value.split(/[\s,]+/).filter(Boolean).map((x) => parseInt(x, 16)).filter((n) => !Number.isNaN(n));
    prev = {};
    $("cdDone").innerHTML = "";
    try {
      await invoke("candump_start", {
        ...window.OT.connArgs(), path, format: $("cdFormat").value,
        filterIds: filter.length ? filter : null, durationS: parseInt($("cdDur").value, 10) || null,
      });
    } catch (e) {
      status("bad", String(e));
      return;
    }
    $("cdStart").disabled = true;
    $("cdStop").disabled = false;
    status("ok", "capturing");
    timer = setInterval(tick, 500);
  }
  async function stop() {
    try { finished(await invoke("candump_stop")); } catch (e) { status("bad", String(e)); }
  }

  $("cdPick").addEventListener("click", pick);
  $("cdFormat").addEventListener("change", () => {
    if (path) { path = path.replace(/\.(log|csv|asc)$/i, "." + EXT[$("cdFormat").value]); $("cdPath").textContent = path; }
  });
  $("cdStart").addEventListener("click", start);
  $("cdStop").addEventListener("click", stop);
  panel.querySelectorAll(".seg button").forEach((b) => b.addEventListener("click", () => {
    sortBy = b.dataset.s;
    panel.querySelectorAll(".seg button").forEach((x) => x.classList.toggle("on", x === b));
  }));
  if (window.OT && window.OT.gate) window.OT.gate(panel, ["can"], "CAN capture", { disable: ["#cdStart"] });
})();
