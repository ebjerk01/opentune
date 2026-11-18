// Pure UI helpers (src/util.js).
import test from "node:test";
import assert from "node:assert/strict";
// util.js is a classic script: importing it attaches window/globalThis.OTUtil.
import "../src/util.js";

const U = globalThis.OTUtil;

test("esc escapes markup and tolerates null", () => {
  assert.equal(U.esc('<a href="x">&</a>'), "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;");
  assert.equal(U.esc(null), "");
  assert.equal(U.esc(undefined), "");
  assert.equal(U.esc(0), "0");
  assert.equal(U.escQ("it's"), "it&#39;s");
});

test("baseName takes the last path segment of either separator", () => {
  assert.equal(U.baseName("/a/b/stock.bin"), "stock.bin");
  assert.equal(U.baseName("C:\\dumps\\tuned.ori"), "tuned.ori");
  assert.equal(U.baseName(""), "—");
  assert.equal(U.baseName(null), "—");
});

test("hex8 and parseHexInput round-trip offsets", () => {
  assert.equal(U.hex8(0), "0x00000000");
  assert.equal(U.hex8(0x1abc), "0x00001ABC");
  assert.equal(U.hex8(0xffffffff), "0xFFFFFFFF");
  assert.equal(U.parseHexInput("0x1ABC"), 0x1abc);
  assert.equal(U.parseHexInput("0X10"), 16);
  assert.equal(U.parseHexInput("ff"), 255);
  assert.ok(Number.isNaN(U.parseHexInput("zz")));
  assert.ok(Number.isNaN(U.parseHexInput("")));
  assert.equal(U.parseHexInput(U.hex8(4096)), 4096);
});

test("cell and axis number formatting", () => {
  assert.equal(U.fmtCell(1.234), "1.23");
  assert.equal(U.fmtCell(-0.5), "-0.50");
  assert.equal(U.fmtCell(123.456), "123.5");
  assert.equal(U.fmtCell(-100), "-100.0");
  assert.equal(U.fmtAxis(null), "");
  assert.equal(U.fmtAxis(1500), "1500");
  assert.equal(U.fmtAxis(2.25), "2.3");
});

test("clone2d copies rows", () => {
  const g = [[1, 2], [3, 4]];
  const c = U.clone2d(g);
  c[0][0] = 9;
  assert.equal(g[0][0], 1);
  assert.deepEqual(c, [[9, 2], [3, 4]]);
});

test("heatColor clamps and spans the ramp", () => {
  assert.equal(U.heatColor(0), "rgb(43,108,176)");
  assert.equal(U.heatColor(1), "rgb(197,48,48)");
  assert.equal(U.heatColor(-5), U.heatColor(0));
  assert.equal(U.heatColor(7), U.heatColor(1));
  assert.equal(U.heatColor(0.5), "rgb(214,158,46)");
});

test("splitList trims and drops empties", () => {
  assert.deepEqual(U.splitList(" 22 F190, ,1902 ,,"), ["22 F190", "1902"]);
  assert.deepEqual(U.splitList(null), []);
});

test("project edit identity and changed-cell count", () => {
  assert.equal(U.editKey({ kind: "def", defId: "edc16", mapName: "limiter" }), "def:edc16:limiter");
  assert.equal(U.editKey({ kind: "adhoc", mapName: "m", mapdef: { address: "0x100" } }), "adhoc:m:0x100");
  assert.equal(U.editKey({ kind: "adhoc", mapName: "m" }), "adhoc:m:");
  assert.equal(U.cellsChanged([[1, 2], [3, 4]], [[1, 2], [3, 4]]), 0);
  assert.equal(U.cellsChanged([[1, 2.5], [3, 5]], [[1, 2], [3, 4]]), 2);
  assert.equal(U.cellsChanged([[1, 2], [3, 4]], [[1]]), 0, "missing baseline cells count as unchanged");
  assert.equal(U.cellsChanged(null, [[1]]), 0);
});

test("small markup pieces escape their input", () => {
  assert.equal(U.confPill("High"), '<span class="pill ok">High</span>');
  assert.equal(U.confPill("None"), '<span class="pill bad">None</span>');
  assert.equal(U.confPill("<x>"), '<span class="pill accent">&lt;x&gt;</span>');
  assert.equal(U.listOrDash([]), "—");
  assert.equal(U.listOrDash(["a<", "b"]), "<code>a&lt;</code> <code>b</code>");
  assert.match(U.imgBadge("match"), /image matches/);
  assert.match(U.imgBadge("mismatch"), /CHANGED/);
  assert.match(U.imgBadge("whatever"), /not verified/);
});

test("markdown: links are limited to http(s) and guide anchors", () => {
  assert.deepEqual(U.mdHref("https://example.org/x"), { href: "https://example.org/x", external: true });
  assert.deepEqual(U.mdHref("#/guide/20-utilities"), { href: "#/guide/20-utilities", guide: "20-utilities" });
  assert.equal(U.mdHref("javascript:alert(1)"), null);
  assert.equal(U.mdHref("../secret"), null);
  const html = U.mdInline("[ok](https://example.org) and [bad](javascript:x) and [g](#/guide/a)");
  assert.match(html, /<a class="mdlink" href="https:\/\/example.org" target="_blank"/);
  assert.ok(!html.includes("javascript:"), html);
  assert.match(html, /data-guide="a"/);
});

test("markdown: inline code, emphasis and escaping", () => {
  assert.equal(U.mdInline("**bold** and *em*"), "<strong>bold</strong> and <em>em</em>");
  assert.equal(U.mdInline("`a*b*<c>`"), "<code>a*b*&lt;c&gt;</code>");
  assert.equal(U.mdInline("<script>"), "&lt;script&gt;");
  assert.equal(U.mdInline("nul\u0000byte"), "nulbyte");
});

test("markdown: blocks", () => {
  const md = [
    "# Title",
    "",
    "Para one",
    "continues.",
    "",
    "- item a",
    "  more a",
    "- item b",
    "",
    "1. first",
    "2. second",
    "",
    "| k | v |",
    "|---|---|",
    "| a | `b` |",
    "",
    "> quoted",
    "",
    "---",
    "```",
    "<raw> **not bold**",
    "```",
  ].join("\n");
  const html = U.renderMarkdown(md);
  assert.match(html, /<h1 class="mdh1">Title<\/h1>/);
  assert.match(html, /<p>Para one continues.<\/p>/);
  assert.match(html, /<ul class="mdlist"><li>item a more a<\/li><li>item b<\/li><\/ul>/);
  assert.match(html, /<ol class="mdlist"><li>first<\/li><li>second<\/li><\/ol>/);
  assert.match(html, /<thead><tr><th>k<\/th><th>v<\/th><\/tr><\/thead><tbody><tr><td>a<\/td><td><code>b<\/code><\/td><\/tr>/);
  assert.match(html, /<blockquote class="mdquote">quoted<\/blockquote>/);
  assert.match(html, /<hr\/>/);
  assert.match(html, /<pre class="mdcode">&lt;raw&gt; \*\*not bold\*\*<\/pre>/);
  assert.equal(U.renderMarkdown("a\r\nb"), "<p>a b</p>");
});

test("panel help strip markup", () => {
  const html = U.helpStripHtml({ risk: "writes-ecu", needs: ["CAN adapter", "<x>"], guide: "29-service" }, ["Plan", "Run's step"]);
  assert.match(html, /<span class="pill bad">Can write to the car<\/span>/);
  assert.match(html, /<span class="ph-need">CAN adapter<\/span><span class="ph-need">&lt;x&gt;<\/span>/);
  assert.match(html, /ph-guide/);
  assert.match(html, /<ol class="ph-steps"><li>Plan<\/li><li>Run&#39;s step<\/li><\/ol>/);
  const bare = U.helpStripHtml({}, undefined);
  assert.match(bare, /pill neutral">Offline/);
  assert.match(bare, /Nothing — works offline/);
  assert.ok(!bare.includes("ph-guide") && !bare.includes("ph-steps"));
  assert.match(U.helpStripHtml({ risk: "unknown-risk" }), /Offline/);
});

test("logModeBadge describes fast and polling sessions", () => {
  assert.equal(U.logModeBadge(null), null);
  const fast = U.logModeBadge({
    mode: "fast-2c", reason: null, polled: ["obd_rpm"], redefines: 1,
    dynamic_dids: [{ did: 0xf200, record_len: 13, channels: ["a", "b"] }],
  });
  assert.equal(fast.text, "fast-2c");
  assert.equal(fast.cls, "ok");
  assert.match(fast.title, /0xF200 \(13 B, 2 ch\)/);
  assert.match(fast.title, /1 channel\(s\) polled/);
  assert.match(fast.title, /re-defined 1×/);
  const fb = U.logModeBadge({ mode: "polling", reason: "0x2C refused: NRC 0x11" });
  assert.deepEqual([fb.text, fb.cls, fb.title], ["polling", "accent", "0x2C refused: NRC 0x11"]);
  assert.equal(U.logModeBadge({ mode: "polling", reason: "fast mode not requested" }).cls, "neutral");
});

test("seriesPath scales a series into its box", () => {
  assert.deepEqual(U.seriesPath([], [], 100, 10), { d: "", lo: null, hi: null });
  const p = U.seriesPath([0, 1, 2], [0, 50, 100], 100, 10);
  assert.equal(p.d, "M0,10L50,5L100,0");
  assert.deepEqual([p.lo, p.hi], [0, 100]);
  assert.equal(U.seriesPath([5, 6], [3, 3], 10, 10).d, "M0,5L10,5", "flat series mid-height");
  assert.equal(U.seriesPath([1], [7], 10, 10).d, "M5,5");
});

test("tsFmt and tsCheckValue follow the INI's digits and limits", () => {
  assert.equal(U.tsFmt(12.345, 1), "12.3");
  assert.equal(U.tsFmt(7, 0), "7");
  assert.equal(U.tsFmt(0.1 + 0.2, null), "0.3");
  assert.equal(U.tsFmt(null, 2), "—");
  const m = { name: "reqFuel", min: 0, max: 25.5, units: "ms" };
  assert.equal(U.tsCheckValue(m, 12), null);
  assert.equal(U.tsCheckValue(m, 25.5), null);
  assert.equal(U.tsCheckValue(m, 30), "reqFuel = 30 is outside 0..25.5 ms");
  assert.match(U.tsCheckValue(m, NaN), /not a number/);
  // Expression limits ({ expr }) are left to the backend.
  assert.equal(U.tsCheckValue({ name: "x", min: { expr: "a" }, max: null }, -5), null);
});

test("fmtLogRate shows samples/s and requests per sample", () => {
  assert.equal(U.fmtLogRate(null), "");
  assert.equal(U.fmtLogRate({ ticks: 0 }), "");
  assert.equal(U.fmtLogRate({ ticks: 10, samples_per_s: 4.26, requests_per_sample: 1 }), "4.3 samples/s · 1.00 req/sample");
  assert.equal(U.fmtLogRate({ ticks: 99, samples_per_s: 51.04, requests_per_sample: 3 }), "51 samples/s · 3.00 req/sample");
});
