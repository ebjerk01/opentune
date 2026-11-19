// OpenTune — Render check: what WebKitGTK renders with, and how smooth the
// content pane scrolls. Makes environment problems visible in the app instead
// of guessed at (a launcher that set WEBKIT_DISABLE_COMPOSITING_MODE=1 once
// made every panel CPU-painted and laggy).
//
// * render_env (backend, startup.rs): the WebKit/GTK-relevant environment the
//   process sees — a fixed whitelist, nothing else — and the DMA-BUF decision.
// * A one-time, dismissible warning when compositing is disabled.
// * Debug panel card: the environment, the WebGL renderer, and frame timing
//   (requestAnimationFrame) while the user scrolls a panel for ~5 s.
// * OPENTUNE_PERF=1: logs the environment at startup and measures the first
//   scroll after every panel switch; lines go to the debug log and stderr
//   (render_note).
//
// The rAF sampler runs only for the 5 s after a scroll starts; nothing here
// runs while the app is idle. The pure parts are exported for the node tests.
"use strict";
(function (root) {
  const SAMPLE_MS = 5000;
  const LONG_MS = 50; // a frame this long is a visible stutter
  const WAIT_SCROLL_MS = 15000; // manual check: give up when nobody scrolls
  const DISMISS_KEY = "ot.render.compositingWarnDismissed";

  // ---- pure -------------------------------------------------------------------
  // Nearest-rank percentile of an ascending array.
  function percentile(sorted, p) {
    if (!sorted.length) return 0;
    const k = Math.ceil((p / 100) * sorted.length) - 1;
    return sorted[Math.max(0, Math.min(sorted.length - 1, k))];
  }
  const r1 = (x) => Math.round(x * 10) / 10;
  // Frame intervals (ms) from rAF timestamps.
  function deltas(ts) {
    const out = [];
    for (let i = 1; i < (ts || []).length; i++) out.push(ts[i] - ts[i - 1]);
    return out;
  }
  // Stats over frame intervals: count, fps, p50/p95/max frame time and the
  // number of long frames (> longMs). Non-finite and non-positive values are
  // dropped (a timestamp that did not advance).
  function frameStats(ds, longMs) {
    const lim = longMs == null ? LONG_MS : longMs;
    const d = (ds || []).filter((x) => Number.isFinite(x) && x > 0);
    if (!d.length) return { frames: 0, fps: 0, p50: 0, p95: 0, max: 0, long: 0, longMs: lim };
    const s = d.slice().sort((a, b) => a - b);
    const total = d.reduce((a, b) => a + b, 0);
    return {
      frames: d.length,
      fps: r1((d.length * 1000) / total),
      p50: r1(percentile(s, 50)),
      p95: r1(percentile(s, 95)),
      max: r1(s[s.length - 1]),
      long: d.filter((x) => x > lim).length,
      longMs: lim,
    };
  }
  // smooth / uneven / slow, for the results table.
  function verdict(st) {
    if (!st || !st.frames) return "no frames";
    if (st.p50 > 25 || st.fps < 40) return "slow";
    if (st.p95 > 25 || st.long > 0) return "uneven";
    return "smooth";
  }
  function statsLine(panel, st, scrolls) {
    return `scroll ${panel}: ${st.frames} frames, ${st.fps} fps, p50 ${st.p50} ms, p95 ${st.p95} ms, max ${st.max} ms, ` +
      `${st.long} long (>${st.longMs} ms), ${scrolls} scroll events — ${verdict(st)}`;
  }

  // WebKitGTK disables compositing for any value except "0".
  function compositingDisabled(env) {
    const v = env && env.vars ? env.vars.WEBKIT_DISABLE_COMPOSITING_MODE : null;
    return v != null && v !== "0";
  }
  const WARN_TEXT = "WEBKIT_DISABLE_COMPOSITING_MODE is set: everything is CPU-painted, so scrolling will be slow. " +
    "Remove it from the launcher or wrapper that starts OpenTune — see Troubleshooting (docs/TROUBLESHOOTING-STARTUP.md, " +
    "“Recommended launch environment”).";
  // The warning to show, or null (not set, or dismissed before).
  function compositingWarning(env, dismissed) {
    return !dismissed && compositingDisabled(env) ? WARN_TEXT : null;
  }

  // The DMA-BUF line: decision, the rule that fired, the render-node drivers.
  function dmabufLine(env) {
    const d = env && env.dmabuf;
    if (!d) return null;
    const drv = (d.drivers || []).length ? d.drivers.join(", ") : "none found";
    return `DMA-BUF renderer: ${d.enabled ? "on" : "off"} — ${d.reason || "?"} (render-node driver: ${drv})`;
  }
  // Display / log lines for render_env's result.
  function envLines(env) {
    if (!env) return ["render_env unavailable"];
    const out = [];
    Object.keys(env.vars || {}).forEach((k) => out.push(`${k}=${env.vars[k] == null ? "(unset)" : env.vars[k]}`));
    Object.keys(env.present || {}).forEach((k) => out.push(`${k}: ${env.present[k] ? "set" : "unset"}`));
    out.push(`/dev/dri render nodes: ${(env.dri_render_nodes || []).join(", ") || "none"}`);
    const dl = dmabufLine(env);
    if (dl) out.push(dl);
    if ((env.set_by_opentune || []).length) out.push(`set by OpenTune: ${env.set_by_opentune.join(", ")}`);
    if (env.perf) out.push("OPENTUNE_PERF: on");
    return out;
  }

  // ---- runtime ------------------------------------------------------------------
  const doc = root.document;
  const call = (cmd, args) => {
    try {
      const t = root.__TAURI__;
      return t && t.core ? Promise.resolve(t.core.invoke(cmd, args)) : Promise.reject(new Error("no backend"));
    } catch (e) { return Promise.reject(e); }
  };
  const note = (line) => { call("render_note", { line }).catch(() => {}); };
  const byId = (id) => (doc ? doc.getElementById(id) : null);
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  // run: the manual check in progress; auto: the OPENTUNE_PERF sampler.
  const S = { env: null, envP: null, gl: null, results: {}, run: null, auto: null, armed: false };

  function loadEnv() {
    if (!S.envP) S.envP = call("render_env").then((e) => (S.env = e)).catch(() => null);
    return S.envP;
  }

  // The GL renderer WebKit's WebGL sees (llvmpipe = software). One throwaway
  // context, released right away; only on the Debug card or OPENTUNE_PERF.
  function glRenderer() {
    if (S.gl) return S.gl;
    try {
      const c = doc.createElement("canvas");
      const gl = c.getContext && (c.getContext("webgl") || c.getContext("experimental-webgl"));
      if (!gl) return (S.gl = "WebGL unavailable");
      const ext = gl.getExtension("WEBGL_debug_renderer_info");
      const r = ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
      const v = ext ? gl.getParameter(ext.UNMASKED_VENDOR_WEBGL) : gl.getParameter(gl.VENDOR);
      const lose = gl.getExtension("WEBGL_lose_context");
      if (lose) lose.loseContext();
      S.gl = `${r}${v ? " (" + v + ")" : ""}${ext ? "" : " [masked]"}`;
    } catch (e) { S.gl = "WebGL unavailable (" + e + ")"; }
    return S.gl;
  }

  // ---- warning banner ---------------------------------------------------------
  function dismissed() { try { return root.localStorage.getItem(DISMISS_KEY) === "1"; } catch (e) { return false; } }
  function showWarning(env) {
    const msg = compositingWarning(env, dismissed());
    const main = byId("main");
    if (!msg || !main || doc.querySelector(".rc-compwarn")) return;
    const b = doc.createElement("div");
    b.id = "rcWarn";
    b.className = "banner info rc-warn rc-compwarn";
    b.setAttribute("role", "alert");
    b.innerHTML = '<span class="rc-warn-text"></span><button type="button" class="btn sm" id="rcWarnDismiss">Dismiss</button>';
    b.querySelector(".rc-warn-text").textContent = msg;
    main.insertBefore(b, main.firstChild);
    b.querySelector("#rcWarnDismiss").addEventListener("click", () => {
      try { root.localStorage.setItem(DISMISS_KEY, "1"); } catch (e) { /* shows again next start */ }
      b.remove();
    });
  }

  // The DMA-BUF self-heal's one-shot notice ("the last start did not draw"):
  // the backend reports it on the one start that detected it.
  function showNotice(env) {
    const n = env && env.dmabuf && env.dmabuf.notice;
    const main = byId("main");
    if (!n || !main || doc.querySelector(".rc-notice")) return;
    const b = doc.createElement("div");
    b.className = "banner info rc-warn rc-notice";
    b.setAttribute("role", "status");
    b.innerHTML = '<span class="rc-warn-text"></span><button type="button" class="btn sm">OK</button>';
    b.querySelector(".rc-warn-text").textContent = n + " Details: Debug → Render check.";
    main.insertBefore(b, main.firstChild);
    b.querySelector("button").addEventListener("click", () => b.remove());
  }

  // ---- frame sampler ----------------------------------------------------------
  // Wait for the first scroll (anywhere but the sidebar), then record rAF
  // timestamps for SAMPLE_MS. Returns a handle with cancel().
  function measure(panel, opts) {
    const o = opts || {};
    let ts = [], scrolls = 0, raf = 0, timer = 0, waitTimer = 0, started = false, done = false;
    const onScroll = (e) => {
      const t = e && e.target;
      if (t && t.closest && t.closest("#sidebar")) return;
      scrolls++;
      if (!started) start();
    };
    function start() {
      started = true;
      clearTimeout(waitTimer);
      if (o.onStart) o.onStart();
      const tick = (t) => { ts.push(t); raf = root.requestAnimationFrame(tick); };
      raf = root.requestAnimationFrame(tick);
      timer = setTimeout(finish, SAMPLE_MS);
    }
    function stop() {
      done = true;
      root.cancelAnimationFrame(raf);
      clearTimeout(timer);
      clearTimeout(waitTimer);
      doc.removeEventListener("scroll", onScroll, true);
    }
    function finish() {
      if (done) return;
      stop();
      const st = frameStats(deltas(ts));
      S.results[panel] = { st, scrolls };
      const line = statsLine(panel, st, scrolls);
      note(line);
      if (o.onDone) o.onDone(st, scrolls, line);
    }
    doc.addEventListener("scroll", onScroll, { capture: true, passive: true });
    if (o.waitMs) waitTimer = setTimeout(() => { if (!started) { stop(); if (o.onDone) o.onDone(null, 0, null); } }, o.waitMs);
    return { cancel: stop };
  }

  // OPENTUNE_PERF=1: measure the first scroll after each panel switch.
  function panelSwitched(name) {
    if (!S.armed || S.run) return; // a manual check owns the sampler
    if (S.auto) S.auto.cancel();
    const h = measure(name, { onDone: () => { if (S.auto === h) S.auto = null; } });
    S.auto = h;
  }

  // ---- Debug panel card -------------------------------------------------------
  function panels() {
    return Array.from(doc.querySelectorAll(".nav-item[data-panel]"))
      .filter((n) => !n.hidden && n.dataset.panel !== "debug")
      .map((n) => ({ id: n.dataset.panel, label: (n.textContent || n.dataset.panel).replace(/^\s*\S+\s+/, "").trim() || n.dataset.panel }));
  }
  function renderResults() {
    const t = byId("rcResults");
    if (!t) return;
    const rows = Object.keys(S.results).map((p) => {
      const { st, scrolls } = S.results[p];
      return `<tr><td>${esc(p)}</td><td class="mono">${st.fps}</td><td class="mono">${st.p50}</td><td class="mono">${st.p95}</td>` +
        `<td class="mono">${st.max}</td><td class="mono">${st.long}</td><td class="mono">${scrolls}</td><td>${verdict(st)}</td></tr>`;
    });
    t.innerHTML = rows.length
      ? "<thead><tr><th>Panel</th><th>fps</th><th>p50 ms</th><th>p95 ms</th><th>max ms</th><th>long</th><th>scrolls</th><th></th></tr></thead><tbody>" + rows.join("") + "</tbody>"
      : "";
  }
  async function fillEnv() {
    const pre = byId("rcEnv");
    if (!pre) return;
    pre.textContent = "reading…";
    const env = await loadEnv();
    pre.textContent = envLines(env).concat(["WebGL renderer: " + glRenderer()]).join("\n");
  }
  function overlay(text) {
    let o = doc.querySelector(".rc-overlay");
    if (!text) { if (o) o.remove(); return; }
    if (!o) { o = doc.createElement("div"); o.id = "rcOverlay"; o.className = "rc-overlay"; o.setAttribute("role", "status"); doc.body.appendChild(o); }
    o.textContent = text;
  }
  function runManual() {
    const sel = byId("rcPanel"), status = byId("rcStatus");
    const panel = sel && sel.value;
    if (!panel || S.run) return;
    if (S.auto) { S.auto.cancel(); S.auto = null; }
    S.run = { cancel() {} }; // claim the sampler before the panel switch
    const back = () => { if (typeof root.showPanel === "function") root.showPanel("debug"); };
    if (typeof root.showPanel === "function") root.showPanel(panel);
    overlay("Render check: scroll this panel now (up and down, ~5 s)…");
    S.run = measure(panel, {
      waitMs: WAIT_SCROLL_MS,
      onStart: () => overlay("Render check: measuring — keep scrolling…"),
      onDone: (st, _n, line) => {
        S.run = null;
        overlay(null);
        back();
        if (status) status.textContent = line ? "" : "No scrolling detected — press Measure and scroll the panel.";
        renderResults();
      },
    });
  }
  function buildCard() {
    const body = doc.querySelector("#panel-debug .panel-body");
    if (!body || body.querySelector("#rcCard")) return;
    const c = doc.createElement("section");
    c.className = "card";
    c.id = "rcCard";
    c.innerHTML =
      '<h2>Render check</h2>' +
      '<p class="hint muted small">How WebKitGTK draws on this machine and how smoothly panels scroll. Pick a panel, press <b>Measure</b>, then scroll it for about 5 seconds. Results also go to the session log.</p>' +
      '<div class="row"><button id="rcEnvRefresh" class="btn">Read environment</button></div>' +
      '<pre id="rcEnv" class="mono log rc-env"></pre>' +
      '<div class="row"><label>Panel <select id="rcPanel" class="inp"></select></label>' +
      '<button id="rcRun" class="btn">Measure scrolling (5 s)</button><span id="rcStatus" class="muted small"></span></div>' +
      '<table id="rcResults" class="tbl rc-results"></table>';
    body.appendChild(c);
    byId("rcEnvRefresh").addEventListener("click", () => { S.envP = null; fillEnv(); });
    byId("rcRun").addEventListener("click", runManual);
  }
  function onDebugShown() {
    buildCard();
    const sel = byId("rcPanel");
    if (sel) {
      sel.innerHTML = "";
      panels().forEach((p) => { const o = doc.createElement("option"); o.value = p.id; o.textContent = p.label; sel.appendChild(o); });
    }
    fillEnv();
    renderResults();
  }

  async function boot() {
    if (!doc) return;
    if (typeof root.onPanelFirstShow === "function") root.onPanelFirstShow("debug", onDebugShown);
    const env = await loadEnv();
    if (!env) return;
    showWarning(env);
    showNotice(env);
    if (env.perf) {
      S.armed = true;
      note("env: " + envLines(env).join("; "));
      note("WebGL renderer: " + glRenderer());
    }
  }

  const OTRender = {
    percentile, deltas, frameStats, verdict, statsLine, compositingDisabled, compositingWarning, envLines, dmabufLine,
    panelSwitched, measure, showWarning, showNotice, boot, _state: S,
  };
  root.OTRender = OTRender;
  if (typeof module !== "undefined" && module.exports) module.exports = OTRender;
  if (doc && root.__TAURI__) boot();
})(typeof window !== "undefined" ? window : globalThis);
