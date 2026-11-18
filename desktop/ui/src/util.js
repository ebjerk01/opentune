// OpenTune — pure UI helpers: escaping, number / hex formatting, the guide's
// Markdown renderer, project-edit bookkeeping and the panel-help strip markup.
// No DOM and no Tauri: index.html loads this first (window.OTUtil, and the app
// scripts take bare names from it), and the node tests require() it.
"use strict";
(function (root) {
  // HTML-escape text for innerHTML (the app's attribute values use "…").
  const esc = (s) => String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  // The file name of a path (either separator); an em dash for none.
  const baseName = (p) => (p ? p.split(/[\\/]/).pop() : "—");

  // ---- hex / numbers -------------------------------------------------------
  // 32-bit offset as "0x0000ABCD".
  const hex8 = (n) => "0x" + Number(n).toString(16).padStart(8, "0").toUpperCase();
  // Parse a typed hex value, "0x" optional; NaN when it does not start with hex.
  const parseHexInput = (s) => parseInt(String(s == null ? "" : s).replace(/^0x/i, ""), 16);
  // Map-grid cell and axis labels.
  const fmtCell = (v) => (Math.abs(v) >= 100 ? (+v).toFixed(1) : (+v).toFixed(2));
  const fmtAxis = (v) => (v == null ? "" : (v % 1 ? (+v).toFixed(1) : String(+v)));
  const clone2d = (g) => g.map((r) => r.slice());
  // 0..1 -> blue→green→yellow→orange→red (clamped).
  function heatColor(t) {
    const stops = [[43, 108, 176], [56, 161, 105], [214, 158, 46], [221, 107, 32], [197, 48, 48]];
    const x = Math.max(0, Math.min(1, t)) * (stops.length - 1);
    const i = Math.floor(x), f = x - i, a = stops[i], b = stops[Math.min(i + 1, stops.length - 1)];
    return `rgb(${Math.round(a[0] + (b[0] - a[0]) * f)},${Math.round(a[1] + (b[1] - a[1]) * f)},${Math.round(a[2] + (b[2] - a[2]) * f)})`;
  }
  // "a, b,, c" -> ["a", "b", "c"].
  function splitList(s) {
    return (s || "").split(",").map((x) => x.trim()).filter((x) => x.length);
  }

  // ---- small markup pieces ---------------------------------------------------
  function confPill(c) { const cls = c === "High" ? "ok" : c === "None" ? "bad" : "accent"; return `<span class="pill ${cls}">${esc(c)}</span>`; }
  function listOrDash(a) { return a && a.length ? a.map((x) => `<code>${esc(x)}</code>`).join(" ") : "—"; }
  function imgBadge(status) {
    switch (status) {
      case "match": return `<span class="img-badge match">image matches ✓</span>`;
      case "mismatch": return `<span class="img-badge mismatch">image CHANGED ✗</span>`;
      case "missing": return `<span class="img-badge missing">image not found</span>`;
      case "no_image": return `<span class="img-badge none">no base image</span>`;
      default: return `<span class="img-badge none">not verified</span>`;
    }
  }

  // ---- tuning-project edits ----------------------------------------------------
  // Identity of a map edit inside a project (one record per map).
  function editKey(e) {
    return e.kind === "adhoc"
      ? `adhoc:${e.mapName}:${e.mapdef ? e.mapdef.address : ""}`
      : `def:${e.defId}:${e.mapName}`;
  }
  // Number of cells that differ from the baseline (a missing baseline cell counts as unchanged).
  function cellsChanged(cells, baseline) {
    if (!cells || !baseline) return 0;
    let n = 0;
    for (let r = 0; r < cells.length; r++)
      for (let c = 0; c < (cells[r] || []).length; c++)
        if (Math.abs(cells[r][c] - ((baseline[r] || [])[c] ?? cells[r][c])) > 1e-9) n++;
    return n;
  }

  // ---- guide Markdown ------------------------------------------------------
  // Minimal, dependency-free Markdown → HTML: headings, bold/italic/code,
  // links, fenced code, lists (with continuation lines), tables, blockquotes,
  // hr, paragraphs. Everything is escaped; only http(s) and #/guide/<id> links
  // become anchors, any other link target renders as plain text.
  function mdHref(url) {
    const u = String(url).trim();
    if (/^https?:\/\/[^\s"'<>`]+$/i.test(u)) return { href: u, external: true };
    const g = u.match(/^#\/guide\/([A-Za-z0-9_-]+)$/);
    if (g) return { href: u, guide: g[1] };
    return null;
  }
  function mdInline(s) {
    // Code spans and links are cut out into placeholders first, so their text is
    // not re-interpreted as emphasis, then restored after escaping the rest.
    const slots = [];
    const keep = (html) => "\u0000" + (slots.push(html) - 1) + "\u0000";
    const emph = (t) => t
      .replace(/\*\*([^*]+)\*\*/g, (_, c) => `<strong>${c}</strong>`)
      .replace(/(^|[^*])\*([^*\n]+)\*/g, (_, p, c) => `${p}<em>${c}</em>`);
    let t = String(s == null ? "" : s).replace(/\u0000/g, "");
    t = t.replace(/`([^`]+)`/g, (_, c) => keep(`<code>${esc(c)}</code>`));
    t = t.replace(/\[([^\]]+)\]\(([^()\s]+)\)/g, (m, txt, url) => {
      const h = mdHref(url);
      const inner = emph(esc(txt));
      if (!h) return keep(inner);
      return keep(h.guide
        ? `<a class="mdlink" href="${esc(h.href)}" data-guide="${esc(h.guide)}">${inner}</a>`
        : `<a class="mdlink" href="${esc(h.href)}" target="_blank" rel="noopener noreferrer">${inner}</a>`);
    });
    t = emph(esc(t));
    for (let n = 0; n < 4 && t.indexOf("\u0000") >= 0; n++) t = t.replace(/\u0000(\d+)\u0000/g, (_, i) => slots[+i]);
    return t;
  }
  // Does `line` start a block other than a paragraph (so it ends a list item)?
  const mdBlockStart = (line) =>
    /^```/.test(line) || /^#{1,4}\s/.test(line) || /^\s*---+\s*$/.test(line) ||
    /^\s*>\s?/.test(line) || /^\s*\|.*\|\s*$/.test(line);
  function renderMarkdown(md) {
    const lines = md.replace(/\r\n/g, "\n").split("\n");
    let html = "", i = 0;
    let para = [];
    const flushPara = () => { if (para.length) { html += `<p>${mdInline(para.join(" "))}</p>`; } para = []; };
    while (i < lines.length) {
      let line = lines[i];
      if (/^```/.test(line)) {
        flushPara(); i++; let code = [];
        while (i < lines.length && !/^```/.test(lines[i])) { code.push(lines[i]); i++; }
        i++; html += `<pre class="mdcode">${esc(code.join("\n"))}</pre>`; continue;
      }
      if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|?[\s:-]*-[\s:|-]*$/.test(lines[i + 1])) {
        flushPara();
        const cells = (row) => row.trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim());
        const head = cells(line); i += 2;
        let bodyrows = "";
        while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) { const c = cells(lines[i]); bodyrows += `<tr>${c.map((x) => `<td>${mdInline(x)}</td>`).join("")}</tr>`; i++; }
        html += `<table class="tbl mdtable"><thead><tr>${head.map((h) => `<th>${mdInline(h)}</th>`).join("")}</tr></thead><tbody>${bodyrows}</tbody></table>`;
        continue;
      }
      let hm = line.match(/^(#{1,4})\s+(.*)$/);
      if (hm) { flushPara(); const lvl = hm[1].length; html += `<h${lvl} class="mdh${lvl}">${mdInline(hm[2])}</h${lvl}>`; i++; continue; }
      if (/^\s*---+\s*$/.test(line)) { flushPara(); html += "<hr/>"; i++; continue; }
      if (/^\s*>\s?/.test(line)) {
        flushPara(); let q = [];
        while (i < lines.length && /^\s*>\s?/.test(lines[i])) { q.push(lines[i].replace(/^\s*>\s?/, "")); i++; }
        html += `<blockquote class="mdquote">${mdInline(q.join(" "))}</blockquote>`; continue;
      }
      if (/^\s*([-*+]|\d+\.)\s+/.test(line)) {
        flushPara();
        const ordered = /^\s*\d+\.\s+/.test(line);
        let items = [];
        // An item runs until a blank line, the next item or another block; lines
        // in between (indented or not) continue the current item.
        while (i < lines.length) {
          const l = lines[i];
          if (/^\s*([-*+]|\d+\.)\s+/.test(l)) items.push(l.replace(/^\s*([-*+]|\d+\.)\s+/, ""));
          else if (/^\s*$/.test(l) || mdBlockStart(l)) break;
          else items[items.length - 1] += " " + l.trim();
          i++;
        }
        html += `<${ordered ? "ol" : "ul"} class="mdlist">${items.map((it) => `<li>${mdInline(it)}</li>`).join("")}</${ordered ? "ol" : "ul"}>`;
        continue;
      }
      if (/^\s*$/.test(line)) { flushPara(); i++; continue; }
      para.push(line.trim()); i++;
    }
    flushPara();
    return html;
  }

  // ---- panel help strip (panel-help.js) -----------------------------------------
  // The help strip escapes single quotes too: its text comes from extension data.
  const escQ = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const HELP_RISK = {
    none: { cls: "neutral", text: "Offline" },
    "read-only": { cls: "ok", text: "Read-only" },
    "writes-file": { cls: "accent", text: "Writes a file" },
    "writes-ecu": { cls: "bad", text: "Can write to the car" },
  };
  // Inner HTML of one panel's help strip: risk pill, needs, guide button, steps.
  function helpStripHtml(h, steps) {
    const r = HELP_RISK[h.risk] || HELP_RISK.none;
    const needs = (h.needs || []).map((n) => `<span class="ph-need">${escQ(n)}</span>`).join("");
    const list = (steps || []).map((s) => `<li>${escQ(s)}</li>`).join("");
    return `
      <div class="ph-top">
        <span class="pill ${r.cls}">${r.text}</span>
        <div class="ph-needs">${needs || '<span class="ph-need">Nothing — works offline</span>'}</div>
        ${h.guide ? '<button class="btn sm ph-guide" type="button">Open full guide →</button>' : ""}
      </div>
      ${list ? `<ol class="ph-steps">${list}</ol>` : ""}`;
  }

  // ---- live log (feat_logging.js) ---------------------------------------------
  // Mode badge for a LiveStatus: { cls, text, title }, or null before a session.
  function logModeBadge(st) {
    if (!st || !st.mode) return null;
    if (st.mode === "fast-2c") {
      const dids = (st.dynamic_dids || [])
        .map((d) => "0x" + Number(d.did).toString(16).toUpperCase().padStart(4, "0") + " (" + d.record_len + " B, " + (d.channels || []).length + " ch)");
      const polled = (st.polled || []).length;
      return {
        cls: "ok",
        text: "fast-2c",
        title: "Dynamic DIDs: " + (dids.join(", ") || "none") + (polled ? "; " + polled + " channel(s) polled alongside" : "") +
          (st.redefines ? "; re-defined " + st.redefines + "×" : ""),
      };
    }
    return { cls: st.reason === "fast mode not requested" ? "neutral" : "accent", text: "polling", title: st.reason || "polling" };
  }
  // "12.3 samples/s · 1.00 req/sample" from a log_tick rate object.
  function fmtLogRate(r) {
    if (!r || !r.ticks) return "";
    const sps = Number(r.samples_per_s) || 0;
    const rps = Number(r.requests_per_sample) || 0;
    return (sps >= 10 ? sps.toFixed(0) : sps.toFixed(1)) + " samples/s · " + rps.toFixed(2) + " req/sample";
  }

  // ---- Live-log CSV (feat_logging.js, feat_dashboard.js) ------------------------
  // The Log viewer reads this layout (opentune-logview, "OpenTune Live log"):
  // one row per channel per tick, `ts` in Unix ms. `text` (last, optional for
  // readers) carries text channels such as identification DIDs; their `value`
  // is empty.
  const LIVE_LOG_HEAD = ["ts", "param_id", "name", "raw", "value", "units", "note", "text"];
  function csvField(v) {
    const s = String(v == null ? "" : v);
    return /[",\n\r]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
  }
  // One CSV row object from a Live-log sample ({paramId,name,raw,value,units,note,text}).
  function liveLogRow(ts, s) {
    return {
      ts, paramId: s.paramId, name: s.name,
      raw: s.raw == null ? "" : s.raw,
      value: s.value == null ? "" : s.value,
      units: s.units || "", note: s.note || "",
      text: s.text == null ? "" : s.text,
    };
  }
  // CSV lines for rows (header first unless `noHeader`), "\n"-terminated.
  function liveLogCsv(rows, noHeader) {
    const out = noHeader ? [] : [LIVE_LOG_HEAD.join(",")];
    (rows || []).forEach((r) => {
      out.push([r.ts, r.paramId, r.name, r.raw, r.value, r.units, r.note, r.text].map(csvField).join(","));
    });
    return out.length ? out.join("\n") + "\n" : "";
  }
  // What a live table cell shows for a sample: the text of a text channel, the
  // value, else the raw number, else a dash.
  function liveShown(s, fmtNum) {
    if (!s) return "—";
    if (s.text != null) return String(s.text);
    if (s.value != null) return fmtNum ? fmtNum(s.value) : String(s.value);
    if (s.raw != null) return String(s.raw);
    return "—";
  }
  // Dashboard poll -> Live-log rows. `values` maps channel key -> number,
  // positional array (multi-sensor PIDs: one channel per answering sensor,
  // `key#i`), string (a text channel) or null; `table` maps key -> {name, unit};
  // `errors` maps key -> why it is null.
  function dashLogRows(ts, values, table, errors) {
    const rows = [];
    Object.keys(values || {}).sort().forEach((k) => {
      const t = (table && table[k]) || {};
      const name = t.name || k;
      const unit = t.unit || "";
      const v = values[k];
      const row = (id, nm, x) => {
        if (typeof x === "string") rows.push({ ts, paramId: id, name: nm, raw: "", value: "", units: unit || "text", note: "", text: x });
        else if (typeof x === "number" && isFinite(x)) rows.push({ ts, paramId: id, name: nm, raw: "", value: x, units: unit, note: "", text: "" });
        else rows.push({ ts, paramId: id, name: nm, raw: "", value: "", units: unit, note: (errors && errors[k]) || "no value", text: "" });
      };
      if (Array.isArray(v)) v.forEach((x, i) => { if (x != null) row(k + "#" + i, name + " S" + (i + 1), x); });
      else row(k, name, v);
    });
    return rows;
  }
  // "m:ss" (or "h:mm:ss") for an elapsed time in ms.
  function fmtElapsed(ms) {
    const s = Math.max(0, Math.floor((Number(ms) || 0) / 1000));
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), r = s % 60;
    const two = (x) => String(x).padStart(2, "0");
    return h ? h + ":" + two(m) + ":" + two(r) : m + ":" + two(r);
  }

  // ---- small line charts (feat_haldex.js, feat_ts.js) ---------------------------
  // SVG path for a series in a w x h box: { d, lo, hi }. t and v are equal-length
  // arrays (t ascending); a flat series is drawn mid-height.
  function seriesPath(t, v, w, h) {
    const n = Math.min((t || []).length, (v || []).length);
    if (!n) return { d: "", lo: null, hi: null };
    let lo = Infinity, hi = -Infinity;
    for (let i = 0; i < n; i++) { if (v[i] < lo) lo = v[i]; if (v[i] > hi) hi = v[i]; }
    const t0 = t[0], ts = t[n - 1] - t0 || 1, vs = hi - lo;
    const r = (x) => Math.round(x * 100) / 100;
    let d = "";
    for (let i = 0; i < n; i++) {
      const x = n === 1 ? w / 2 : ((t[i] - t0) / ts) * w;
      const y = vs ? h - ((v[i] - lo) / vs) * h : h / 2;
      d += (i ? "L" : "M") + r(x) + "," + r(y);
    }
    return { d, lo, hi };
  }

  // ---- aftermarket ECU editor (feat_ts.js) --------------------------------------
  // A number in a constant's digits (null/undefined -> "—").
  function tsFmt(v, digits) {
    if (v == null || !isFinite(v)) return "—";
    return digits != null ? Number(v).toFixed(Math.min(9, digits)) : String(+Number(v).toPrecision(10));
  }
  // Client-side pre-check of a typed value against a constant's INI limits
  // (the backend checks again and is authoritative): an error text or null.
  function tsCheckValue(meta, x) {
    if (typeof x !== "number" || !isFinite(x)) return (meta && meta.name ? meta.name + ": " : "") + "not a number";
    const lo = meta && typeof meta.min === "number" ? meta.min : null;
    const hi = meta && typeof meta.max === "number" ? meta.max : null;
    if ((lo != null && x < lo) || (hi != null && x > hi)) {
      return `${meta.name} = ${x} is outside ${lo == null ? "-∞" : lo}..${hi == null ? "∞" : hi}${meta.units ? " " + meta.units : ""}`;
    }
    return null;
  }

  const OTUtil = {
    esc, baseName, hex8, parseHexInput, fmtCell, fmtAxis, clone2d, heatColor, splitList,
    confPill, listOrDash, imgBadge, editKey, cellsChanged,
    mdHref, mdInline, renderMarkdown, escQ, HELP_RISK, helpStripHtml,
    logModeBadge, fmtLogRate, seriesPath, tsFmt, tsCheckValue,
    LIVE_LOG_HEAD, liveLogRow, liveLogCsv, liveShown, dashLogRows, fmtElapsed,
  };
  root.OTUtil = OTUtil;
  if (typeof module !== "undefined" && module.exports) module.exports = OTUtil;
})(typeof globalThis !== "undefined" ? globalThis : this);
