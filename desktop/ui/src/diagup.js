// OpenTune — diagnostics upload UI (backend: src-tauri/src/diagupload/,
// docs/DIAGNOSTICS.md). The public app has no upload target: everything
// here stays hidden unless this build (an embedder) or OPENTUNE_DIAG_ENDPOINT
// configures one, or OPENTUNE_DIAG=off says why it is off.
//
// * "Diagnostics" card in About & updates (the settings) and in the Debug
//   panel: mode and host, "Allow sending diagnostics", the automatic-mode
//   off switch, "Send diagnostics…" and the upload history (each entry can
//   show the exact payload sent).
// * Send dialog: an optional note, Preview (the exact redacted payload, as
//   it will be sent), then Send. A changed note needs a new preview.
// * Automatic mode (embedder builds only): a one-time notice ("This build
//   sends redacted diagnostics automatically to <host>" — Turn off / What is
//   sent) and a sidebar-footer indicator "Diagnostics: auto → <host>" that
//   opens the settings.
// * Manual mode after a crash: the next start only offers to send.
//
// The pure parts are exported for the node tests.
"use strict";
(function (root) {
  const WHAT_IS_SENT =
    "Only this machine's diagnostics log (errors, crashes, failed commands, startup steps) and a summary: " +
    "app version, build channel, OS and WebKit version, render setting, error counts and how often each panel was opened, " +
    "plus your note. Before anything is shown or sent, VINs, part and serial numbers, file paths, user and host names, " +
    "IP and MAC addresses, e-mail addresses and SecurityAccess seeds and keys are replaced by markers such as <VIN> and <PATH>, " +
    "and long hex payloads are cut. Never sent: flash images, datalogs, definitions, CAN captures or any other file you opened.";

  // ---- pure -------------------------------------------------------------------
  function fmtBytes(n) {
    const b = Number(n) || 0;
    if (b < 1024) return b + " B";
    if (b < 1024 * 1024) return (b / 1024).toFixed(1) + " KiB";
    return (b / 1024 / 1024).toFixed(2) + " MiB";
  }
  const isAuto = (st) => !!(st && st.mode === "auto" && !st.env_off);
  // Sidebar footer: only for an embedder's automatic mode.
  function indicatorText(st) {
    if (!isAuto(st)) return null;
    return st.auto_active ? `Diagnostics: auto → ${st.host}` : "Diagnostics: auto (off)";
  }
  const shouldShowNotice = (st) => isAuto(st) && !!st.notice_needed;
  const noticeText = (st) => `This build sends redacted diagnostics automatically to ${st && st.host}.`;
  function crashOfferText(st) {
    const c = st && st.crash_offer;
    if (!c || st.mode !== "manual") return null;
    const why = c.reason === "panic" ? "it crashed" : "it did not finish starting";
    return `OpenTune closed unexpectedly last time (${why}). You can review the diagnostics and send them to ${st.host}.`;
  }
  // Is there anything to show at all?
  const visible = (st) => !!(st && (st.mode !== "none" || st.env_off || (st.notes && st.notes.length)));
  function modeLine(st) {
    if (!st) return "";
    if (st.env_off) return "Diagnostics upload is turned off (OPENTUNE_DIAG=off). Nothing is sent.";
    if (st.mode === "none") return "No diagnostics upload is configured in this build. Nothing is sent.";
    const from = st.source === "env" ? "from OPENTUNE_DIAG_ENDPOINT" : "set by this build";
    const label = st.label ? ` — ${st.label}` : "";
    if (st.mode === "manual") return `Manual: diagnostics go to ${st.host} only when you send them (${from})${label}.`;
    return `Automatic: this build sends redacted diagnostics to ${st.host}${st.auto_active ? "" : " — turned off"}${label}.`;
  }
  function policyText(st) {
    const p = st && st.policy;
    if (!p) return "";
    const what = [];
    if (p.crash_on_next_start) what.push("a crash report at the next start");
    if (p.on_error) what.push("new errors (at most one report per 10 minutes)");
    if (p.periodic_min) what.push(`a session summary every ${p.periodic_min} min when something happened`);
    return what.length ? "Sends: " + what.join("; ") + ". At most 20 uploads a day." : "";
  }
  // Newest first, ready to display.
  function historyRows(h) {
    return (h || []).slice().reverse().map((e) => ({
      id: e.id,
      time: String(e.ts || "").replace("T", " ").replace(/\.\d+Z$/, " UTC"),
      trigger: e.trigger,
      size: fmtBytes(e.bytes),
      status: e.status,
    }));
  }
  // Send dialog buttons.
  function dialogState(st, s) {
    const allowed = !!(st && st.configured && st.allow_send && !st.env_off);
    return {
      canPreview: allowed && !s.busy,
      canSend: allowed && !s.busy && !!s.previewId && !s.noteChanged,
    };
  }

  // ---- runtime ----------------------------------------------------------------
  const doc = root.document;
  let status = null;
  const call = (cmd, args) => {
    try {
      const t = root.__TAURI__;
      return t && t.core ? Promise.resolve(t.core.invoke(cmd, args)) : Promise.reject(new Error("no backend"));
    } catch (e) { return Promise.reject(e); }
  };
  const errText = (e) => (root.OTDiag ? root.OTDiag.errText(e) : String(e));
  const checked = (ev, el) => !!(ev && ev.target && "checked" in ev.target ? ev.target.checked : el.checked);
  const set = (args) => call("diag_upload_set", args).then((st) => { status = st; render(); return st; });

  async function refresh() {
    status = await call("diag_upload_status");
    render();
    return status;
  }

  function modal(html, cls) {
    const back = doc.createElement("div");
    back.className = "dgu-back";
    back.innerHTML = `<div class="dgu-modal ${cls || ""}" role="dialog" aria-modal="true">${html}</div>`;
    doc.body.appendChild(back);
    const q = (k) => back.querySelector(`[data-k="${k}"]`);
    return { root: back, q, close: () => back.remove() };
  }

  // -- sidebar indicator
  function renderIndicator() {
    const foot = doc.querySelector(".sidebar-foot");
    if (!foot) return;
    let el = foot.querySelector("#diagUpInd");
    const text = indicatorText(status);
    if (!text) { if (el) el.hidden = true; return; }
    if (!el) {
      el = doc.createElement("button");
      el.id = "diagUpInd";
      el.type = "button";
      el.className = "dgu-ind";
      el.title = "Diagnostics upload settings";
      el.addEventListener("click", openSettings);
      foot.appendChild(el);
    }
    el.hidden = false;
    el.textContent = text;
    el.classList.toggle("off", !status.auto_active);
  }
  function openSettings() {
    if (typeof root.showPanel === "function") root.showPanel("about");
    const c = doc.querySelector("#panel-about .dgu-card");
    if (c && typeof c.scrollIntoView === "function") c.scrollIntoView();
  }

  // -- settings cards (About & updates; Debug)
  const cards = [];
  function cardHtml() {
    return '<h2>Diagnostics</h2>' +
      '<p class="dgu-mode" data-k="mode"></p>' +
      '<p class="muted small" data-k="policy"></p>' +
      '<div class="row dgu-controls">' +
      '<label data-k="autoWrap"><input type="checkbox" data-k="auto"> Send diagnostics automatically</label>' +
      '<label data-k="allowWrap"><input type="checkbox" data-k="allow"> Allow sending diagnostics</label>' +
      '<button class="btn" data-k="send">Send diagnostics…</button>' +
      '<button class="btn" data-k="what">What is sent</button></div>' +
      '<p class="muted small" data-k="whatText" hidden></p>' +
      '<p class="muted small" data-k="notes"></p>' +
      '<p class="muted small" data-k="queue"></p>' +
      '<table class="dgu-hist" data-k="hist"></table>' +
      '<span class="muted small" data-k="msg"></span>';
  }
  function buildCard(host) {
    if (!host || host.querySelector(".dgu-card")) return null;
    const c = doc.createElement("section");
    c.className = "card dgu-card";
    c.innerHTML = cardHtml();
    host.appendChild(c);
    const q = (k) => c.querySelector(`[data-k="${k}"]`);
    q("auto").addEventListener("change", (ev) => {
      set({ autoOff: !checked(ev, q("auto")) }).catch((e) => { q("msg").textContent = errText(e); });
    });
    q("allow").addEventListener("change", (ev) => {
      set({ allowSend: checked(ev, q("allow")) }).catch((e) => { q("msg").textContent = errText(e); });
    });
    q("send").addEventListener("click", () => openSendDialog(false));
    q("what").addEventListener("click", () => { const w = q("whatText"); w.hidden = !w.hidden; });
    q("whatText").textContent = WHAT_IS_SENT;
    const card = { el: c, q };
    cards.push(card);
    renderCard(card);
    return card;
  }
  function renderCard(card) {
    const st = status;
    const { el, q } = card;
    el.hidden = !visible(st);
    if (!st) return;
    q("mode").textContent = modeLine(st);
    q("policy").textContent = st.mode === "auto" ? policyText(st) : "";
    const configured = !!st.configured && !st.env_off;
    q("autoWrap").hidden = st.mode !== "auto" || st.env_off;
    q("auto").checked = !st.auto_off;
    q("allowWrap").hidden = !configured;
    q("allow").checked = !!st.allow_send;
    q("send").hidden = !configured;
    q("send").disabled = !st.allow_send;
    q("notes").textContent = (st.notes || []).join(" · ");
    const qu = st.queue || {};
    q("queue").textContent = qu.items
      ? `${qu.items} upload(s) waiting offline (${fmtBytes(qu.bytes)}), retried at the next allowed slot${st.next_slot ? " (" + st.next_slot + ")" : ""}.`
      : "";
    renderHistory(q("hist"), st.history);
  }
  function renderHistory(table, history) {
    const rows = historyRows(history);
    table.innerHTML = rows.length
      ? "<tr><th>Time</th><th>Trigger</th><th>Size</th><th>Status</th><th></th></tr>" +
        rows.map((_, i) => `<tr><td data-c="time"></td><td data-c="trigger"></td><td data-c="size"></td><td data-c="status"></td><td><button class="btn small" data-row="${i}">Show payload</button></td></tr>`).join("")
      : "";
    const trs = Array.from(table.querySelectorAll("tr")).slice(1);
    trs.forEach((tr, i) => {
      for (const k of ["time", "trigger", "size", "status"]) {
        const td = tr.querySelector(`[data-c="${k}"]`);
        if (td) td.textContent = rows[i][k];
      }
      const b = tr.querySelector("button");
      if (b) b.addEventListener("click", () => showPayload(rows[i].id));
    });
  }
  function render() {
    renderIndicator();
    cards.forEach(renderCard);
  }

  async function showPayload(id) {
    const m = modal('<h2>Payload sent</h2><p class="muted small">Exactly what was uploaded (or queued).</p><pre class="mono dgu-pre" data-k="pre"></pre><div class="row"><button class="btn" data-k="close">Close</button></div>');
    m.q("close").addEventListener("click", m.close);
    try { m.q("pre").textContent = await call("diag_upload_payload", { id }); } catch (e) { m.q("pre").textContent = errText(e); }
    return m;
  }

  // -- send dialog (manual)
  function openSendDialog(crash) {
    const m = modal(
      '<h2 data-k="title"></h2>' +
      '<p class="muted small" data-k="what"></p>' +
      '<label class="dgu-allow"><input type="checkbox" data-k="allow"> Allow sending diagnostics</label>' +
      '<label class="dgu-note">Note (optional) <textarea class="inp" rows="3" data-k="note" placeholder="What were you doing?"></textarea></label>' +
      '<div class="row"><button class="btn" data-k="preview">Preview</button>' +
      '<button class="btn primary" data-k="send">Send</button>' +
      '<button class="btn" data-k="close">Cancel</button>' +
      '<span class="muted small" data-k="msg"></span></div>' +
      '<pre class="mono dgu-pre" data-k="pre" hidden></pre>', "dgu-send");
    const s = { previewId: null, noteChanged: false, busy: false };
    const q = m.q;
    q("title").textContent = `Send diagnostics to ${status && status.host}`;
    q("what").textContent = WHAT_IS_SENT;
    q("allow").checked = !!(status && status.allow_send);
    const sync = () => {
      const d = dialogState(status, s);
      q("preview").disabled = !d.canPreview;
      q("send").disabled = !d.canSend;
    };
    q("allow").addEventListener("change", (ev) => {
      set({ allowSend: checked(ev, q("allow")) }).then(sync, (e) => { q("msg").textContent = errText(e); });
    });
    q("note").addEventListener("input", () => {
      if (s.previewId) { s.noteChanged = true; q("msg").textContent = "The note changed: preview again before sending."; }
      sync();
    });
    q("preview").addEventListener("click", async () => {
      s.busy = true; sync();
      try {
        const p = await call("diag_upload_preview", { note: q("note").value || null, crash: !!crash });
        s.previewId = p.id; s.noteChanged = false;
        q("pre").hidden = false;
        q("pre").textContent = p.text;
        q("msg").textContent = `${fmtBytes(p.bytes)} — exactly this text is sent.`;
      } catch (e) { q("msg").textContent = errText(e); }
      s.busy = false; sync();
    });
    q("send").addEventListener("click", async () => {
      if (!s.previewId || s.noteChanged) return;
      s.busy = true; sync();
      try {
        const h = await call("diag_upload_send", { id: s.previewId });
        q("msg").textContent = `Sent (${fmtBytes(h.bytes)}). Thank you.`;
      } catch (e) {
        q("msg").textContent = errText(e);
      }
      s.previewId = null;
      s.busy = false; sync();
      refresh().catch(() => {});
    });
    q("close").addEventListener("click", m.close);
    sync();
    return { modal: m, state: s, sync };
  }

  // -- automatic mode: one-time notice
  function showNotice() {
    const m = modal(
      '<h2>Diagnostics</h2><p data-k="text"></p>' +
      '<p class="muted small" data-k="what" hidden></p>' +
      '<div class="row"><button class="btn primary" data-k="ok">OK</button>' +
      '<button class="btn" data-k="off">Turn off</button>' +
      '<button class="btn" data-k="whatBtn">What is sent</button></div>', "dgu-notice");
    m.q("text").textContent = noticeText(status) + " You can turn this off here or later under About & updates → Diagnostics.";
    m.q("what").textContent = WHAT_IS_SENT;
    // Shown: automatic sending may start (the user has had the chance to turn it off).
    set({ noticeSeen: true }).catch(() => {});
    m.q("ok").addEventListener("click", m.close);
    m.q("off").addEventListener("click", () => { set({ autoOff: true }).catch(() => {}); m.close(); });
    m.q("whatBtn").addEventListener("click", () => { m.q("what").hidden = false; });
    return m;
  }

  // -- manual mode: crash offer
  function showCrashOffer() {
    const text = crashOfferText(status);
    if (!text) return null;
    const m = modal(
      '<h2>OpenTune closed unexpectedly</h2><p data-k="text"></p>' +
      '<div class="row"><button class="btn primary" data-k="review">Review and send…</button>' +
      '<button class="btn" data-k="later">Not now</button>' +
      '<button class="btn" data-k="dismiss">Don\'t offer again</button></div>', "dgu-offer");
    m.q("text").textContent = text + " Nothing is sent unless you do.";
    m.q("review").addEventListener("click", () => { m.close(); openSendDialog(true); });
    m.q("later").addEventListener("click", m.close);
    m.q("dismiss").addEventListener("click", () => { set({ dismissCrash: true }).catch(() => {}); m.close(); });
    return m;
  }

  // A panel was opened (counted for the automatic session summary).
  function panelOpened(name) {
    if (!status || status.mode === "none" || status.env_off) return;
    call("diag_upload_panel", { panel: String(name) }).catch(() => {});
  }

  async function init() {
    const st = await refresh();
    const about = doc.querySelector("#panel-about");
    if (about && visible(st)) buildCard(about);
    if (shouldShowNotice(st)) showNotice();
    else if (crashOfferText(st)) showCrashOffer();
    return st;
  }

  const OTDiagUp = {
    WHAT_IS_SENT, fmtBytes, indicatorText, shouldShowNotice, noticeText, crashOfferText, modeLine, policyText,
    historyRows, dialogState, visible,
    init, refresh, buildCard, openSendDialog, showNotice, showCrashOffer, showPayload, panelOpened,
    get status() { return status; },
  };
  root.OTDiagUp = OTDiagUp;
  if (typeof module !== "undefined" && module.exports) module.exports = OTDiagUp;
  if (doc && root.__TAURI__) {
    const ready = init().catch(() => null);
    OTDiagUp.ready = ready;
    if (typeof root.onPanelFirstShow === "function") {
      root.onPanelFirstShow("debug", () => {
        ready.then(() => {
          const body = doc.querySelector("#panel-debug .panel-body");
          if (body && visible(status)) buildCard(body);
        });
      });
    }
  }
})(typeof window !== "undefined" ? window : globalThis);
