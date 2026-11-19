// Demo fixtures for the Log viewer. Preview only: a synthetic 60 s drive (two
// pulls, a cruise and a lift-off) generated here at 20 Hz, plus two demo maps
// with inline axes for the map overlay. Not a recorded log of any car.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  // ---- the synthetic log ---------------------------------------------------
  const HZ = 20, DUR = 60;
  let seed = 7;
  const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648 - 0.5) * 2;
  const CH = [
    ["Engine speed", "rpm"], ["Accelerator pedal", "%"], ["Engine load", "%"], ["Boost pressure", "kPa"],
    ["Injection quantity", "mg/stroke"], ["Coolant temperature", "°C"], ["Vehicle speed", "km/h"],
  ].map(([name, unit]) => ({ name, unit }));
  const T = [], COLS = CH.map(() => []);
  // Pedal script: idle, pull in 2nd, shift, pull in 3rd, cruise, lift, pull, idle.
  const pedal = (t) => t < 4 ? 0 : t < 12 ? 100 : t < 13 ? 0 : t < 22 ? 95 : t < 38 ? 28 + 4 * Math.sin(t) : t < 43 ? 0 : t < 50 ? 85 : t < 53 ? 15 : 0;
  let rpm = 820, spd = 0, boost = 101, gear = 2;
  for (let i = 0; i <= DUR * HZ; i++) {
    const t = i / HZ, p = Math.max(0, Math.min(100, pedal(t) + rnd() * 1.5));
    if (t >= 12.5 && t < 13) gear = 3;
    if (t >= 43) gear = 4;
    const accel = p > 5 ? p / 100 * (gear === 2 ? 2.4 : gear === 3 ? 1.6 : 1.1) : -0.9;
    spd = Math.max(0, spd + accel / HZ * 3.6);
    const target = Math.max(800, spd * (gear === 2 ? 52 : gear === 3 ? 34 : 26));
    rpm += (target - rpm) * 0.25;
    const bt = 101 + (p / 100) * Math.min(1, Math.max(0, (rpm - 1300) / 1200)) * 125;
    boost += (bt - boost) * 0.12;
    const load = Math.min(100, 12 + p * 0.82);
    const iq = p > 5 ? 6 + p * 0.42 * Math.min(1, boost / 180) : 3 + Math.max(0, (900 - rpm) / 100);
    T.push(Math.round(t * 1000) / 1000);
    [rpm + rnd() * 8, p, load + rnd(), boost + rnd() * 0.8, iq + rnd() * 0.3, Math.min(90, 78 + t * 0.2), spd].forEach((v, k) => COLS[k].push(Math.round(v * 100) / 100));
  }
  // Two short dropouts so the stats show missing samples.
  for (let i = 600; i < 606; i++) COLS[3][i] = null;

  const range = (t0, t1) => {
    let a = 0, b = T.length;
    if (t0 != null) while (a < T.length && T[a] < t0) a++;
    if (t1 != null) { b = a; while (b < T.length && T[b] <= t1) b++; }
    return [a, Math.max(a, b)];
  };
  function stats(lo, hi) {
    return CH.map((c, k) => {
      let n = 0, bad = 0, sum = 0, sq = 0, mn = null, mx = null, tmn = null, tmx = null;
      for (let i = lo; i < hi; i++) {
        const v = COLS[k][i];
        if (v == null) { bad++; continue; }
        n++; sum += v; sq += v * v;
        if (mn == null || v < mn) { mn = v; tmn = T[i]; }
        if (mx == null || v > mx) { mx = v; tmx = T[i]; }
      }
      const mean = n ? sum / n : null;
      return { name: c.name, unit: c.unit, count: n, invalid: bad, min: mn, max: mx, mean, std: n ? Math.sqrt(Math.max(0, sq / n - mean * mean)) : null, t_min: tmn, t_max: tmx };
    });
  }
  const known = (path) => { if (!/\.(csv|txt|tsv|log)$/i.test(String(path))) throw `read '${path}': not a CSV log`; };

  F.log_open = async ({ path, timeUnit }) => {
    known(path);
    await wait(200);
    // The demo log has a seconds column; a unit override is acknowledged but the
    // preview keeps plotting in seconds.
    const forced = timeUnit && timeUnit !== "auto";
    return {
      path, format: "wide", delimiter: ",", decimal_comma: false, header: true, units_row: true,
      time: { column: "Time", unit: "s", source: forced ? "override" : "header", label: "s", epoch_start: null },
      rows: T.length, t0: 0, t1: DUR, duration: DUR, median_dt: 1 / HZ, rate_hz: HZ,
      channels: stats(0, T.length),
      notes: ["Preview: a synthetic 60 s drive generated in the browser — not a recorded log."]
        .concat(forced ? [`Preview: time unit '${timeUnit}' noted; the demo log stays in seconds.`] : []),
    };
  };
  F.log_series = async ({ channels, t0, t1, buckets }) => {
    const [lo, hi] = range(t0, t1);
    const n = hi - lo, B = Math.max(1, buckets || 1000);
    const dec = n > 2 * B;
    const series = (channels || []).map((name) => {
      const k = CH.findIndex((c) => c.name === name);
      if (k < 0) throw `no channel '${name}' in this log`;
      const t = [], v = [];
      if (!dec) { for (let i = lo; i < hi; i++) { t.push(T[i]); v.push(COLS[k][i]); } }
      else {
        for (let b = 0; b < B; b++) {
          const a = lo + Math.floor((b * n) / B), e = lo + Math.floor(((b + 1) * n) / B);
          let imn = -1, imx = -1;
          for (let i = a; i < e; i++) { const x = COLS[k][i]; if (x == null) continue; if (imn < 0 || x < COLS[k][imn]) imn = i; if (imx < 0 || x > COLS[k][imx]) imx = i; }
          if (imn < 0) { t.push(T[a]); v.push(null); continue; }
          [imn, imx].sort((x, y) => x - y).forEach((i) => { t.push(T[i]); v.push(COLS[k][i]); });
        }
      }
      return { name, unit: CH[k].unit, t, v };
    });
    return { t0: t0 == null ? 0 : t0, t1: t1 == null ? DUR : t1, points_in_range: n, decimated: dec, buckets: dec ? B : n, series, stats: stats(lo, hi) };
  };
  F.log_sample_at = ({ t }) => {
    let i = Math.round(t * HZ);
    i = Math.max(0, Math.min(T.length - 1, i));
    return { index: i, t: T[i], values: CH.map((c, k) => ({ name: c.name, unit: c.unit, value: COLS[k][i] })) };
  };

  // Drive view replay: every row of the synthetic drive on one time axis.
  F.log_replay = async ({ path, channels }) => {
    known(path);
    await wait(120);
    const names = channels && channels.length ? channels : CH.map((c) => c.name);
    const series = names.map((n) => {
      const k = CH.findIndex((c) => c.name === n);
      if (k < 0) throw `no channel '${n}' in this log`;
      return { name: CH[k].name, unit: CH[k].unit, t: [], v: COLS[k].slice() };
    });
    return { path, rows: T.length, stride: 1, t: T.slice(), series, time_label: "s", median_dt: 1 / HZ, epoch_start: null };
  };

  // ---- map overlay ----------------------------------------------------------
  const RPM = [800, 1250, 1750, 2000, 2250, 2500, 3000, 3500, 4000, 4500];
  const IQ = [0, 5, 10, 20, 30, 40, 50, 60];
  const MAPS = [
    { name: "Boost target (preview demo map)", rows: IQ.length, cols: RPM.length, unit: "kPa",
      x_axis: { name: "engine speed", unit: "rpm", values: RPM }, y_axis: { name: "injection quantity", unit: "mg/stroke", values: IQ },
      cell: (r, c) => Math.round(101 + Math.min(1, IQ[r] / 45) * Math.min(1, Math.max(0, (RPM[c] - 1000) / 1300)) * (130 - Math.max(0, RPM[c] - 3500) / 50)) },
    { name: "Torque limiter (preview demo curve)", rows: 1, cols: RPM.length, unit: "Nm",
      x_axis: { name: "engine speed", unit: "rpm", values: RPM }, y_axis: { name: "", unit: "", values: [] },
      cell: (r, c) => Math.round(180 + Math.min(1, (RPM[c] - 800) / 1000) * 160 - Math.max(0, RPM[c] - 3000) * 0.06) },
  ];
  const axisInfo = (a, count) => ({ name: a.name, unit: a.unit, count, kind: a.values.length ? "inline" : "none" });
  F.log_def_maps = async ({ ecudefId, ecudefPath }) => {
    if (!ecudefId && !ecudefPath) throw "choose a definition";
    await wait(120);
    const d = ((window.__PREVIEW__ || {}).ecudefs || []).find((x) => x.id === ecudefId);
    return {
      id: ecudefId || "preview-demo", name: d ? d.name : "Preview demo definition",
      maps: MAPS.map((m, i) => ({
        name: m.name, rows: m.rows, cols: m.cols, unit: m.unit, x_axis: axisInfo(m.x_axis, m.cols), y_axis: axisInfo(m.y_axis, m.rows), needs_image: false,
        // The app sends `def` only for a definition file; the preview's demo maps
        // are not in any bundled definition, so it always sends it and "Open in
        // Maps" opens them as ad-hoc maps.
        def: mapDef(m, i),
      })),
    };
  };
  function mapDef(m, i) {
    const ax = (a, n) => (a.values.length ? { name: a.name, unit: a.unit, count: n, values: a.values } : null);
    return {
      name: m.name, notes: "CONFIDENCE: EXAMPLE ONLY. Preview demo map.", address: 0x1e0000 + i * 0x400,
      data_type: "u16", endian: "big", rows: m.rows, cols: m.cols, scaling: { factor: 1, offset: 0 }, unit: m.unit,
      x_axis: ax(m.x_axis, m.cols), y_axis: ax(m.y_axis, m.rows),
    };
  }
  const UNIT = { "1/min": "rpm", "u/min": "rpm", "mg/hub": "mg/stroke", "mg/str": "mg/stroke" };
  const normU = (u) => { const s = String(u || "").toLowerCase().replace(/\s+/g, ""); return UNIT[s] || s; };
  const nearest = (axis, v) => { let best = 0; axis.forEach((a, i) => { if (Math.abs(a - v) < Math.abs(axis[best] - v)) best = i; }); return best; };
  function bracket(axis, v) {
    if (v <= axis[0]) return [[0, 1]];
    if (v >= axis[axis.length - 1]) return [[axis.length - 1, 1]];
    let i = 0; while (axis[i + 1] < v) i++;
    const f = (v - axis[i]) / (axis[i + 1] - axis[i]);
    return [[i, 1 - f], [i + 1, f]];
  }
  F.log_map_hits = async (a) => {
    const m = MAPS.find((x) => x.name === a.mapName);
    if (!m) throw `map '${a.mapName}' not in this definition`;
    const kx = CH.findIndex((c) => c.name === a.xChannel);
    if (kx < 0) throw "choose the X channel";
    const curve = m.rows <= 1;
    const ky = curve ? -1 : CH.findIndex((c) => c.name === a.yChannel);
    if (!curve && ky < 0) throw "choose the Y channel";
    const kv = a.valueChannel ? CH.findIndex((c) => c.name === a.valueChannel) : -1;
    const check = (k, axis) => {
      if (k < 0) return null;
      const cu = normU(CH[k].unit), au = normU(axis.unit);
      return { channel: CH[k].name, channel_unit: CH[k].unit, axis: axis.name, axis_unit: axis.unit, check: !cu || !au ? "unknown" : cu === au ? "match" : "mismatch" };
    };
    const ux = check(kx, m.x_axis), uy = curve ? null : check(ky, m.y_axis);
    const warnings = [], bad = [ux, uy].filter((u) => u && u.check === "mismatch").map((u) => `${u.axis}: channel '${u.channel}' is in ${u.channel_unit}, the axis in ${u.axis_unit}`);
    if (bad.length && !a.acceptUnits) throw `unit mismatch — ${bad.join("; ")}. OpenTune does not convert units: pick the matching channel, or explicitly accept the values as logged.`;
    bad.forEach((b) => warnings.push(b + " (accepted as logged, not converted)"));
    if (curve && a.yChannel) warnings.push("this map is a 1-row curve; the Y channel is not used");
    await wait(150);
    const R = m.rows, C = m.cols;
    const grid = (v) => Array.from({ length: R }, () => Array(C).fill(v));
    const hits = grid(0), time = grid(0), vmin = grid(null), vmax = grid(null), vsum = grid(0), vw = grid(0);
    const out = { x_low: 0, x_high: 0, y_low: 0, y_high: 0, any: 0 };
    const [lo, hi] = range(a.t0, a.t1);
    let used = 0, skipped = 0;
    const bil = a.mode === "bilinear";
    for (let i = lo; i < hi; i++) {
      const x = COLS[kx][i], y = curve ? 0 : COLS[ky][i];
      if (x == null || y == null) { skipped++; continue; }
      used++;
      const xa = m.x_axis.values, ya = curve ? [0] : m.y_axis.values;
      let o = false;
      if (x < xa[0]) { out.x_low++; o = true; } else if (x > xa[xa.length - 1]) { out.x_high++; o = true; }
      if (!curve) { if (y < ya[0]) { out.y_low++; o = true; } else if (y > ya[ya.length - 1]) { out.y_high++; o = true; } }
      if (o) out.any++;
      const dt = i + 1 < T.length ? T[i + 1] - T[i] : 1 / HZ;
      const cells = bil
        ? bracket(ya, y).flatMap(([r, wr]) => bracket(xa, x).map(([c, wc]) => [r, c, wr * wc]))
        : [[curve ? 0 : nearest(ya, y), nearest(xa, x), 1]];
      cells.forEach(([r, c, w]) => {
        hits[r][c] += w; time[r][c] += w * dt;
        if (kv >= 0 && COLS[kv][i] != null) {
          const v = COLS[kv][i];
          vmin[r][c] = vmin[r][c] == null ? v : Math.min(vmin[r][c], v);
          vmax[r][c] = vmax[r][c] == null ? v : Math.max(vmax[r][c], v);
          vsum[r][c] += w * v; vw[r][c] += w;
        }
      });
    }
    const mx = (g) => Math.max(0, ...g.flat());
    const img = a.imagePath;
    return {
      rows: R, cols: C, mode: bil ? "bilinear" : "nearest", hits, time,
      values: kv >= 0 ? { min: vmin, max: vmax, mean: vsum.map((row, r) => row.map((s, c) => (vw[r][c] ? s / vw[r][c] : null))) } : null,
      samples_total: hi - lo, samples_used: used, samples_skipped: skipped, outside: out,
      max_hits: mx(hits), max_time: mx(time), dwell_cap: 0,
      map: {
        name: m.name, unit: m.unit, rows: R, cols: C,
        x_axis: { name: m.x_axis.name, unit: m.x_axis.unit, values: m.x_axis.values },
        y_axis: { name: m.y_axis.name, unit: m.y_axis.unit, values: m.y_axis.values },
        cells: img ? Array.from({ length: R }, (_, r) => Array.from({ length: C }, (_, c) => m.cell(r, c))) : null,
        cells_error: null,
      },
      units: { x: ux, y: uy }, value_channel: kv >= 0 ? { name: CH[kv].name, unit: CH[kv].unit } : null,
      time_label: "s", range: { t0: T[lo], t1: T[Math.max(lo, hi - 1)] }, warnings,
    };
  };
})();
