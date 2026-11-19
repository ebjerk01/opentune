// Sidebar registry (src/nav.js): tier / build-channel filtering on a tiny DOM.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import dom from "./support/dom.cjs";

const { makeWindow, load } = dom;

const GROUPS = ["calibration", "vehicle", "reference", "help", "bench"];
const tick = () => new Promise((r) => setImmediate(r));

async function navFor(channel) {
  const tauri = { core: { invoke: async (cmd) => (cmd === "build_channel" ? channel : null) } };
  const { ctx, window, document } = makeWindow({ strictIds: true, tauri });
  const nav = document.createElement("div");
  nav.id = "nav";
  document.body.appendChild(nav);
  for (const g of GROUPS) {
    const div = document.createElement("div");
    div.className = "nav-group";
    div.dataset.group = g;
    nav.appendChild(div);
  }
  const shown = [];
  window.showPanel = (id) => shown.push(id);
  load(ctx, "nav.js");
  const group = (g) => document.querySelector(`#nav .nav-group[data-group="${g}"]`);
  return { window, document, OTNav: window.OTNav, group, shown };
}

test("public entries go to their group, bench entries to Bench", async () => {
  const { OTNav, group } = await navFor("stable");
  const a = OTNav.register({ id: "image", label: "Image", group: "calibration" });
  const b = OTNav.register({ id: "mytool", label: "My <tool>", group: "calibration", tier: "bench" });
  const c = OTNav.register({ id: "scan", label: "Scan" });
  assert.equal(a.parentElement, group("calibration"));
  assert.equal(b.parentElement, group("bench"), "bench tier ignores the requested group");
  assert.equal(c.parentElement, group("vehicle"), "default group is vehicle");
  assert.equal(a.dataset.tier, "public");
  assert.equal(b.dataset.tier, "bench");
  assert.match(b.innerHTML, /My &lt;tool&gt;/);
  // Registering the same id again re-tiers the existing button.
  const again = OTNav.register({ id: "mytool", label: "My tool", group: "reference", tier: "public" });
  assert.equal(again, b);
  assert.equal(b.parentElement, group("reference"));
  assert.equal(b.dataset.tier, "public");
});

test("stable hides bench entries and empty groups", async () => {
  const { OTNav, group } = await navFor("stable");
  OTNav.register({ id: "image", label: "Image", group: "calibration" });
  const b = OTNav.register({ id: "bench1", label: "Bench 1", tier: "bench" });
  await tick();
  assert.equal(OTNav.channel, "stable");
  assert.equal(OTNav.benchEnabled(), false);
  assert.equal(b.hidden, true);
  assert.equal(group("bench").hidden, true, "bench group has no visible item");
  assert.equal(group("help").hidden, true, "empty group hides");
  assert.equal(group("calibration").hidden, false);
});

for (const channel of ["alpha", "bench"]) {
  test(`${channel} shows bench entries`, async () => {
    const { OTNav, group } = await navFor(channel);
    const b = OTNav.register({ id: "bench1", label: "Bench 1", tier: "bench" });
    await tick();
    assert.equal(OTNav.benchEnabled(), true);
    assert.equal(b.hidden, false);
    assert.equal(group("bench").hidden, false);
    // Registered after the channel resolved: visible straight away.
    const late = OTNav.register({ id: "bench2", label: "Bench 2", tier: "bench" });
    assert.equal(late.hidden, false);
  });
}

test("onChannel runs listeners once the channel is known", async () => {
  const { OTNav } = await navFor("bench");
  const seen = [];
  OTNav.onChannel((c) => seen.push("early:" + c));
  await tick();
  OTNav.onChannel((c) => seen.push("late:" + c));
  assert.deepEqual(seen, ["early:bench", "late:bench"]);
});

test("a hidden active panel switches to the first visible one", async () => {
  const { OTNav, shown } = await navFor("stable");
  OTNav.register({ id: "image", label: "Image", group: "calibration" });
  const b = OTNav.register({ id: "bench1", label: "Bench 1", tier: "bench" });
  b.classList.add("active");
  await tick();
  assert.deepEqual(shown, ["image"]);
});

// ---- collapsible groups -----------------------------------------------------

// A Map-backed localStorage; `broken` makes every call throw (private mode,
// blocked storage).
function memStore(init, broken) {
  const m = new Map(Object.entries(init || {}));
  if (broken) {
    const no = () => { throw new Error("storage blocked"); };
    return { getItem: no, setItem: no, removeItem: no, m };
  }
  return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), m };
}

// Groups with header buttons as in index.html; `image` is the first-paint
// active entry in Calibration. `plainLabel` uses a <div> header instead.
function navWithHeaders(store, { plainLabel = false } = {}) {
  const tauri = { core: { invoke: async (cmd) => (cmd === "build_channel" ? "bench" : null) } };
  const { ctx, window, document } = makeWindow({ strictIds: true, tauri });
  window.localStorage = store;
  const nav = document.createElement("div");
  nav.id = "nav";
  document.body.appendChild(nav);
  for (const g of GROUPS) {
    const div = document.createElement("div");
    div.className = "nav-group";
    div.dataset.group = g;
    const h = document.createElement(plainLabel ? "div" : "button");
    h.className = "nav-group-label";
    div.appendChild(h);
    nav.appendChild(div);
  }
  const image = document.createElement("button");
  image.className = "nav-item active";
  image.dataset.panel = "image";
  nav.children[0].appendChild(image);
  window.showPanel = () => {};
  load(ctx, "nav.js");
  const group = (g) => document.querySelector(`#nav .nav-group[data-group="${g}"]`);
  const head = (g) => group(g).querySelector(".nav-group-label");
  return { window, document, OTNav: window.OTNav, group, head };
}
const KEY = "ot.nav.collapsed";

test("group headers are buttons with aria-expanded and a chevron", () => {
  const { group, head } = navWithHeaders(memStore());
  for (const g of GROUPS) {
    assert.equal(head(g).tagName, "BUTTON");
    assert.equal(head(g).getAttribute("aria-expanded"), "true");
    assert.equal(head(g).getAttribute("type"), "button");
    assert.ok(head(g).querySelector(".ng-chev"), "chevron");
    assert.equal(group(g).dataset.collapsed, undefined);
  }
});

test("a plain label is upgraded to a header button", () => {
  const { head, group, OTNav } = navWithHeaders(memStore(), { plainLabel: true });
  assert.equal(head("vehicle").tagName, "BUTTON");
  head("vehicle").click();
  assert.equal(group("vehicle").dataset.collapsed, "1");
  assert.equal(OTNav.isCollapsed("vehicle"), true);
});

test("clicking a header collapses and expands the group and remembers it", () => {
  const store = memStore();
  const { group, head, OTNav } = navWithHeaders(store);
  head("vehicle").click();
  assert.equal(group("vehicle").dataset.collapsed, "1");
  assert.equal(head("vehicle").getAttribute("aria-expanded"), "false");
  assert.deepEqual(JSON.parse(store.m.get(KEY)), ["vehicle"]);
  head("reference").click();
  assert.deepEqual(JSON.parse(store.m.get(KEY)).sort(), ["reference", "vehicle"]);
  head("vehicle").click();
  assert.equal(group("vehicle").dataset.collapsed, undefined);
  assert.equal(head("vehicle").getAttribute("aria-expanded"), "true");
  assert.deepEqual(JSON.parse(store.m.get(KEY)), ["reference"]);
  assert.equal(OTNav.isCollapsed("reference"), true);
});

test("remembered state is restored on load; unknown or corrupt values are ignored", () => {
  const a = navWithHeaders(memStore({ [KEY]: JSON.stringify(["reference", "bogus", 7]) }));
  assert.equal(a.group("reference").dataset.collapsed, "1");
  assert.equal(a.head("reference").getAttribute("aria-expanded"), "false");
  assert.equal(a.group("vehicle").dataset.collapsed, undefined);
  for (const bad of ["{not json", '"help"', "null", '{"a":1}']) {
    const b = navWithHeaders(memStore({ [KEY]: bad }));
    for (const g of GROUPS) assert.equal(b.group(g).dataset.collapsed, undefined, `${bad}: ${g}`);
  }
});

test("blocked localStorage: groups still toggle, nothing throws", () => {
  const { group, head } = navWithHeaders(memStore({}, true));
  assert.equal(group("help").dataset.collapsed, undefined);
  head("help").click();
  assert.equal(group("help").dataset.collapsed, "1");
  head("help").click();
  assert.equal(group("help").dataset.collapsed, undefined);
});

test("navigating into a collapsed group expands it for the session only", () => {
  const store = memStore({ [KEY]: JSON.stringify(["vehicle", "calibration"]) });
  const { group, head, OTNav } = navWithHeaders(store);
  // At first paint the remembered state wins: Calibration stays collapsed
  // (its active Image entry stays visible through CSS).
  assert.equal(group("calibration").dataset.collapsed, "1");
  OTNav.activated("image");
  assert.equal(group("calibration").dataset.collapsed, "1", "re-activating the boot panel does not expand");
  const scan = OTNav.register({ id: "scan", label: "Scan", group: "vehicle" });
  assert.equal(scan.parentElement, group("vehicle"));
  assert.equal(group("vehicle").dataset.collapsed, "1", "registering keeps the group collapsed");
  OTNav.activated("scan");
  assert.equal(group("vehicle").dataset.collapsed, undefined, "auto-expanded");
  assert.equal(head("vehicle").getAttribute("aria-expanded"), "true");
  assert.deepEqual(JSON.parse(store.m.get(KEY)).sort(), ["calibration", "vehicle"], "stored choice unchanged");
  // Collapsing it again by hand sticks, even while its panel is active.
  head("vehicle").click();
  assert.equal(group("vehicle").dataset.collapsed, "1");
  OTNav.activated("scan");
  assert.equal(group("vehicle").dataset.collapsed, "1");
});

test("CSS: a collapsed group hides only its inactive entries", () => {
  const css = fs.readFileSync(path.join(dom.UI, "src", "styles.css"), "utf8");
  assert.match(css, /\.nav-group\[data-collapsed\] > \.nav-item:not\(\.active\) \{ display: none; \}/);
  // No rule hides the active entry of a collapsed group.
  assert.doesNotMatch(css, /\[data-collapsed\][^{]*\.nav-item\.active[^{]*\{[^}]*display:\s*none/);
});

test("index.html group headers are keyboard-reachable buttons", () => {
  const html = fs.readFileSync(path.join(dom.UI, "index.html"), "utf8");
  const heads = html.match(/<[a-z]+ [^>]*class="nav-group-label[^"]*"[^>]*>/g) || [];
  assert.equal(heads.length, GROUPS.length);
  for (const h of heads) {
    assert.match(h, /^<button type="button"/);
    assert.match(h, /aria-expanded="true"/);
  }
});
