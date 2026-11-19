// App shell layout: a fixed-height window with two independent scroll
// containers (the sidebar nav and the content pane); the document never
// scrolls. Checked on the stylesheet and index.html, plus showPanel's
// per-panel scroll memory on the DOM stub.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import dom from "./support/dom.cjs";

const { makeWindow, indexScripts, load, UI } = dom;
const css = fs.readFileSync(path.join(UI, "src", "styles.css"), "utf8");
const html = fs.readFileSync(path.join(UI, "index.html"), "utf8");

// Declarations of the first top-level rule for `sel` (a rule starting a line).
function rule(sel) {
  const re = new RegExp("(?:^|\\n)" + sel.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + "\\s*\\{([^}]*)\\}");
  const m = css.match(re);
  assert.ok(m, `rule for ${sel}`);
  const out = {};
  for (const d of m[1].replace(/\/\*[\s\S]*?\*\//g, "").split(";")) {
    const i = d.indexOf(":");
    if (i > 0) out[d.slice(0, i).trim()] = d.slice(i + 1).trim();
  }
  return out;
}

test("the shell has a fixed height and the document does not scroll", () => {
  const app = rule("#app");
  assert.equal(app.height, "100vh");
  assert.equal(app.overflow, "hidden");
  assert.equal(app["grid-template-rows"], "minmax(0, 1fr)", "the row may not grow to the content height");
  assert.match(app["grid-template-columns"], /minmax\(0, 1fr\)$/);
  assert.equal(rule("body").overflow, "hidden");
});

test("sidebar nav and content pane are their own scroll containers", () => {
  const side = rule("#sidebar"), nav = rule("#nav"), main = rule("#main");
  for (const [name, r] of [["#sidebar", side], ["#nav", nav], ["#main", main]]) {
    assert.equal(r["overflow-y"], "auto", `${name} scrolls`);
    assert.equal(r["overscroll-behavior"], "contain", `${name} does not chain its scroll to the page`);
  }
  assert.equal(side["min-height"], "0");
  assert.equal(main["min-height"], "0");
  assert.equal(nav.flex, "1 1 0");
  // brand and footer (adapter box) stay put while the nav scrolls
  assert.match(css, /\.brand, \.sidebar-foot \{ flex: none; \}/);
});

test("index.html: #nav lives in #sidebar, #main is its sibling", () => {
  const side = html.indexOf('<aside id="sidebar">'), nav = html.indexOf('<nav id="nav">');
  const sideEnd = html.indexOf("</aside>"), main = html.indexOf('<main id="main">');
  assert.ok(side > 0 && side < nav && nav < sideEnd, "#nav inside #sidebar");
  assert.ok(sideEnd < main, "#main after (not inside) #sidebar");
  assert.ok(html.indexOf('<div id="app">') < side);
});

test("panel switches avoid transform animations on the whole panel", () => {
  const fade = css.match(/@keyframes fade \{([^\n]*)\}/);
  assert.ok(fade);
  assert.doesNotMatch(fade[1], /transform/);
  assert.doesNotMatch(css, /transition:\s*all\b/, "transition: all animates layout properties");
});

test("showPanel keeps each panel's scroll position in #main", () => {
  const env = makeWindow();
  const main = env.document.createElement("main");
  main.id = "main";
  env.document.body.appendChild(main);
  for (const s of indexScripts()) load(env.ctx, s);
  const { window } = env;
  const seen = [];
  const activated = window.OTNav.activated;
  window.OTNav.activated = (id) => { seen.push(id); return activated(id); };
  window.showPanel("maps");
  main.scrollTop = 300;
  window.showPanel("guide");
  assert.equal(main.scrollTop, 0, "a panel opens at its own (top) position");
  main.scrollTop = 40;
  window.showPanel("maps");
  assert.equal(main.scrollTop, 300, "back to where Maps was");
  window.showPanel("maps");
  assert.equal(main.scrollTop, 300, "re-showing the same panel does not jump");
  window.showPanel("guide");
  assert.equal(main.scrollTop, 40);
  assert.deepEqual(seen, ["maps", "guide", "maps", "maps", "guide"], "showPanel reports every switch to OTNav");
});

test("cheap repaints: no filter / path transitions on live tiles, contained tiles and grids", () => {
  const paused = rule(".dv-paused .dv-tile");
  assert.equal(paused.filter, undefined, "a filter re-rasterises every paused tile");
  assert.ok(paused.opacity, "paused tiles are dimmed with opacity");
  assert.equal(rule(".g-val").transition, undefined, "the gauge arc path is not animated on every update");
  assert.equal(rule(".dv-tile").contain, "content");
  assert.equal(rule(".heatwrap").contain, "content");
});

test("sidebar adapter rows fit the pane and keep the hidden DoIP row hidden", () => {
  const row = rule(".conn-row");
  assert.equal(row.display, "flex");
  assert.equal(row["min-width"], "0");
  const field = rule(".conn-row > .inp");
  assert.equal(field["min-width"], "0", "the port input (size=20) must be allowed to shrink below its intrinsic width");
  assert.equal(rule(".conn-row[hidden]").display, "none", "display: flex would beat the UA [hidden] rule");
});
