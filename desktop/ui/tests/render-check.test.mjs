// Render check (src/render-check.js): frame-timing statistics, the
// compositing warning, the environment lines, the one-time banner and the
// Debug card on the DOM stub. Synthetic numbers only.
import test from "node:test";
import assert from "node:assert/strict";
import dom from "./support/dom.cjs";
// render-check.js is a classic script: importing it attaches globalThis.OTRender
// (no document here, so it does not boot).
import "../src/render-check.js";

const R = globalThis.OTRender;
const { makeWindow, indexScripts, load } = dom;
const tick = async (n = 8) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

test("percentile is nearest-rank", () => {
  const s = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
  assert.equal(R.percentile(s, 50), 5);
  assert.equal(R.percentile(s, 95), 10);
  assert.equal(R.percentile(s, 0), 1);
  assert.equal(R.percentile([], 50), 0);
});

test("frameStats: a steady 60 Hz run", () => {
  const ts = Array.from({ length: 301 }, (_, i) => i * (1000 / 60));
  const st = R.frameStats(R.deltas(ts));
  assert.equal(st.frames, 300);
  assert.equal(st.fps, 60);
  assert.equal(st.p50, 16.7);
  assert.equal(st.p95, 16.7);
  assert.equal(st.long, 0);
  assert.equal(R.verdict(st), "smooth");
});

test("frameStats: CPU-painted scrolling with stutters", () => {
  // 90 frames at 33 ms, 10 at 120 ms: 4170 ms total.
  const d = Array(90).fill(33).concat(Array(10).fill(120));
  const st = R.frameStats(d);
  assert.equal(st.frames, 100);
  assert.equal(st.fps, Math.round((100 * 1000 / 4170) * 10) / 10);
  assert.equal(st.p50, 33);
  assert.equal(st.p95, 120);
  assert.equal(st.max, 120);
  assert.equal(st.long, 10);
  assert.equal(st.longMs, 50);
  assert.equal(R.verdict(st), "slow");
  assert.equal(R.frameStats(d, 100).long, 10);
  assert.equal(R.frameStats(d, 150).long, 0);
});

test("frameStats: uneven, empty and junk input", () => {
  const st = R.frameStats(Array(95).fill(16.7).concat(Array(5).fill(70)));
  assert.equal(R.verdict(st), "uneven");
  assert.deepEqual(R.frameStats([]), { frames: 0, fps: 0, p50: 0, p95: 0, max: 0, long: 0, longMs: 50 });
  assert.equal(R.frameStats([0, -3, NaN, Infinity, 16]).frames, 1, "non-advancing and bad values dropped");
  assert.equal(R.verdict(R.frameStats([])), "no frames");
  assert.deepEqual(R.deltas([5]), []);
  const line = R.statsLine("maps", R.frameStats([16, 16, 60]), 12);
  assert.match(line, /^scroll maps: 3 frames, .* fps, p50 16 ms, p95 60 ms, max 60 ms, 1 long \(>50 ms\), 12 scroll events — slow$/);
});

test("compositing warning: any value but 0 disables compositing; dismissal sticks", () => {
  const env = (v) => ({ vars: { WEBKIT_DISABLE_COMPOSITING_MODE: v } });
  assert.equal(R.compositingDisabled(env("1")), true);
  assert.equal(R.compositingDisabled(env("")), true, "WebKit treats an empty value as set");
  assert.equal(R.compositingDisabled(env("0")), false);
  assert.equal(R.compositingDisabled(env(null)), false);
  assert.equal(R.compositingDisabled(null), false);
  assert.match(R.compositingWarning(env("1"), false), /CPU-painted.*Troubleshooting/);
  assert.equal(R.compositingWarning(env("1"), true), null, "dismissed once = never again");
  assert.equal(R.compositingWarning(env(null), false), null);
});

const ENV = {
  os: "linux",
  vars: { GDK_BACKEND: null, WEBKIT_DISABLE_COMPOSITING_MODE: "1", WEBKIT_DISABLE_DMABUF_RENDERER: null, XDG_SESSION_TYPE: "wayland" },
  present: { APPIMAGE: true, DISPLAY: true, WAYLAND_DISPLAY: true },
  dri_render_nodes: ["renderD128"],
  dmabuf: { enabled: true, reason: "no known-bad case (DMA-BUF on)", drivers: ["amdgpu"] },
  set_by_opentune: [],
  perf: false,
};

test("envLines shows the whitelisted values, presence flags and the DMA-BUF decision", () => {
  const lines = R.envLines(ENV);
  assert.ok(lines.includes("WEBKIT_DISABLE_COMPOSITING_MODE=1"));
  assert.ok(lines.includes("GDK_BACKEND=(unset)"));
  assert.ok(lines.includes("APPIMAGE: set"));
  assert.ok(lines.includes("/dev/dri render nodes: renderD128"));
  assert.ok(lines.some((l) => /^DMA-BUF renderer: on — .*amdgpu/.test(l)));
  assert.deepEqual(R.envLines(null), ["render_env unavailable"]);
  assert.equal(R.dmabufLine({}), null);
});

function app(invokeMap, storage) {
  const tauri = {
    core: { invoke: (cmd, args) => (cmd in invokeMap ? Promise.resolve(invokeMap[cmd](args)) : new Promise(() => {})) },
    dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
    event: { listen: () => new Promise(() => {}) },
  };
  const env = makeWindow({ tauri });
  if (storage) env.window.localStorage = storage;
  const main = env.document.createElement("main"); main.id = "main"; env.document.body.appendChild(main);
  for (const s of indexScripts()) load(env.ctx, s);
  return env;
}

test("the banner shows once, and Dismiss remembers it", async () => {
  const store = {};
  const ls = { getItem: (k) => (k in store ? store[k] : null), setItem: (k, v) => { store[k] = String(v); }, removeItem: (k) => { delete store[k]; } };
  const notes = [];
  const a = app({ render_env: () => ENV, render_note: ({ line }) => { notes.push(line); return null; } }, ls);
  await tick();
  const b = a.document.getElementById("rcWarn");
  assert.ok(b && b.parentElement, "banner shown");
  assert.equal(b.parentElement.id, "main");
  assert.equal(b.getAttribute("role"), "alert");
  b.querySelector("#rcWarnDismiss").click();
  assert.equal(b.parentElement, null, "dismissed");
  assert.equal(notes.length, 0, "no perf logging without OPENTUNE_PERF");

  const again = app({ render_env: () => ENV }, ls);
  await tick();
  assert.equal(again.document.querySelector(".rc-warn"), null, "not shown after a dismissal");

  const ok = app({ render_env: () => ({ ...ENV, vars: { ...ENV.vars, WEBKIT_DISABLE_COMPOSITING_MODE: null } }) });
  await tick();
  assert.equal(ok.document.querySelector(".rc-warn"), null, "no banner with compositing on");

  // Storage that throws: the banner still works.
  const broken = { getItem() { throw new Error("blocked"); }, setItem() { throw new Error("blocked"); } };
  const c = app({ render_env: () => ENV }, broken);
  await tick();
  const cb = c.document.getElementById("rcWarn");
  assert.ok(cb);
  cb.querySelector("#rcWarnDismiss").click();
  assert.equal(cb.parentElement, null);
});

test("the DMA-BUF self-heal notice is shown on the start that reports it", async () => {
  const notice = "The last start did not draw a frame with the DMA-BUF renderer on: this start uses the fallback (DMA-BUF off).";
  const env = { ...ENV, vars: { ...ENV.vars, WEBKIT_DISABLE_COMPOSITING_MODE: null },
    dmabuf: { enabled: false, rule: "self_heal", reason: "self-heal: last start did not draw (0/2 clean starts before one retry)", drivers: ["amdgpu"], notice } };
  const a = app({ render_env: () => env });
  await tick();
  const n = a.document.querySelector(".rc-notice");
  assert.ok(n, "notice shown");
  assert.equal(a.document.querySelector("#rcWarn"), null, "no compositing warning");
  n.querySelector("button").click();
  assert.equal(n.parentElement, null);
  assert.ok(R.envLines(env).includes("DMA-BUF renderer: off — self-heal: last start did not draw (0/2 clean starts before one retry) (render-node driver: amdgpu)"));
  const quiet = app({ render_env: () => ({ ...env, dmabuf: { ...env.dmabuf, notice: null } }) });
  await tick();
  assert.equal(quiet.document.querySelector(".rc-notice"), null);
});

test("OPENTUNE_PERF=1 logs the environment at startup", async () => {
  const notes = [];
  app({ render_env: () => ({ ...ENV, perf: true }), render_note: ({ line }) => { notes.push(line); return null; } });
  await tick();
  assert.ok(notes.some((l) => l.startsWith("env: ") && l.includes("WEBKIT_DISABLE_COMPOSITING_MODE=1")));
  assert.ok(notes.some((l) => l.startsWith("WebGL renderer: ")));
});

test("the Debug panel gets a Render check card listing the panels", async () => {
  const a = app({ render_env: () => ENV });
  const { document, window } = a;
  // index.html's Debug panel body and two nav entries.
  const panel = document.createElement("section"); panel.id = "panel-debug"; panel.className = "panel";
  const body = document.createElement("div"); body.className = "panel-body"; panel.appendChild(body);
  document.getElementById("main").appendChild(panel);
  const nav = document.createElement("nav"); nav.id = "nav"; document.body.appendChild(nav);
  for (const p of ["image", "maps", "debug"]) { const b = document.createElement("button"); b.className = "nav-item"; b.dataset.panel = p; b.textContent = "x " + p; nav.appendChild(b); }
  window.showPanel("debug");
  await tick();
  const card = document.getElementById("rcCard");
  assert.ok(card && card.parentElement === body);
  const opts = document.getElementById("rcPanel").querySelectorAll("option");
  assert.deepEqual(opts.map((o) => o.value), ["image", "maps"], "every panel but Debug itself");
  assert.match(document.getElementById("rcEnv").textContent, /WEBKIT_DISABLE_COMPOSITING_MODE=1[\s\S]*WebGL renderer: WebGL unavailable/);
});
