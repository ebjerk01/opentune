// Sidebar registry (src/nav.js): tier / build-channel filtering on a tiny DOM.
import test from "node:test";
import assert from "node:assert/strict";
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
