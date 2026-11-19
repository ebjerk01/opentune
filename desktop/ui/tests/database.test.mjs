// ECU database panel (src/feat_database.js): the access-profile card (OBD /
// bench / boot / security with confidence pills, the "open case" marker and
// the boot points) rendered from the repository's database/units, on the DOM
// stub, plus escaping and the no-profile case.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import dom from "./support/dom.cjs";

const { makeWindow, load } = dom;
const UNITS = path.join(dom.UI, "..", "..", "database", "units");
const units = fs.readdirSync(UNITS).filter((f) => f.endsWith(".json")).sort()
  .map((f) => JSON.parse(fs.readFileSync(path.join(UNITS, f), "utf8")));
const unit = (id) => units.find((u) => u.id === id);
const tick = (ms = 30) => new Promise((r) => setTimeout(r, ms));

function panel() {
  const calls = [];
  const invoke = async (cmd, args) => {
    calls.push([cmd, args]);
    if (cmd === "db_list") return units.map((u) => ({ id: u.id, kind: u.kind, maker: u.maker, family: u.family, name: u.name, cpu: u.cpu, flash_kib: u.flash_kib, bus: u.bus, read_methods: u.read_methods, checksum: u.checksum, ecudef: u.ecudef, vehicle_count: u.vehicles.length, has_pinout: u.connectors.some((c) => c.pins.length), pinout_confidence: u.pinout_confidence, has_access: !!u.access }));
    if (cmd === "db_get") { const u = unit(args.id); if (!u) throw `unknown unit '${args.id}'`; return u; }
    throw new Error(`unexpected ${cmd}`);
  };
  const { ctx, window, document } = makeWindow({ tauri: { core: { invoke } } });
  const main = document.createElement("main");
  main.id = "main";
  document.body.appendChild(main);
  load(ctx, "nav.js");
  load(ctx, "feat_database.js");
  const p = document.getElementById("panel-database");
  return { window, p, $: (id) => p.querySelector("#" + id), calls };
}

test("EDC15C6: access card shows every part, open case and the unverified DQ4 point", async () => {
  const { $, p, window } = panel();
  assert.ok(p && !p.placeholder, "feat_database.js creates #panel-database");
  await tick();
  const row = $("dbList").querySelectorAll(".db-row").find((b) => b.dataset.id === "bosch-edc15c6-mercedes");
  assert.ok(row, "EDC15C6 is listed");
  row.click();
  await tick();
  const h = $("dbDetail").innerHTML;
  assert.match(h, /<h2>Access<\/h2>/);
  for (const t of ["OBD port", "Bench", "Boot mode", "Security"]) assert.ok(h.includes(`>${t}</h3>`), t);
  assert.ok(h.includes('<span class="pill bad">open case</span>'), "open-case marker");
  assert.ok(h.includes("the case has to be opened"), "open-case banner");
  assert.ok(h.includes("board pad"), "entry label");
  // The boot point: board location, action with resistor, unverified pill.
  assert.match(h, /<tr class="pin-unv">[\s\S]*Flash DQ4 to CPU P0L\.4\/AD4[\s\S]*Board: flash DQ4 \(pin 24 of the SO-44 29F400\/29F800\)[\s\S]*pull to ground \(8000 Ω\)[\s\S]*>unverified<\/span> <span class="small">confirm by continuity/);
  assert.ok(h.includes("OpenTune ships no OEM key"));
  assert.ok(h.includes("Full read over diagnostics</div><div class=\"v\">required"));
  // Confidence pills per part (obd/boot/security unverified, bench community).
  assert.equal((h.match(/<span class="pill accent" title="Community — one source">community<\/span>/g) || []).length, 1);
  // Flash note and numbered sources the refs point at.
  assert.ok(h.includes("Flash note"));
  assert.match(h, /<ol class="db-src small"><li value="1">/);
  const u = unit("bosch-edc15c6-mercedes");
  const last = u.access.boot.sources[u.access.boot.sources.length - 1] + 1;
  assert.ok(h.includes(`[${last}]`) && h.includes(`<li value="${last}">`), "source refs resolve to numbered entries");
  assert.equal(typeof window.OTDbAccess.html, "function");
});

test("every filled unit renders its card; units without a profile say so", () => {
  const { window } = panel();
  let filled = 0;
  for (const u of units) {
    const h = window.OTDbAccess.html(u);
    if (u.access) {
      filled++;
      assert.ok(h.includes("Boot mode") && h.includes("Security"), u.id);
      if (!u.access.boot.possible) assert.ok(!h.includes("open case"), u.id);
    } else assert.match(h, /No access profile for this unit yet/, u.id);
  }
  assert.ok(filled >= 4, `filled: ${filled}`);
});

test("connector boot points and markup in access data are escaped", () => {
  const { window } = panel();
  const h = window.OTDbAccess.html({
    access: {
      obd: { mode: "unknown", protocol: "<script>x</script>", notes: "n", confidence: "community", sources: [0] },
      bench: { possible: true, how: "h", confidence: "verified", sources: [0, 1] },
      boot: { possible: true, entry: "connector-pin", reachable: "external-connector", notes: "n", confidence: "community", sources: [0],
        points: [{ name: "<b>boot</b>", location: "connector:A-7", action: "pull-to-gnd", confidence: "community" }] },
      security: { diag_read: "unknown", flash_write: "not-required", notes: "OpenTune ships no OEM key.", confidence: "unverified", sources: [0] },
    },
  });
  assert.ok(!h.includes("<script>") && !h.includes("<b>boot</b>"));
  assert.ok(h.includes("&lt;script&gt;"));
  assert.ok(h.includes("Connector pin A-7"));
  assert.ok(h.includes('<span class="pill ok">external connector</span>'));
  assert.ok(!h.includes("the case has to be opened"));
  assert.ok(h.includes("not required"));
});
