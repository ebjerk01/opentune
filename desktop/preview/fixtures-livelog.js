// Demo fixtures for the Live log panel (polling and Fast 0x2C sessions).
// Preview only: a synthetic UDS logger definition and made-up engine values.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const t0 = Date.now();
  const secs = () => (Date.now() - t0) / 1000;
  const j = (a) => (Math.random() - 0.5) * 2 * a;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  const PARAMS = [
    { id: "rpm", name: "Engine speed", units: "rpm", did: 0x1000 },
    { id: "ect", name: "Coolant temperature", units: "°C", did: 0x1001 },
    { id: "map", name: "Boost pressure (abs)", units: "kPa", did: 0x1002 },
    { id: "iat", name: "Intake air temperature", units: "°C", did: 0x1003 },
    { id: "inj_qty", name: "Injection quantity", units: "mg/str", did: 0x1004 },
    { id: "obd_load", name: "Calculated load (OBD 01 04)", units: "%", did: null },
  ];
  F.log_params = ({ defPath }) => ({
    path: defPath, version: "preview", count: PARAMS.length,
    params: PARAMS.map(({ id, name, units }) => ({ id, name, units })),
  });

  function sample(p) {
    const t = secs();
    const load = Math.max(0, Math.sin(t / 3)) ** 2;
    const v = {
      rpm: 820 + load * 2400 + j(15), ect: Math.min(90, 70 + t / 5), map: 99 + load * 120 + j(1),
      iat: 24 + load * 9, inj_qty: 4 + load * 38 + j(0.5), obd_load: 14 + load * 70,
    }[p.id];
    return { paramId: p.id, name: p.name, raw: Math.round(v), value: Math.round(v * 10) / 10, units: p.units, note: "" };
  }
  const pick = (ids) => (ids && ids.length ? PARAMS.filter((p) => ids.includes(p.id)) : PARAMS);
  F.log_poll_once = async ({ ids }) => {
    await wait(40 * pick(ids).length);
    return { ts: Date.now(), count: pick(ids).length, samples: pick(ids).map(sample), target: { tx: "7E0", rx: "7E8", assumed: true } };
  };

  // Fast session: the UDS DIDs pack into one dynamic DID, the OBD PID is polled.
  let sess = null;
  F.log_start = async ({ ids, dynamicDid, maxRecord }) => {
    await wait(500);
    const chans = pick(ids);
    const packed = chans.filter((p) => p.did != null);
    const did = parseInt(String(dynamicDid || "F200").replace(/^0x/i, ""), 16) || 0xf200;
    const rec = packed.length * 2;
    const fits = rec <= (maxRecord || 32);
    const status = fits && packed.length
      ? { mode: "fast-2c", reason: null, dynamic_dids: [{ did, record_len: rec, channels: packed.map((p) => p.id) }],
          polled: chans.filter((p) => p.did == null).map((p) => p.id), notes: [], redefines: 0, ticks: 0, setup_requests: 2 + packed.length, tick_requests: 0 }
      : { mode: "polling", reason: `0x2C defineByIdentifier of 0x${did.toString(16).toUpperCase()} refused: NRC 0x31 (requestOutOfRange)`,
          dynamic_dids: [], polled: chans.map((p) => p.id), notes: [], redefines: 0, ticks: 0, setup_requests: packed.length + 1, tick_requests: 0 };
    sess = { chans, status, t: Date.now(), perTick: status.mode === "fast-2c" ? 1 + status.polled.length : chans.length };
    return { status, channels: chans.length, target: { tx: "7E0", rx: "7E8", assumed: true } };
  };
  F.log_tick = async () => {
    if (!sess) throw "no live log session — press Start";
    await wait(sess.status.mode === "fast-2c" ? 15 : 12 * sess.chans.length);
    sess.status.ticks += 1;
    sess.status.tick_requests += sess.perTick;
    const el = (Date.now() - sess.t) / 1000;
    return {
      ts: Date.now(), count: sess.chans.length, samples: sess.chans.map(sample), status: sess.status, running: true, ended: null,
      rate: { ticks: sess.status.ticks, elapsed_s: el, samples_per_s: el ? sess.status.ticks / el : 0, requests_per_sample: sess.perTick, channels: sess.chans.length },
      target: { tx: "7E0", rx: "7E8", assumed: true },
    };
  };
  F.log_stop = () => {
    const s = sess;
    sess = null;
    return { running: false, status: s ? s.status : null, ended: null, rate: null, target: null };
  };
})();
