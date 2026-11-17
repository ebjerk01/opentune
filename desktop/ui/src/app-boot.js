// OpenTune — app-wide adapter status, the Debug panel, the sidebar connection
// bar and boot(). Loaded last of the app scripts (see app-core.js); feature
// panels (feat_*.js) load after it.
"use strict";

// ---- adapter status (app-wide) ---------------------------------------------
// The global connection + capability framework lives in ot-caps.js (window.OT).
// Here we just mirror the Debug tab's connection inputs into OT.conn and reflect
// the probe result into the Debug tab's detail line; OT owns the sidebar badge.
// Every feature panel reads the shared window.OT.conn.
function syncConnFromDebug() {
  if (!window.OT) return;
  const g = (id, d) => { const n = $(id); return n ? n.value : d; };
  window.OT.setConn({
    driver: g("dbgDriver", "elm327obd") || "elm327obd",
    port: g("dbgPort", ""),
    baud: g("dbgBaud", ""),
    bus: g("dbgBus", "hs"),
  });
}
async function refreshAdapterStatus() {
  syncConnFromDebug();
  const st = window.OT ? await window.OT.probe() : null;
  const dbg = $("dbgAdapter");
  if (dbg) {
    if (st && st.connected) {
      const fw = st.firmware ? " " + st.firmware : "";
      const v = st.battery_mv != null ? ` · ${(st.battery_mv / 1000).toFixed(1)}V` : "";
      dbg.textContent = `adapter: ${st.name || st.driver}${fw}${v}`;
    } else {
      dbg.textContent = "adapter: not found" + (st && st.error ? " (" + st.error + ")" : "");
    }
  }
  return st;
}

// ---- debug panel (alpha only) ----------------------------------------------
async function dbgRefreshLog() {
  try {
    const lines = await invoke("debug_log_dump");
    $("dbgLog").textContent = lines.join("\n");
    $("dbgLog").scrollTop = $("dbgLog").scrollHeight;
  } catch (e) { $("dbgLog").textContent = String(e); }
}
async function initDebug() {
  let channel = "stable";
  try { channel = await invoke("build_channel"); } catch (e) {}
  // The Debug tab is a Bench-tier entry: nav.js shows it only on alpha/bench builds.
  if (channel !== "alpha" && channel !== "bench") return;

  // Populate the driver dropdown.
  try {
    const list = await driverList();
    const sel = $("dbgDriver");
    sel.innerHTML = "";
    list.forEach((d) => { const o = el("option"); o.value = d.id; o.textContent = `${d.id} — ${d.name}`; sel.appendChild(o); });
    sel.value = "elm327obd";
  } catch (e) {}

  // Keep the global connection in sync as the user edits the inputs, so every
  // feature panel uses the current driver/port/baud/bus.
  ["dbgDriver", "dbgPort", "dbgBaud", "dbgBus"].forEach((id) => {
    const n = $(id);
    if (n) n.addEventListener("change", syncConnFromDebug);
  });
  syncConnFromDebug();

  $("dbgSend").addEventListener("click", async () => {
    syncConnFromDebug();
    const args = {
      ...window.OT.connArgs(),
      request: $("dbgReq").value,
      timeoutMs: parseInt($("dbgTimeout").value || "1000", 10),
    };
    $("dbgResp").textContent = "…";
    try { $("dbgResp").textContent = await invoke("debug_raw_request", args); }
    catch (e) { $("dbgResp").textContent = "ERROR: " + e; }
    dbgRefreshLog();
  });

  $("dbgSimRun").addEventListener("click", async () => {
    try {
      await invoke("debug_simulate", {
        dids: splitList($("dbgSimDids").value),
        requests: splitList($("dbgSimReqs").value),
        requireSecurityWrite: false,
      });
    } catch (e) { $("dbgResp").textContent = "sim ERROR: " + e; }
    dbgRefreshLog();
  });

  $("dbgDetect").addEventListener("click", () => refreshAdapterStatus());

  $("dbgLogRefresh").addEventListener("click", dbgRefreshLog);
  $("dbgLogClear").addEventListener("click", async () => { try { await invoke("debug_log_clear"); } catch (e) {} dbgRefreshLog(); });
  $("dbgLogSave").addEventListener("click", async () => {
    try {
      const lines = await invoke("debug_log_dump");
      const blob = new Blob([lines.join("\n")], { type: "text/plain" });
      const a = el("a"); a.href = URL.createObjectURL(blob);
      a.download = "opentune-debug-log.txt"; a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    } catch (e) {}
  });
}

// ---- global connection bar (every panel) -----------------------------------
// Always-visible adapter connection control in the sidebar, writing to the
// shared window.OT.conn so every feature panel uses it. Mirrors into the Debug
// tab inputs when present.
async function initConnBar() {
  const foot = document.querySelector(".sidebar-foot");
  if (!foot || !window.OT) return;
  const bar = el("div", "conn-bar");
  bar.innerHTML =
    '<div class="nav-group-label">Adapter</div>' +
    '<div class="conn-row"><select id="connDriver" class="inp" title="interface driver"></select></div>' +
    '<div class="conn-row"><input id="connPort" class="inp" placeholder="/dev/ttyUSB0" title="serial port">' +
    '<select id="connBus" class="inp" title="CAN bus — match the dongle HS/MS switch"><option value="hs">HS</option><option value="ms">MS</option></select></div>' +
    '<div class="conn-row" title="Target module CAN request / response ids (hex). Blank = use the definition\'s req_id/resp_id; reads then default to the engine (7E0/7E8). Writes, routines and service runs are refused without a known target.">' +
    '<input id="connTx" class="inp mono" placeholder="tx 7E0" size="5">' +
    '<input id="connRx" class="inp mono" placeholder="rx 7E8" size="5"></div>' +
    '<div class="conn-row"><button id="connDetect" class="btn">Detect adapter</button></div>';
  foot.insertBefore(bar, $("adapterBadge"));
  try {
    const list = await driverList();
    const sel = $("connDriver"); sel.innerHTML = "";
    list.forEach((d) => { const o = el("option"); o.value = d.id; o.textContent = d.id; sel.appendChild(o); });
    sel.value = window.OT.conn.driver || "elm327obd";
  } catch (e) {}
  $("connPort").value = window.OT.conn.port || "";
  $("connBus").value = window.OT.conn.bus || "hs";
  const push = () => {
    window.OT.setConn({
      driver: $("connDriver").value, port: $("connPort").value, bus: $("connBus").value,
      txId: $("connTx").value, rxId: $("connRx").value,
    });
    if ($("dbgDriver")) $("dbgDriver").value = $("connDriver").value;
    if ($("dbgPort")) $("dbgPort").value = $("connPort").value;
    if ($("dbgBus")) $("dbgBus").value = $("connBus").value;
  };
  $("connTx").value = window.OT.conn.txId || "";
  $("connRx").value = window.OT.conn.rxId || "";
  ["connDriver", "connPort", "connBus", "connTx", "connRx"].forEach((id) => $(id).addEventListener("change", push));
  $("connDetect").addEventListener("click", () => { push(); window.OT.probe(); });
}

// Feature panels (feat_*.js) are loaded after this script and call
// window.showPanel to switch to their injected sections.
window.showPanel = showPanel;

// =================== BOOT ===================
async function boot() {
  if (!TAURI.core) { document.body.innerHTML = '<p style="padding:2rem">Tauri API unavailable — run inside the OpenTune app.</p>'; return; }
  // 1. Paint the shell now: the first panel is visible before any backend call
  //    returns, and nothing below blocks it.
  showPanel("image");
  refreshImageHints();
  renderRecent();
  renderProject();
  requestAnimationFrame(() => requestAnimationFrame(() => startupMark("first frame painted")));

  // 2. Fill the panels concurrently; each init handles its own errors. The
  //    Debug inputs and the sidebar connection bar share OT.conn, so those two
  //    stay in order, and the adapter probe follows them.
  const dataPaths = (async () => {
    try {
      const dp = await invoke("data_paths");
      $("dataStatus").innerHTML = `ecudefs: ${dp.ecudefs ? "✓" : "✗"} · catalog: ${dp.catalog ? "✓" : "✗"} · guide: ${dp.guide ? "✓" : "✗"} · packs: ${dp.packs ? "✓" : "✗"}`;
      // Feature panels open their file pickers in the bundled definition packs.
      window.OT = window.OT || {};
      window.OT.packsDir = dp.packs || null;
    } catch (e) { $("dataStatus").textContent = "data resolve failed"; }
  })();
  const conn = (async () => {
    await initDebug();
    await initConnBar();
    // App-wide adapter probe (fire-and-forget so it never delays the UI).
    refreshAdapterStatus();
  })();
  await Promise.allSettled([dataPaths, initDrivers(), initSeedKey(), initUpdates(), initChecksumAlgos(), conn]);
  startupMark("boot done");
  try {
    // For the startup trace: the slowest page resources (scripts, CSS).
    const slow = performance.getEntriesByType("resource")
      .sort((a, b) => b.duration - a.duration).slice(0, 4)
      .map((r) => `${r.name.split("/").pop()} ${Math.round(r.startTime)}+${Math.round(r.duration)} ms`).join(", ");
    if (slow) startupMark("slowest resources: " + slow);
  } catch (e) {}

  // 3. Heavier, not-yet-visible work in idle time: the definition lists (Maps /
  //    Compare load them on first show anyway). The guide loads on first open.
  const idle = window.requestIdleCallback || ((fn) => setTimeout(fn, 300));
  idle(() => ensureDefs(), { timeout: 2000 });
}
boot();
