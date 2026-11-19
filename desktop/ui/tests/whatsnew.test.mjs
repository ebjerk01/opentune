// "What's new" after an update (src/whatsnew.js): the earlier-install
// snapshot, the notes rendering and the once-only popup on a DOM stub.
import test from "node:test";
import assert from "node:assert/strict";
import dom from "./support/dom.cjs";
import "../src/whatsnew.js";

const W = globalThis.OTWhatsNew;
const { makeWindow, indexScripts, load } = dom;
const tick = async (n = 8) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };

const store = (keys) => ({ length: keys.length, key: (i) => keys[i] ?? null, getItem: () => null, setItem() {}, removeItem() {} });

const NOTES = {
  version: "0.8.0", previous: "0.7.0",
  notes: {
    version: "0.8.0", date: "2026-11-20",
    groups: [
      { heading: "Added", items: Array.from({ length: 9 }, (_, i) => `Added thing ${i + 1} with \`code\``) },
      { heading: "Fixed", items: ["Fix <one>", "Fix two", "Fix three"] },
    ],
  },
};

test("earlier-install snapshot looks only at OpenTune's own keys", () => {
  assert.equal(W.hadOwnState(store([])), false);
  assert.equal(W.hadOwnState(store(["unrelated", "otherapp.x"])), false);
  assert.equal(W.hadOwnState(store(["ot.nav.collapsed"])), true);
  assert.equal(W.hadOwnState(store(["opentune.recentProjects"])), true);
  assert.equal(W.hadOwnState(store(["ot_autodebug_history"])), true);
  assert.equal(W.hadOwnState(null), false);
  assert.equal(W.hadOwnState({ get length() { throw new Error("denied"); } }), false);
});

test("whatsnew.js is the first app script, before anything can write storage", () => {
  assert.equal(indexScripts()[0], "whatsnew.js");
});

test("notes: escaped, inline code, a limit and the no-section fallback", () => {
  assert.equal(W.title(NOTES), "What's new in v0.8.0");
  assert.equal(W.subtitle(NOTES), "Updated from v0.7.0 · released 2026-11-20");
  assert.equal(W.subtitle({ ...NOTES, previous: "" }), "Updated · released 2026-11-20");
  assert.equal(W.itemCount(NOTES), 12);
  const first = W.bodyHtml(NOTES, W.SHOW_FIRST);
  assert.equal((first.match(/<li>/g) || []).length, 10);
  assert.match(first, /<code>code<\/code>/);
  assert.match(first, /Fix &lt;one&gt;/);
  assert.doesNotMatch(first, /Fix two/);
  assert.equal((W.bodyHtml(NOTES, 0).match(/<li>/g) || []).length, 12);
  assert.equal(W.bodyHtml({ version: "0.9.0", previous: "0.8.0", notes: null }), "<p>Updated to v0.9.0.</p>");
});

function app(keys, info) {
  const calls = [];
  const tauri = {
    core: {
      invoke: (cmd, args) => {
        calls.push({ cmd, args: JSON.parse(JSON.stringify(args ?? null)) });
        return Promise.resolve(cmd === "whatsnew_check" ? info : null);
      },
    },
  };
  const env = makeWindow({ tauri });
  env.window.localStorage = store(keys);
  env.window.setTimeout = (fn) => { fn(); return 1; };
  load(env.ctx, "whatsnew.js");
  return { ...env, calls, named: (c) => calls.filter((x) => x.cmd === c) };
}

test("an update shows once: Show all, then Esc records it", async () => {
  const a = app(["ot.nav.collapsed"], NOTES);
  await tick();
  assert.deepEqual(a.named("whatsnew_check")[0].args, { priorInstall: true });
  const back = a.document.querySelector(".wn-back");
  assert.ok(back, "popup shown");
  assert.match(back.innerHTML, /<h2 id="wnTitle">What's new in v0\.8\.0<\/h2>/);
  assert.match(back.innerHTML, /data-k="all">Show all \(2 more\)</);
  const all = back.querySelector('[data-k="all"]');
  assert.ok(all);
  all.click();
  assert.match(back.querySelector('[data-k="body"]').innerHTML, /Fix three/);
  assert.equal(back.querySelector('[data-k="all"]'), null);
  assert.equal(a.named("whatsnew_seen").length, 0, "not recorded while open");
  for (const fn of a.document.listeners.keydown || []) fn({ key: "Escape" });
  assert.equal(a.document.querySelector(".wn-back"), null, "closed");
  back.querySelector('[data-k="ok"]').click();
  assert.equal(a.named("whatsnew_seen").length, 1, "recorded once");
});

test("Got it and a backdrop click close it; nothing on a fresh install", async () => {
  const a = app([], { ...NOTES, notes: null });
  await tick();
  assert.deepEqual(a.named("whatsnew_check")[0].args, { priorInstall: false });
  const back = a.document.querySelector(".wn-back");
  assert.match(back.innerHTML, /Updated to v0\.8\.0\./);
  assert.equal(back.querySelector('[data-k="all"]'), null);
  back.querySelector('[data-k="ok"]').click();
  assert.equal(a.document.querySelector(".wn-back"), null);
  assert.equal(a.named("whatsnew_seen").length, 1);

  const b = app([], NOTES);
  await tick();
  const bb = b.document.querySelector(".wn-back");
  bb.querySelector('[data-k="body"]').dispatch("click");
  assert.ok(b.document.querySelector(".wn-back"), "a click inside stays open");
  bb.dispatch("click");
  assert.equal(b.document.querySelector(".wn-back"), null, "backdrop closes");

  const c = app([], null);
  await tick();
  assert.equal(c.document.querySelector(".wn-back"), null, "backend says nothing to show");
  assert.equal(c.named("whatsnew_seen").length, 0);
});
