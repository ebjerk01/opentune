// OpenTune — one-time "What's new" after an update (backend:
// src-tauri/src/whatsnew.rs). The notes come from the CHANGELOG compiled into
// the app; nothing is fetched.
//
// This script loads FIRST (before every other script in index.html): at load
// it records whether OpenTune's own localStorage keys already exist. Nothing
// has run yet in this session, so only an earlier session can have written
// them. The backend uses that to tell an update from a release without the
// state file (0.7.x) apart from a fresh install, which never sees the popup.
//
// The popup opens after first paint, closes with "Got it", Esc or a click
// outside, and is recorded as seen on close.
//
// The pure parts are exported for the node tests.
"use strict";
(function (root) {
  const SHOW_FIRST = 10;
  // Keys OpenTune itself writes: ot.nav.collapsed, ot.dash.*, ot.drive.*,
  // ot.insp.*, opentune.recentProjects, opentune.packpicker.*,
  // ot_autodebug_history.
  const OWN_KEY = /^(ot[._]|opentune\.)/;

  // ---- pure -------------------------------------------------------------------
  function hadOwnState(storage) {
    try {
      if (!storage) return false;
      for (let i = 0; i < storage.length; i++) {
        if (OWN_KEY.test(String(storage.key(i) || ""))) return true;
      }
    } catch (e) { /* storage unavailable */ }
    return false;
  }

  const esc = (s) => String(s == null ? "" : s)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  // Escaped text with `inline code` rendered.
  const inline = (s) => esc(s).replace(/`([^`]+)`/g, "<code>$1</code>");

  function itemCount(info) {
    const n = info && info.notes;
    return n ? n.groups.reduce((a, g) => a + g.items.length, 0) : 0;
  }

  function title(info) { return `What's new in v${info.version}`; }

  function subtitle(info) {
    const n = info.notes;
    const from = info.previous ? `Updated from v${info.previous}` : "Updated";
    return n && n.date ? `${from} · released ${n.date}` : from;
  }

  // The notes as HTML: the first `limit` items (all when limit is falsy).
  function bodyHtml(info, limit) {
    const n = info && info.notes;
    if (!n || !n.groups.length) return `<p>Updated to v${esc(info.version)}.</p>`;
    let left = limit || Infinity;
    let out = "";
    for (const g of n.groups) {
      if (left <= 0) break;
      const items = g.items.slice(0, left);
      left -= items.length;
      out += `<h3 class="wn-head">${esc(g.heading)}</h3><ul class="wn-list">` +
        items.map((i) => `<li>${inline(i)}</li>`).join("") + "</ul>";
    }
    return out;
  }

  // ---- runtime ----------------------------------------------------------------
  const doc = root.document;
  const prior = hadOwnState((() => { try { return root.localStorage; } catch (e) { return null; } })());
  const call = (cmd, args) => {
    try {
      const t = root.__TAURI__;
      return t && t.core ? Promise.resolve(t.core.invoke(cmd, args)) : Promise.reject(new Error("no backend"));
    } catch (e) { return Promise.reject(e); }
  };

  function show(info) {
    if (!doc || !info || doc.querySelector(".wn-back")) return null;
    const total = itemCount(info);
    const back = doc.createElement("div");
    back.className = "dgu-back wn-back";
    back.innerHTML =
      '<div class="dgu-modal wn-modal" role="dialog" aria-modal="true" aria-labelledby="wnTitle">' +
      `<h2 id="wnTitle">${esc(title(info))}</h2>` +
      `<p class="muted small">${esc(subtitle(info))}</p>` +
      `<div data-k="body">${bodyHtml(info, SHOW_FIRST)}</div>` +
      '<div class="row wn-actions">' +
      (total > SHOW_FIRST ? `<button type="button" class="btn" data-k="all">Show all (${total - SHOW_FIRST} more)</button>` : "") +
      '<button type="button" class="btn primary" data-k="ok">Got it</button></div></div>';
    doc.body.appendChild(back);
    const q = (k) => back.querySelector(`[data-k="${k}"]`);
    let closed = false;
    const onKey = (ev) => { if (ev && ev.key === "Escape") close(); };
    function close() {
      if (closed) return;
      closed = true;
      back.remove();
      if (doc.removeEventListener) doc.removeEventListener("keydown", onKey);
      call("whatsnew_seen").catch(() => {});
    }
    q("ok").addEventListener("click", close);
    const all = q("all");
    if (all) all.addEventListener("click", () => { q("body").innerHTML = bodyHtml(info, 0); all.remove(); });
    back.addEventListener("click", (ev) => { if (ev && ev.target === back) close(); });
    if (doc.addEventListener) doc.addEventListener("keydown", onKey);
    try { q("ok").focus(); } catch (e) { /* stub DOM */ }
    return { root: back, q, close };
  }

  async function init() {
    const info = await call("whatsnew_check", { priorInstall: prior });
    return info ? show(info) : null;
  }

  const OTWhatsNew = { SHOW_FIRST, hadOwnState, bodyHtml, itemCount, title, subtitle, show, init, get prior() { return prior; } };
  root.OTWhatsNew = OTWhatsNew;
  if (typeof module !== "undefined" && module.exports) module.exports = OTWhatsNew;
  if (doc && root.__TAURI__) {
    // After first paint, out of the startup path.
    const later = () => root.setTimeout(() => { OTWhatsNew.ready = init().catch(() => null); }, 800);
    if (doc.readyState === "complete") later();
    else root.addEventListener("load", later, { once: true });
  }
})(typeof window !== "undefined" ? window : globalThis);
