// OpenTune — Drive view logic (pure: no DOM, no Tauri).
// The glanceable Drive view (feat_driveview.js) shows a few big tiles for live
// OBD values or a replayed log. Everything that decides what a tile says lives
// here so the node tests can check it: unit-aware formatting, default channel
// choice from a log header, warn/crit thresholds, stale / no-data / link-lost
// states, min/max/peak hold, the replay clock and the sparkline path.
// Also hosts the small live feed the Dashboard and Live log panels publish
// their polls to (window.OTDrive.feed), so the Drive view never opens an
// adapter path of its own.
"use strict";
(function (root) {
  // ---- units ---------------------------------------------------------------
  const UNIT_ALIAS = {
    "°c": "degc", degc: "degc", c: "degc", celsius: "degc",
    "°f": "degf", degf: "degf", f: "degf",
    "1/min": "rpm", "u/min": "rpm", "min-1": "rpm", "min^-1": "rpm", rpm: "rpm",
    "km/h": "km/h", kmh: "km/h", kph: "km/h", "km/hr": "km/h",
    mph: "mph", "mi/h": "mph", "m/s": "m/s",
    kpa: "kpa", bar: "bar", hpa: "hpa", mbar: "hpa", psi: "psi",
    "%": "%", v: "v", "g/s": "g/s", "λ": "lambda", lambda: "lambda",
  };
  // A unit spelling reduced to one canonical form ("" when there is none).
  function normUnit(u) {
    const s = String(u == null ? "" : u).trim().toLowerCase().replace(/^[[(]\s*|\s*[\])]$/g, "").replace(/\s+/g, "");
    return UNIT_ALIAS[s] || s;
  }
  const SPEED_UNITS = ["km/h", "mph", "m/s"];
  const TEMP_UNITS = ["degc", "degf", "k"];
  const PRESSURE_UNITS = ["kpa", "bar", "hpa", "psi"];

  // A value in the unit it is shown in: kPa <-> bar follows the pressure
  // preference ("bar" | "kPa"); everything else stays as logged. Returns
  // { v, unit, dp } (dp = decimals for the shown unit and magnitude).
  function display(v, unit, pUnit) {
    const n = normUnit(unit);
    let x = typeof v === "number" && isFinite(v) ? v : null;
    let u = unit == null ? "" : String(unit);
    if (n === "kpa" && pUnit === "bar") { x = x == null ? null : x / 100; u = "bar"; }
    else if (n === "bar" && pUnit === "kPa") { x = x == null ? null : x * 100; u = "kPa"; }
    return { v: x, unit: u, dp: decimals(x, normUnit(u)) };
  }
  function decimals(v, n) {
    if (["rpm", "degc", "degf", "km/h", "mph", "kpa", "hpa", "%"].includes(n)) return 0;
    if (n === "bar" || n === "lambda") return 2;
    if (n === "v" || n === "g/s" || n === "psi" || n === "m/s") return 1;
    const a = Math.abs(v == null ? 0 : v);
    return a >= 100 ? 0 : a >= 10 ? 1 : 2;
  }
  // "1 234"-free plain text of a number with dp decimals; "—" for none. A
  // signed value gets an explicit "+"; "-0" is never shown.
  function fmtNum(v, dp, signed) {
    if (typeof v !== "number" || !isFinite(v)) return "—";
    let s = v.toFixed(Math.max(0, dp || 0));
    if (/^-0(\.0+)?$/.test(s)) s = s.slice(1);
    if (signed && v > 0 && !/^0(\.0+)?$/.test(s)) s = "+" + s;
    return s;
  }
  // Text + shown unit for a value in its logged unit.
  function fmtValue(v, unit, pUnit, signed) {
    const d = display(v, unit, pUnit);
    return { text: fmtNum(d.v, d.dp, signed), unit: d.unit };
  }

  // ---- default channels --------------------------------------------------------
  // Role order is the default tile order. `keys` are exact ids (Dashboard keys,
  // Live-log param ids); `re` matches a log header name; `not` excludes names;
  // `units` (canonical) are accepted when the channel has a unit. `optional`
  // tiles stay hidden until they have had a value (a GPS source is optional).
  const ROLES = [
    { role: "rpm", label: "RPM", keys: ["rpm", "engine_speed", "n_engine"], re: /engine ?speed|\brpm\b|drehzahl/i, not: /target|limit|idle set/i, units: ["rpm"] },
    { role: "speed", label: "Speed", keys: ["speed", "vss", "vehicle_speed", "vehicle_speed_kmh", "veh_speed", "obd_speed", "obd_vss", "spd"], re: /vehicle ?speed|^speed\b|geschwindigkeit/i, not: /gps|error/i, units: SPEED_UNITS },
    { role: "coolant", label: "Coolant", keys: ["coolant_temp", "coolant", "ect"], re: /coolant|\bect\b|kühlmittel/i, not: /target|set ?point/i, units: TEMP_UNITS },
    { role: "iat", label: "Intake air", keys: ["intake_air_temp", "iat"], re: /intake air temp|intake air\b|\biat\b|charge air temp/i, units: TEMP_UNITS },
    { role: "boost", label: "Boost", keys: ["boost_kpa", "boost_bar", "boost"], re: /boost/i, not: /command|target|set ?point|desired|spec|soll|limit/i, units: PRESSURE_UNITS },
    { role: "load", label: "Load", keys: ["engine_load", "load", "calc_load"], re: /engine load|\bload\b/i, not: /target|limit/i, units: ["%"] },
    { role: "gps_speed", label: "GPS speed", keys: ["gps_speed_kmh"], re: /gps speed/i, units: SPEED_UNITS, optional: true },
    { role: "speed_error", label: "Speed error", keys: ["speed_error_kmh"], re: /speed error/i, units: SPEED_UNITS, optional: true, signed: true },
  ];
  const lc = (s) => String(s == null ? "" : s).toLowerCase();
  function unitFits(role, unit) {
    const n = normUnit(unit);
    return !n || !role.units || role.units.includes(n);
  }
  // Units must fit either way: a log's "Speed" column in rpm is not the
  // vehicle speed, even though its name is a known id.
  function roleMatches(role, ch, byKeyOnly) {
    if (!unitFits(role, ch.unit)) return false;
    if (role.keys.includes(lc(ch.key))) return true;
    if (byKeyOnly) return false;
    const name = String(ch.name || ch.key || "");
    return role.re.test(name) && !(role.not && role.not.test(name));
  }
  // The role a channel plays ({role, …} or null).
  function roleOf(ch) {
    if (!ch) return null;
    return ROLES.find((r) => roleMatches(r, ch, true)) || ROLES.find((r) => roleMatches(r, ch, false)) || null;
  }
  // The default selection for a channel list [{key, name, unit}]: one channel
  // per role, in role order, exact ids first, then header names. Returns keys.
  function pickDefault(channels) {
    const list = channels || [];
    const used = new Set();
    const out = [];
    ROLES.forEach((r) => {
      const hit = list.find((c) => !used.has(c.key) && roleMatches(r, c, true)) || list.find((c) => !used.has(c.key) && roleMatches(r, c, false));
      if (hit) { used.add(hit.key); out.push(hit.key); }
    });
    return out;
  }
  // A saved selection filtered to the channels that exist, else the default.
  function selectionFor(channels, saved) {
    const have = new Set((channels || []).map((c) => c.key));
    const kept = (saved || []).filter((k) => have.has(k));
    return kept.length ? kept : pickDefault(channels);
  }

  // ---- thresholds ------------------------------------------------------------
  // The Dashboard's channel table row for a channel: same key, same name, or
  // the row of the channel's role. Its range and warn/crit apply only when the
  // units agree (never converted): { min, max, warn, crit, row } or nulls.
  function thresholdsFor(ch, table) {
    const none = { min: null, max: null, warn: null, crit: null, row: null };
    if (!ch || !table || !table.length) return none;
    const name = lc(ch.name);
    let row = table.find((t) => t.key === ch.key) || (name && table.find((t) => lc(t.name) === name));
    if (!row) {
      const r = roleOf(ch);
      if (r) row = r.keys.map((k) => table.find((t) => t.key === k)).find(Boolean);
    }
    if (!row) return none;
    if (!normUnit(ch.unit) || normUnit(ch.unit) !== normUnit(row.unit)) return Object.assign({}, none, { row, unitMismatch: !!normUnit(ch.unit) });
    const num = (x) => (typeof x === "number" && isFinite(x) ? x : null);
    return { min: num(row.min), max: num(row.max), warn: num(row.warn), crit: num(row.crit), row };
  }
  // "crit" | "warn" | "" for a value against { warn, crit } (at or above).
  function level(v, th) {
    if (typeof v !== "number" || !isFinite(v) || !th) return "";
    if (th.crit != null && v >= th.crit) return "crit";
    if (th.warn != null && v >= th.warn) return "warn";
    return "";
  }
  // Bar range: the table's range when known, else what has been seen.
  function barRange(th, peak) {
    if (th && th.min != null && th.max != null && th.max > th.min) return { lo: th.min, hi: th.max };
    if (peak && peak.min != null && peak.max != null) {
      const lo = Math.min(0, peak.min);
      const hi = peak.max > lo ? peak.max : lo + 1;
      return { lo, hi };
    }
    return null;
  }
  // 0..1 position of v on a range (clamped); null without both.
  function frac(v, rng) {
    if (!rng || typeof v !== "number" || !isFinite(v)) return null;
    return Math.max(0, Math.min(1, (v - rng.lo) / (rng.hi - rng.lo)));
  }

  // ---- stale / link state --------------------------------------------------------
  const STALE_MIN_S = 2.5;
  // A value older than this (seconds) is stale: three update intervals, never
  // less than STALE_MIN_S.
  function staleAfter(intervalS) {
    return Math.max(STALE_MIN_S, 3 * (typeof intervalS === "number" && intervalS > 0 ? intervalS : 0));
  }
  // Link of a live source: "lost" after a failing poll or no frame for
  // staleS; "waiting" before the first frame; "stopped" when the source said so.
  function linkState(o) {
    if (o.stopped) return "stopped";
    if (o.failing) return "lost";
    if (o.lastFrameT == null) return "waiting";
    return o.now - o.lastFrameT > o.staleS ? "lost" : "up";
  }
  // Tile state: "lost" (the link is down: nothing shown is live), "nodata"
  // (never had a value), "stale" (last value older than staleS) or "live".
  function tileState(o) {
    if (o.link && o.link !== "up" && o.link !== "replay") return o.lastT == null ? "nodata" : "lost";
    if (o.lastT == null) return "nodata";
    return o.now - o.lastT > o.staleS ? "stale" : "live";
  }
  // "4 s ago" / "2 min ago" for an age in seconds.
  function fmtAge(s) {
    if (typeof s !== "number" || !isFinite(s) || s < 0) return "";
    if (s < 60) return Math.floor(s) + " s ago";
    if (s < 3600) return Math.floor(s / 60) + " min ago";
    return Math.floor(s / 3600) + " h ago";
  }

  // ---- min / max / peak hold ------------------------------------------------------
  function peakNew() { return { min: null, max: null, tMin: null, tMax: null, n: 0 }; }
  function peakAdd(p, v, t) {
    if (typeof v !== "number" || !isFinite(v)) return p;
    if (p.min == null || v < p.min) { p.min = v; p.tMin = t; }
    if (p.max == null || v > p.max) { p.max = v; p.tMax = t; }
    p.n += 1;
    return p;
  }

  // ---- live history (sparklines) ------------------------------------------------
  const HIST_S = 60;
  function histNew() { return { t: [], v: [] }; }
  // Append (t, v) (v may be null = gap) and drop points older than spanS.
  function histPush(h, t, v, spanS) {
    h.t.push(t);
    h.v.push(typeof v === "number" && isFinite(v) ? v : null);
    const cut = t - (spanS || HIST_S);
    let k = 0;
    while (k < h.t.length && h.t[k] < cut) k++;
    if (k) { h.t.splice(0, k); h.v.splice(0, k); }
    return h;
  }
  // SVG path of (t, v) inside [tA, tB] in a w x h box; nulls break the line.
  // A flat series draws mid-height. "" when there is nothing to draw.
  function sparkPath(t, v, tA, tB, w, h) {
    let lo = Infinity, hi = -Infinity;
    for (let i = 0; i < t.length; i++) {
      if (v[i] == null || t[i] < tA || t[i] > tB) continue;
      if (v[i] < lo) lo = v[i];
      if (v[i] > hi) hi = v[i];
    }
    if (!isFinite(lo)) return "";
    if (hi - lo < 1e-9) { lo -= 1; hi += 1; }
    const span = tB - tA || 1;
    let d = "", pen = false;
    for (let i = 0; i < t.length; i++) {
      if (t[i] < tA || t[i] > tB) continue;
      if (v[i] == null) { pen = false; continue; }
      const x = ((t[i] - tA) / span) * w;
      const y = h - 1 - ((v[i] - lo) / (hi - lo)) * (h - 2);
      d += (pen ? "L" : "M") + x.toFixed(1) + " " + y.toFixed(1);
      pen = true;
    }
    return d;
  }

  // ---- live frames ---------------------------------------------------------------
  // A Dashboard obd_read `values` object -> [{key, name, unit, value}].
  // Multi-sensor arrays become `key#i` ("Name S1"…, answering sensors only);
  // text values (the VIN) are not drive-view channels.
  function dashFrame(values, byKey) {
    const out = [];
    Object.keys(values || {}).forEach((k) => {
      const t = (byKey && byKey[k]) || {};
      const v = values[k];
      if (typeof v === "string") return;
      if (Array.isArray(v)) {
        v.forEach((x, i) => { if (x != null) out.push({ key: k + "#" + i, name: (t.name || k) + " S" + (i + 1), unit: t.unit || "", value: num(x) }); });
      } else out.push({ key: k, name: t.name || k, unit: t.unit || "", value: num(v) });
    });
    return out;
  }
  // Live-log / GPS samples ({paramId, name, value, units, text}) -> frame values.
  function samplesFrame(samples) {
    return (samples || []).filter((s) => s && s.paramId && s.text == null).map((s) => ({ key: String(s.paramId), name: s.name || String(s.paramId), unit: s.units || "", value: num(s.value) }));
  }
  const num = (x) => (typeof x === "number" && isFinite(x) ? x : null);

  // A tiny publish/subscribe bus. Events: {type: "frame", source, tMs, values},
  // {type: "error", source, message, busy}, {type: "stop", source}.
  function makeFeed() {
    const subs = [];
    return {
      on(fn) { subs.push(fn); return () => { const i = subs.indexOf(fn); if (i >= 0) subs.splice(i, 1); }; },
      emit(ev) { subs.slice().forEach((fn) => { try { fn(ev); } catch (e) { /* a broken view must not break polling */ } }); },
      get size() { return subs.length; },
    };
  }

  // ---- replay ------------------------------------------------------------------
  // Index-time logs (no time column) replay at this many samples per second.
  const INDEX_HZ = 5;
  const SPEEDS = [1, 2, 4, 8];
  // A log_replay result -> replay data: shared t, channels with a
  // last-valid-sample index per row, the log's time rate and update interval.
  function replayPrepare(r) {
    const t = (r && r.t) || [];
    const chans = ((r && r.series) || []).map((s) => {
      const v = s.v || [];
      const lv = new Int32Array(t.length);
      let last = -1;
      for (let i = 0; i < t.length; i++) { if (v[i] != null) last = i; lv[i] = last; }
      return { key: s.name, name: s.name, unit: s.unit || "", v, lv };
    });
    const index = r && r.time_label === "samples";
    let dt = r && r.median_dt > 0 ? r.median_dt : 0;
    if (!dt && t.length > 1) dt = (t[t.length - 1] - t[0]) / (t.length - 1);
    return {
      t, chans, byKey: Object.fromEntries(chans.map((c) => [c.key, c])),
      t0: t.length ? t[0] : 0, t1: t.length ? t[t.length - 1] : 0,
      rate: index ? INDEX_HZ : 1, index, dt, stride: (r && r.stride) || 1,
    };
  }
  // Last index with t[i] <= x (-1 when none). t ascending.
  function idxAt(t, x) {
    let lo = 0, hi = t.length - 1, ans = -1;
    while (lo <= hi) {
      const m = (lo + hi) >> 1;
      if (t[m] <= x) { ans = m; lo = m + 1; } else hi = m - 1;
    }
    return ans;
  }
  // The latest valid value of a channel at log time x: { v, at } (nulls when
  // there is none yet).
  function replaySample(d, ch, x) {
    const i = idxAt(d.t, x);
    const j = i < 0 ? -1 : ch.lv[i];
    return j < 0 ? { v: null, at: null } : { v: ch.v[j], at: d.t[j] };
  }
  // Fold every valid sample with x0 < t <= x1 into a peak record (playback
  // between two frames: no spike is skipped at 8x).
  function replayFold(d, ch, peak, x0, x1) {
    const a = idxAt(d.t, x0) + 1, b = idxAt(d.t, x1);
    for (let i = Math.max(0, a); i <= b; i++) if (ch.v[i] != null) peakAdd(peak, ch.v[i], d.t[i]);
    return peak;
  }
  // Points of a channel in [xA, xB], at most maxPts (every k-th, plus the last).
  function replayWindow(d, ch, xA, xB, maxPts) {
    const a = Math.max(0, idxAt(d.t, xA)), b = idxAt(d.t, xB);
    const t = [], v = [];
    if (b < 0) return { t, v };
    const step = Math.max(1, Math.ceil((b - a + 1) / Math.max(2, maxPts || 120)));
    for (let i = a; i <= b; i += step) { t.push(d.t[i]); v.push(ch.v[i]); }
    if (t[t.length - 1] !== d.t[b]) { t.push(d.t[b]); v.push(ch.v[b]); }
    return { t, v };
  }

  // The replay clock: log position `pos` in [t0, t1], `speed` x, `rate` log
  // units per wall second at 1x. Wall times are ms.
  function clockNew(t0, t1, rate) {
    return { t0, t1: Math.max(t0, t1), rate: rate || 1, pos: t0, speed: 1, playing: false, wall: 0 };
  }
  function clockPos(c, wall) {
    if (!c.playing) return c.pos;
    return Math.min(c.t1, c.pos + ((wall - c.wall) / 1000) * c.speed * c.rate);
  }
  function clockPlay(c, wall) {
    if (c.pos >= c.t1) c.pos = c.t0; // play again from the start
    c.wall = wall;
    c.playing = true;
    return c;
  }
  function clockPause(c, wall) {
    c.pos = clockPos(c, wall);
    c.playing = false;
    return c;
  }
  function clockSeek(c, x, wall) {
    c.pos = Math.max(c.t0, Math.min(c.t1, x));
    c.wall = wall;
    return c;
  }
  function clockSpeed(c, s, wall) {
    c.pos = clockPos(c, wall);
    c.wall = wall;
    c.speed = SPEEDS.includes(s) ? s : 1;
    return c;
  }
  // Advance to `wall`: { from, to, ended } (ended = reached t1 and stopped).
  function clockTick(c, wall) {
    const from = c.pos;
    const to = clockPos(c, wall);
    c.pos = to;
    c.wall = wall;
    const ended = c.playing && to >= c.t1;
    if (ended) c.playing = false;
    return { from, to, ended };
  }
  // "m:ss" / "h:mm:ss" from seconds (or "#n" for an index log).
  function fmtClock(s, index) {
    if (typeof s !== "number" || !isFinite(s)) return "—";
    if (index) return "#" + Math.round(s);
    const x = Math.max(0, Math.floor(s));
    const h = Math.floor(x / 3600), m = Math.floor((x % 3600) / 60), r = x % 60;
    const two = (n) => String(n).padStart(2, "0");
    return h ? h + ":" + two(m) + ":" + two(r) : m + ":" + two(r);
  }

  // ---- keyboard ---------------------------------------------------------------
  // F = fullscreen, R = reset peaks, Space = pause / play, Escape = leave
  // fullscreen. Ignored while typing in a field or with a modifier held.
  function keyAction(ev) {
    if (!ev || ev.ctrlKey || ev.metaKey || ev.altKey) return null;
    const el = ev.target || {};
    const tag = String(el.tagName || "").toUpperCase();
    const type = lc(el.type);
    const typing = tag === "TEXTAREA" || tag === "SELECT" || el.isContentEditable ||
      (tag === "INPUT" && !["range", "checkbox", "radio", "button"].includes(type));
    if (typing) return null;
    const k = ev.key;
    if (k === "f" || k === "F") return "fullscreen";
    if (k === "r" || k === "R") return "reset";
    // Space on a focused control activates that control instead.
    if (k === " " || k === "Spacebar") return tag === "BUTTON" || tag === "INPUT" ? null : "pause";
    if (k === "Escape") return "exit";
    return null;
  }

  const OTDrive = {
    normUnit, display, fmtNum, fmtValue, ROLES, roleOf, pickDefault, selectionFor,
    thresholdsFor, level, barRange, frac, STALE_MIN_S, staleAfter, linkState, tileState, fmtAge,
    peakNew, peakAdd, HIST_S, histNew, histPush, sparkPath, dashFrame, samplesFrame, makeFeed,
    INDEX_HZ, SPEEDS, replayPrepare, idxAt, replaySample, replayFold, replayWindow,
    clockNew, clockPos, clockPlay, clockPause, clockSeek, clockSpeed, clockTick, fmtClock, keyAction,
  };
  OTDrive.feed = makeFeed();
  root.OTDrive = OTDrive;
  if (typeof module !== "undefined" && module.exports) module.exports = OTDrive;
})(typeof globalThis !== "undefined" ? globalThis : this);
