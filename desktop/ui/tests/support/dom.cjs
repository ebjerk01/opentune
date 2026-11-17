// A tiny DOM stand-in for the UI tests: enough of Element / document for the
// app scripts to load and for nav.js to register entries. Not a browser.
//
// Selectors: descendant chains of compound parts made of tag, #id, .class and
// [attr="value"] (e.g. '#nav .nav-item[data-tier="bench"]'). getElementById
// returns a detached placeholder for ids the page would have in index.html, so
// load-time `$("x").onclick = ...` works; `strictIds` turns that off.
"use strict";
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");

const UI = path.join(__dirname, "..", "..");

class ClassList {
  constructor(el) { this.el = el; }
  get _set() { return new Set(String(this.el.className || "").split(/\s+/).filter(Boolean)); }
  _write(s) { this.el.className = Array.from(s).join(" "); }
  add(...c) { const s = this._set; c.forEach((x) => s.add(x)); this._write(s); }
  remove(...c) { const s = this._set; c.forEach((x) => s.delete(x)); this._write(s); }
  contains(c) { return this._set.has(c); }
  toggle(c, force) {
    const on = force === undefined ? !this.contains(c) : !!force;
    if (on) this.add(c); else this.remove(c);
    return on;
  }
}

function parsePart(p) {
  const m = { tag: null, id: null, classes: [], attrs: [] };
  const re = /([a-zA-Z][\w-]*)|#([\w-]+)|\.([\w-]+)|\[([\w-]+)(?:="([^"]*)")?\]/g;
  let x;
  while ((x = re.exec(p))) {
    if (x[1]) m.tag = x[1].toUpperCase();
    else if (x[2]) m.id = x[2];
    else if (x[3]) m.classes.push(x[3]);
    else m.attrs.push([x[4], x[5]]);
  }
  return m;
}

function attrOf(el, name) {
  if (name === "id") return el.id || null;
  if (name === "class") return el.className || null;
  if (name.startsWith("data-")) {
    const k = name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    return k in el.dataset ? String(el.dataset[k]) : null;
  }
  return name in el.attributes ? el.attributes[name] : null;
}

function matchesPart(el, m) {
  if (m.tag && el.tagName !== m.tag) return false;
  if (m.id && el.id !== m.id) return false;
  if (m.classes.some((c) => !el.classList.contains(c))) return false;
  return m.attrs.every(([k, v]) => (v === undefined ? attrOf(el, k) != null : attrOf(el, k) === v));
}

function matches(el, selector) {
  return selector.split(",").some((sel) => {
    const parts = sel.trim().split(/\s+/).map(parsePart);
    if (!matchesPart(el, parts[parts.length - 1])) return false;
    let i = parts.length - 2;
    for (let a = el.parentElement; a && i >= 0; a = a.parentElement) if (matchesPart(a, parts[i])) i--;
    return i < 0;
  });
}

class Element {
  constructor(doc, tag) {
    this.ownerDocument = doc;
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.dataset = {};
    this.attributes = {};
    this.style = {};
    this.className = "";
    this.classList = new ClassList(this);
    this.hidden = false;
    this.disabled = false;
    this.value = "";
    this.textContent = "";
    this._html = "";
    this.listeners = {};
    this.selectedOptions = [];
    this._id = "";
  }
  // Markup assigned here becomes child elements (tags + attributes only, no
  // text nodes), so a panel can look up the controls it just rendered.
  get innerHTML() { return this._html; }
  set innerHTML(html) {
    this.children.slice().forEach((c) => c.remove());
    this._html = String(html);
    parseInto(this, this._html);
  }
  get id() { return this._id; }
  set id(v) { this._id = String(v); this.ownerDocument._ids.set(this._id, this); }
  get firstChild() { return this.children[0] || null; }
  get nextSibling() {
    const p = this.parentElement;
    return p ? p.children[p.children.indexOf(this) + 1] || null : null;
  }
  appendChild(c) { return this.insertBefore(c, null); }
  insertBefore(c, ref) {
    if (c.parentElement) c.remove();
    const i = ref ? this.children.indexOf(ref) : -1;
    if (i < 0) this.children.push(c); else this.children.splice(i, 0, c);
    c.parentElement = this;
    return c;
  }
  insertAdjacentElement(_where, c) { return this.parentElement ? this.parentElement.insertBefore(c, this.nextSibling) : c; }
  remove() {
    const p = this.parentElement;
    if (p) p.children.splice(p.children.indexOf(this), 1);
    this.parentElement = null;
  }
  setAttribute(k, v) { if (k === "id") this.id = v; else this.attributes[k] = String(v); }
  getAttribute(k) { return attrOf(this, k); }
  addEventListener(t, fn) { (this.listeners[t] = this.listeners[t] || []).push(fn); }
  removeEventListener() {}
  dispatch(t, ev) { (this.listeners[t] || []).forEach((fn) => fn(Object.assign({ target: this, preventDefault() {} }, ev))); }
  click() { this.dispatch("click"); if (this.onclick) this.onclick({ target: this }); }
  _all() { return this.children.flatMap((c) => [c, ...c._all()]); }
  querySelectorAll(sel) { return this._all().filter((e) => matches(e, sel)); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
  closest(sel) { for (let e = this; e; e = e.parentElement) if (matches(e, sel)) return e; return null; }
  matches(sel) { return matches(this, sel); }
  getContext() { return null; }
  getBoundingClientRect() { return { left: 0, top: 0, width: 0, height: 0 }; }
  focus() {}
  scrollIntoView() {}
}

const VOID = new Set(["area", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "wbr"]);

function parseInto(root, html) {
  const stack = [root];
  const tagRe = /<!--[\s\S]*?-->|<\/([a-zA-Z][\w-]*)\s*>|<([a-zA-Z][\w-]*)((?:\s+[^\s=>\/]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+))?)*)\s*(\/?)>/g;
  let m;
  while ((m = tagRe.exec(html))) {
    if (m[1]) {
      const tag = m[1].toUpperCase();
      const i = stack.map((e) => e.tagName).lastIndexOf(tag);
      if (i > 0) stack.length = i;
      continue;
    }
    if (!m[2]) continue;
    const el = root.ownerDocument.createElement(m[2]);
    const attrRe = /([^\s=>\/]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g;
    let a;
    while ((a = attrRe.exec(m[3] || ""))) {
      const k = a[1], v = a[2] ?? a[3] ?? a[4] ?? "";
      if (k === "class") el.className = v;
      else if (k === "id") el.id = v;
      else if (k.startsWith("data-")) el.dataset[k.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = v;
      else if (k === "hidden") el.hidden = true;
      else if (k === "value") el.value = v;
      else el.attributes[k] = v;
    }
    stack[stack.length - 1].appendChild(el);
    if (!m[4] && !VOID.has(m[2].toLowerCase())) stack.push(el);
  }
}

function makeDocument({ strictIds = false } = {}) {
  const doc = { _ids: new Map(), readyState: "complete", listeners: {} };
  doc.createElement = (tag) => new Element(doc, tag);
  doc.head = new Element(doc, "head");
  doc.body = new Element(doc, "body");
  doc.documentElement = new Element(doc, "html");
  doc.getElementById = (id) => {
    if (doc._ids.has(id)) return doc._ids.get(id);
    if (strictIds) return null;
    const e = new Element(doc, "div");
    e.id = id; // a detached placeholder for an index.html element
    e.placeholder = true;
    return e;
  };
  doc.querySelectorAll = (sel) => doc.body.querySelectorAll(sel);
  doc.querySelector = (sel) => doc.body.querySelector(sel);
  doc.addEventListener = (t, fn) => { (doc.listeners[t] = doc.listeners[t] || []).push(fn); };
  return doc;
}

// A browser-ish global for vm: window === globalThis, no-op timers, a Tauri
// stub whose invoke never settles (so nothing runs after the first await).
function makeWindow(opts = {}) {
  const document = makeDocument(opts);
  const sandbox = {
    document,
    console,
    navigator: { userAgent: "node" },
    location: { href: "tauri://localhost/", hash: "" },
    performance: { now: () => 0, getEntriesByType: () => [] },
    localStorage: { getItem: () => null, setItem() {}, removeItem() {} },
    setTimeout: () => 0, clearTimeout() {}, setInterval: () => 0, clearInterval() {},
    requestAnimationFrame: () => 0, cancelAnimationFrame() {},
    getComputedStyle: () => ({ getPropertyValue: () => "" }),
    MutationObserver: class { observe() {} disconnect() {} },
    ResizeObserver: class { observe() {} disconnect() {} },
    fetch: () => new Promise(() => {}),
    alert() {}, confirm: () => false,
    addEventListener() {}, removeEventListener() {},
    __TAURI__: opts.tauri || {
      core: { invoke: () => new Promise(() => {}) },
      dialog: { open: () => new Promise(() => {}), save: () => new Promise(() => {}) },
      event: { listen: () => new Promise(() => {}) },
    },
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.globalThis = sandbox;
  const ctx = vm.createContext(sandbox);
  return { ctx, window: sandbox, document };
}

// The app scripts, in index.html order.
function indexScripts() {
  const html = fs.readFileSync(path.join(UI, "index.html"), "utf8");
  return Array.from(html.matchAll(/<script src="\.\/src\/([^"]+)"><\/script>/g), (m) => m[1]);
}

function load(ctx, name) {
  const file = path.join(UI, "src", name);
  new vm.Script(fs.readFileSync(file, "utf8"), { filename: file }).runInContext(ctx);
}

module.exports = { makeWindow, makeDocument, indexScripts, load, UI, Element };
