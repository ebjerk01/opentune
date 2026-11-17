// OpenTune — ECU / TCU database.
// Searchable reference of control units: CPU, flash, how to read them (OBD /
// bench / boot), checksum family, vehicles, and bench pinouts with a confidence
// level. Offline. Backend: db_list, db_get.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const KIND = { ecu: "Engine ECU", tcu: "Gearbox TCU", awd: "AWD unit" };
  const CONF = {
    verified: ["ok", "Verified — 2+ sources agree"],
    community: ["accent", "Community — one source"],
    unverified: ["bad", "Unverified"],
  };
  const SIG = { "12v_perm": "+12 V permanent", "12v_ign": "+12 V ignition", gnd: "Ground", can_h: "CAN high", can_l: "CAN low", kline: "K-line", boot: "Boot", other: "Other" };

  let kind = "";
  let q = "";
  let current = null;
  let seq = 0;

  const panel = document.createElement("section");
  panel.className = "panel wide";
  panel.id = "panel-database";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>ECU database</h1>
      <p>Look up engine, gearbox and AWD control units: what is inside, how to read them, which checksum applies, and how to wire them on the bench.</p>
    </header>
    <div class="row">
      <input class="inp wide" id="dbQ" type="search" placeholder="Search: EDC16, 03G906021, Golf, TC1796…" autocomplete="off">
      <div class="seg" role="group" aria-label="Kind">
        <button type="button" data-k="" class="on">All</button><button type="button" data-k="ecu">ECU</button><button type="button" data-k="tcu">TCU</button><button type="button" data-k="awd">AWD</button>
      </div>
      <span class="small muted" id="dbCount"></span>
    </div>
    <div class="db-layout">
      <div class="card db-list" id="dbList"></div>
      <div class="db-detail" id="dbDetail"><div class="card muted">Pick a unit to see its details.</div></div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "database", label: "ECU database", icon: "▤", group: "reference", tier: "public" });

  const $ = (id) => panel.querySelector("#" + id);
  // "unknown" means no source says either way: show it, but never as available.
  const yes = (v) => v === true || (typeof v === "string" && v !== "none" && v !== "unknown" && v !== "");
  const word = (v) => (v === "unknown" ? "unknown" : yes(v) ? (typeof v === "string" ? v : "yes") : "no");

  function methods(m) {
    m = m || {};
    return [["OBD", m.obd], ["Bench", m.bench], ["Boot", m.boot]]
      .map(([k, v]) => `<span class="rm${yes(v) ? " on" : ""}" title="${esc(typeof v === "string" ? v : yes(v) ? "yes" : "no")}">${k}</span>`).join("");
  }

  async function list() {
    const my = ++seq;
    let rows = [];
    try { rows = await invoke("db_list", { kind: kind || null, q: q || null }); } catch (e) {
      $("dbList").innerHTML = `<div class="banner bad">${esc(e)}</div>`;
      return;
    }
    if (my !== seq) return;
    $("dbCount").textContent = `${rows.length} unit${rows.length === 1 ? "" : "s"}`;
    if (!rows.length) { $("dbList").innerHTML = '<p class="muted small">No unit matches. Try a family (EDC17), a part number or a car.</p>'; return; }
    $("dbList").innerHTML = rows.map((r) => `
      <button type="button" class="db-row${current === r.id ? " active" : ""}" data-id="${esc(r.id)}">
        <span class="db-name">${esc(r.name)}</span>
        <span class="db-kind">${esc(KIND[r.kind] || r.kind)}</span>
        <span class="db-sub">${esc([r.maker, r.cpu, r.flash_kib ? r.flash_kib + " KiB" : ""].filter(Boolean).join(" · "))}</span>
        <span class="db-meta">${methods(r.read_methods)}${r.has_pinout ? `<i class="cdot ${(CONF[r.pinout_confidence] || CONF.unverified)[0]}" title="pinout: ${esc(r.pinout_confidence)}"></i>` : ""}</span>
      </button>`).join("");
    $("dbList").querySelectorAll(".db-row").forEach((b) => b.addEventListener("click", () => show(b.dataset.id)));
    if (!current && rows[0]) show(rows[0].id);
  }

  function kv(k, v) { return v == null || v === "" || (Array.isArray(v) && !v.length) ? "" : `<div class="k">${k}</div><div class="v">${esc(Array.isArray(v) ? v.join(", ") : v)}</div>`; }

  function pinout(u) {
    const conns = (u.connectors || []).filter((c) => c.pins && c.pins.length);
    const conf = CONF[u.pinout_confidence] || CONF.unverified;
    const head = `<div class="card-title-row"><h2>Bench pinout</h2>${conns.length ? `<span class="pill ${conf[0]}">${conf[1]}</span>` : ""}</div>`;
    if (!conns.length) return `<div class="card">${head}<p class="muted small">No sourced pinout for this unit yet. ${esc((u.bench && u.bench.notes) || "")}</p></div>`;
    const b = u.bench || {};
    const sum = [["Power", b.power], ["Ignition", b.ign], ["Ground", b.ground], ["CAN H / L", [].concat(b.can_h || [], b.can_l || [])], ["K-line", b.kline], ["Boot", b.boot]]
      .filter(([, v]) => v && (!Array.isArray(v) || v.length))
      .map(([k, v]) => `<div class="k">${k}</div><div class="v">${esc([].concat(v).join(", "))}</div>`).join("");
    return `<div class="card">${head}
      <div class="banner bad">Check every pin with a multimeter before applying power. Use a current-limited bench supply (about 1 A) and 120 Ω between CAN high and low if the unit is alone on the bus.${u.safety_note ? "<br>" + esc(u.safety_note) : ""}</div>
      ${sum ? `<div class="meta-grid db-bench">${sum}</div>` : ""}
      ${conns.map((c) => `
        <h3 class="db-conn">${esc(c.name)}</h3>${c.description ? `<p class="small muted">${esc(c.description)}</p>` : ""}
        <div class="db-tw"><table class="tbl"><thead><tr><th class="mono">Pin</th><th>Function</th><th>Signal</th><th>Note</th></tr></thead><tbody>
        ${c.pins.map((p) => `<tr${p.confidence === "unverified" ? ' class="pin-unv"' : ""}><td class="mono">${esc(p.pin)}</td><td>${esc(p.function)}${p.confidence === "unverified" ? ' <span class="pill bad">disputed — do not wire</span>' : ""}</td><td><span class="sig sig-${esc(p.signal)}">${esc(SIG[p.signal] || p.signal)}</span></td><td class="small muted">${esc(p.note || "")}</td></tr>`).join("")}
        </tbody></table></div>`).join("")}
      ${b.notes ? `<p class="note">${esc(b.notes)}</p>` : ""}
    </div>`;
  }

  async function show(id) {
    current = id;
    $("dbList").querySelectorAll(".db-row").forEach((b) => b.classList.toggle("active", b.dataset.id === id));
    let u;
    try { u = await invoke("db_get", { id }); } catch (e) { $("dbDetail").innerHTML = `<div class="banner bad">${esc(e)}</div>`; return; }
    const m = u.read_methods || {};
    const vehicles = (u.vehicles || []).map((v) => `<span class="ph-need">${esc(typeof v === "string" ? v : [v.make, v.model, v.engine, v.years ? "(" + v.years + ")" : ""].filter(Boolean).join(" "))}</span>`).join("");
    const sources = (u.sources || []).map((s) => `<li><a href="${esc(s.url)}" target="_blank" rel="noopener">${esc(s.title || s.url)}</a></li>`).join("");
    $("dbDetail").innerHTML = `
      <div class="card">
        <div class="card-title-row"><h2 class="db-title">${esc(u.name)}</h2><span class="pill neutral">${esc(KIND[u.kind] || u.kind)}</span></div>
        <div class="meta-grid">
          ${kv("Maker", u.maker)}${kv("Family", u.family)}${kv("CPU", u.cpu)}${kv("Flash", u.flash_kib ? u.flash_kib + " KiB" : null)}${kv("EEPROM", u.eeprom)}
          ${kv("Bus", u.bus)}${kv("Protocols", u.protocols)}${kv("Part numbers", u.part_number_examples)}
          ${u.checksum ? `<div class="k">Checksum</div><div class="v">${esc(u.checksum)} <span class="pill ok">supported</span></div>` : kv("Checksum", "not supported in OpenTune yet")}
          ${u.ecudef ? `<div class="k">Definition</div><div class="v">${esc(u.ecudef)}</div>` : ""}
        </div>
        ${u.notes ? `<p class="note">${esc(u.notes)}</p>` : ""}
      </div>
      <div class="card"><h2>How to read it</h2>
        <div class="db-methods">
          ${[["OBD port", m.obd], ["Bench", m.bench], ["Boot mode", m.boot]].map(([k, v]) => `
            <div class="dbm${yes(v) ? " on" : ""}"><div class="s-k">${k}</div><div class="dbm-v">${esc(word(v))}</div></div>`).join("")}
        </div>
        ${m.notes ? `<p class="small muted">${esc(m.notes)}</p>` : ""}
      </div>
      ${vehicles ? `<div class="card"><h2>Vehicles</h2><div class="ph-needs">${vehicles}</div></div>` : ""}
      ${pinout(u)}
      ${sources ? `<div class="card"><h2>Sources</h2><ul class="db-src small">${sources}</ul></div>` : ""}`;
  }

  let t = null;
  $("dbQ").addEventListener("input", (e) => { q = e.target.value.trim(); clearTimeout(t); t = setTimeout(list, 150); });
  panel.querySelectorAll(".seg button").forEach((b) => b.addEventListener("click", () => {
    kind = b.dataset.k;
    panel.querySelectorAll(".seg button").forEach((x) => x.classList.toggle("on", x === b));
    list();
  }));
  list();
})();
