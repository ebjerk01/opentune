// Data-driven wiring diagrams (src/pinout-svg.js) and the guide ```pinout``` fence.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import "../src/util.js";
import "../src/pinout-svg.js";

const P = globalThis.OTPinout;
const U = globalThis.OTUtil;
const here = dirname(fileURLToPath(import.meta.url));
const unitsDir = join(here, "../../../database/units");
const unit = (id) => JSON.parse(readFileSync(join(unitsDir, `${id}.json`), "utf8"));

test("EDC15C6: both disputes are dashed, agreed pins show 2 sources", () => {
  const s = P.svg(unit("bosch-edc15c6-mercedes"));
  assert.match(s, /^<svg class="pinout-svg"/);
  for (const lab of ["1-1", "1-5", "1-6", "1-7", "1-8", "2-13", "3-28"]) assert.ok(s.includes(`>${lab}<`), lab);
  // Four disputed pins (ground 1-5/1-6, second KL15 1-7/1-8) are dashed.
  assert.equal((s.match(/<path class="pd-w s-[a-z0-9_]+ pd-dash"/g) || []).length, 4);
  for (const lab of ["1-5", "1-6", "1-7", "1-8"]) assert.ok(s.includes(`<title>${lab} — disputed, confirm by continuity</title>`), lab);
  // KL30 2-13, KL15 1-1 and K-line 3-28 agree across two sources.
  assert.equal((s.match(/2 src ✓/g) || []).length, 3);
  for (const t of ["+12 V permanent (KL30)", "+12 V ignition (KL15)", "Ground (−)", "K-line"]) assert.ok(s.includes(t), t);
  assert.ok(s.includes("confirm by continuity before wiring"));
  assert.ok(s.includes("not that this unit was measured"));
  assert.ok(!/boot strap/i.test(s), "no boot target: boot is board-level, not on the connector");
});

test("every unit with pins renders a diagram; units without pins render nothing", () => {
  let drawn = 0;
  for (const f of readdirSync(unitsDir).filter((f) => f.endsWith(".json"))) {
    const u = JSON.parse(readFileSync(join(unitsDir, f), "utf8"));
    const pins = (u.connectors || []).reduce((n, c) => n + (c.pins || []).length, 0);
    const s = P.svg(u);
    if (pins) { assert.match(s, /<svg[\s\S]*<\/svg>$/, f); drawn++; } else assert.equal(s, "", f);
  }
  assert.ok(drawn >= 15, `diagrams drawn: ${drawn}`);
});

test("markup in unit data is escaped", () => {
  const s = P.svg({ id: "x", name: "<b>x</b>", pinout_confidence: "community",
    connectors: [{ name: "A<", pins: [{ pin: "1", function: "<script>alert(1)</script>", signal: "gnd", sources: [0] }] }] });
  assert.ok(!s.includes("<script>"));
  assert.ok(s.includes("&lt;script&gt;"));
});

test("guide fence ```pinout <id>``` becomes a placeholder; other fences stay code", () => {
  const h = U.renderMarkdown("Intro\n\n```pinout bosch-edc15c6-mercedes\n```\n\n```\ncode\n```");
  assert.ok(h.includes('<div class="pinout-embed" data-unit="bosch-edc15c6-mercedes"></div>'), h);
  assert.ok(h.includes('<pre class="mdcode">code</pre>'));
  // Ids are restricted: no markup can be smuggled in.
  const bad = U.renderMarkdown('```pinout x"><img src=x>\n```');
  assert.ok(!bad.includes("pinout-embed"));
});

test("hydrate fills placeholders and reports missing units", async () => {
  const slot = (id) => ({ attr: id, innerHTML: "", getAttribute() { return this.attr; } });
  const a = slot("bosch-edc15c6-mercedes"), b = slot("nope");
  const root = { querySelectorAll: () => [a, b] };
  await P.hydrate(root, async (id) => { if (id === "nope") throw new Error("unknown unit"); return unit(id); });
  assert.match(a.innerHTML, /^<svg/);
  assert.match(b.innerHTML, /banner bad/);
  assert.match(b.innerHTML, /unknown unit/);
});

test("EDC15C6 guide page embeds its diagram", () => {
  const md = readFileSync(join(here, "../../src-tauri/guide/44-edc15c6-bench.md"), "utf8");
  assert.ok(md.includes("```pinout bosch-edc15c6-mercedes"));
  for (const p of ["S2-13", "S1-1", "S3-28", "S1-5", "S1-6", "S1-7", "S1-8"]) assert.ok(md.includes(p), p);
});
