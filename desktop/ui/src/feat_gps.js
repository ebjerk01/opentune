// OpenTune — optional GPS source card, shared by the Live log and Dashboard
// panels (both Bench tier). One app-wide source: a u-blox style receiver on a
// serial / USB CDC-ACM port, or a recorded NMEA file replayed in a loop.
// Backend: gps_ports, gps_open, gps_fix, gps_close (feat_gps.rs). While a
// source is open one timer polls gps_fix and every mounted card repaints;
// the panels hand in their vehicle speed (setVehicleKmh) for the speed error
// and append samples() to their CSV rows.
"use strict";
(function () {
  const T = () => window.__TAURI__ || {};
  const invoke = (c, a) => T().core.invoke(c, a);
  const { esc, fmtLatLon } = window.OTUtil;
  const POLL_MS = 500;
  const FRESH_MS = 3000; // a vehicle speed / poll older than this is not used
  const FIX = ["none", "GPS", "DGPS", "PPS", "RTK fixed", "RTK float", "estimated", "manual", "simulated"];

  const G = {
    open: false, source: "", kind: null, last: null, at: 0,
    timer: null, inFlight: false, vehicle: null, views: [], error: "",
  };

  const n = (v, dp) => (typeof v === "number" && isFinite(v) ? v.toFixed(dp) : "—");
  // [key, label, unit, value(res) -> text, needsGoodFix]
  const TILES = [
    ["pos", "Position", "", (r) => fmtLatLon(r.fix.lat, true) + "  " + fmtLatLon(r.fix.lon, false), true],
    ["speed", "GPS speed", "km/h", (r) => n(r.fix.speedKmh, 1), true],
    ["alt", "Altitude", "m", (r) => n(r.fix.altM, 1), true],
    ["course", "Course", "°", (r) => n(r.fix.courseDeg, 0), true],
    ["sats", "Satellites", "", (r) => (r.fix.sats == null ? "—" : String(r.fix.sats)), false],
    ["fix", "Fix", "", (r) => FIX[r.fix.fix] || String(r.fix.fix), false],
    ["hdop", "HDOP", "", (r) => n(r.fix.hdop, 1), false],
    ["err", "Speed error", "km/h", (r) => {
      const s = (r.samples || []).find((x) => x.paramId === "speed_error_kmh");
      return s && typeof s.value === "number" ? (s.value >= 0 ? "+" : "") + s.value.toFixed(1) : "—";
    }, true],
  ];

  function msg(e) { return typeof e === "string" ? e : (e && e.message) || String(e); }

  // ---- one card per host panel ----------------------------------------------
  function mount(container, opts) {
    if (!container) return null;
    const id = (opts && opts.id) || "gps" + G.views.length;
    const el = document.createElement("div");
    el.className = "card gps-card";
    el.innerHTML = `
      <h2>GPS <span class="small muted">optional · NMEA receiver or recorded file</span></h2>
      <div class="row">
        <input class="inp mono" data-r="src" list="${esc(id)}Ports" placeholder="/dev/ttyACM0, COM5 or an NMEA file" style="min-width:14rem" />
        <datalist id="${esc(id)}Ports"></datalist>
        <button class="btn sm" type="button" data-a="ports" title="List serial ports (u-blox receivers first)">Ports</button>
        <button class="btn sm" type="button" data-a="file" title="Replay a recorded NMEA file">File…</button>
        <button class="btn primary sm" type="button" data-a="open">Open</button>
        <button class="btn sm" type="button" data-a="close" disabled>Close</button>
        <span class="pill neutral" data-r="pill">off</span>
      </div>
      <p class="small muted" data-r="note">A u-blox style receiver shows up as a serial port (USB CDC-ACM). Optional: logging works without it.</p>
      <div class="dash-grid gps-grid" data-r="tiles" hidden></div>`;
    container.appendChild(el);
    const q = (sel) => el.querySelector(sel);
    const view = { el, id, speedError: !!(opts && opts.speedError), q };
    q('[data-a="ports"]').addEventListener("click", () => ports(view));
    q('[data-a="file"]').addEventListener("click", () => pickFile(view));
    q('[data-a="open"]').addEventListener("click", () => open(view));
    q('[data-a="close"]').addEventListener("click", () => close());
    G.views.push(view);
    paint(view);
    return view;
  }

  async function ports(view) {
    try {
      const list = (await invoke("gps_ports")) || [];
      const dl = view.el.querySelector("datalist");
      if (dl) dl.innerHTML = list.map((p) => `<option value="${esc(p.name)}">${esc((p.product || "") + (p.likely_gnss ? " (GNSS)" : ""))}</option>`).join("");
      const best = list.find((p) => p.likely_gnss) || list[0];
      const src = view.q('[data-r="src"]');
      if (best && !src.value) src.value = best.name;
      note(view, list.length ? `${list.length} serial port(s)` + (best && best.likely_gnss ? `; ${best.name} looks like a GNSS receiver` : "") : "no serial ports found");
    } catch (e) {
      note(view, "ports: " + msg(e));
    }
  }

  async function pickFile(view) {
    try {
      const r = await T().dialog.open({ title: "Recorded NMEA file", multiple: false, directory: false, filters: [{ name: "NMEA log", extensions: ["nmea", "txt", "log", "ubx"] }] });
      const p = Array.isArray(r) ? r[0] : r;
      if (p) view.q('[data-r="src"]').value = p;
    } catch (e) { /* dialog dismissed */ }
  }

  async function open(view) {
    const source = String(view.q('[data-r="src"]').value || "").trim();
    if (!source) return note(view, "choose a port or an NMEA file first");
    G.error = "";
    try {
      const r = await invoke("gps_open", { source, baud: null });
      G.open = true;
      G.source = r.source || source;
      G.kind = r.kind;
      G.last = null;
      if (!G.timer) G.timer = setInterval(poll, POLL_MS);
      poll();
    } catch (e) {
      G.error = "open failed: " + msg(e);
    }
    G.views.forEach(paint);
  }

  async function close() {
    if (G.timer) clearInterval(G.timer);
    G.timer = null;
    G.open = false;
    G.last = null;
    try { await invoke("gps_close"); } catch (e) { /* already closed */ }
    G.views.forEach(paint);
  }

  async function poll() {
    if (G.inFlight || !G.open) return;
    G.inFlight = true;
    try {
      const v = G.vehicle && Date.now() - G.vehicle.at < FRESH_MS ? G.vehicle.kmh : null;
      const r = await invoke("gps_fix", { vehicleKmh: v });
      if (r && r.open === false) {
        G.open = false;
        G.last = null;
      } else {
        G.last = r;
        G.at = Date.now();
      }
      G.error = "";
    } catch (e) {
      G.error = "gps: " + msg(e);
    } finally {
      G.inFlight = false;
    }
    G.views.forEach(paint);
  }

  function note(view, text) {
    const p = view.q('[data-r="note"]');
    if (p) p.textContent = text;
  }

  function paint(view) {
    const q = view.q;
    q('[data-a="open"]').disabled = false;
    q('[data-a="close"]').disabled = !G.open;
    const pill = q('[data-r="pill"]');
    const tiles = q('[data-r="tiles"]');
    const r = G.last;
    if (!G.open) {
      pill.className = "pill " + (G.error ? "bad" : "neutral");
      pill.textContent = G.error ? "error" : "off";
      if (G.error) note(view, G.error);
      tiles.hidden = true;
      return;
    }
    if (G.open && view.q('[data-r="src"]').value === "") view.q('[data-r="src"]').value = G.source;
    pill.className = "pill " + (!r ? "accent" : r.good ? "ok" : r.stale ? "bad" : "accent");
    pill.textContent = !r ? "opening…" : r.good ? "fix" : r.stale ? "stale" : "no fix";
    const what = G.kind === "replay" ? "replaying " : "";
    const s = r && r.stats;
    const counters = s ? ` · ${s.rmc + s.gga + s.vtg} sentence(s)` + (s.bad_checksum + s.partial + s.malformed + s.noise + s.overlong ? `, ${s.bad_checksum + s.partial + s.malformed + s.noise + s.overlong} rejected` : "") : "";
    const utc = r && r.fix && r.fix.utc ? ` · UTC ${r.fix.utc}` : "";
    note(view, G.error || `${what}${G.source}${utc}${counters}${r && r.note ? " — " + r.note : ""}`);
    tiles.hidden = false;
    if (!tiles.children.length) {
      tiles.innerHTML = TILES.filter((t) => t[0] !== "err" || view.speedError).map(([k, label, unit]) =>
        `<div class="tile${k === "pos" ? " text" : ""}" data-k="${k}"><div class="t-label"><span>${esc(label)}</span><i class="t-dot"></i></div>` +
        `<div><span class="t-val">—</span><span class="t-unit">${esc(unit)}</span></div></div>`).join("");
    }
    TILES.forEach(([k, , , fmt, needsGood]) => {
      const d = tiles.querySelector(`[data-k="${k}"]`);
      if (!d) return;
      const on = !!r && !r.stale && (!needsGood || r.good);
      const text = on ? fmt(r) : "—";
      const val = d.querySelector(".t-val");
      if (val) val.textContent = text;
      d.className = "tile" + (k === "pos" ? " text" : "") + (on && text !== "—" ? " live" : "");
    });
  }

  window.OTGps = {
    mount,
    isOpen: () => G.open,
    // The host panel's latest vehicle speed in km/h (null = none).
    setVehicleKmh(kmh) {
      G.vehicle = typeof kmh === "number" && isFinite(kmh) ? { kmh, at: Date.now() } : null;
    },
    // The gps_* (and speed_error_kmh) samples of the latest poll, for CSV rows;
    // empty when no source is open or the last poll is too old.
    samples() {
      if (!G.open || !G.last || Date.now() - G.at > FRESH_MS) return [];
      return G.last.samples || [];
    },
  };
})();
