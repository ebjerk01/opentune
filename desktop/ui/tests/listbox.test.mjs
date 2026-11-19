// Custom listbox (src/listbox.js) replacing the sidebar's native adapter
// <select>s: the placement math (down / up / clamped), keyboard handling and
// type-ahead, and the external contract the callers rely on (value get/set,
// change events, option refresh, disabled), on the DOM stub.
import test from "node:test";
import assert from "node:assert/strict";
import dom from "./support/dom.cjs";
// listbox.js is a classic script: importing it attaches globalThis.OTListbox.
import "../src/listbox.js";

const L = globalThis.OTListbox;
const { makeWindow, indexScripts, load } = dom;
const tick = async (n = 8) => { for (let i = 0; i < n; i++) await new Promise((r) => setImmediate(r)); };
const rect = (left, top, width, height) => ({ left, top, width, height, right: left + width, bottom: top + height });

// ---- placement ---------------------------------------------------------------

test("place: opens down when the list fits below", () => {
  const p = L.place(rect(20, 100, 180, 30), { width: 1280, height: 800 }, { width: 120, height: 200 });
  assert.equal(p.dir, "down");
  assert.equal(p.top, 132); // bottom + 2 px gap
  assert.equal(p.height, 200);
  assert.equal(p.width, 180, "at least as wide as the button");
  assert.equal(p.left, 20);
  assert.ok(p.maxHeight >= 200);
});

test("place: the fullscreen sidebar case opens up, fully on screen", () => {
  // Driver picker pinned at the bottom of a 1080p fullscreen window.
  const vp = { width: 1920, height: 1080 };
  const p = L.place(rect(20, 1000, 190, 32), vp, { width: 150, height: 400 });
  assert.equal(p.dir, "up");
  assert.equal(p.top, 1000 - 2 - 400);
  assert.ok(p.top >= 6 && p.top + p.height <= vp.height - 6, "inside the viewport");
});

test("place: neither side fits — the roomier side, scrolling inside", () => {
  const vp = { width: 800, height: 600 };
  const low = L.place(rect(10, 400, 100, 30), vp, { width: 100, height: 900 });
  assert.equal(low.dir, "up");
  assert.equal(low.maxHeight, 400 - 2 - 6);
  assert.equal(low.height, low.maxHeight);
  assert.equal(low.top, 6);
  const high = L.place(rect(10, 150, 100, 30), vp, { width: 100, height: 900 });
  assert.equal(high.dir, "down");
  assert.equal(high.maxHeight, 600 - 6 - (180 + 2));
  assert.ok(high.top + high.height <= 600 - 6);
});

test("place: clamped to the left and right edges and the viewport width", () => {
  const vp = { width: 400, height: 600 };
  const right = L.place(rect(300, 50, 90, 30), vp, { width: 250, height: 100 });
  assert.equal(right.width, 250);
  assert.equal(right.left, 400 - 6 - 250, "pushed back from the right edge");
  const left = L.place(rect(-40, 50, 90, 30), vp, { width: 100, height: 100 });
  assert.equal(left.left, 6);
  const wide = L.place(rect(10, 50, 90, 30), vp, { width: 1000, height: 100 });
  assert.equal(wide.width, 400 - 12);
  assert.equal(wide.left, 6);
});

test("place: an anchor scrolled past the bottom still yields an on-screen list", () => {
  const vp = { width: 800, height: 600 };
  const p = L.place(rect(10, 650, 100, 30), vp, { width: 100, height: 120 });
  assert.equal(p.dir, "up");
  assert.ok(p.top >= 6 && p.top + p.height <= 594);
});

// ---- keyboard ------------------------------------------------------------------

test("keyAction: closed — arrows / Enter / Space / Home / End open; letters type", () => {
  const st = { open: false, active: -1, selected: 2, n: 5 };
  for (const k of ["ArrowDown", "ArrowUp", "Enter", " "]) assert.deepEqual(L.keyAction(k, st), { type: "open", index: 2, prevent: true }, k);
  assert.equal(L.keyAction("Home", st).index, 0);
  assert.equal(L.keyAction("End", st).index, 4);
  assert.equal(L.keyAction("e", st).type, "type");
  assert.deepEqual(L.keyAction("Tab", st), { type: "none", prevent: false });
  assert.equal(L.keyAction("Shift", st).type, "none");
});

test("keyAction: open — move, page, select, close; disabled options are skipped", () => {
  const st = { open: true, active: 1, selected: 1, n: 5, disabled: [false, false, true, false, false] };
  assert.equal(L.keyAction("ArrowDown", st).index, 3, "skips the disabled option");
  assert.equal(L.keyAction("ArrowUp", st).index, 0);
  assert.equal(L.keyAction("ArrowUp", { ...st, active: 0 }).index, 0, "clamped at the top, no wrap");
  assert.equal(L.keyAction("ArrowDown", { ...st, active: 4 }).index, 4, "clamped at the bottom");
  assert.equal(L.keyAction("PageDown", st).index, 4);
  assert.equal(L.keyAction("PageUp", { ...st, active: 4 }).index, 0);
  assert.equal(L.keyAction("Home", st).index, 0);
  assert.equal(L.keyAction("End", st).index, 4);
  assert.deepEqual(L.keyAction("Enter", st), { type: "select", index: 1, prevent: true });
  assert.deepEqual(L.keyAction(" ", st), { type: "select", index: 1, prevent: true });
  assert.equal(L.keyAction(" ", { ...st, typing: true }).type, "type", "Space inside a type-ahead word");
  assert.deepEqual(L.keyAction("ArrowUp", { ...st, alt: true }), { type: "select", index: 1, prevent: true });
  assert.deepEqual(L.keyAction("Escape", st), { type: "close", prevent: true });
  assert.deepEqual(L.keyAction("Tab", st), { type: "close", prevent: false }, "Tab closes without a change");
});

test("typeahead: prefix match, repeated letter cycles, disabled skipped", () => {
  const labels = ["elm327", "elm327obd", "gsusb", "slcan", "socketcan", "vcar"];
  assert.equal(L.typeahead(labels, "s", -1), 3);
  assert.equal(L.typeahead(labels, "s", 3), 4, "a single letter moves on");
  assert.equal(L.typeahead(labels, "ss", 4), 3, "the same letter again cycles round");
  assert.equal(L.typeahead(labels, "elm327o", 0), 1);
  assert.equal(L.typeahead(labels, "elm", 0), 0, "a longer query stays on the current match");
  assert.equal(L.typeahead(labels, "V", 0), 5, "case-insensitive");
  assert.equal(L.typeahead(labels, "x", 0), -1);
  assert.equal(L.typeahead(labels, "s", -1, [false, false, false, true, false, false]), 4);
  assert.equal(L.typeahead([], "a", -1), -1);
});

// ---- contract on the DOM stub ----------------------------------------------------

function setup() {
  const env = makeWindow();
  load(env.ctx, "listbox.js");
  const { document, window } = env;
  const row = document.createElement("div");
  document.body.appendChild(row);
  const sel = document.createElement("select");
  sel.id = "connDriver";
  sel.className = "inp";
  sel.setAttribute("title", "interface driver");
  row.appendChild(sel);
  const add = (ids) => ids.forEach((id) => { const o = document.createElement("option"); o.value = id; o.textContent = id; sel.appendChild(o); });
  add(["elm327obd", "slcan", "vcar"]);
  sel.value = "elm327obd";
  const ctrl = window.OTListbox.enhance(sel);
  const changes = [];
  sel.addEventListener("change", (e) => changes.push(e.target.value));
  const key = (k, extra) => ctrl.key(Object.assign({ key: k, preventDefault() {}, stopPropagation() {} }, extra));
  const label = () => ctrl.button.querySelector(".ot-lb-val").textContent;
  return { ...env, sel, ctrl, changes, key, label, add, row };
}

test("enhance: the select stays as the hidden value store, a combobox button follows it", () => {
  const { sel, ctrl, label, window } = setup();
  assert.equal(sel.hidden, true);
  assert.equal(sel.getAttribute("aria-hidden"), "true");
  assert.equal(sel.nextSibling, ctrl.button, "button right after the select");
  const b = ctrl.button;
  assert.equal(b.tagName, "BUTTON");
  assert.equal(b.getAttribute("role"), "combobox");
  assert.equal(b.getAttribute("aria-haspopup"), "listbox");
  assert.equal(b.getAttribute("aria-expanded"), "false");
  assert.equal(b.getAttribute("aria-label"), "interface driver");
  assert.ok(b.classList.contains("inp") && b.classList.contains("ot-lb"), "keeps the select's classes");
  assert.equal(label(), "elm327obd");
  sel.value = "vcar";
  assert.equal(sel.value, "vcar", "value reads back");
  assert.equal(label(), "vcar", "a programmatic set updates the button");
  assert.equal(window.OTListbox.of(sel), ctrl);
  assert.equal(window.OTListbox.enhance(sel), ctrl, "enhancing twice is a no-op");
});

test("keyboard: open, move, Enter selects and fires one change; Escape changes nothing", () => {
  const { sel, ctrl, changes, key, label, document } = setup();
  key("ArrowDown");
  assert.equal(ctrl.open, true);
  assert.equal(ctrl.button.getAttribute("aria-expanded"), "true");
  const pop = ctrl.popup;
  assert.equal(pop.parentElement, document.body, "popup lives on <body>, outside the sidebar");
  assert.equal(pop.getAttribute("role"), "listbox");
  assert.equal(ctrl.button.getAttribute("aria-controls"), pop.id);
  const opts = pop.querySelectorAll('[role="option"]');
  assert.equal(opts.length, 3);
  assert.equal(opts[0].getAttribute("aria-selected"), "true");
  assert.equal(ctrl.button.getAttribute("aria-activedescendant"), opts[0].id);
  key("ArrowDown");
  assert.equal(ctrl.button.getAttribute("aria-activedescendant"), opts[1].id);
  key("End");
  assert.equal(ctrl.active, 2);
  key("Enter");
  assert.equal(ctrl.open, false);
  assert.equal(pop.parentElement, null, "popup removed");
  assert.equal(sel.value, "vcar");
  assert.equal(label(), "vcar");
  assert.deepEqual(changes, ["vcar"]);

  key(" ");
  key("Home");
  key("Escape");
  assert.equal(ctrl.open, false);
  assert.equal(sel.value, "vcar", "Escape keeps the value");
  key("Enter"); key("Enter"); // re-selecting the same value
  assert.deepEqual(changes, ["vcar"], "no change event without a change");
});

test("type-ahead opens on the matching option", () => {
  const { ctrl, key } = setup();
  key("s");
  assert.equal(ctrl.open, true);
  assert.equal(ctrl.active, 1);
});

test("mouse: clicking an option selects it; outside pointerdown and closeAll close", () => {
  const { sel, ctrl, changes, document, window } = setup();
  ctrl.button.click();
  assert.equal(ctrl.open, true);
  const opt = ctrl.popup.querySelectorAll(".ot-lb-opt")[1];
  ctrl.popup.dispatch("click", { target: opt });
  assert.equal(sel.value, "slcan");
  assert.deepEqual(changes, ["slcan"]);

  ctrl.button.click();
  const inside = ctrl.popup.querySelectorAll(".ot-lb-opt")[0];
  document.listeners.pointerdown.forEach((fn) => fn({ target: inside }));
  assert.equal(ctrl.open, true, "a press inside the list keeps it open");
  document.listeners.pointerdown.forEach((fn) => fn({ target: document.body }));
  assert.equal(ctrl.open, false, "outside press closes");
  assert.equal((document.listeners.pointerdown || []).length, 0, "listener removed on close");

  ctrl.button.click();
  window.OTListbox.closeAll();
  assert.equal(ctrl.open, false, "panel switch closes it");
  ctrl.button.click();
  ctrl.button.dispatch("blur");
  assert.equal(ctrl.open, false, "blur closes it");
  ctrl.button.click();
  document.listeners.scroll.forEach((fn) => fn({ target: document.body }));
  assert.equal(ctrl.open, false, "scrolling the page under it closes it");
});

test("option list refresh and disabled state", () => {
  const { sel, ctrl, key, label, add } = setup();
  sel.innerHTML = "";
  add(["doip", "gsusb"]);
  sel.value = "gsusb";
  assert.equal(label(), "gsusb");
  ctrl.refresh();
  key("ArrowDown");
  assert.equal(ctrl.popup.querySelectorAll(".ot-lb-opt").length, 2, "the popup lists the current options");
  assert.deepEqual(sel.querySelectorAll("option").map((o) => o.value), ["doip", "gsusb"], "the options stay in the select");
  sel.disabled = true;
  assert.equal(ctrl.open, false, "disabling closes it");
  assert.equal(ctrl.button.disabled, true);
  assert.equal(ctrl.button.getAttribute("aria-disabled"), "true");
  key("ArrowDown");
  assert.equal(ctrl.open, false, "a disabled listbox does not open");
  ctrl.button.click();
  sel.disabled = false;
  assert.equal(ctrl.button.disabled, false);
});

test("sidebar adapter bar: both pickers are listboxes and still drive OT.conn", async () => {
  const drivers = [{ id: "elm327obd", name: "ELM" }, { id: "slcan", name: "SLCAN" }, { id: "vcar", name: "Virtual car" }];
  const tauri = {
    core: { invoke: (cmd) => (cmd === "build_channel" ? Promise.resolve("stable") : cmd === "list_drivers" ? Promise.resolve(drivers) : new Promise(() => {})) },
    dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
    event: { listen: () => new Promise(() => {}) },
  };
  const env = makeWindow({ tauri });
  const { document, window } = env;
  const main = document.createElement("main"); main.id = "main"; document.body.appendChild(main);
  const foot = document.createElement("div"); foot.className = "sidebar-foot"; document.body.appendChild(foot);
  const badge = document.createElement("div"); badge.id = "adapterBadge"; foot.appendChild(badge);
  for (const s of indexScripts()) load(env.ctx, s);
  await tick(20);
  const $ = (id) => document.getElementById(id);
  for (const id of ["connDriver", "connBus"]) {
    const c = window.OTListbox.of($(id));
    assert.ok(c, `${id} enhanced`);
    assert.equal($(id).hidden, true);
  }
  const drv = window.OTListbox.of($("connDriver"));
  assert.equal(drv.button.querySelector(".ot-lb-val").textContent, "elm327obd");
  const k = (key) => drv.key({ key, preventDefault() {}, stopPropagation() {} });
  k("ArrowDown"); k("End"); k("Enter");
  assert.equal(window.OT.conn.driver, "vcar", "a pick reaches OT.conn through the change event");
  const bus = window.OTListbox.of($("connBus"));
  bus.key({ key: "End", preventDefault() {}, stopPropagation() {} });
  bus.key({ key: "Enter", preventDefault() {}, stopPropagation() {} });
  assert.equal(window.OT.conn.bus, "ms");
});
