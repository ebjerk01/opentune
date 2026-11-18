// Demo fixtures for the calibration panels: Image, Analyze, Identify, Checksum,
// Maps (viewer / editor / write-back, XDF import, promote), Compare, Flash (to
// file), Vault and Project. Preview only: the "images" are synthetic byte
// patterns generated here (a stock / tuned pair plus a small patch file); map
// layouts come from the bundled ecudefs, but every cell value is made up.
// Nothing here is, or was taken from, a real ECU dump.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const P = window.__PREVIEW__ || {};
  const DEFS = P.ecudefs || [];
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const hex8 = (n) => "0x" + (n >>> 0).toString(16).toUpperCase().padStart(8, "0");
  const base = (p) => String(p).split(/[\\/]/).pop();
  const stamp = () => new Date().toISOString().replace(/[-:]/g, "").replace(/\..*/, "").replace("T", "-");
  const hash = (s) => { let h = 2166136261; for (const ch of String(s)) { h ^= ch.charCodeAt(0); h = Math.imul(h, 16777619); } return h >>> 0; };
  const DEMO_VIN = "WVWZZZ00000000000";
  const DEF_ID = "bosch-edc16u34"; // the definition the demo image claims to fit
  const demoDef = () => DEFS.find((d) => d.id === DEF_ID) || DEFS.find((d) => (d.map_meta || []).length) || null;

  // ---- synthetic images ------------------------------------------------------
  const MiB = 1024 * 1024;
  const IDENT = [
    [0x10000, "OPENTUNE PREVIEW DEMO IMAGE - SYNTHETIC BYTES, NOT AN ECU DUMP"],
    [0x10040, "DEMO-ECU PREVIEW  HW 0000000000  SW 0000000000"],
    [0x10080, DEMO_VIN],
  ];
  const TUNED_REGIONS = [[0x1c0000, 0x200], [0x1c4000, 0x80]];
  const images = {};
  function image(path) {
    if (!path) throw "open an image first";
    if (!images[path]) {
      const b = base(path);
      if (!/\.(bin|ori|mod|frf|dat|hex|eep)$/i.test(b)) throw `read ${path}: not a flash image`;
      const patch = /patch/i.test(b);
      images[path] = { kind: /tuned/i.test(b) ? "tuned" : patch ? "patch" : "stock", size: patch ? 4096 : 2 * MiB, csBad: /tuned/i.test(b), edits: {} };
    }
    return images[path];
  }
  // A new file written from `src` (fix, map write, flash) inherits its content.
  function derive(src, out, patch) {
    const s = image(src);
    images[out] = Object.assign({}, s, { edits: Object.assign({}, s.edits) }, patch || {});
    return images[out];
  }
  function byteAt(img, off) {
    if (img.kind === "patch") return (off * 7 + 0x40) & 255;
    for (const [at, text] of IDENT) if (off >= at && off < at + text.length) return text.charCodeAt(off - at);
    if (off >= img.size * 0.875) return 0xff;
    let v = (Math.imul(off, 2654435761) >>> 13) & 255;
    if ((off & 3) === 0) v &= 0x3f; // a little structure, so it reads like data
    if (img.kind === "tuned" && TUNED_REGIONS.some(([a, n]) => off >= a && off < a + n)) v = (v + 0x17) & 255;
    return v;
  }

  F.image_info = ({ path }) => { const i = image(path); return { path, name: base(path), size: i.size }; };
  F.hex_preview = ({ path, offset, rows }) => {
    const i = image(path);
    const start = Math.max(0, Math.min(Math.floor((offset || 0) / 16) * 16, Math.max(0, i.size - 16)));
    const lines = [];
    for (let a = start; a < Math.min(i.size, start + (rows || 16) * 16); a += 16) {
      const bytes = Array.from({ length: Math.min(16, i.size - a) }, (_, k) => byteAt(i, a + k));
      lines.push({ addr: hex8(a), hex: bytes.map((b) => b.toString(16).toUpperCase().padStart(2, "0")), ascii: bytes.map((b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("") });
    }
    return { offset: start, total: i.size, lines };
  };

  // ---- identify / analyze -----------------------------------------------------
  const brief = (d) => d && { id: d.id, name: d.name, maker: d.maker, family: d.family, protocol: d.protocol, checksum: d.checksum, seedkey: d.seedkey };
  const hit = (v, keys) => v && (keys || []).some((k) => k && (String(v).toUpperCase().startsWith(k.toUpperCase()) || k.toUpperCase().startsWith(String(v).toUpperCase())));
  const DEMO_CATALOG = [{ make: "Demo make (preview)", models: ["Preview model"], years: "—", family: "EDC16", source: "preview demo" }];
  F.identify = async ({ path }) => {
    const i = image(path);
    await wait(250);
    if (i.kind === "patch") return { ecu_name: null, hw_number: null, sw_number: null, vin: null, size: i.size, matched: null, catalog: [] };
    return { ecu_name: "DEMO-ECU PREVIEW", hw_number: "0000000000", sw_number: "0000000000", vin: DEMO_VIN, size: i.size, matched: brief(demoDef()), catalog: DEMO_CATALOG };
  };
  F.match_identity = ({ hw, sw }) => {
    hw = String(hw || "").trim(); sw = String(sw || "").trim();
    if (!hw && !sw) throw "enter a hardware or software number";
    // Same rule as the app: a definition matches on its identify prefixes.
    const d = DEFS.find((x) => x.identify && (hit(hw, x.identify.part_numbers) || hit(sw, x.identify.sw_numbers)));
    return { matched: brief(d), catalog: [] };
  };

  // ---- checksum -------------------------------------------------------------
  function blocks(path, algo) {
    const i = image(path);
    if (i.kind === "patch") return [];
    const regs = algo === "crc32-tail" ? [["tail CRC32", 0, i.size]] : [["block 1 (code)", 0, 0xe0000], ["block 2 (calibration)", 0x1c0000, 0x40000]];
    return regs.map(([name, a, n], k) => {
      const good = hash(`${algo}|${name}|${i.kind}`);
      const bad = i.csBad && k === regs.length - 1;
      return { name, ok: !bad, region: `${hex8(a)} +${n}`, stored: hex8(bad ? good ^ 0x00a5f00d : good), computed: hex8(good) };
    });
  }
  F.checksum_verify = async ({ path, algo }) => { await wait(200); return blocks(path, algo); };
  function vaultCopy(path, reason) {
    const b = `demo/opentune-vault/${base(path).replace(/\.\w+$/, "")}.${stamp()}.bin`;
    VAULT.push({ name: base(b), path: b, size: image(path).size, has_meta: true, meta: `source: ${path}\nreason: ${reason}\nsaved: ${new Date().toISOString()}\n(preview demo backup)` });
    return b;
  }
  F.checksum_fix = async ({ path, algo, outPath }) => {
    await wait(300);
    const bad = blocks(path, algo).filter((b) => !b.ok);
    const target = outPath || path;
    const backup = !outPath || outPath === path ? vaultCopy(path, "checksum fix (in place)") : null;
    derive(path, target, { csBad: false });
    return { in_place: target === path, backup, out_path: target, fixes: bad.map((b) => `${b.name} @ ${b.region}: ${b.stored} -> ${b.computed}`) };
  };

  // ---- maps ---------------------------------------------------------------
  const RANGES = [
    [/^(rpm|1\/min|u\/min|min-1)$/, 750, 4500], [/mg\/(stroke|hub|str)|^mg$/, 5, 60], [/^(hpa|mbar)$/, 990, 2400], [/^kpa$/, 100, 240],
    [/^nm$/, 60, 380], [/^(°|deg|grad|°kw|°crk|°ca)/, -6, 24], [/^%$/, 5, 95], [/^(°c|degc|c)$/, -20, 110], [/^v$/, 9, 15],
    [/^ms$/, 0.5, 8], [/^km\/h$/, 0, 200], [/^bar$/, 200, 1800],
  ];
  function unitRange(unit) {
    const u = String(unit || "").toLowerCase().replace(/\s+/g, "");
    const r = RANGES.find(([re]) => re.test(u));
    return r ? [r[1], r[2]] : [10, 100];
  }
  const round = (v, f) => { const q = f && f > 0 && f < 50 ? Math.round(v / f) * f : v; return Math.round(q * 10000) / 10000; };
  function axis(a, n) {
    if (!a || n <= 1) return { name: a ? a.name : null, unit: a ? a.unit : "", values: n <= 1 ? [] : Array.from({ length: n }, (_, i) => i) };
    if (a.values && a.values.length === n) return { name: a.name, unit: a.unit, values: a.values.slice() };
    const [lo, hi] = unitRange(a.unit);
    return { name: a.name, unit: a.unit, values: Array.from({ length: n }, (_, i) => round(lo + ((hi - lo) * i) / (n - 1), hi - lo > 50 ? 1 : 0.1)) };
  }
  function synthCells(key, m, tuned) {
    const R = Math.max(1, m.rows), C = Math.max(1, m.cols);
    let [lo, hi] = unitRange(m.unit);
    if (m.min != null && m.max != null && m.max > m.min) { const s = m.max - m.min; lo = m.min + 0.15 * s; hi = m.min + 0.75 * s; }
    const h = hash(key), ph = (h % 628) / 100, f = (m.scaling && m.scaling.factor) || 1;
    const boosted = tuned && h % 3 !== 1;
    return Array.from({ length: R }, (_, r) => Array.from({ length: C }, (_, c) => {
      const y = R > 1 ? r / (R - 1) : 0.5, x = C > 1 ? c / (C - 1) : 0.5;
      let t = 0.12 + 0.5 * y + 0.32 * x - 0.35 * Math.max(0, x - 0.75) + 0.03 * Math.sin(ph + r * 0.9 + c * 0.6);
      if (boosted && y >= 0.5 && x >= 0.3) t *= 1.12;
      return round(lo + (hi - lo) * Math.max(0, Math.min(1.2, t)), f);
    }));
  }
  const metaOf = (defId) => { const d = DEFS.find((x) => x.id === defId); if (!d) throw "ecudef not found"; return d; };
  function mapMeta(defId, name) {
    const m = (metaOf(defId).map_meta || []).find((x) => x.name === name);
    if (!m) throw `map '${name}' not found in ${defId}`;
    return m;
  }
  const dt = (t) => { const s = String(t || "u16"); return s[0].toUpperCase() + s.slice(1); };
  const en = (e) => (String(e || "big").toLowerCase().startsWith("l") ? "Little" : "Big");
  function cellsFor(path, key, m) {
    const i = image(path);
    return i.edits[key] ? i.edits[key].map((r) => r.slice()) : synthCells(key, m, i.kind === "tuned");
  }
  function decoded(path, key, m) {
    const cells = cellsFor(path, key, m);
    const flat = cells.flat();
    return {
      name: m.name, address: hex8(m.address || 0), rows: Math.max(1, m.rows), cols: Math.max(1, m.cols), unit: m.unit || "",
      data_type: dt(m.data_type), endian: en(m.endian), scaling: { factor: (m.scaling || {}).factor ?? 1, offset: (m.scaling || {}).offset ?? 0 },
      min: Math.min(...flat), max: Math.max(...flat), def_min: m.min ?? null, def_max: m.max ?? null, cells,
      x_axis: axis(m.x_axis, Math.max(1, m.cols)), y_axis: axis(m.y_axis, Math.max(1, m.rows)),
      notes: "Preview: synthetic demo values — the app decodes this map from the opened image." + (m.notes ? " " + m.notes : ""),
    };
  }
  F.list_maps = ({ ecudefId }) => (metaOf(ecudefId).map_meta || []).map((m) => ({
    name: m.name, address: hex8(m.address || 0), rows: m.rows, cols: m.cols, data_type: dt(m.data_type), endian: en(m.endian), unit: m.unit,
    factor: m.scaling.factor, offset: m.scaling.offset, min: m.min, max: m.max, byte_len: m.rows * m.cols * (/32/.test(m.data_type) ? 4 : /8/.test(m.data_type) ? 1 : 2), notes: m.notes,
  }));
  const defKey = (defId, name) => `${defId}|${name}`;
  const adhocKey = (m) => `adhoc|${m.name}|${m.address}`;
  F.decode_map = async ({ path, ecudefId, mapName }) => {
    if (image(path).kind === "patch") throw `map needs ${hex8(0x1d0000)} bytes but the opened image is only ${hex8(4096)} — open a full dump for this ECU`;
    await wait(120);
    return decoded(path, defKey(ecudefId, mapName), mapMeta(ecudefId, mapName));
  };
  F.decode_mapdef = async ({ path, map }) => {
    image(path);
    await wait(100);
    return Object.assign(decoded(path, adhocKey(map), map), { mapdef: map });
  };
  function writeCells(path, key, m, newCells, outPath, algo, fix) {
    const before = cellsFor(path, key, m);
    if (newCells.length !== before.length || newCells.some((r, i) => r.length !== before[i].length)) throw `new cells must be ${before.length}×${before[0].length}`;
    let changed = 0;
    newCells.forEach((r, i) => r.forEach((v, j) => { if (Math.abs(v - before[i][j]) > 1e-9) changed++; }));
    const backup = vaultCopy(path, "map write");
    const target = outPath || path;
    const out = target === path ? image(path) : derive(path, target);
    out.edits[key] = newCells.map((r) => r.slice());
    let checksum = null;
    if (fix) {
      if (!algo) checksum = { error: "no checksum algorithm is set for this map — pick one on the Checksum panel and re-run it manually" };
      else { const bad = changed ? [{ name: "block 2 (calibration)", region: "0x001C0000 +262144" }] : []; out.csBad = false; checksum = { in_place: true, backup: null, out_path: target, fixes: bad.map((b) => `${b.name} @ ${b.region}: recomputed (preview)`) }; }
    } else if (changed) out.csBad = true;
    return { out_path: target, backup, in_place: target === path, cells_total: newCells.flat().length, cells_changed: changed, checksum_algo: algo || null, checksum };
  }
  F.map_write = async ({ path, ecudefId, mapName, newCells, outPath, fixChecksum }) => {
    await wait(250);
    const d = metaOf(ecudefId);
    const r = writeCells(path, defKey(ecudefId, mapName), mapMeta(ecudefId, mapName), newCells, outPath, d.checksum, fixChecksum);
    return Object.assign(r, { checksum_source: `definition ${d.name}` });
  };
  F.mapdef_write = async ({ path, map, newCells, outPath, fixChecksum, checksumAlgo }) => {
    await wait(250);
    return Object.assign(writeCells(path, adhocKey(map), map, newCells, outPath, checksumAlgo, fixChecksum), { checksum_source: "the Checksum panel" });
  };

  // ---- analyze ------------------------------------------------------------
  F.analyze_image = async ({ path }) => {
    const i = image(path);
    await wait(450);
    const d = demoDef();
    const isImg = i.kind !== "patch";
    const cs = isImg ? blocks(path, (d && d.checksum) || "edc16") : [];
    return {
      path, size: i.size, size_class: isImg ? "2 MiB" : "4 KiB", family: isImg ? "EDC16" : "Unknown",
      family_how: isImg ? ["preview: synthetic image — the demo backend claims this family so the panels have something to show"] : [],
      supplier: isImg ? "Bosch" : null, variant: null, family_confidence: isImg ? "medium" : "none",
      recognition: isImg ? { primary: { supplier: "Bosch", family: "EDC16", variant: "preview demo", confidence: "medium" }, others: [], contested: false, image_kind: "raw" } : { primary: null, others: [], contested: false, image_kind: "raw" },
      identity: isImg ? IDENT.slice(1).map(([offset, value], k) => ({ kind: k ? "vin" : "ecu_name", value: value.trim(), offset })) : [],
      part_numbers: isImg ? ["000906000"] : [], bosch_numbers: isImg ? ["0000000000"] : [], sw_numbers: isImg ? ["0000000000"] : [], mercedes_numbers: [],
      epk_hits: 0, vin: isImg ? DEMO_VIN : null, vin_decoded: isImg ? { make: "Volkswagen", year: null, year_ambiguous: true } : null,
      part_decoded: isImg ? { normalized: "000906000", kind: "Vag", supplier: null, vag_type_group: "000", vag_main_group: "906", vag_subgroup: "000", vag_version: null, bosch_family: null, bosch_consecutive: null, flags: ["placeholder number (preview)"] } : null,
      catalog_match: null, ecudef_match: isImg && d ? { id: d.id, name: d.name, maker: d.maker, family: d.family } : null,
      assignment: { make: null, model: null, engine: null, basis: "preview demo image — no real identity" }, confidence: "Low",
      checksum_algo: isImg ? (d && d.checksum) || "edc16" : null, checksum_blocks: cs,
      maps: isImg && d ? (d.map_meta || []).slice(0, 8).map((m) => ({ name: m.name, address: hex8(m.address || 0), rows: m.rows, cols: m.cols, data_type: dt(m.data_type), unit: m.unit })) : [],
    };
  };

  // ---- compare / findmaps -----------------------------------------------
  F.findmaps = async ({ stock, tuned, minLen }) => {
    const a = image(stock), b = image(tuned);
    await wait(300);
    if (a.size !== b.size) throw `sizes differ: ${a.size} vs ${b.size} bytes`;
    if (a.kind === b.kind && JSON.stringify(a.edits) === JSON.stringify(b.edits)) return [];
    return TUNED_REGIONS.filter(([, n]) => n >= (minLen || 1)).map(([s, n], k) => ({
      index: k + 1, start: hex8(s), end: hex8(s + n - 1), len: n, hint: n >= 256 ? "16 × u16 (stride 32)" : "8 × u16 (stride 16)",
    }));
  };
  F.compare_maps = async ({ defId, stock, tuned, mapName }) => {
    const d = metaOf(defId), a = image(stock), b = image(tuned);
    await wait(250);
    let total = 0, changedMaps = 0;
    const maps = (d.map_meta || []).map((m) => {
      const key = defKey(defId, m.name);
      const pa = cellsFor(stock, key, m), pb = cellsFor(tuned, key, m);
      let changed = 0, best = 0, sum = 0;
      pa.forEach((row, r) => row.forEach((v, c) => { const dd = pb[r][c] - v; if (Math.abs(dd) > 1e-9) { changed++; sum += dd; if (Math.abs(dd) > Math.abs(best)) best = dd; } }));
      total += changed; if (changed) changedMaps++;
      const e = { name: m.name, rows: m.rows, cols: m.cols, unit: m.unit, data_type: dt(m.data_type), changed, d_max: round(best, 0.0001), d_avg: changed ? round(sum / changed, 0.0001) : 0, out_of_range: false };
      if (mapName === m.name) Object.assign(e, { stock: pa, tuned: pb, x_axis: axis(m.x_axis, m.cols), y_axis: axis(m.y_axis, m.rows) });
      return e;
    });
    return { def: d.id, def_name: d.name, stock_size: a.size, tuned_size: b.size, total_changed: total, changed_maps: changedMaps, maps };
  };

  // ---- XDF / A2L import + promote ------------------------------------------
  const IMPORT = [
    { name: "Demo boost target (preview XDF)", address: 0x1c0000, rows: 8, cols: 16, data_type: "u16", endian: "big", scaling: { factor: 1, offset: 0 }, unit: "hPa", shape: "map",
      x_axis: { name: "engine speed", unit: "rpm", values: [] }, y_axis: { name: "injection quantity", unit: "mg/stroke", values: [] } },
    { name: "Demo smoke limiter (preview XDF)", address: 0x1c4000, rows: 4, cols: 16, data_type: "u16", endian: "big", scaling: { factor: 0.01, offset: 0 }, unit: "mg/stroke", shape: "map",
      x_axis: { name: "engine speed", unit: "rpm", values: [] }, y_axis: { name: "air mass", unit: "%", values: [] } },
    { name: "Demo idle speed (preview XDF)", address: 0x1c8000, rows: 1, cols: 1, data_type: "u16", endian: "big", scaling: { factor: 1, offset: 0 }, unit: "rpm", shape: "value", x_axis: null, y_axis: null },
  ];
  F.import_definitions = async ({ path, base: b }) => {
    await wait(300);
    const off = b ? parseInt(String(b).replace(/^0x/i, ""), 16) || 0 : 0;
    const maps = IMPORT.map((m) => Object.assign(JSON.parse(JSON.stringify(m)), { address: m.address + off, address_hex: hex8(m.address + off) }));
    return /\.a2l$/i.test(path)
      ? { kind: "a2l", project: "PREVIEW_DEMO", module: "DEMO", base: off || null, maps }
      : { kind: "xdf", title: "Preview demo XDF (synthetic layout)", base_offset: off, maps };
  };
  F.promote_region = ({ path, start, len, name, rows, cols, dataType, endian, factor, offset }) => {
    const i = image(path);
    const a = parseInt(String(start).replace(/^0x/i, ""), 16);
    if (!Number.isFinite(a)) throw `bad start address '${start}'`;
    const w = /32/.test(dataType) ? 4 : /8/.test(dataType) ? 1 : 2;
    if (a + rows * cols * w > i.size) throw `region ${hex8(a)} + ${rows * cols * w} bytes runs past the end of the image (${i.size} bytes)`;
    if (len && rows * cols * w > len) throw `${rows}×${cols} ${dataType} needs ${rows * cols * w} bytes but the region is ${len}`;
    return { name: name || `region ${hex8(a)}`, address: a, rows, cols, data_type: dataType, endian, scaling: { factor, offset }, unit: "", x_axis: null, y_axis: null };
  };

  // ---- flash to file / vault ------------------------------------------------
  const VAULT = [{ name: "preview-stock.20261101-090000.bin", path: "demo/opentune-vault/preview-stock.20261101-090000.bin", size: 2 * MiB, has_meta: true, meta: "source: demo/preview-stock.bin\nreason: map write\n(preview demo backup)" }];
  F.vault_list = ({ dir }) => (String(dir || "").includes("opentune-vault") ? VAULT.slice() : []);
  F.flash = async ({ path, addr, dataPath }) => {
    const t = image(path), d = image(dataPath);
    const a = parseInt(String(addr || "").replace(/^0x/i, ""), 16);
    if (!Number.isFinite(a)) throw `bad address '${addr}'`;
    if (a + d.size > t.size) throw `region ${hex8(a)} +${d.size} runs past the end of the target (${t.size} bytes)`;
    await wait(500);
    const backup = vaultCopy(path, "flash to file");
    t.csBad = true;
    return { target: "file", bytes_written: d.size, voltage_gate: "skipped — file target, no battery to measure", backup, backup_len: t.size, verified: true, region: `${hex8(a)} +${d.size} (${hex8(d.size)})` };
  };

  // ---- project (.otproj) -------------------------------------------------
  const PROJECTS = {};
  F.project_save = ({ path, project }) => {
    PROJECTS[path] = JSON.parse(JSON.stringify(project));
    const bp = project.base_image && project.base_image.path;
    return { path, name: project.name, edits: (project.edits || []).length, image_fingerprinted: !!bp, base_image: { path: bp, size: bp ? image(bp).size : 0, hash: "preview" } };
  };
  F.project_open = async ({ path }) => {
    await wait(150);
    let p = PROJECTS[path];
    if (!p) {
      const d = demoDef();
      const m = d && (d.map_meta || [])[0];
      const baseline = m ? synthCells(defKey(d.id, m.name), m, false) : [];
      p = {
        format: "opentune-project", version: 1, name: "Preview demo tune", notes: "A demo project: one edited map against the synthetic stock image.",
        ecudef_id: d ? d.id : null, checksum_algo: d ? d.checksum : null, base_image: { path: "demo/preview-stock.bin", size: 2 * MiB, hash: "preview" },
        edits: m ? [{ kind: "def", def_id: d.id, map_name: m.name, mapdef: null, baseline, notes: "+6 % in the upper half (preview)",
          cells: baseline.map((row, r) => row.map((v, c) => (r >= baseline.length / 2 ? round(v * 1.06, m.scaling.factor) : v))) }] : [],
        created: "2026-11-01T09:00:00Z", saved: "2026-11-01T09:30:00Z",
      };
    }
    return { path, project: p, image_status: "match", image_path: p.base_image.path, stored_size: 2 * MiB, stored_hash: "preview", current_size: 2 * MiB, current_hash: "preview" };
  };
})();
