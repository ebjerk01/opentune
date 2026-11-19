// OpenTune — Drive view: big, glanceable tiles for watching values while
// driving, and the same view replaying a recorded log offline.
//
// Live: listens to the polls the Dashboard (obd_read) or a running Live log
// session publish on window.OTDrive.feed, and can start / stop the
// Dashboard's polling. It never opens an adapter path of its own: every
// request still goes through the Dashboard and the app's AdapterHub. Live is
// car-facing, so it is shown only on alpha / bench builds (Bench tier).
// Replay: any log the Log viewer opens (log_open + log_replay), played at
// 1x/2x/4x/8x with a scrub bar. Fully offline; no adapter needed.
// Keys: F fullscreen, R reset min/max/peak, Space pause, Escape leave fullscreen.
// The decisions (formatting, thresholds, stale / lost, replay clock) are the
// pure functions of driveview-core.js.
"use strict";
(function () {
  const D = window.OTDrive;
  if (!D) return;
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const { esc, baseName } = window.OTUtil;
  const msg = (e) => (typeof e === "string" ? e : (e && e.message) || String(e));
  const store = {
    get(k, d) { try { const v = localStorage.getItem("ot.drive." + k); return v == null ? d : JSON.parse(v); } catch (e) { return d; } },
    set(k, v) { try { localStorage.setItem("ot.drive." + k, JSON.stringify(v)); } catch (e) { /* storage unavailable */ } },
  };
  const PAINT_MS = 100;
  const SPARK_W = 100, SPARK_H = 30;
  // Pseudo channels the GPS card adds to a live frame.
  const GPS_CHANS = [
    { key: "gps_speed_kmh", name: "GPS speed over ground", unit: "km/h" },
    { key: "speed_error_kmh", name: "Speed error (vehicle − GPS)", unit: "km/h" },
  ];

  const S = {
    mode: store.get("mode", "replay"),
    bench: false,
    pUnit: store.get("punit", "bar"),
    table: [],
    paused: false,
    kiosk: false,
    timer: null,
    sel: [],
    live: freshLive(),
    rp: { path: null, data: null, clock: null, peaks: {}, notes: [], dragging: false },
  };
  function freshLive() {
    return { src: null, lastFrameT: null, interval: 0.5, errors: 0, lastError: "", stopped: false, chans: {}, last: {}, hist: {}, peaks: {} };
  }

  // ---- panel ---------------------------------------------------------------
  const panel = document.createElement("section");
  panel.className = "panel wide";
  panel.id = "panel-driveview";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Drive view</h1>
      <p>Big, high-contrast tiles to read at arm's length: live values from the Dashboard's polling, or a
         recorded log played back offline. F = fullscreen, R = reset min/max, Space = pause.</p>
    </header>
    <div class="card dv-bar">
      <div class="row">
        <div class="seg" role="group" aria-label="Source" id="dvMode">
          <button type="button" data-m="live" id="dvModeLive">Live</button><button type="button" data-m="replay">Replay</button>
        </div>
        <span class="dv-live-ctl">
          <button class="btn primary" id="dvStart" type="button" title="Start the Dashboard's OBD polling">Start</button>
          <button class="btn" id="dvStop" type="button">Stop</button>
        </span>
        <span class="dv-replay-ctl">
          <button class="btn primary" id="dvOpen" type="button">Open log…</button>
          <span class="small muted mono" id="dvFile">no log opened</span>
        </span>
        <button class="btn sm" id="dvPick" type="button">Channels…</button>
        <div class="seg" role="group" aria-label="Pressure unit" id="dvPUnit">
          <button type="button" data-u="bar">bar</button><button type="button" data-u="kPa">kPa</button>
        </div>
        <button class="btn sm" id="dvReset" type="button" title="Reset min / max / peak (R)">Reset peaks</button>
        <button class="btn sm" id="dvFull" type="button" title="Fullscreen (F)">Fullscreen</button>
      </div>
      <div id="dvPicker" class="pid-picker" hidden></div>
      <p class="small muted" id="dvNote"></p>
    </div>
    <div class="dv-stage">
      <div class="dv-status" id="dvStatus" data-s="idle">
        <span class="dv-state" id="dvState">idle</span>
        <span class="dv-detail" id="dvDetail"></span>
        <span class="dv-spacer"></span>
        <button class="btn sm dv-kiosk-only" id="dvExit" type="button">Exit fullscreen</button>
      </div>
      <div class="dv-transport" id="dvTransport" hidden>
        <button class="btn primary dv-play" id="dvPlay" type="button" aria-label="Play">▶</button>
        <div class="seg" role="group" aria-label="Replay speed" id="dvSpeed">
          ${D.SPEEDS.map((s) => `<button type="button" data-s="${s}">${s}×</button>`).join("")}
        </div>
        <input class="dv-scrub" id="dvScrub" type="range" min="0" max="1000" step="1" value="0" aria-label="Position in the log" />
        <span class="mono dv-time" id="dvTime">0:00 / 0:00</span>
      </div>
      <div class="dv-grid" id="dvGrid"></div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  // Replay needs no car: a public panel next to the Log viewer. Its live mode
  // is shown only on Bench builds (see applyChannel).
  window.OTNav.register({ id: "driveview", label: "Drive view", icon: "◉", group: "calibration", tier: "public" });

  const $ = (id) => panel.querySelector("#" + id);
  const active = () => panel.classList.contains("active");

  // ---- channel catalogue + selection ------------------------------------------------
  function dedupe(list) {
    const seen = new Set();
    return list.filter((c) => c && !seen.has(c.key) && seen.add(c.key));
  }
  function liveSource() { return S.live.src || "dashboard"; }
  function catalog() {
    if (S.mode === "replay") return S.rp.data ? S.rp.data.chans.map((c) => ({ key: c.key, name: c.name, unit: c.unit })) : [];
    const seen = Object.values(S.live.chans);
    if (liveSource() !== "dashboard") return dedupe(seen.concat(GPS_CHANS));
    const tbl = S.table.filter((t) => t.pid != null || t.derived).map((t) => ({ key: t.key, name: t.name, unit: t.unit }));
    return dedupe(tbl.concat(GPS_CHANS, seen));
  }
  const selKey = () => (S.mode === "replay" ? "sel.replay" : "sel.live." + liveSource());
  function chooseSelection() {
    S.sel = D.selectionFor(catalog(), store.get(selKey(), null));
    if (S.mode === "live" && window.OTDash) window.OTDash.want(S.sel);
  }
  function chanOf(key) {
    if (S.mode === "replay") return S.rp.data && S.rp.data.byKey[key];
    return S.live.chans[key] || catalog().find((c) => c.key === key) || { key, name: key, unit: "" };
  }
  function label(ch) {
    const r = D.roleOf(ch);
    return r ? r.label : ch.name || ch.key;
  }

  // ---- tiles -----------------------------------------------------------------
  const thCache = {};
  function thresholds(ch) {
    const id = S.mode + "|" + ch.key + "|" + ch.unit;
    if (!(id in thCache)) thCache[id] = D.thresholdsFor(ch, S.table);
    return thCache[id];
  }
  function buildTiles() {
    const grid = $("dvGrid");
    grid.innerHTML = S.sel.map((k, i) => {
      const ch = chanOf(k) || { key: k, name: k, unit: "" };
      return `<div class="dv-tile${i < 2 ? " dv-hero" : ""}" data-k="${esc(k)}" data-state="nodata" data-level="" title="${esc(ch.name)}${ch.unit ? " [" + esc(ch.unit) + "]" : ""}">
        <div class="dv-head"><span class="dv-name">${esc(label(ch))}</span><span class="dv-badge"></span></div>
        <div class="dv-main"><span class="dv-val">—</span><span class="dv-unit"></span></div>
        <div class="dv-sub"></div>
        <div class="dv-meter"><i class="dv-fill"></i><i class="dv-mark dv-warn"></i><i class="dv-mark dv-crit"></i><i class="dv-peak"></i></div>
        <svg class="dv-spark" viewBox="0 0 ${SPARK_W} ${SPARK_H}" preserveAspectRatio="none" aria-hidden="true"><path d=""></path></svg>
        <div class="dv-foot"><span class="dv-min">min —</span><span class="dv-max">max —</span></div>
      </div>`;
    }).join("") || `<p class="muted">${S.mode === "replay" ? "Open a log to replay it here." : "No channels yet: press Start (or start a Live log session)."}</p>`;
    paint();
  }

  function pos(el, f) {
    if (f == null) { el.hidden = true; return; }
    el.hidden = false;
    el.style.left = (f * 100).toFixed(1) + "%";
  }

  // One tile. `x` = { ch, v, at, now, staleS, link, peak, spark: {t, v, a, b}, age(s) }.
  function paintTile(el, x) {
    const ch = x.ch;
    const role = D.roleOf(ch);
    const signed = !!(role && role.signed);
    const state = D.tileState({ link: x.link, lastT: x.at, now: x.now, staleS: x.staleS });
    el.hidden = !!(role && role.optional && state === "nodata" && !(x.peak && x.peak.n));
    const th = thresholds(ch);
    const live = state === "live";
    const lvl = live ? D.level(x.v, th) : "";
    el.dataset.state = state;
    el.dataset.level = lvl;
    const shown = D.fmtValue(live ? x.v : null, ch.unit, S.pUnit, signed);
    el.querySelector(".dv-val").textContent = shown.text;
    el.querySelector(".dv-unit").textContent = D.display(0, ch.unit, S.pUnit).unit;
    const last = x.at == null ? null : D.fmtValue(x.v, ch.unit, S.pUnit, signed);
    const lastTxt = last ? `last ${last.text}${last.unit ? " " + last.unit : ""} · ${x.age(x.now - x.at)}` : "";
    const badge = { live: lvl ? lvl.toUpperCase() : "", stale: "STALE", lost: "LINK LOST", nodata: "NO DATA" }[state];
    el.querySelector(".dv-badge").textContent = badge;
    el.querySelector(".dv-sub").textContent = live ? "" : state === "nodata" ? (x.link === "replay" ? "no sample yet" : "waiting for a value") : lastTxt;
    // Meter: value, warn/crit marks and the held peak.
    const rng = D.barRange(th, x.peak);
    const fill = el.querySelector(".dv-fill");
    const f = live ? D.frac(x.v, rng) : null;
    fill.style.width = f == null ? "0%" : (f * 100).toFixed(1) + "%";
    pos(el.querySelector(".dv-warn"), D.frac(th.warn, rng));
    pos(el.querySelector(".dv-crit"), D.frac(th.crit, rng));
    pos(el.querySelector(".dv-peak"), x.peak ? D.frac(x.peak.max, rng) : null);
    const p = x.peak || {};
    const mm = (v) => { const s = D.fmtValue(v, ch.unit, S.pUnit, signed); return s.text; };
    el.querySelector(".dv-min").textContent = "min " + mm(p.min);
    el.querySelector(".dv-max").textContent = "max " + mm(p.max);
    const sp = x.spark;
    el.querySelector(".dv-spark path").setAttribute("d", sp ? D.sparkPath(sp.t, sp.v, sp.a, sp.b, SPARK_W, SPARK_H) : "");
  }

  function status(s, state, detail) {
    const st = $("dvStatus");
    st.dataset.s = s;
    $("dvState").textContent = state;
    $("dvDetail").textContent = detail || "";
  }

  // ---- paint loop ----------------------------------------------------------------
  function paint() {
    if (S.mode === "replay") paintReplay();
    else paintLive();
  }

  function paintLive() {
    const L = S.live;
    if (S.paused) {
      status("paused", "Paused", "display frozen — these values are not live · Space to resume");
      return;
    }
    const now = Date.now() / 1000;
    const staleS = D.staleAfter(L.interval);
    const link = D.linkState({ now, lastFrameT: L.lastFrameT, staleS, failing: L.errors >= 2, stopped: L.stopped });
    const src = L.src === "livelog" ? "Live log session" : "Dashboard polling";
    const dashOn = window.OTDash && window.OTDash.running();
    if (link === "up") status("up", "Live", `${src} · every ${Math.round(L.interval * 1000)} ms`);
    else if (link === "waiting") status(dashOn ? "waiting" : "idle", dashOn ? "Waiting…" : "Not running", dashOn ? "first values on the way" : "press Start, or start a Live log session");
    else if (link === "stopped") status("stopped", "Stopped", `${src} stopped — values below are not live`);
    else status("lost", "Link lost", (L.lastError ? L.lastError + " · " : "") + `no fresh values for ${Math.max(0, Math.floor(now - (L.lastFrameT || now)))} s`);
    panel.querySelectorAll(".dv-tile").forEach((el) => {
      const k = el.dataset.k;
      const s = L.last[k];
      const h = L.hist[k];
      paintTile(el, {
        ch: chanOf(k), v: s ? s.v : null, at: s ? s.at : null, now, staleS, link,
        peak: L.peaks[k], spark: h ? { t: h.t, v: h.v, a: now - D.HIST_S, b: now } : null, age: D.fmtAge,
      });
    });
  }

  function paintReplay() {
    const R = S.rp;
    if (!R.data) { status("idle", "Replay", "open a log (Live-log CSV, Dashboard recording, CLI log or any CSV the Log viewer reads)"); return; }
    const d = R.data, c = R.clock;
    const wall = Date.now();
    const step = D.clockTick(c, wall);
    if (step.to > step.from) S.sel.forEach((k) => { const ch = d.byKey[k]; if (ch) D.replayFold(d, ch, peak(R.peaks, k), step.from, step.to); });
    if (step.ended) syncPlay();
    const span = c.t1 - c.t0;
    if (!R.dragging) $("dvScrub").value = String(span > 0 ? Math.round(((c.pos - c.t0) / span) * 1000) : 0);
    $("dvTime").textContent = `${D.fmtClock(c.pos - (d.index ? 0 : c.t0), d.index)} / ${D.fmtClock(c.t1 - (d.index ? 0 : c.t0), d.index)}`;
    status(c.playing ? "replay" : "paused", c.playing ? `Replay ${c.speed}×` : "Replay paused", baseName(R.path));
    const staleS = D.staleAfter(d.dt);
    const ageFmt = d.index ? (a) => Math.round(a) + " samples ago" : D.fmtAge;
    const win = D.HIST_S * c.rate;
    panel.querySelectorAll(".dv-tile").forEach((el) => {
      const ch = d.byKey[el.dataset.k];
      if (!ch) return;
      const s = D.replaySample(d, ch, c.pos);
      paintTile(el, {
        ch, v: s.v, at: s.at, now: c.pos, staleS, link: "replay", peak: R.peaks[ch.key],
        spark: Object.assign(D.replayWindow(d, ch, c.pos - win, c.pos, 120), { a: c.pos - win, b: c.pos }), age: ageFmt,
      });
    });
  }
  function peak(map, k) { return (map[k] = map[k] || D.peakNew()); }

  function startLoop() {
    if (S.timer) return;
    S.timer = setInterval(paint, PAINT_MS);
    paint();
  }
  function stopLoop() {
    if (S.timer) clearInterval(S.timer);
    S.timer = null;
  }

  // ---- live feed -----------------------------------------------------------------
  D.feed.on((ev) => {
    const L = S.live;
    if (ev.type === "frame") {
      const now = Date.now() / 1000;
      // Two sources at once: keep the one already showing while it is fresh.
      if (L.src && L.src !== ev.source && L.lastFrameT != null && !L.stopped && now - L.lastFrameT < D.staleAfter(L.interval)) return;
      const switched = L.src !== ev.source;
      if (switched) { Object.assign(L, freshLive()); L.src = ev.source; }
      if (L.lastFrameT != null) L.interval = Math.min(10, Math.max(0.05, 0.7 * L.interval + 0.3 * (now - L.lastFrameT)));
      L.lastFrameT = now;
      L.errors = 0;
      L.lastError = "";
      L.stopped = false;
      let fresh = switched;
      (ev.values || []).forEach((c) => {
        if (!L.chans[c.key]) { L.chans[c.key] = { key: c.key, name: c.name, unit: c.unit }; fresh = true; }
        if (c.value != null) { L.last[c.key] = { v: c.value, at: now }; D.peakAdd(peak(L.peaks, c.key), c.value, now); }
        D.histPush((L.hist[c.key] = L.hist[c.key] || D.histNew()), now, c.value, D.HIST_S);
      });
      // A source's channels arrive with its first frame: pick its tiles then.
      if (fresh && S.mode === "live") { chooseSelection(); buildTiles(); }
    } else if (ev.type === "error") {
      if (L.src && ev.source !== L.src) return;
      if (!ev.busy) L.errors += 1;
      L.lastError = ev.message || "";
    } else if (ev.type === "stop") {
      if (!L.src || ev.source === L.src) L.stopped = true;
    }
  });

  async function liveStart() {
    if (!window.OTDash) return note("The Dashboard is not available in this build.");
    if (window.OT && !window.OT.has(["diag"])) return note("No adapter with a diagnostic link: set the connection in the sidebar and press Detect.");
    S.live.stopped = false;
    chooseSelection();
    try {
      await window.OTDash.start();
      note("");
    } catch (e) {
      note("Start: " + msg(e));
    }
  }
  function liveStop() {
    if (window.OTDash) window.OTDash.stop();
  }

  // ---- replay --------------------------------------------------------------------
  async function pickLog() {
    try {
      const r = await T().dialog.open({ multiple: false, directory: false, filters: [{ name: "CSV log", extensions: ["csv", "txt", "tsv", "log"] }] });
      const p = Array.isArray(r) ? r[0] : r;
      if (p) await openLog(p);
    } catch (e) {
      note(msg(e));
    }
  }
  async function openLog(path) {
    setMode("replay");
    $("dvFile").textContent = baseName(path);
    note("opening…");
    try {
      const sum = await invoke("log_open", { path, timeUnit: "auto" });
      const r = await invoke("log_replay", { path });
      const d = D.replayPrepare(r);
      if (!d.t.length) throw "the log has no samples";
      S.rp = { path, data: d, clock: D.clockNew(d.t0, d.t1, d.rate), peaks: {}, notes: [], dragging: false };
      D.clockSpeed(S.rp.clock, store.get("speed", 1), Date.now());
      const notes = [];
      if (sum && sum.format) notes.push(sum.format === "opentune_long" ? "OpenTune Live log" : "CSV (one column per channel)");
      notes.push(`${r.rows} samples · ${d.chans.length} channel(s)`);
      if (d.index) notes.push(`no time column: replayed at ${D.INDEX_HZ} samples per second at 1×`);
      if (d.stride > 1) notes.push(`long log: each frame holds the latest value of ${d.stride} samples`);
      chooseSelection();
      const missing = D.ROLES.filter((ro) => !ro.optional && !d.chans.some((c) => D.roleOf(c) === ro)).map((ro) => ro.label);
      if (missing.length) notes.push("not in this log: " + missing.join(", "));
      note(notes.join(" · "));
      $("dvTransport").hidden = false;
      seek(d.t0);
      syncSpeed();
      syncPlay();
      buildTiles();
    } catch (e) {
      note(msg(e));
    }
  }
  function seek(x) {
    const R = S.rp;
    if (!R.data) return;
    D.clockSeek(R.clock, x, Date.now());
    // What the view now shows counts toward min / max (skipped spans do not).
    S.sel.forEach((k) => { const ch = R.data.byKey[k]; if (!ch) return; const s = D.replaySample(R.data, ch, R.clock.pos); if (s.at != null) D.peakAdd(peak(R.peaks, k), s.v, s.at); });
  }
  function togglePlay() {
    const c = S.rp.clock;
    if (!c) return;
    if (c.playing) D.clockPause(c, Date.now());
    else D.clockPlay(c, Date.now());
    syncPlay();
    paint();
  }
  function syncPlay() {
    const on = !!(S.rp.clock && S.rp.clock.playing);
    const b = $("dvPlay");
    b.textContent = on ? "❚❚" : "▶";
    b.setAttribute("aria-label", on ? "Pause" : "Play");
  }
  function syncSpeed() {
    const s = S.rp.clock ? S.rp.clock.speed : store.get("speed", 1);
    $("dvSpeed").querySelectorAll("button").forEach((b) => b.classList.toggle("on", Number(b.dataset.s) === s));
  }

  // ---- picker ----------------------------------------------------------------
  function drawPicker() {
    const box = $("dvPicker");
    const cat = catalog();
    const chosen = new Set(S.sel);
    box.innerHTML = cat.length
      ? `<div class="row tight"><button class="btn sm" type="button" id="dvDefault">Default set</button><span class="small muted">Ticked channels become tiles, in the order ticked; the first two are large.</span></div>
         <div class="dv-pick-list">${cat.map((c) => `<label class="pp"><input type="checkbox" value="${esc(c.key)}" ${chosen.has(c.key) ? "checked" : ""}><span>${esc(c.name)}</span><span class="muted mono">${esc(c.unit)}</span></label>`).join("")}</div>`
      : '<span class="small muted">No channels yet.</span>';
    box.querySelectorAll("input").forEach((cb) => cb.addEventListener("change", () => {
      const on = new Set(Array.from(box.querySelectorAll("input")).filter((x) => x.checked).map((x) => x.value));
      S.sel = S.sel.filter((k) => on.has(k)).concat(Array.from(on).filter((k) => !S.sel.includes(k)));
      store.set(selKey(), S.sel);
      if (S.mode === "live" && window.OTDash) window.OTDash.want(S.sel);
      buildTiles();
    }));
    const def = box.querySelector("#dvDefault");
    if (def) def.addEventListener("click", () => { store.set(selKey(), null); chooseSelection(); drawPicker(); buildTiles(); });
  }

  // ---- mode, fullscreen, keys ---------------------------------------------------------
  function note(t) { $("dvNote").textContent = t || ""; }
  // `remember` = a user's choice (kept for next time).
  function setMode(m, remember) {
    if (m === "live" && !S.bench) m = "replay";
    S.mode = m;
    if (remember) store.set("mode", m);
    $("dvMode").querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.dataset.m === m));
    panel.querySelectorAll(".dv-live-ctl").forEach((e) => { e.hidden = m !== "live"; });
    panel.querySelectorAll(".dv-replay-ctl").forEach((e) => { e.hidden = m !== "replay"; });
    $("dvTransport").hidden = m !== "replay" || !S.rp.data;
    S.paused = false;
    if (m === "live" && S.rp.clock && S.rp.clock.playing) { D.clockPause(S.rp.clock, Date.now()); syncPlay(); }
    note("");
    // Replay: stop asking the Dashboard for extra channels.
    if (m !== "live" && window.OTDash) window.OTDash.want([]);
    chooseSelection();
    if (!$("dvPicker").hidden) drawPicker();
    buildTiles();
  }
  async function setFullscreen(on) {
    S.kiosk = on;
    panel.classList.toggle("dv-kiosk", on);
    try {
      const w = T().window;
      if (w && w.getCurrentWindow) { await w.getCurrentWindow().setFullscreen(on); return; }
    } catch (e) { /* not allowed: the in-window kiosk layout still applies */ }
    try {
      if (on && document.documentElement.requestFullscreen && !document.fullscreenElement) await document.documentElement.requestFullscreen();
      else if (!on && document.fullscreenElement && document.exitFullscreen) await document.exitFullscreen();
    } catch (e) { /* browser refused: kiosk layout only */ }
  }
  function resetPeaks() {
    if (S.mode === "replay") { S.rp.peaks = {}; seek(S.rp.clock ? S.rp.clock.pos : 0); }
    else S.live.peaks = {};
    paint();
  }
  function pause() {
    if (S.mode === "replay") return togglePlay();
    S.paused = !S.paused;
    panel.classList.toggle("dv-paused", S.paused);
    paint();
  }
  document.addEventListener("keydown", (ev) => {
    if (!active()) return;
    const a = D.keyAction(ev);
    if (!a) return;
    if (a === "exit" && !S.kiosk) return;
    ev.preventDefault();
    if (a === "fullscreen") setFullscreen(!S.kiosk);
    else if (a === "exit") setFullscreen(false);
    else if (a === "reset") resetPeaks();
    else if (a === "pause") pause();
  });

  // ---- wiring ----------------------------------------------------------------------
  $("dvMode").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => setMode(b.dataset.m, true)));
  $("dvStart").addEventListener("click", liveStart);
  $("dvStop").addEventListener("click", liveStop);
  $("dvOpen").addEventListener("click", pickLog);
  $("dvPick").addEventListener("click", () => { const p = $("dvPicker"); p.hidden = !p.hidden; if (!p.hidden) drawPicker(); });
  $("dvReset").addEventListener("click", resetPeaks);
  $("dvFull").addEventListener("click", () => setFullscreen(!S.kiosk));
  $("dvExit").addEventListener("click", () => setFullscreen(false));
  $("dvPlay").addEventListener("click", togglePlay);
  $("dvSpeed").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => {
    const s = Number(b.dataset.s);
    store.set("speed", s);
    if (S.rp.clock) D.clockSpeed(S.rp.clock, s, Date.now());
    syncSpeed();
  }));
  const scrub = $("dvScrub");
  scrub.addEventListener("pointerdown", () => { S.rp.dragging = true; });
  scrub.addEventListener("pointerup", () => { S.rp.dragging = false; });
  scrub.addEventListener("change", () => { S.rp.dragging = false; });
  scrub.addEventListener("input", () => {
    const c = S.rp.clock;
    if (!c) return;
    seek(c.t0 + (Number(scrub.value) / 1000) * (c.t1 - c.t0));
    paint();
  });
  $("dvPUnit").querySelectorAll("button").forEach((b) => {
    b.classList.toggle("on", b.dataset.u === S.pUnit);
    b.addEventListener("click", () => {
      S.pUnit = b.dataset.u;
      store.set("punit", S.pUnit);
      $("dvPUnit").querySelectorAll("button").forEach((x) => x.classList.toggle("on", x === b));
      paint();
    });
  });
  // Paint only while visible; leave fullscreen when the panel is left.
  new MutationObserver(() => {
    if (active()) startLoop();
    else { stopLoop(); if (S.kiosk) setFullscreen(false); }
  }).observe(panel, { attributes: true, attributeFilter: ["class"] });
  // Live is car-facing: Bench tier (alpha / bench builds) only.
  function applyChannel() {
    S.bench = window.OTNav.benchEnabled();
    $("dvModeLive").hidden = !S.bench;
    $("dvMode").hidden = !S.bench;
    setMode(S.bench ? store.get("mode", "replay") : "replay");
  }
  window.OTNav.onChannel(applyChannel);
  if (window.OT && window.OT.onCaps) {
    window.OT.onCaps(() => {
      const ok = window.OT.has(["diag"]);
      $("dvStart").disabled = !ok;
      $("dvStart").title = ok ? "Start the Dashboard's OBD polling" : "Needs a connected adapter with a diagnostic link";
    });
  }

  async function init() {
    try { S.table = (await invoke("obd_pid_table")) || []; } catch (e) { S.table = []; }
    Object.keys(thCache).forEach((k) => delete thCache[k]);
    chooseSelection();
    buildTiles();
  }
  if (window.onPanelFirstShow) window.onPanelFirstShow("driveview", init);
  else init();
  setMode(S.mode);

  // Other panels open the view: the Log viewer with its log, the Dashboard live.
  window.OTDriveView = {
    openLog(path) { if (window.showPanel) window.showPanel("driveview"); return openLog(path); },
    showLive() { if (window.showPanel) window.showPanel("driveview"); setMode("live", true); },
    // Repaint now (the loop runs only while the panel is visible).
    refresh: paint,
  };
})();
