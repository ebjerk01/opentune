// Diagnostics upload UI (src/diagup.js): the settings card, the sidebar
// indicator, the automatic-mode notice, the crash offer and the
// preview-then-send dialog. The host names are reserved (.invalid).
import test from "node:test";
import assert from "node:assert/strict";
import dom from "./support/dom.cjs";
import "../src/diagup.js";

const U = globalThis.OTDiagUp;
const { makeWindow, indexScripts, load } = dom;
const tick = async (n = 12) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

const BASE = {
  configured: true, source: "embedder", host: "diag.example.invalid", label: null, policy: null, env_off: false, insecure: false,
  allow_send: false, auto_off: false, auto_active: false, notice_needed: false, crash_offer: null, history: [],
  queue: { items: 0, bytes: 0, cap: 2097152 }, next_slot: null, notes: [],
};
const NONE = { ...BASE, configured: false, mode: "none", source: null, host: null };
const AUTO = {
  ...BASE, mode: "auto", label: "bench build", auto_active: true, notice_needed: true,
  policy: { crash_on_next_start: true, on_error: true, periodic_min: 30 },
};
const MANUAL = { ...BASE, mode: "manual", source: "env" };

test("indicator, notice and offer texts", () => {
  assert.equal(U.indicatorText(AUTO), "Diagnostics: auto → diag.example.invalid");
  assert.equal(U.indicatorText({ ...AUTO, auto_active: false, auto_off: true }), "Diagnostics: auto (off)");
  assert.equal(U.indicatorText(MANUAL), null, "no indicator in manual mode");
  assert.equal(U.indicatorText(NONE), null);
  assert.equal(U.indicatorText({ ...AUTO, env_off: true }), null);
  assert.equal(U.shouldShowNotice(AUTO), true);
  assert.equal(U.shouldShowNotice({ ...AUTO, notice_needed: false }), false);
  assert.equal(U.shouldShowNotice({ ...MANUAL, notice_needed: true }), false);
  assert.equal(U.noticeText(AUTO), "This build sends redacted diagnostics automatically to diag.example.invalid.");
  const crash = { session: "2026-11-19T10:00:00.000Z", reason: "panic" };
  assert.match(U.crashOfferText({ ...MANUAL, crash_offer: crash }), /closed unexpectedly last time \(it crashed\)/);
  assert.match(U.crashOfferText({ ...MANUAL, crash_offer: { ...crash, reason: "no first frame" } }), /did not finish starting/);
  assert.equal(U.crashOfferText({ ...AUTO, crash_offer: crash }), null, "auto mode reports, it does not offer");
  assert.match(U.modeLine(MANUAL), /^Manual: .* only when you send them \(from OPENTUNE_DIAG_ENDPOINT\)/);
  assert.match(U.modeLine(AUTO), /^Automatic: .*diag\.example\.invalid — bench build\.$/);
  assert.match(U.modeLine({ ...AUTO, env_off: true }), /OPENTUNE_DIAG=off/);
  assert.match(U.policyText(AUTO), /crash report.*new errors.*every 30 min/);
  assert.equal(U.visible(NONE), false, "the public app shows nothing");
  assert.equal(U.visible({ ...NONE, env_off: true }), true);
  assert.match(U.WHAT_IS_SENT, /Never sent: flash images, datalogs, definitions, CAN captures/);
});

test("history rows and the send dialog gate", () => {
  const rows = U.historyRows([
    { id: "1", ts: "2026-11-19T10:00:00.123Z", trigger: "manual", bytes: 2048, status: "sent (200)" },
    { id: "2", ts: "2026-11-19T11:00:00.000Z", trigger: "error", bytes: 10, status: "queued: timed out" },
  ]);
  assert.deepEqual(rows.map((r) => r.id), ["2", "1"], "newest first");
  assert.equal(rows[1].time, "2026-11-19 10:00:00 UTC");
  assert.equal(rows[1].size, "2.0 KiB");
  const s = { previewId: null, noteChanged: false, busy: false };
  assert.deepEqual(U.dialogState({ ...MANUAL, allow_send: false }, s), { canPreview: false, canSend: false });
  assert.deepEqual(U.dialogState({ ...MANUAL, allow_send: true }, s), { canPreview: true, canSend: false });
  assert.deepEqual(U.dialogState({ ...MANUAL, allow_send: true }, { ...s, previewId: "p" }), { canPreview: true, canSend: true });
  assert.equal(U.dialogState({ ...MANUAL, allow_send: true }, { ...s, previewId: "p", noteChanged: true }).canSend, false);
  assert.equal(U.dialogState(NONE, { ...s, previewId: "p" }).canSend, false);
  assert.equal(U.dialogState({ ...MANUAL, allow_send: true, env_off: true }, { ...s, previewId: "p" }).canSend, false);
});

// The whole app on a DOM stub with a fake backend.
function app(map) {
  const calls = [];
  const tauri = {
    core: {
      invoke: (cmd, args) => {
        calls.push({ cmd, args });
        return cmd in map ? Promise.resolve().then(() => map[cmd](args)) : new Promise(() => {});
      },
    },
    dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
    event: { listen: () => new Promise(() => {}) },
  };
  const env = makeWindow({ tauri });
  const d = env.document;
  const add = (parent, tag, props) => { const e = d.createElement(tag); Object.assign(e, props); parent.appendChild(e); return e; };
  add(d.body, "main", { id: "main" });
  add(add(d.body, "aside", { id: "sidebar" }), "div", { className: "sidebar-foot" });
  add(d.body, "section", { id: "panel-about", className: "panel" });
  add(add(d.body, "section", { id: "panel-debug", className: "panel" }), "div", { className: "panel-body" });
  for (const s of indexScripts()) load(env.ctx, s);
  // Plain copies: the arguments come from the vm realm.
  const named = (cmd) => calls.filter((c) => c.cmd === cmd).map((c) => ({ cmd: c.cmd, args: JSON.parse(JSON.stringify(c.args ?? null)) }));
  return { ...env, calls, named, U: env.window.OTDiagUp };
}
const $k = (root, k) => root.querySelector(`[data-k="${k}"]`);

test("diagup.js loads after the diagnostics log card", () => {
  const s = indexScripts();
  assert.ok(s.indexOf("diagup.js") > s.indexOf("diaglog-card.js"));
  assert.ok(s.indexOf("diagup.js") > s.indexOf("app-core.js"));
});

test("the public app (nothing configured) shows and sends nothing", async () => {
  const a = app({ diag_upload_status: () => NONE });
  await tick();
  assert.equal(a.named("diag_upload_status").length, 1);
  assert.equal(a.document.querySelector(".dgu-back"), null, "no notice, no offer");
  assert.equal(a.document.querySelector(".dgu-card"), null, "no settings card");
  const ind = a.document.querySelector("#diagUpInd");
  assert.ok(!ind || ind.hidden);
  a.U.panelOpened("maps");
  assert.equal(a.named("diag_upload_panel").length, 0);
});

test("auto mode: notice once, Turn off, indicator opens the settings", async () => {
  let st = { ...AUTO };
  const a = app({
    diag_upload_status: () => st,
    diag_upload_set: (args) => {
      if (args.noticeSeen) st = { ...st, notice_needed: false };
      if ("autoOff" in args) st = { ...st, auto_off: args.autoOff, auto_active: !args.autoOff };
      return st;
    },
    diag_upload_panel: () => null,
  });
  await tick();
  const ind = a.document.querySelector("#diagUpInd");
  assert.ok(ind && !ind.hidden);
  assert.equal(ind.textContent, "Diagnostics: auto → diag.example.invalid");
  const notice = a.document.querySelector(".dgu-back");
  assert.ok(notice, "the one-time notice is shown");
  assert.match($k(notice, "text").textContent, /^This build sends redacted diagnostics automatically to diag\.example\.invalid\./);
  assert.deepEqual(a.named("diag_upload_set")[0].args, { noticeSeen: true }, "marked seen when shown");
  $k(notice, "whatBtn").click();
  assert.equal($k(notice, "what").hidden, false);
  assert.match($k(notice, "what").textContent, /Never sent/);
  $k(notice, "off").click();
  await tick();
  assert.deepEqual(a.named("diag_upload_set")[1].args, { autoOff: true });
  assert.equal(a.document.querySelector(".dgu-back"), null, "closed");
  assert.equal(ind.textContent, "Diagnostics: auto (off)");
  // The settings card in About & updates: the off switch is unchecked.
  const card = a.document.querySelector("#panel-about .dgu-card");
  assert.ok(card);
  assert.equal($k(card, "auto").checked, false);
  assert.match($k(card, "mode").textContent, /turned off/);
  // Back on from the card.
  $k(card, "auto").checked = true;
  $k(card, "auto").dispatch("change");
  await tick();
  assert.deepEqual(a.named("diag_upload_set")[2].args, { autoOff: false });
  assert.equal(ind.textContent, "Diagnostics: auto → diag.example.invalid");
  // The indicator opens the settings.
  const shown = [];
  a.window.showPanel = (n) => shown.push(n);
  ind.click();
  assert.deepEqual(shown, ["about"]);
  // Panel opens are counted.
  a.U.panelOpened("maps");
  assert.deepEqual(a.named("diag_upload_panel")[0].args, { panel: "maps" });
  // Reload: the notice is not shown again.
  const b = app({ diag_upload_status: () => st });
  await tick();
  assert.equal(b.document.querySelector(".dgu-back"), null);
});

test("manual mode after a crash: offer, consent, preview, then send exactly that", async () => {
  let st = { ...MANUAL, crash_offer: { session: "2026-11-19T10:00:00.000Z", reason: "panic" } };
  let n = 0;
  const a = app({
    diag_upload_status: () => st,
    diag_upload_set: (args) => { if ("allowSend" in args) st = { ...st, allow_send: args.allowSend }; return st; },
    diag_upload_preview: (args) => ({ id: `p${++n}`, trigger: args.crash ? "crash" : "manual", bytes: 42, text: `{"note": "${args.note || ""}"}` }),
    diag_upload_send: ({ id }) => {
      st = { ...st, crash_offer: null, history: [{ id, ts: "2026-11-19T10:01:00.000Z", trigger: "crash", bytes: 42, status: "sent (200)" }] };
      return st.history[0];
    },
    diag_upload_payload: ({ id }) => `payload of ${id}`,
  });
  await tick();
  assert.equal(a.named("diag_upload_preview").length + a.named("diag_upload_send").length, 0, "nothing on start");
  const offer = a.document.querySelector(".dgu-back");
  assert.match($k(offer, "text").textContent, /closed unexpectedly.*Nothing is sent unless you do/);
  $k(offer, "review").click();
  const dlg = a.document.querySelector(".dgu-back");
  assert.match($k(dlg, "title").textContent, /Send diagnostics to diag\.example\.invalid/);
  // Not allowed yet: neither Preview nor Send.
  assert.equal($k(dlg, "preview").disabled, true);
  assert.equal($k(dlg, "send").disabled, true);
  $k(dlg, "allow").checked = true;
  $k(dlg, "allow").dispatch("change");
  await tick();
  assert.deepEqual(a.named("diag_upload_set").at(-1).args, { allowSend: true });
  assert.equal($k(dlg, "preview").disabled, false);
  assert.equal($k(dlg, "send").disabled, true, "no send before a preview");
  $k(dlg, "note").value = "maps froze";
  $k(dlg, "preview").click();
  await tick();
  assert.deepEqual(a.named("diag_upload_preview")[0].args, { note: "maps froze", crash: true });
  assert.equal($k(dlg, "pre").hidden, false);
  assert.equal($k(dlg, "pre").textContent, '{"note": "maps froze"}');
  assert.equal($k(dlg, "send").disabled, false);
  // A changed note needs a new preview.
  $k(dlg, "note").value = "maps froze twice";
  $k(dlg, "note").dispatch("input");
  assert.equal($k(dlg, "send").disabled, true);
  $k(dlg, "send").click();
  await tick();
  assert.equal(a.named("diag_upload_send").length, 0);
  $k(dlg, "preview").click();
  await tick();
  $k(dlg, "send").click();
  await tick();
  assert.deepEqual(a.named("diag_upload_send").map((c) => c.args), [{ id: "p2" }], "the latest preview is sent");
  assert.match($k(dlg, "msg").textContent, /^Sent/);
  $k(dlg, "close").click();
  // History in the settings card, with the exact payload.
  const card = a.document.querySelector("#panel-about .dgu-card");
  const btn = $k(card, "hist").querySelector("button");
  assert.ok(btn, "history row");
  btn.click();
  await tick();
  const viewer = a.document.querySelector(".dgu-back");
  assert.equal($k(viewer, "pre").textContent, "payload of p2");
  assert.deepEqual(a.named("diag_upload_payload")[0].args, { id: "p2" });
});

test("the Debug panel gets the card when an upload is configured", async () => {
  const a = app({ diag_upload_status: () => ({ ...MANUAL, allow_send: true }) });
  await tick();
  a.window.onPanelFirstShow("debug", () => {});
  a.window.showPanel("debug");
  await tick();
  const card = a.document.querySelector("#panel-debug .panel-body .dgu-card");
  assert.ok(card);
  assert.equal($k(card, "send").hidden, false);
  assert.equal($k(card, "send").disabled, false);
  assert.equal($k(card, "autoWrap").hidden, true, "no auto switch in manual mode");
});
