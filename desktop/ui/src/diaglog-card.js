// OpenTune — Debug panel card "Diagnostics log": the tail of the local
// diagnostics log file (src-tauri/src/diaglog.rs; capture in diaglog.js),
// a level filter, Open folder, Copy last 200 lines and Clear.
// The pure parts are exported for the node tests.
"use strict";
(function (root) {
  const TAIL = 100; // records shown
  const COPY = 200; // records copied
  const LEVELS = ["debug", "info", "warn", "error"];

  // ---- pure -------------------------------------------------------------------
  // One record as a display line: time, level, source, message (+ short ctx).
  function formatRecord(r) {
    if (!r) return "";
    const ts = String(r.ts || "").replace("T", " ").replace(/Z$/, "");
    let line = `${ts}  ${String(r.level || "?").toUpperCase().padEnd(5)}  ${String(r.src || "?").padEnd(7)}  ${r.msg == null ? "" : r.msg}`;
    const c = r.ctx;
    if (c && typeof c === "object") {
      if (c.location) line += `  (${c.location})`;
      else if (c.source) line += `  (${c.source}${c.line ? ":" + c.line : ""})`;
    }
    return line;
  }
  // The text "Copy last 200 lines" puts on the clipboard: the JSON lines, as
  // they are in the file.
  function copyText(records) {
    return (records || []).map((r) => JSON.stringify(r)).join("\n");
  }
  // "1.2 MiB in 2 files — /path"
  function infoLine(info) {
    if (!info) return "diagnostics log unavailable";
    if (info.state !== "on") return "The diagnostics log is off for this session (see the session log for why).";
    const kib = (info.bytes || 0) / 1024;
    const size = kib >= 1024 ? (kib / 1024).toFixed(1) + " MiB" : Math.round(kib) + " KiB";
    return `${size} in ${info.files} file${info.files === 1 ? "" : "s"} — ${info.file || info.dir || ""}`;
  }

  // ---- runtime ------------------------------------------------------------------
  const doc = root.document;
  const byId = (id) => (doc ? doc.getElementById(id) : null);
  const call = (cmd, args) => {
    try {
      const t = root.__TAURI__;
      return t && t.core ? Promise.resolve(t.core.invoke(cmd, args)) : Promise.reject(new Error("no backend"));
    } catch (e) { return Promise.reject(e); }
  };
  const status = (text) => { const s = byId("dlStatus"); if (s) s.textContent = text || ""; };
  const errText = (e) => (root.OTDiag ? root.OTDiag.errText(e) : String(e));

  async function refresh() {
    const pre = byId("dlTail"), sel = byId("dlLevel"), info = byId("dlInfo");
    if (!pre) return;
    const level = sel && sel.value ? sel.value : "debug";
    try {
      const [recs, inf] = await Promise.all([call("diag_log_tail", { limit: TAIL, level }), call("diag_log_info")]);
      pre.textContent = (recs || []).map(formatRecord).join("\n") || "(no records at this level)";
      if (info) info.textContent = infoLine(inf);
      pre.scrollTop = pre.scrollHeight;
    } catch (e) { pre.textContent = "cannot read the diagnostics log: " + errText(e); }
  }

  async function writeClipboard(text) {
    const nav = root.navigator;
    if (nav && nav.clipboard && typeof nav.clipboard.writeText === "function") return nav.clipboard.writeText(text);
    // Fallback: a hidden textarea + execCommand (older WebKitGTK).
    const ta = doc.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed"; ta.style.opacity = "0";
    doc.body.appendChild(ta);
    try {
      if (typeof ta.select === "function") ta.select();
      if (!(typeof doc.execCommand === "function" && doc.execCommand("copy"))) throw new Error("clipboard unavailable");
    } finally { ta.remove(); }
  }

  async function copyLast() {
    try {
      const recs = await call("diag_log_tail", { limit: COPY });
      await writeClipboard(copyText(recs));
      status(`Copied ${(recs || []).length} line(s). Review them before sharing.`);
    } catch (e) { status("Copy failed: " + errText(e)); }
  }

  async function openFolder() {
    try { const p = await call("diag_log_open_folder"); status("Opened " + p); } catch (e) { status("Cannot open the folder: " + errText(e)); }
  }

  async function clearLog() {
    if (!root.confirm("Clear the diagnostics log? Every record on this machine is deleted (the rotated files too); this session's header is kept.")) return;
    try { await call("diag_log_clear"); status("Cleared."); } catch (e) { status("Clear failed: " + errText(e)); }
    refresh();
  }

  function buildCard() {
    const body = doc.querySelector("#panel-debug .panel-body");
    if (!body || body.querySelector("#dlCard")) return;
    const c = doc.createElement("section");
    c.className = "card";
    c.id = "dlCard";
    c.innerHTML =
      '<h2>Diagnostics log</h2>' +
      '<p class="hint muted small">Errors, crashes, failed commands and startup steps, kept in a local file on this machine. It leaves the machine only through the Diagnostics card (shown when this build has an upload target), redacted and after a preview. SecurityAccess seeds and keys are masked and long hex payloads cut before they are written; file contents are never recorded.</p>' +
      '<div class="row"><label>Level <select id="dlLevel" class="inp">' +
      '<option value="debug">all</option><option value="info">info and up</option><option value="warn">warnings and errors</option><option value="error">errors only</option>' +
      '</select></label>' +
      '<button id="dlRefresh" class="btn">Refresh</button>' +
      '<button id="dlOpen" class="btn">Open folder</button>' +
      '<button id="dlCopy" class="btn">Copy last 200 lines</button>' +
      '<button id="dlClear" class="btn">Clear…</button></div>' +
      '<div class="row"><span id="dlInfo" class="mono muted small"></span><span id="dlStatus" class="muted small"></span></div>' +
      '<pre id="dlTail" class="mono log"></pre>';
    body.appendChild(c);
    byId("dlLevel").addEventListener("change", refresh);
    byId("dlRefresh").addEventListener("click", refresh);
    byId("dlOpen").addEventListener("click", openFolder);
    byId("dlCopy").addEventListener("click", copyLast);
    byId("dlClear").addEventListener("click", clearLog);
  }
  function onDebugShown() {
    buildCard();
    refresh();
  }

  const OTDiagCard = { formatRecord, copyText, infoLine, buildCard, refresh, LEVELS };
  root.OTDiagCard = OTDiagCard;
  if (typeof module !== "undefined" && module.exports) module.exports = OTDiagCard;
  if (doc && root.__TAURI__ && typeof root.onPanelFirstShow === "function") root.onPanelFirstShow("debug", onDebugShown);
})(typeof window !== "undefined" ? window : globalThis);
