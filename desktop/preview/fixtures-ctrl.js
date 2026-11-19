// Demo fixtures for the Controllers panel (feat_ctrl.js). Preview only: the
// list and the definitions are the repository's ctrldefs/ as bundled by
// build.py (plus any controllers/ in the bundled packs); "Decode" ignores the
// chosen file and decodes a SYNTHETIC capture built from the definition's own
// frame layouts (smooth made-up values, a stray 0x7FF id). Nothing here is
// from a real vehicle, and the preview does not run the validator (the app
// does: ctrl_get reports every issue).
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const P = window.__PREVIEW__ || {};
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const clone = (v) => JSON.parse(JSON.stringify(v));
  const num = (v) => (typeof v === "string" ? parseInt(v, 16) : Number(v));
  const hex3 = (n) => "0x" + n.toString(16).toUpperCase().padStart(3, "0");

  // ---- DBC bit layout, as opentune-ctrldef's signal.rs ----------------------
  function bitPositions(start, len, order) {
    if (!len || len > 64) return null;
    const out = [];
    if (order === "big") {
      let pos = start;
      for (let i = 0; i < len; i++) { out.push(pos); pos = pos % 8 === 0 ? pos + 15 : pos - 1; }
    } else {
      for (let i = 0; i < len; i++) out.push(start + i);
    }
    return out.some((p) => p < 0 || p >= 512) ? null : out;
  }
  function extract(data, start, len, order) {
    const pos = bitPositions(start, len, order);
    if (!pos || pos.some((p) => (p >> 3) >= data.length)) return null;
    const bit = (p) => BigInt((data[p >> 3] >> (p % 8)) & 1);
    let v = 0n;
    if (order === "big") for (const p of pos) v = (v << 1n) | bit(p);
    else pos.forEach((p, i) => { v |= bit(p) << BigInt(i); });
    return v;
  }
  function insert(data, start, len, order, value) {
    const pos = bitPositions(start, len, order);
    if (!pos || pos.some((p) => (p >> 3) >= data.length)) return false;
    const n = pos.length, v = BigInt(value);
    pos.forEach((p, i) => {
      const shift = BigInt(order === "big" ? n - 1 - i : i);
      const b = Number((v >> shift) & 1n);
      data[p >> 3] = (data[p >> 3] & ~(1 << (p % 8))) | (b << (p % 8));
    });
    return true;
  }
  function interpolate(curve, x) {
    if (!curve.length) return x;
    if (curve.length === 1 || x <= curve[0][0]) return curve[0][1];
    for (let i = 1; i < curve.length; i++) {
      const [x0, y0] = curve[i - 1], [x1, y1] = curve[i];
      if (x <= x1) return x1 === x0 ? y1 : y0 + ((y1 - y0) * (x - x0)) / (x1 - x0);
    }
    return curve[curve.length - 1][1];
  }
  function decodeSignal(s, data) {
    const rawB = extract(data, s.start, s.len, s.byte_order || "little");
    if (rawB == null) return null;
    const raw = Number(rawB);
    const invalid = (s.invalid || []).includes(raw) || (s.raw_range && (raw < s.raw_range[0] || raw > s.raw_range[1]));
    let value;
    if (invalid) value = { type: "invalid" };
    else if (s.kind === "bool") value = { type: "bool", value: raw !== 0 };
    else if (s.kind === "enum") { const e = (s.values || []).find((x) => x.raw === raw); value = { type: "enum", value: e ? e.name : null }; }
    else if (s.kind === "ascii") { const b = raw & 0xff; value = { type: "ascii", value: b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : "\\x" + b.toString(16).toUpperCase().padStart(2, "0") }; }
    else {
      const x = s.signed && s.len < 64 && rawB >> BigInt(s.len - 1) ? Number(rawB - (1n << BigInt(s.len))) : raw;
      const f = s.factor == null ? 1 : s.factor;
      value = { type: "number", value: (s.curve || []).length ? interpolate(s.curve, x) : x * f + (s.offset || 0) };
    }
    const out = { name: s.name, raw, value };
    if (s.unit) out.unit = s.unit;
    return out;
  }
  function decodeFrame(bus, frame, data) {
    if (data.length < (frame.min_len || 1)) return null;
    return { bus: bus.name, frame: frame.name, id: num(frame.id), signals: frame.signals.map((s) => decodeSignal(s, data)).filter(Boolean) };
  }

  // A smooth made-up raw value for sample i of signal k.
  function synthRaw(s, i, k) {
    const w = 0.5 + 0.35 * Math.sin(i / 9 + k);
    if (s.kind === "bool") return Math.floor(i / 25 + k) % 2;
    if (s.kind === "enum" && (s.values || []).length) return s.values[Math.floor(i / 30 + k) % s.values.length].raw;
    if (s.kind === "ascii") return "PRNDS".charCodeAt(Math.floor(i / 40 + k) % 5);
    const [lo, hi] = s.raw_range || [0, Math.min(2 ** Math.min(s.len, 32) - 1, 2 ** 32)];
    let r = Math.round(lo + (hi - lo) * w);
    if ((s.invalid || []).includes(r)) r = Math.max(lo, r - 1);
    return r;
  }

  // ---- the catalog --------------------------------------------------------------
  function allDefs() {
    const out = (P.ctrldefs || []).map((x) => ({ path: x.path, def: x.def, source: "builtin", pack: null }));
    const files = (P.vcar && P.vcar.files) || {};
    for (const p of P.packs || []) {
      for (const d of (p.defs || []).filter((x) => x.kind === "controllers")) {
        if (files[d.path]) out.push({ path: d.path, def: files[d.path], source: "pack", pack: { id: p.id, name: p.name, version: p.version, license: p.license, dir: p.dir, valid: p.valid } });
      }
    }
    return out;
  }
  const norm = (s) => String(s || "").replace(/\s+/g, "").toUpperCase();
  function identifies(def, q) {
    const v = norm(q), id = def.identify || {};
    return [].concat(id.part_numbers || [], id.sw_numbers || [], id.gearbox_codes || [])
      .some((k) => { const n = norm(k); return n && !/[*?]/.test(n) && v.includes(n); });
  }
  function row(x) {
    const d = x.def;
    const low = `${d.id} ${d.family}`.toLowerCase();
    return {
      id: d.id, name: d.name, kind: d.kind, maker: d.maker, family: d.family, vehicles: d.vehicles || [],
      validation: d.validation, source: x.source, pack: x.pack, path: x.path,
      buses: (d.buses || []).map((b) => b.name),
      counts: {
        diagnostics: (d.diagnostics || []).length, frames: (d.buses || []).reduce((n, b) => n + b.frames.length, 0),
        live: (d.live || []).length, tables: (d.tables || []).length, gearsets: (d.gearsets || []).length,
      },
      errors: 0, warnings: 0,
      related: { dsg: d.id === "vag-dq250-02e", haldex: low.includes("haldex") },
    };
  }

  F.ctrl_list = async ({ q, kind }) => {
    await wait(40);
    const all = allDefs();
    const ql = String(q || "").trim().toLowerCase();
    const defs = all
      .filter((x) => !kind || x.def.kind === kind)
      .filter((x) => !ql || `${x.def.id} ${x.def.name} ${x.def.maker} ${x.def.family} ${(x.def.vehicles || []).join(" ")}`.toLowerCase().includes(ql) || identifies(x.def, ql))
      .map(row);
    return { builtin_dir: "ctrldefs", total: all.length, defs, failed: [] };
  };

  F.ctrl_get = async ({ path }) => {
    await wait(40);
    const x = allDefs().find((d) => d.path === path);
    if (!x) throw `read '${path}': no such file (preview)`;
    return Object.assign(row(x), { def: clone(x.def), issues: [] });
  };

  F.ctrl_decode_log = async ({ path, log, bus, limit }) => {
    await wait(200);
    const x = allDefs().find((d) => d.path === path);
    if (!x) throw `read '${path}': no such file (preview)`;
    const def = x.def;
    const buses = def.buses || [];
    if (!buses.length) throw `${def.id} defines no CAN frame sets to decode with`;
    const want = bus && bus !== "all" ? bus : buses.length === 1 ? buses[0].name : null;
    const sets = want ? buses.filter((b) => b.name.toLowerCase() === want.toLowerCase()) : buses;
    if (!sets.length) throw `unknown frame set '${want}' (${buses.map((b) => b.name).join(", ")})`;
    // the synthetic capture: ~2 s of every frame of the chosen set(s)
    const raw = [];
    for (const b of sets) {
      for (const f of b.frames) {
        const cyc = f.cycle_ms || 100, n = Math.max(5, Math.min(200, Math.round(2000 / cyc)));
        for (let i = 0; i < n; i++) {
          const data = new Array(f.dlc || 8).fill(0);
          f.signals.forEach((s, k) => insert(data, s.start, s.len, s.byte_order || "little", synthRaw(s, i, k)));
          raw.push({ t: (i * cyc) / 1000, bus: b, frame: f, data });
        }
      }
    }
    for (let i = 0; i < 15; i++) raw.push({ t: i * 0.13, data: [0], other: 0x7ff });
    raw.sort((a, b) => a.t - b.t);
    const push = (st, v) => {
      st.samples++;
      if (v.type === "number") { st.min = st.min == null ? v.value : Math.min(st.min, v.value); st.max = st.max == null ? v.value : Math.max(st.max, v.value); }
      else if (v.type === "invalid") st.invalid++;
      else { const k = v.type === "bool" ? (v.value ? "1" : "0") : v.type === "ascii" ? `'${v.value}'` : v.value == null ? "?" : v.value; st.counts[k] = (st.counts[k] || 0) + 1; }
      st.last = v;
    };
    const newStats = () => ({ samples: 0, invalid: 0, counts: {} });
    const perFrame = {}, live = {}, rows = [];
    let decoded = 0;
    for (const r of raw) {
      if (r.other) continue;
      const d = decodeFrame(r.bus, r.frame, r.data);
      if (!d) continue;
      decoded++;
      const pf = perFrame[d.frame] || (perFrame[d.frame] = { name: d.frame, bus: d.bus, id: hex3(d.id), count: 0, first: r.t, last: r.t, sig: {} });
      pf.count++; pf.last = r.t;
      for (const s of d.signals) {
        push(pf.sig[s.name] || (pf.sig[s.name] = Object.assign(newStats(), { name: s.name, unit: s.unit || "" })), s.value);
        for (const l of def.live || []) {
          const src = l.source || {};
          if (src.kind === "can_signal" && src.bus.toLowerCase() === d.bus.toLowerCase() && src.frame === d.frame && src.signal === s.name) {
            push(live[l.name] || (live[l.name] = Object.assign(newStats(), { name: l.name, unit: l.unit || "", source: `${src.bus}/${src.frame}.${src.signal}` })), s.value);
          }
        }
      }
      if (rows.length < (limit || 2000)) rows.push({ t: r.t, id: hex3(d.id), bus: d.bus, frame: d.frame, signals: d.signals });
    }
    const order = (def.live || []).map((l) => l.name);
    return {
      controller: def.id, validation: def.validation,
      unconfirmed: ["implemented", "synthetic-tested"].includes(def.validation),
      bus: want || "all", sets: buses.map((b) => b.name),
      frames_in_log: raw.length, decoded, bad_lines: 0,
      live: Object.values(live).sort((a, b) => order.indexOf(a.name) - order.indexOf(b.name)),
      per_frame: Object.values(perFrame).sort((a, b) => (a.name < b.name ? -1 : 1)).map((f) => ({
        name: f.name, bus: f.bus, id: f.id, count: f.count, span: f.last > f.first ? f.last - f.first : null,
        signals: Object.values(f.sig).sort((a, b) => (a.name < b.name ? -1 : 1)),
      })),
      undecoded_ids: [{ id: "0x7FF", count: 15 }], undecoded_total: 15,
      rows, rows_total: decoded, log: log || "demo/preview-drivetrain.log",
      preview: "synthetic capture built from the definition's own layouts",
    };
  };

  window.__PREVIEW_CTRL__ = { extract, insert, decodeSignal, decodeFrame, bitPositions };
})();
