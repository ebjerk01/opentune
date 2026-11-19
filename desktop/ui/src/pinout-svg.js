// OpenTune — data-driven bench wiring diagrams.
// Draws a unit's `connectors[].pins[]` (database/units/*.json, served by
// db_get) as an inline SVG: connector pins on the left, wired to the bench
// targets they need on the right (+12 V permanent / ignition, ground, K-line,
// CAN, boot). It is a WIRING diagram, not a picture of the connector face: the
// sources give pin numbers and functions, not physical pin positions.
//
// Confidence follows the database: a pin's own `confidence`, else the unit's
// `pinout_confidence`. `verified` = two or more sources agree, `community` = one
// source, `unverified` = disputed or single conflicting source. Unverified pins
// are drawn dashed with a "?" and the note "confirm by continuity".
//
// Pure: OTPinout.svg(unit) returns a string (node-testable). OTPinout.hydrate
// fills `.pinout-embed[data-unit]` placeholders (from the guide's
// ```pinout <unit-id>``` blocks) using a db_get-style loader.
"use strict";
(function (root) {
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  // Bench targets, top to bottom. Signals not listed here ("other") get no wire.
  const TARGETS = [
    ["12v_perm", "+12 V permanent (KL30)", "fused, current-limited supply"],
    ["12v_ign", "+12 V ignition (KL15)", "same supply, switched"],
    ["gnd", "Ground (−)", "supply negative"],
    ["kline", "K-line", "to the interface"],
    ["can_h", "CAN high", "to the interface"],
    ["can_l", "CAN low", "to the interface"],
    ["boot", "Boot strap", "only for boot-mode reads"],
  ];
  const CONF_WORD = { verified: "sources agree", community: "one source", unverified: "disputed — confirm by continuity" };

  const pinConf = (unit, p) => p.confidence || unit.pinout_confidence || "unverified";
  const pinNum = (s) => { const n = parseInt(String(s).replace(/^\D+/, ""), 10); return Number.isFinite(n) ? n : 1e9; };

  // Theme-aware: the app's tokens when present, sane fallbacks standalone.
  const STYLE = `
    .pd-t{font:600 12px ui-sans-serif,system-ui,sans-serif;fill:var(--ink,#1b1f27)}
    .pd-s{font:11px ui-sans-serif,system-ui,sans-serif;fill:var(--ink-soft,#596073)}
    .pd-m{font:700 11px ui-monospace,Menlo,Consolas,monospace;fill:var(--ink,#1b1f27)}
    .pd-box{fill:var(--card,#fff);stroke:var(--card-border,#c9ced8);stroke-width:1}
    .pd-row-unv{fill:var(--bad-bg,#fbe4e2)}
    .pd-w{fill:none;stroke-width:2.2}
    .pd-dash{stroke-dasharray:6 4}
    .pd-q{font:700 11px ui-sans-serif,system-ui,sans-serif;fill:var(--bad,#c7352b)}
    .s-12v_perm{stroke:var(--bad,#c7352b);fill:var(--bad,#c7352b)}
    .s-12v_ign{stroke:var(--warn,#b26b00);fill:var(--warn,#b26b00)}
    .s-gnd{stroke:var(--ink-soft,#596073);fill:var(--ink-soft,#596073)}
    .s-kline{stroke:var(--good,#1a8a4a);fill:var(--good,#1a8a4a)}
    .s-can_h{stroke:var(--accent,#e8711a);fill:var(--accent,#e8711a)}
    .s-can_l{stroke:#3b6fd8;fill:#3b6fd8}
    .s-boot{stroke:#8a4fd1;fill:#8a4fd1}
    .s-other{stroke:var(--muted,#8b92a3);fill:var(--muted,#8b92a3)}`;

  /** Wiring diagram for one unit, or "" when it has no pins. */
  function svg(unit) {
    const conns = ((unit && unit.connectors) || []).filter((c) => c.pins && c.pins.length);
    if (!conns.length) return "";
    const W = 760, LX = 12, LW = 340, RX = 540, RW = 208, ROW = 24, HEAD = 22, GAP = 14, TOP = 12;

    // Pin-label column wide enough for the longest "<connector>-<pin>" label
    // (monospace ~7 px/char at 11 px); the function text fills the rest.
    const maxLab = Math.max(...conns.flatMap((c) => c.pins.map((p) => `${c.name}-${p.pin}`.length)));
    const FX = LX + 10 + maxLab * 7 + 14;
    const fnMax = Math.max(12, Math.floor((LX + LW - 64 - FX) / 6.1));

    // Left side: one block per connector, pins in numeric order.
    let y = TOP, left = "";
    const anchors = []; // {y, signal, unv}
    for (const c of conns) {
      const pins = c.pins.slice().sort((a, b) => pinNum(a.pin) - pinNum(b.pin));
      const h = HEAD + pins.length * ROW + 6;
      left += `<rect class="pd-box" x="${LX}" y="${y}" width="${LW}" height="${h}" rx="7"/>`;
      left += `<text class="pd-t" x="${LX + 10}" y="${y + 16}">Connector ${esc(c.name)}${c.description ? `<tspan class="pd-s" dx="8">— ${esc(c.description)}</tspan>` : ""}</text>`;
      pins.forEach((p, i) => {
        const ry = y + HEAD + i * ROW;
        const cy = ry + ROW / 2;
        const unv = pinConf(unit, p) === "unverified";
        const label = `${c.name}-${p.pin}`;
        if (unv) left += `<rect class="pd-row-unv" x="${LX + 1}" y="${ry + 1}" width="${LW - 2}" height="${ROW - 2}" rx="4"/>`;
        left += `<text class="pd-m" x="${LX + 10}" y="${cy + 4}">${esc(label)}</text>`;
        const fn = String(p.function || "");
        left += `<text class="pd-s" x="${FX}" y="${cy + 4}"><title>${esc(fn)}${p.note ? " — " + esc(p.note) : ""}</title>${esc(fn.length > fnMax ? fn.slice(0, fnMax - 1) + "…" : fn)}</text>`;
        const nsrc = new Set(p.sources || []).size;
        if (unv) left += `<text class="pd-q" x="${LX + LW - 18}" y="${cy + 4}">?</text>`;
        else if (nsrc >= 2) left += `<text class="pd-s" x="${LX + LW - 14}" y="${cy + 4}" text-anchor="end"><title>${nsrc} sources agree</title>${nsrc} src ✓</text>`;
        left += `<circle class="s-${esc(p.signal)}" cx="${LX + LW}" cy="${cy}" r="4"/>`;
        anchors.push({ y: cy, signal: p.signal, unv, label });
      });
      y += h + GAP;
    }
    const leftH = y - GAP;

    // Right side: only the targets some pin needs, spread over the height.
    const used = TARGETS.filter(([s]) => anchors.some((a) => a.signal === s));
    const TH = 40;
    const span = Math.max(leftH - TOP, used.length * (TH + 10));
    const step = used.length ? span / used.length : 0;
    const tY = {};
    let right = "";
    used.forEach(([s, name, sub], i) => {
      const ty = TOP + i * step + (step - TH) / 2;
      tY[s] = ty + TH / 2;
      right += `<rect class="pd-box" x="${RX}" y="${ty}" width="${RW}" height="${TH}" rx="7"/>`;
      right += `<rect class="s-${s}" x="${RX}" y="${ty}" width="5" height="${TH}" rx="2"/>`;
      right += `<text class="pd-t" x="${RX + 14}" y="${ty + 17}">${esc(name)}</text>`;
      right += `<text class="pd-s" x="${RX + 14}" y="${ty + 32}">${esc(sub)}</text>`;
    });

    // Wires.
    let wires = "";
    for (const a of anchors) {
      if (!(a.signal in tY)) continue;
      const x1 = LX + LW + 4, x2 = RX, y2 = tY[a.signal], mx = (x1 + x2) / 2;
      wires += `<path class="pd-w s-${a.signal}${a.unv ? " pd-dash" : ""}" style="fill:none" d="M${x1} ${a.y} C${mx} ${a.y} ${mx} ${y2} ${x2} ${y2}"><title>${esc(a.label)}${a.unv ? " — disputed, confirm by continuity" : ""}</title></path>`;
    }

    // Legend.
    const LY = Math.max(leftH, TOP + span) + 18;
    const anyUnv = anchors.some((a) => a.unv);
    const conf = unit.pinout_confidence || "unverified";
    let legend = `<line class="pd-w s-gnd" x1="${LX}" y1="${LY}" x2="${LX + 30}" y2="${LY}"/>`;
    legend += `<text class="pd-s" x="${LX + 38}" y="${LY + 4}">pinout ${esc(conf)} (${esc(CONF_WORD[conf] || conf)})</text>`;
    if (anyUnv) {
      legend += `<line class="pd-w s-gnd pd-dash" x1="${LX + 300}" y1="${LY}" x2="${LX + 330}" y2="${LY}"/>`;
      legend += `<text class="pd-s" x="${LX + 338}" y="${LY + 4}"><tspan class="pd-q">?</tspan> disputed — confirm by continuity before wiring</text>`;
    }
    legend += `<text class="pd-s" x="${LX}" y="${LY + 22}">"2 src ✓" = two sources agree on that pin; "verified" means sources agree, not that this unit was measured.</text>`;
    legend += `<text class="pd-s" x="${LX}" y="${LY + 38}">Check every pin with a multimeter on your unit before applying power.</text>`;
    const H = LY + 50;

    const title = `Bench wiring for ${unit.name || unit.id || "this unit"}`;
    return `<svg class="pinout-svg" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="100%" role="img" aria-label="${esc(title)}"><title>${esc(title)}</title><style>${STYLE}</style>${left}${wires}${right}${legend}</svg>`;
  }

  /** Fill `.pinout-embed[data-unit]` placeholders under `root` with diagrams. */
  async function hydrate(rootEl, getUnit) {
    const slots = rootEl ? rootEl.querySelectorAll(".pinout-embed[data-unit]") : [];
    for (const el of slots) {
      const id = el.getAttribute("data-unit");
      try {
        const u = await getUnit(id);
        const s = svg(u);
        el.innerHTML = s || `<div class="banner info">No sourced pinout for ${esc(id)} yet.</div>`;
      } catch (e) {
        el.innerHTML = `<div class="banner bad">Pinout ${esc(id)}: ${esc(e && e.message ? e.message : e)}</div>`;
      }
    }
  }

  root.OTPinout = { svg, hydrate, TARGETS };
})(typeof window !== "undefined" ? window : globalThis);
