// OpenTune — diagnostics log, frontend capture (backend: src-tauri/src/diaglog.rs,
// docs/DIAGNOSTICS.md). Loaded right after util.js so it is in place before
// the other scripts run:
//
// * window "error" and "unhandledrejection" → diag_log (src "js"): the
//   message, where it happened and a short stack.
// * Every backend command that rejects → diag_log (src "cmd"): the command
//   name and its error text, never its arguments. All panels and plugin
//   overlays call window.__TAURI__.core.invoke, so wrapping that once is the
//   one central place every command's error passes (Tauri 2 gives the Rust
//   side no hook on a command's response; see diaglog.rs).
//
// Messages and stacks are cut short here, the same message again within 10 s
// is counted instead of re-sent, and the backend rate-limits on top. The log
// stays on this machine. The pure parts are exported for the node tests.
"use strict";
(function (root) {
  const MAX_MSG = 500;
  const MAX_STACK = 2000;
  const REPEAT_MS = 10000;
  // Not wrapped: the log's own commands (no loops) and fire-and-forget notes.
  const QUIET = new Set(["diag_log", "diag_log_tail", "diag_log_info", "diag_log_clear", "diag_log_open_folder", "startup_mark", "render_note"]);

  const clip = (s, n) => {
    const t = String(s == null ? "" : s);
    return t.length > n ? t.slice(0, n) + "…" : t;
  };
  // Readable text for whatever was thrown or rejected (Error, string, object).
  function errText(e) {
    if (e == null) return String(e);
    if (typeof e === "string") return e;
    if (typeof e.message === "string" && e.message) return (e.name && e.name !== "Error" ? e.name + ": " : "") + e.message;
    try { const j = JSON.stringify(e); if (j && j !== "{}") return j; } catch (x) { /* fall through */ }
    return String(e);
  }
  const stackOf = (e) => (e && typeof e.stack === "string" ? clip(e.stack, MAX_STACK) : undefined);

  // window "error" (an ErrorEvent).
  function errorRecord(ev) {
    const e = ev && ev.error;
    const ctx = {};
    if (ev && ev.filename) ctx.source = clip(String(ev.filename).replace(/^.*\/\/[^/]*/, ""), 200);
    if (ev && ev.lineno) ctx.line = ev.lineno;
    if (ev && ev.colno) ctx.col = ev.colno;
    const st = stackOf(e);
    if (st) ctx.stack = st;
    return { level: "error", src: "js", msg: clip((ev && ev.message) || errText(e), MAX_MSG), ctx };
  }
  // window "unhandledrejection".
  function rejectionRecord(reason) {
    const ctx = {};
    const st = stackOf(reason);
    if (st) ctx.stack = st;
    return { level: "error", src: "js", msg: clip("unhandled rejection: " + errText(reason), MAX_MSG), ctx };
  }
  // A command that rejected.
  function cmdRecord(cmd, err) {
    return { level: "warn", src: "cmd", msg: clip(`${cmd} failed: ${errText(err)}`, MAX_MSG), ctx: { cmd: clip(cmd, 80) } };
  }

  // Wrap `send`: the same src + msg within REPEAT_MS is counted, and the
  // count goes out (as its own record) before the next different one.
  function makeReporter(send, now) {
    let last = null;
    return function report(rec) {
      const t = now();
      const key = rec.src + "|" + rec.msg;
      if (last && last.key === key && t - last.t < REPEAT_MS) { last.n++; return false; }
      if (last && last.n) send({ level: "info", src: last.src, msg: `previous message repeated ${last.n} more time(s)`, ctx: {} });
      last = { key, src: rec.src, t, n: 0 };
      send(rec);
      return true;
    };
  }

  // A copy of Tauri's (frozen) core namespace whose invoke reports rejected
  // commands. It reads core.invoke at call time, so a replaced invoke still
  // runs.
  function wrapCore(core, report) {
    if (!core || typeof core.invoke !== "function" || core.__otDiag) return core;
    const invoke = function (cmd, args, opts) {
      let p;
      try { p = core.invoke(cmd, args, opts); } catch (e) { if (!QUIET.has(cmd)) report(cmdRecord(cmd, e)); throw e; }
      if (QUIET.has(cmd) || !p || typeof p.then !== "function") return p;
      return p.then(undefined, (e) => { report(cmdRecord(cmd, e)); throw e; });
    };
    return Object.assign({}, core, { invoke, __otDiag: true });
  }

  // Hook a window up: wrap invoke, listen for uncaught errors. Returns the
  // reporter (null without a Tauri backend).
  function install(win) {
    const t = win && win.__TAURI__;
    if (!t || !t.core || t.core.__otDiag) return null;
    const core = t.core;
    const send = (rec) => {
      try {
        const p = core.invoke("diag_log", rec);
        if (p && typeof p.catch === "function") p.catch(() => {});
      } catch (e) { /* never let logging throw */ }
    };
    const report = makeReporter(send, () => Date.now());
    try { t.core = wrapCore(core, report); } catch (e) { /* read-only: errors still logged below */ }
    if (typeof win.addEventListener === "function") {
      win.addEventListener("error", (ev) => { try { report(errorRecord(ev)); } catch (e) { /* ignore */ } });
      win.addEventListener("unhandledrejection", (ev) => { try { report(rejectionRecord(ev && ev.reason)); } catch (e) { /* ignore */ } });
    }
    return report;
  }

  const OTDiag = { errText, errorRecord, rejectionRecord, cmdRecord, makeReporter, wrapCore, install, QUIET, REPEAT_MS };
  root.OTDiag = OTDiag;
  if (typeof module !== "undefined" && module.exports) module.exports = OTDiag;
  if (root.document && root.__TAURI__) OTDiag.report = install(root);
})(typeof window !== "undefined" ? window : globalThis);
