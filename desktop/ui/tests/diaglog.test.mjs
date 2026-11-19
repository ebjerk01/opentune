// Diagnostics log, frontend half: error capture and the command-error wrapper
// (src/diaglog.js), and the Debug panel card (src/diaglog-card.js).
import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import dom from "./support/dom.cjs";
// Classic scripts: importing attaches globalThis.OTDiag / OTDiagCard (no
// document here, so neither installs itself).
import "../src/diaglog.js";
import "../src/diaglog-card.js";

const D = globalThis.OTDiag;
const C = globalThis.OTDiagCard;
const { makeWindow, indexScripts, load } = dom;
const tick = async (n = 8) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

test("errText reads errors, strings and objects", () => {
  assert.equal(D.errText("no adapter"), "no adapter");
  assert.equal(D.errText(new Error("boom")), "boom");
  assert.equal(D.errText(new TypeError("x is undefined")), "TypeError: x is undefined");
  assert.equal(D.errText({ code: 7 }), '{"code":7}');
  assert.equal(D.errText(null), "null");
});

test("records are short and carry no arguments", () => {
  const c = D.cmdRecord("image_info", "x".repeat(5000));
  assert.equal(c.src, "cmd");
  assert.equal(c.level, "warn");
  assert.ok(c.msg.startsWith("image_info failed: xxx"));
  assert.equal(c.msg.length, 501, "cut to 500 + ellipsis");
  assert.deepEqual(c.ctx, { cmd: "image_info" });
  const err = new Error("bad");
  err.stack = "Error: bad\n" + "    at f (tauri://localhost/src/app-core.js:1:1)\n".repeat(200);
  const e = D.errorRecord({ message: "Uncaught Error: bad", filename: "tauri://localhost/src/app-maps.js", lineno: 12, colno: 3, error: err });
  assert.equal(e.src, "js");
  assert.equal(e.level, "error");
  assert.equal(e.msg, "Uncaught Error: bad");
  assert.equal(e.ctx.source, "/src/app-maps.js", "origin stripped");
  assert.equal(e.ctx.line, 12);
  assert.equal(e.ctx.stack.length, 2001);
  const r = D.rejectionRecord("timeout waiting for 7E8");
  assert.equal(r.msg, "unhandled rejection: timeout waiting for 7E8");
});

test("the same message within 10 s is counted, not re-sent", () => {
  const sent = [];
  let now = 0;
  const report = D.makeReporter((r) => sent.push(r), () => now);
  const rec = D.cmdRecord("log_tick", "adapter gone");
  assert.equal(report(rec), true);
  now = 100; assert.equal(report(rec), false);
  now = 200; assert.equal(report(rec), false);
  now = 300; report(D.cmdRecord("obd_live", "timeout"));
  assert.deepEqual(sent.map((r) => r.msg), [
    "log_tick failed: adapter gone",
    "previous message repeated 2 more time(s)",
    "obd_live failed: timeout",
  ]);
  now = 300 + D.REPEAT_MS; report(D.cmdRecord("obd_live", "timeout"));
  assert.equal(sent.length, 4, "after the window the message goes out again");
});

test("wrapCore reports rejected commands once and passes results through", async () => {
  const reported = [];
  const core = Object.freeze({
    invoke: (cmd) => {
      if (cmd === "ok") return Promise.resolve(42);
      if (cmd === "sync_throw") throw new Error("not a function");
      return Promise.reject(`${cmd}: no such file`);
    },
    Channel: function () {},
  });
  const w = D.wrapCore(core, (r) => reported.push(r));
  assert.equal(typeof w.Channel, "function", "the rest of the namespace is kept");
  assert.equal(await w.invoke("ok"), 42);
  assert.equal(reported.length, 0);
  await assert.rejects(w.invoke("image_info", { path: "/secret/file.bin" }), (e) => e === "image_info: no such file");
  assert.equal(reported.length, 1);
  assert.equal(reported[0].msg, "image_info failed: image_info: no such file");
  assert.ok(!JSON.stringify(reported[0]).includes("/secret/file.bin"), "arguments are never logged");
  assert.throws(() => w.invoke("sync_throw"));
  assert.equal(reported.length, 2);
  for (const q of D.QUIET) await w.invoke(q).catch(() => {});
  assert.equal(reported.length, 2, "the log's own commands are not reported");
  assert.equal(D.wrapCore(w, () => {}), w, "wrapping twice is a no-op");
});

function app(invokeMap) {
  const calls = [];
  const tauri = {
    core: {
      invoke: (cmd, args) => {
        calls.push({ cmd, args });
        return cmd in invokeMap ? Promise.resolve().then(() => invokeMap[cmd](args)) : new Promise(() => {});
      },
    },
    dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
    event: { listen: () => new Promise(() => {}) },
  };
  const listeners = {};
  const env = makeWindow({ tauri });
  env.window.addEventListener = (t, fn) => { (listeners[t] = listeners[t] || []).push(fn); };
  const main = env.document.createElement("main"); main.id = "main"; env.document.body.appendChild(main);
  for (const s of indexScripts()) load(env.ctx, s);
  return { ...env, calls, listeners, tauri };
}

test("loaded first, the wrapper covers every panel's invoke and uncaught errors", async () => {
  const scripts = indexScripts();
  assert.equal(scripts[scripts.indexOf("util.js") + 1], "diaglog.js", "right after util.js");
  assert.ok(scripts.indexOf("diaglog.js") <= 2, "only whatsnew.js and util.js before it");
  assert.ok(scripts.indexOf("diaglog-card.js") > scripts.indexOf("render-check.js"));
  const a = app({ image_info: () => { throw "cannot open image"; }, diag_log: () => null });
  assert.ok(a.window.__TAURI__.core.__otDiag, "core.invoke is wrapped");
  // app-core.js's shared invoke() goes through it.
  const p = vm.runInContext('invoke("image_info", { path: "x.bin" })', a.ctx);
  await assert.rejects(p);
  await tick();
  const logs = a.calls.filter((c) => c.cmd === "diag_log");
  assert.equal(logs.length, 1);
  assert.equal(logs[0].args.src, "cmd");
  assert.equal(logs[0].args.msg, "image_info failed: cannot open image");
  // Uncaught errors and rejections.
  a.listeners.error.forEach((fn) => fn({ message: "Uncaught TypeError: q is null", filename: "tauri://localhost/src/feat_obd.js", lineno: 5 }));
  a.listeners.unhandledrejection.forEach((fn) => fn({ reason: new Error("late") }));
  await tick();
  const js = a.calls.filter((c) => c.cmd === "diag_log" && c.args.src === "js").map((c) => c.args.msg);
  assert.deepEqual(js, ["Uncaught TypeError: q is null", "unhandled rejection: late"]);
});

// ---- card -----------------------------------------------------------------------

const RECS = [
  { ts: "2026-11-19T10:00:00.000Z", level: "info", src: "startup", msg: "session start", ctx: { session: true } },
  { ts: "2026-11-19T10:00:01.500Z", level: "error", src: "rust", msg: "panic: index out of bounds", ctx: { location: "src/maps.rs:10:5" } },
  { ts: "2026-11-19T10:00:02.000Z", level: "warn", src: "cmd", msg: "image_info failed: no such file", ctx: { cmd: "image_info" } },
];

test("card formatting helpers", () => {
  assert.equal(C.formatRecord(RECS[1]), "2026-11-19 10:00:01.500  ERROR  rust     panic: index out of bounds  (src/maps.rs:10:5)");
  assert.equal(C.formatRecord({ ts: "t", level: "warn", src: "js", msg: "m", ctx: { source: "/src/a.js", line: 3 } }), "t  WARN   js       m  (/src/a.js:3)");
  assert.equal(C.copyText(RECS).split("\n").length, 3);
  assert.deepEqual(JSON.parse(C.copyText(RECS).split("\n")[2]), RECS[2]);
  assert.match(C.infoLine({ state: "on", bytes: 3 * 1024 * 1024 / 2, files: 2, file: "/x/logs/opentune.log" }), /^1\.5 MiB in 2 files — \/x\/logs\/opentune\.log$/);
  assert.match(C.infoLine({ state: "off" }), /off for this session/);
});

test("the Debug panel gets a Diagnostics log card: tail, filter, copy, clear", async () => {
  let cleared = 0;
  const a = app({
    diag_log_tail: ({ limit, level }) => RECS.filter((r) => ["debug", "info", "warn", "error"].indexOf(r.level) >= ["debug", "info", "warn", "error"].indexOf(level || "debug")).slice(-limit),
    diag_log_info: () => ({ state: "on", bytes: 2048, files: 1, file: "/home/u/.local/share/org.opentune.desktop/logs/opentune.log" }),
    diag_log_clear: () => { cleared++; return null; },
    diag_log: () => null,
    render_env: () => null,
  });
  const { document, window } = a;
  const panel = document.createElement("section"); panel.id = "panel-debug"; panel.className = "panel";
  const body = document.createElement("div"); body.className = "panel-body"; panel.appendChild(body);
  document.getElementById("main").appendChild(panel);
  window.showPanel("debug");
  await tick();
  const card = document.getElementById("dlCard");
  assert.ok(card && card.parentElement === body, "card in the Debug panel body");
  assert.equal(card.className, "card");
  for (const id of ["dlLevel", "dlRefresh", "dlOpen", "dlCopy", "dlClear", "dlTail"]) assert.ok(card.querySelector("#" + id), id);
  const tail = document.getElementById("dlTail").textContent.split("\n");
  assert.equal(tail.length, 3);
  assert.match(tail[1], /ERROR  rust     panic: index out of bounds/);
  assert.match(document.getElementById("dlInfo").textContent, /^2 KiB in 1 file — .*opentune\.log$/);
  // Level filter.
  document.getElementById("dlLevel").value = "error";
  document.getElementById("dlLevel").dispatch("change");
  await tick();
  assert.equal(document.getElementById("dlTail").textContent.split("\n").length, 1);
  assert.ok(a.calls.some((c) => c.cmd === "diag_log_tail" && c.args.level === "error"));
  // Copy last 200 lines: the JSON lines, all levels.
  let copied = null;
  window.navigator.clipboard = { writeText: async (t) => { copied = t; } };
  document.getElementById("dlCopy").click();
  await tick();
  assert.ok(a.calls.some((c) => c.cmd === "diag_log_tail" && c.args.limit === 200 && c.args.level === undefined));
  assert.equal(copied.split("\n").length, 3);
  assert.match(document.getElementById("dlStatus").textContent, /Copied 3 line/);
  // Clear asks first.
  window.confirm = () => false;
  document.getElementById("dlClear").click();
  await tick();
  assert.equal(cleared, 0, "cancelled");
  window.confirm = () => true;
  document.getElementById("dlClear").click();
  await tick();
  assert.equal(cleared, 1);
});
