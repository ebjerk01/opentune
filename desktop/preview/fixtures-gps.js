// Demo fixtures for the optional GPS card (Live log / Dashboard).
// Preview only: a synthetic receiver driving laps of a made-up 1 km circle in
// open water (North Sea, 56.5 N 3.0 E), with a short fix outage every 45 s.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const LAT0 = 56.5, LON0 = 3.0, R_M = 1000;
  const CHANNELS = [
    ["gps_lat", "GPS latitude", "deg"], ["gps_lon", "GPS longitude", "deg"], ["gps_alt_m", "GPS altitude (MSL)", "m"],
    ["gps_speed_kmh", "GPS speed over ground", "km/h"], ["gps_course_deg", "GPS course over ground", "deg"],
    ["gps_sats", "GPS satellites used", "count"], ["gps_fix", "GPS fix quality (0 none, 1 GPS, 2 DGPS, 4/5 RTK, 6 estimated)", ""],
    ["gps_hdop", "GPS HDOP", ""],
  ];
  let src = null; // { source, kind, t0, sentences }

  F.gps_ports = () => [
    { name: "/dev/ttyACM0", vid: 0x1546, pid: 0x01a9, product: "GNSS receiver", likely_gnss: true },
    { name: "/dev/ttyUSB0", vid: 0x1a86, pid: 0x7523, product: "USB Serial", likely_gnss: false },
  ];
  F.gps_open = ({ source }) => {
    const s = String(source || "").trim();
    if (!s) throw "choose a GPS port or an NMEA file";
    src = { source: s, kind: /\.(nmea|txt|log)$/i.test(s) ? "replay" : "serial", t0: Date.now(), sentences: 0 };
    return { open: true, source: s, kind: src.kind, describe: (src.kind === "replay" ? "NMEA replay " : "serial ") + s };
  };
  F.gps_close = () => { const was = !!src; src = null; return { open: false, closed: was }; };

  F.gps_fix = ({ vehicleKmh }) => {
    if (!src) return { open: false };
    const t = (Date.now() - src.t0) / 1000;
    src.sentences += 3;
    const outage = t % 45 > 40;
    const kmh = 70 + 30 * Math.sin(t / 9);
    // Angle travelled round the circle so far (approximate: mean speed 70 km/h).
    const ang = ((70 / 3.6) * t) / R_M;
    const lat = LAT0 + (R_M * Math.sin(ang)) / 111320;
    const lon = LON0 + (R_M * (1 - Math.cos(ang))) / (111320 * Math.cos((LAT0 * Math.PI) / 180));
    const course = ((90 - (ang * 180) / Math.PI) % 360 + 360) % 360;
    const good = !outage;
    const fix = {
      lat: good ? lat : null, lon: good ? lon : null, altM: good ? 4.2 + Math.sin(t / 20) : null,
      speedKmh: good ? kmh : null, courseDeg: good ? course : null,
      sats: good ? 9 + Math.round(2 * Math.sin(t / 13)) : 3, fix: good ? 1 : 0, hdop: good ? 0.9 : 99.99,
      utc: "2026-06-15 " + new Date(Date.UTC(2026, 5, 15, 10, 0, Math.floor(t))).toISOString().slice(11, 19) + ".000",
      talker: "GN",
    };
    const note = good ? "" : "no fix (GGA quality 0, RMC status V, mode N)";
    const vals = [fix.lat, fix.lon, fix.altM, fix.speedKmh, fix.courseDeg, fix.sats, fix.fix, fix.hdop];
    const samples = CHANNELS.map(([paramId, name, units], i) => ({
      paramId, name, raw: null, value: vals[i] == null ? null : vals[i], units, note: vals[i] == null || !good ? note : "", text: null,
    }));
    if (typeof vehicleKmh === "number") {
      const ok = good && kmh >= 5;
      samples.push({
        paramId: "speed_error_kmh", name: "Speed error (vehicle − GPS)", raw: null,
        value: ok ? vehicleKmh - kmh : null, units: "km/h", note: ok ? "" : good ? "GPS speed below 5 km/h" : note, text: null,
      });
    }
    return {
      open: true, source: src.source, kind: src.kind, good, stale: false, note,
      ageMs: good ? 0 : Math.round(((t % 45) - 40) * 1000), silentMs: 0, ended: null, fix,
      stats: { lines: src.sentences + 2, rmc: src.sentences / 3, gga: src.sentences / 3, vtg: src.sentences / 3, ignored: 2, bad_checksum: 0, partial: 1, malformed: 0, noise: 1, overlong: 0 },
      speedErrorMinKmh: 5, samples,
    };
  };
})();
