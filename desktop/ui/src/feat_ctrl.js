// OpenTune — Controllers panel: drivetrain controller definitions (TCU / AWD).
// Lists the built-in ctrldefs/ set plus the controllers/ of every definition
// pack (installed or loaded from disk through the pack picker), shows one
// definition (identity, diagnostics, CAN frame sets with signals, live
// channels, adaptations, tables, gear ratios, safety notes, sources and its
// validation state) and decodes a CAN log offline with the same reader and
// decoder as `opentune controller decode`. Nothing here talks to a car, so
// the panel is tier "public"; the related live tools (`opentune dsg`, the
// Haldex panel) stay Bench. Backend: feat_ctrl.rs (ctrl_list / ctrl_get /
// ctrl_decode_log); docs/CONTROLLER-DEFS.md.
"use strict";
(function () {
  const TAURI = () => window.__TAURI__ || {};
  const invoke = (c, a) => TAURI().core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const msg = (e) => (typeof e === "string" ? e : (e && e.message) || String(e));
  const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "");
  const KIND = { tcu: "Gearbox TCU", awd: "AWD unit", transfer_case: "Transfer case", differential: "Differential", other: "Driveline" };
  // DEVELOPMENT.md §7 words → pill class.
  const VAL = {
    implemented: "bad", "synthetic-tested": "neutral", "real-image-tested": "accent", "bench-tested": "accent", "vehicle-tested": "ok",
  };
  const ROWS_SHOWN = 400;

  // ---- pure helpers (unit-tested) -----------------------------------------
  // A number without trailing noise (12, 12.5, 0.004), like the CLI.
  function fmtNum(v) {
    if (typeof v !== "number" || !isFinite(v)) return String(v);
    if (Number.isInteger(v)) return String(v);
    return String(Number(v.toPrecision(6)));
  }
  // A decoded value ({type, value}) as text, like the CLI (opentune_ctrldef::Value).
  function fmtValue(v) {
    if (!v) return "";
    switch (v.type) {
      case "number": return fmtNum(v.value);
      case "bool": return v.value ? "1" : "0";
      case "enum": return v.value == null ? "?" : String(v.value);
      case "ascii": return `'${v.value}'`;
      case "invalid": return "n/a";
      default: return String(v.value == null ? "" : v.value);
    }
  }
  // Signal statistics as one line: range, most frequent states, last value.
  function statsText(s, unit) {
    if (!s) return "";
    const parts = [];
    if (s.min != null && s.max != null) parts.push(`${fmtNum(s.min)} .. ${fmtNum(s.max)}${unit ? " " + unit : ""}`);
    const counts = Object.entries(s.counts || {}).sort((a, b) => b[1] - a[1]);
    if (counts.length) parts.push(counts.slice(0, 6).map(([k, n]) => `${k} ×${n}`).join(", ") + (counts.length > 6 ? `, +${counts.length - 6} more` : ""));
    let out = parts.join(", ");
    if (s.last) out += `${out ? "  " : ""}last ${fmtValue(s.last)}`;
    if (s.invalid) out += ` (${s.invalid} n/a)`;
    return out;
  }
  // A signal's scaling as text.
  function sigScale(s) {
    if (s.curve && s.curve.length) return `curve (${s.curve.length} points)`;
    const f = s.factor == null ? 1 : s.factor, o = s.offset || 0;
    if (f === 1 && !o) return "raw";
    return `×${fmtNum(f)}${o ? (o > 0 ? " + " : " − ") + fmtNum(Math.abs(o)) : ""}`;
  }
  function liveSource(src) {
    if (!src) return "";
    if (src.kind === "can_signal") return `CAN ${src.bus}/${src.frame}.${src.signal}`;
    if (src.kind === "kwp_measuring_block") return `KWP 0x21 group ${src.group} cell ${src.cell}`;
    if (src.kind === "uds_did") return `UDS 0x22 ${src.did} byte ${src.byte || 0} len ${src.len}`;
    return src.kind || "";
  }
  const sourceLabel = (r) => (r.source === "pack" && r.pack ? `pack ${r.pack.id}${r.pack.version ? " v" + r.pack.version : ""}` : r.source === "file" ? "file" : "built-in");

  // ---- panel ----------------------------------------------------------------
  const panel = document.createElement("section");
  panel.className = "panel wide";
  panel.id = "panel-controllers";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>Controllers</h1>
      <p>Drivetrain controller definitions — gearbox TCUs and AWD couplings: how to identify and reach them, the CAN frames they send, live channels, tables and gear ratios. Decode a CAN capture offline with a definition's frame sets.</p>
    </header>
    <div class="row">
      <input class="inp wide" id="ctQ" type="search" placeholder="Search: DQ250, 722.6, Haldex, 0AY907554C, 02E…" autocomplete="off">
      <div class="seg" id="ctKind" role="group" aria-label="Kind">
        <button type="button" data-k="" class="on">All</button><button type="button" data-k="tcu">TCU</button><button type="button" data-k="awd">AWD</button>
      </div>
      <span class="small muted" id="ctCount"></span>
    </div>
    <div class="card" id="ctPackCard">
      <h2>From a definition pack</h2>
      <p class="small muted">The list holds the built-in definitions and those in every installed pack. <b>Load pack…</b> adds a pack from disk; <b>File…</b> opens a single definition.</p>
      <div id="ctPicker"></div>
    </div>
    <div class="db-layout">
      <div class="card db-list" id="ctList"></div>
      <div class="db-detail">
        <div id="ctDetail"><div class="card muted">Pick a definition to see it.</div></div>
        <div class="card" id="ctDecodeCard" hidden>
          <h2>Decode a CAN log</h2>
          <p class="small muted">A candump log, OpenTune CSV, SavvyCAN CSV or Vector ASC capture (e.g. from <b>CAN capture</b>), decoded with this definition's frame sets — the same decoder as <span class="mono">opentune controller decode</span>. Offline: nothing is sent.</p>
          <div class="row">
            <button class="btn" id="ctPick" type="button">Open log…</button>
            <span class="small mono muted" id="ctLogPath">no log chosen</span>
            <select class="inp" id="ctBus" title="Frame set"></select>
            <button class="btn primary" id="ctDecode" type="button" disabled>Decode</button>
          </div>
          <div id="ctDecodeOut"></div>
        </div>
      </div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "controllers", label: "Controllers", icon: "⚙", group: "reference", tier: "public" });

  const $ = (id) => panel.querySelector("#" + id);
  let kind = "";
  let q = "";
  let current = null; // path of the definition shown
  let shown = null; // ctrl_get result
  let logPath = null;
  let seq = 0;

  // Pack folders the pack picker loaded from disk (remembered across panels).
  async function extraDirs() {
    try {
      const lib = window.OTPacks ? await window.OTPacks.library() : null;
      return ((lib && lib.packs) || []).filter((p) => p.source === "file").map((p) => p.dir);
    } catch (_) { return []; }
  }

  async function list() {
    const my = ++seq;
    let r;
    try {
      r = await invoke("ctrl_list", { q: q || null, kind: kind || null, extra: await extraDirs() });
    } catch (e) {
      $("ctList").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
      return;
    }
    if (my !== seq) return;
    const rows = r.defs || [];
    $("ctCount").textContent = `${rows.length} of ${r.total || rows.length} definition${(r.total || rows.length) === 1 ? "" : "s"}`;
    const failed = r.failed || [];
    const failedHtml = failed.length
      ? `<details class="small"><summary>${failed.length} file${failed.length === 1 ? "" : "s"} did not load</summary>` +
        failed.map((f) => `<div><span class="mono" title="${esc(f.file)}">${esc(baseName(f.file))}</span>${f.pack ? ` (pack ${esc(f.pack.id)})` : ""} — ${esc(f.message)}</div>`).join("") + `</details>`
      : "";
    if (!rows.length) {
      $("ctList").innerHTML = `<p class="muted small">${r.total ? "No definition matches. Try a family (DQ250), a part number or a car." : "No controller definitions found."}</p>` + failedHtml;
      return;
    }
    $("ctList").innerHTML = rows.map((d) => `
      <button type="button" class="db-row${current === d.path ? " active" : ""}" data-path="${esc(d.path)}">
        <span class="db-name">${esc(d.name)}</span>
        <span class="db-kind">${esc(KIND[d.kind] || d.kind)}</span>
        <span class="db-sub">${esc([d.id, d.validation].join(" · "))}</span>
        <span class="db-meta">${d.errors ? `<span class="pill bad" title="validation errors">${d.errors} err</span>` : ""}<span class="pill ${d.source === "builtin" ? "neutral" : "accent"}">${esc(sourceLabel(d))}</span></span>
      </button>`).join("") + failedHtml;
    $("ctList").querySelectorAll(".db-row").forEach((b) => b.addEventListener("click", () => show(b.dataset.path)));
    if (!current && rows[0]) show(rows[0].path);
  }

  const kv = (k, v) => (v == null || v === "" || (Array.isArray(v) && !v.length) ? "" : `<div class="k">${esc(k)}</div><div class="v">${esc(Array.isArray(v) ? v.join(", ") : v)}</div>`);
  const table = (head, rows) => `<div class="db-tw"><table class="tbl"><thead><tr>${head.map((h) => `<th>${esc(h)}</th>`).join("")}</tr></thead><tbody>${rows.join("")}</tbody></table></div>`;
  const td = (v, cls) => `<td${cls ? ` class="${cls}"` : ""}>${esc(v == null ? "" : v)}</td>`;
  const valPill = (v) => (v ? `<span class="pill ${VAL[v] || "neutral"}">${esc(v)}</span>` : "");

  function issuesHtml(r) {
    const issues = r.issues || [];
    if (!issues.length) return `<span class="pill ok">valid</span>`;
    const errs = issues.filter((i) => i.severity === "error");
    const line = (i) => `<div><span class="mono">[${esc(i.code)}]</span> ${i.location ? esc(i.location) + ": " : ""}${esc(i.message)}</div>`;
    return (errs.length ? `<div class="banner bad"><b>${errs.length} validation error${errs.length === 1 ? "" : "s"}</b>${errs.map(line).join("")}</div>` : `<span class="pill ok">valid</span> `) +
      (issues.length > errs.length ? `<details class="small"><summary>${issues.length - errs.length} warning${issues.length - errs.length === 1 ? "" : "s"}</summary>${issues.filter((i) => i.severity !== "error").map(line).join("")}</details>` : "");
  }

  // Visible sidebar entry for a panel (Bench panels are hidden on stable builds).
  function navVisible(id) {
    const b = document.querySelector(`#nav .nav-item[data-panel="${id}"]`);
    return !!b && !b.hidden;
  }

  function relatedHtml(r) {
    const rel = r.related || {};
    const out = [];
    if (rel.dsg) {
      out.push(`<p><b>DQ250 live read</b> — identification, stored codes and measuring groups over KWP2000 on VW TP2.0 (read-only):
        <span class="mono">opentune dsg identify | dtcs | groups</span>. Needs a raw-CAN adapter (SocketCAN, slcan, gs_usb); not yet tested on a car (Bench). Cell meanings come from this definition's live channels. See <span class="mono">docs/DSG.md</span>.</p>` +
        (navVisible("dsg") ? `<button class="btn sm" type="button" data-goto="dsg">Open the DSG panel</button>` : ""));
    }
    if (rel.haldex) {
      out.push(`<p><b>Haldex tools</b> — OpenHaldex status and mode, the read-only Gen4 read and a Haldex-specific log decoder with charts live in the <b>Haldex AWD</b> panel (Bench group, alpha builds) and <span class="mono">opentune haldex</span>. See <span class="mono">docs/HALDEX.md</span>.</p>` +
        (navVisible("haldex") ? `<button class="btn sm" type="button" data-goto="haldex">Open the Haldex AWD panel</button>` : ""));
    }
    return out.length ? `<div class="card"><h2>Related tools</h2>${out.join("")}</div>` : "";
  }

  function diagHtml(d) {
    if (!(d.diagnostics || []).length) return "";
    return `<div class="card"><h2>Diagnostics</h2>` + table(["Name", "Transport / layer", "Bit/s", "Addressing", "Session", "Identification", "Evidence"], d.diagnostics.map((c) => {
      const addr = c.tp20_address ? `TP2.0 address ${c.tp20_address}${c.rx_id ? ", answer on " + c.rx_id : ""}`
        : c.kline_address ? `K-line ${c.kline_address}` : `tx ${c.tx_id || "-"} rx ${c.rx_id || "-"}${c.extended_ids ? " (29-bit)" : ""}`;
      return `<tr>${td(c.name, "mono")}${td(`${c.transport} / ${c.layer}`)}${td(c.bitrate)}${td(addr, "mono")}${td(c.session || "", "mono")}${td(c.identification || "", "mono")}<td>${valPill(c.validation)}</td></tr>` +
        (c.notes ? `<tr><td></td><td colspan="6" class="small muted">${esc(c.notes)}</td></tr>` : "");
    })) + `</div>`;
  }

  function signalsTable(f) {
    return table(["Signal", "Bits", "Order", "Kind", "Scale", "Unit", "Values / invalid", "Notes"], f.signals.map((s) => {
      const vals = (s.values || []).map((v) => `${v.raw}=${v.name}`);
      const inv = (s.invalid || []).length ? [`n/a: ${s.invalid.join(",")}`] : [];
      const range = s.raw_range ? [`valid ${s.raw_range[0]}..${s.raw_range[1]}`] : [];
      return `<tr>${td(s.name, "mono")}${td(`${s.start}+${s.len}${s.signed ? " signed" : ""}`, "mono")}${td(s.byte_order === "big" ? "Motorola" : "Intel")}${td(s.kind || "number")}${td(sigScale(s), "mono")}${td(s.unit)}${td(vals.concat(inv, range).join(" · "), "small")}${td(s.notes, "small muted")}</tr>`;
    }));
  }

  function busesHtml(d) {
    return (d.buses || []).map((b) => `<div class="card">
      <div class="card-title-row"><h2>CAN set <span class="mono">${esc(b.name)}</span></h2><span class="small muted">${esc(b.description || "")} · ${esc(b.bitrate)} bit/s · ${b.frames.length} frame${b.frames.length === 1 ? "" : "s"}</span></div>
      ${b.notes ? `<p class="small muted">${esc(b.notes)}</p>` : ""}
      ${b.frames.map((f) => `<details class="ct-frame"><summary><span class="mono">${esc(f.id)}</span> <b>${esc(f.name)}</b>${f.sender ? ` from ${esc(f.sender)}` : ""} · ${f.signals.length} signal${f.signals.length === 1 ? "" : "s"}${f.cycle_ms ? ` · ${esc(f.cycle_ms)} ms` : ""}${f.extended ? " · 29-bit" : ""} ${valPill(f.validation)}</summary>
        ${f.notes ? `<p class="small muted">${esc(f.notes)}</p>` : ""}${signalsTable(f)}</details>`).join("")}
    </div>`).join("");
  }

  function liveHtml(d) {
    if (!(d.live || []).length) return "";
    return `<div class="card"><h2>Live channels</h2>` + table(["Channel", "Unit", "Source", "Notes"], d.live.map((l) =>
      `<tr>${td(l.name)}${td(l.unit)}${td(liveSource(l.source), "mono")}${td(l.notes, "small muted")}</tr>`)) + `</div>`;
  }

  function adaptHtml(d) {
    if (!(d.adaptations || []).length) return "";
    return `<div class="card"><h2>Adaptations and basic settings</h2>` + table(["Name", "Kind", "Service", "Changes the unit", "Security", "Evidence"], d.adaptations.map((a) =>
      `<tr>${td(a.name)}${td(String(a.kind).replace(/_/g, " "))}${td(a.service || "not public", "mono")}<td>${a.writes ? '<span class="pill bad">writes</span>' : "no"}</td>${td(a.requires_security ? "needed" : "")}<td>${valPill(a.validation)}</td></tr>` +
      (a.notes ? `<tr><td></td><td colspan="5" class="small muted">${esc(a.notes)}</td></tr>` : ""))) + `</div>`;
  }

  function axisText(a) {
    if (!a) return "";
    const pts = (a.values || []).length ? a.values.map(fmtNum).join(" ") : (a.labels || []).length ? a.labels.join(" ") : a.derived ? `${a.derived}${a.count ? ` (${a.count})` : ""}` : "";
    return `${a.name}${a.unit ? ` [${a.unit}]` : ""}: ${pts}`;
  }

  function tablesHtml(d) {
    if (!(d.tables || []).length) return "";
    const structOnly = d.tables.filter((t) => t.confidence === "structure_only" || !t.storage).length;
    return `<div class="card"><div class="card-title-row"><h2>Tables</h2>${structOnly ? `<span class="pill neutral">${structOnly} structure-only</span>` : ""}</div>
      <p class="small muted">Structure-only tables have known axes and shape but no storage address yet: they cannot be edited in an image.</p>` +
      table(["Table", "Role", "Size", "Axes", "Storage", "Confidence"], d.tables.map((t) => {
        const where = t.storage ? `0x${Number(t.storage.address).toString(16).toUpperCase()} ${t.storage.data_type}` : t.ecudef_map ? `${t.ecudef_map.ecudef}: ${t.ecudef_map.map}` : "";
        return `<tr>${td(t.name)}${td(String(t.role).replace(/_/g, " ") + ((t.profiles || []).length ? ` (${t.profiles.join(",")})` : ""))}${td(`${t.rows}×${t.cols}${t.unit ? " " + t.unit : ""}`)}` +
          `<td class="small">${esc(axisText(t.x_axis))}${t.y_axis ? "<br>" + esc(axisText(t.y_axis)) : ""}</td>` +
          `<td>${where ? `<span class="mono">${esc(where)}</span>` : '<span class="pill neutral">structure only</span>'}</td>${td(String(t.confidence).replace(/_/g, " "))}</tr>`;
      })) + `</div>`;
  }

  function gearsHtml(d) {
    if (!(d.gearsets || []).length) return "";
    return `<div class="card"><h2>Gear ratios</h2>` + table(["Variant", "Applies to", "Gears"], d.gearsets.map((g) =>
      `<tr>${td(g.name)}${td((g.applies_to || []).join(", "), "small")}${td(g.gears.map((x) => `${x.gear} ${fmtNum(x.ratio)}`).join("  "), "mono")}</tr>`)) + `</div>`;
  }

  function detailHtml(r) {
    const d = r.def || {};
    const id = d.identify || {};
    const sources = (d.sources || []).map((s) => `<li>${s.url ? `<a href="${esc(s.url)}" target="_blank" rel="noopener">${esc(s.title)}</a>` : esc(s.title)} <span class="pill neutral">${esc(s.license)}</span> — ${esc(s.used_for)}</li>`).join("");
    return `
      <div class="card">
        <div class="card-title-row"><h2 class="db-title">${esc(d.name)}</h2><span class="pill neutral">${esc(KIND[d.kind] || d.kind)}</span>${valPill(d.validation)}</div>
        <div class="meta-grid">
          ${kv("Id", d.id)}${kv("Maker", d.maker)}${kv("Family", d.family)}${kv("Vehicles", d.vehicles)}
          ${kv("Part numbers", id.part_numbers)}${kv("Software numbers", id.sw_numbers)}${kv("Gearbox codes", id.gearbox_codes)}
          ${kv("ECU definitions", d.ecudefs)}${kv("Database units", d.units)}${kv("Checksum", d.checksum)}
          ${d.flash ? kv("Flash", `${d.flash.size} bytes, ${(d.flash.regions || []).length} region(s)`) : ""}
          ${kv("Source", sourceLabel(r))}<div class="k">File</div><div class="v" title="${esc(r.path)}">${esc(r.path)}</div>
        </div>
        ${d.notes ? `<p class="note">${esc(d.notes)}</p>` : ""}
        <div style="margin-top:10px">${issuesHtml(r)}</div>
      </div>
      ${relatedHtml(r)}
      <div class="card"><h2>Safety</h2><div class="banner bad">${(d.safety || []).map((s) => `<div>${esc(s)}</div>`).join("")}</div></div>
      ${diagHtml(d)}${busesHtml(d)}${liveHtml(d)}${adaptHtml(d)}${tablesHtml(d)}${gearsHtml(d)}
      ${sources ? `<div class="card"><h2>Sources</h2><ul class="db-src small">${sources}</ul></div>` : ""}`;
  }

  async function show(path) {
    if (!path) return;
    current = path;
    $("ctList").querySelectorAll(".db-row").forEach((b) => b.classList.toggle("active", b.dataset.path === path));
    let r;
    try { r = await invoke("ctrl_get", { path }); } catch (e) {
      shown = null;
      $("ctDetail").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
      $("ctDecodeCard").hidden = true;
      return;
    }
    if (current !== path) return;
    shown = r;
    $("ctDetail").innerHTML = detailHtml(r);
    $("ctDetail").querySelectorAll("[data-goto]").forEach((b) => b.addEventListener("click", () => window.showPanel && window.showPanel(b.dataset.goto)));
    const sets = ((r.def && r.def.buses) || []).map((b) => b.name);
    $("ctDecodeCard").hidden = !sets.length;
    $("ctBus").innerHTML = (sets.length > 1 ? `<option value="">all sets</option>` : "") + sets.map((s) => `<option value="${esc(s)}">${esc(s)}</option>`).join("");
    $("ctBus").value = sets.length === 1 ? sets[0] : "";
    $("ctDecodeOut").innerHTML = "";
    $("ctDecode").disabled = !logPath || !sets.length;
  }

  function decodeHtml(r) {
    const head = `<div class="meta-grid" style="margin-top:10px">
      <div class="k">log</div><div class="v">${esc(baseName(r.log))}</div>
      <div class="k">frame set</div><div class="v">${esc(r.bus)}</div>
      <div class="k">frames</div><div class="v">${r.frames_in_log} in the log, ${r.decoded} decoded${r.bad_lines ? `, ${r.bad_lines} unreadable line${r.bad_lines === 1 ? "" : "s"} skipped` : ""}</div>
      ${r.undecoded_total ? `<div class="k">other ids</div><div class="v">${r.undecoded_ids.map((x) => `${esc(x.id)} ×${x.count}`).join(", ")}${r.undecoded_ids.length < 12 ? "" : " …"}</div>` : ""}
    </div>` +
      (r.unconfirmed ? `<div class="banner info" style="margin-top:10px">${esc(r.controller)} is <b>${esc(r.validation)}</b>: its frame layouts are not yet checked against a capture from a real car.</div>` : "");
    if (!r.decoded) return head + `<div class="banner info">No frame of this definition's set${r.sets.length > 1 ? "s" : ""} in the log. Check the bus the capture was taken on and the frame set.</div>`;
    const live = (r.live || []).length
      ? `<h3>Live channels</h3>` + table(["Channel", "Source", "Values"], r.live.map((l) => `<tr>${td(l.name)}${td(l.source, "mono small")}${td(statsText(l, l.unit))}</tr>`))
      : "";
    const frames = `<h3>Per frame</h3>` + r.per_frame.map((f) => `<details class="ct-frame" open><summary><span class="mono">${esc(f.id)}</span> <b>${esc(f.name)}</b> ×${f.count}${f.span ? ` · ${fmtNum(f.span)} s span` : ""}</summary>` +
      table(["Signal", "Values"], f.signals.map((s) => `<tr>${td(s.name, "mono")}${td(statsText(s, s.unit))}</tr>`)) + `</details>`).join("");
    const rows = r.rows.slice(0, ROWS_SHOWN);
    const more = r.rows_total > rows.length ? `<p class="small muted">Showing the first ${rows.length} of ${r.rows_total} decoded frames.</p>` : "";
    const sig = (s) => `${s.name}=${fmtValue(s.value)}${s.value && s.value.type === "number" && s.unit && s.unit !== "raw" ? " " + s.unit : ""}`;
    const table2 = `<h3>Frames</h3>` + more + table(["t (s)", "Id", "Frame", "Signals"], rows.map((x) =>
      `<tr>${td(x.t == null ? "" : x.t.toFixed(3), "mono")}${td(x.id, "mono")}${td(x.frame)}${td(x.signals.map(sig).join("  "), "mono small")}</tr>`));
    return head + live + frames + table2;
  }

  async function decode() {
    if (!shown || !logPath) return;
    const out = $("ctDecodeOut");
    out.innerHTML = `<p class="small muted">Decoding…</p>`;
    $("ctDecode").disabled = true;
    try {
      out.innerHTML = decodeHtml(await invoke("ctrl_decode_log", { path: shown.path, log: logPath, bus: $("ctBus").value || null, limit: 2000 }));
    } catch (e) {
      out.innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
    } finally {
      $("ctDecode").disabled = false;
    }
  }

  // ---- wiring -----------------------------------------------------------------
  if (window.OTPacks) {
    const pp = window.OTPacks.picker({
      panel: "controllers", kind: "controllers",
      filter: { name: "Controller definition", extensions: ["json"] },
      onChange: (sel, how) => {
        if (sel.path) show(sel.path);
        // a pack loaded from disk joins the list
        if (how && how.user) list();
      },
    });
    $("ctPicker").appendChild(pp.el);
  } else {
    $("ctPackCard").hidden = true;
  }
  $("ctPick").addEventListener("click", async () => {
    try {
      const r = await TAURI().dialog.open({ multiple: false, directory: false, filters: [{ name: "CAN log", extensions: ["log", "csv", "asc", "txt"] }, { name: "All files", extensions: ["*"] }] });
      const p = Array.isArray(r) ? r[0] : r;
      if (!p) return;
      logPath = p;
      $("ctLogPath").textContent = baseName(p);
      $("ctLogPath").title = p;
      $("ctDecode").disabled = !shown;
    } catch (e) {
      $("ctDecodeOut").innerHTML = `<div class="banner bad">${esc(msg(e))}</div>`;
    }
  });
  $("ctDecode").addEventListener("click", decode);
  let t = null;
  $("ctQ").addEventListener("input", (e) => { q = e.target.value.trim(); clearTimeout(t); t = setTimeout(list, 150); });
  $("ctKind").querySelectorAll("button").forEach((b) => b.addEventListener("click", () => {
    kind = b.dataset.k;
    $("ctKind").querySelectorAll("button").forEach((x) => x.classList.toggle("on", x === b));
    list();
  }));

  window.OTCtrl = { fmtNum, fmtValue, statsText, sigScale, liveSource, list, show };
  // Load the list when the panel is first opened, not at app startup.
  if (window.onPanelFirstShow) window.onPanelFirstShow("controllers", list);
  else list();
})();
