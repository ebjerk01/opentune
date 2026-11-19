// Demo fixtures for the Live log panel (polling, Fast 0x2C and Subaru SSM
// sessions, and a reconnect after a dropped link). Preview only: a synthetic
// UDS logger definition, a synthetic SSM module and made-up engine values.
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
    // Text channels (identification DIDs): decoded text, no value.
    { id: "uds_f190_vin", name: "VIN (F190)", units: "text", did: 0xf190, text: "WVWZZZ00000000000" },
    { id: "uds_f199_programming_date", name: "Programming date (F199)", units: "bcd", did: 0xf199, text: "241118" },
  ];
  F.log_params = ({ defPath }) => ({
    path: defPath, version: "preview", count: PARAMS.length,
    params: PARAMS.map(({ id, name, units }) => ({ id, name, units })),
  });

  function sample(p) {
    if (p.text) return { paramId: p.id, name: p.name, raw: null, value: null, units: p.units, note: "", text: p.text };
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
    return { ts: Date.now(), count: pick(ids).length, samples: pick(ids).map(sample), target: { tx: "7E0", rx: "7E8", assumed: true }, link: { state: "up", error: null } };
  };

  // Subaru SSM: the synthetic module of opentune_core::protocol::ssmsim
  // (made-up SSM ID / ROM ID, capability bits for three channels).
  const SSM = [
    { id: "P_ECT", name: "Coolant Temperature", units: "C" },
    { id: "P_RPM", name: "Engine Speed", units: "rpm" },
    { id: "P_VSS", name: "Vehicle Speed", units: "km/h" },
  ];
  const SSM_NOTES = [
    "1 parameter(s) for the other module (target): P_TCU",
    "1 parameter(s) not supported by this module (capability bit clear): P_NOPE",
    "1 parameter(s) derived from other parameters (not evaluated): P_DERIVED",
  ];
  function ssmSample(p) {
    const t = secs();
    const load = Math.max(0, Math.sin(t / 3)) ** 2;
    const v = { P_ECT: Math.min(88, 60 + t / 4), P_RPM: 750 + load * 2600 + j(10), P_VSS: load * 90 }[p.id];
    return { paramId: p.id, name: p.name, raw: Math.round(v), value: Math.round(v * 10) / 10, units: p.units, note: "" };
  }
  const link0 = (reconnect) => ({ state: "up", reconnect, dead_ticks: 0, dead_after: 5, attempt: 0, retry_in_ms: null, error: null, drops: 0, reconnects: 0, downtime_ms: 0, last_event: null });
  const marker = (state, note) => ({ paramId: "link", name: "Link status", raw: null, value: null, units: "", note, text: state });

  // Fast session: the UDS DIDs pack into one dynamic DID, the OBD PID is polled.
  let sess = null;
  F.log_start = async (a) => {
    const { ids, dynamicDid, maxRecord, transport, ssmTarget, reconnect } = a || {};
    await wait(500);
    if (transport === "ssm" || transport === "ssm-can") {
      const tcu = ssmTarget === "tcu";
      const chans = tcu ? [{ id: "P_TCU", name: "TCU Channel", units: "C" }] : SSM;
      const status = { mode: "ssm-a8", reason: null, dynamic_dids: [], polled: [], notes: [], redefines: 0, ticks: 0, setup_requests: 0, tick_requests: 0 };
      const notLogged = (ids || []).filter((id) => !chans.some((c) => c.id === id));
      const ssm = {
        target: tcu ? "tcu" : "ecu", wire: transport === "ssm" ? "k-line" : "can", max_packet_len: 80,
        ssm_id: "A05E01", rom_id: "5A5A000001", capability_bytes: 4, logged: chans.length, notes: SSM_NOTES, not_logged: notLogged,
      };
      sess = { chans, status, t: Date.now(), perTick: 1, ssm: true, link: link0(!!reconnect), n: 0 };
      return { status, channels: chans.length, params: chans, ssm, target: { tx: tcu ? "018" : "010", rx: "7E8", assumed: false, ssm: ssm.target, wire: ssm.wire }, link: sess.link };
    }
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
    sess = { chans, status, t: Date.now(), perTick: status.mode === "fast-2c" ? 1 + status.polled.length : chans.length, link: link0(!!reconnect), n: 0 };
    return { status, channels: chans.length, params: chans.map(({ id, name, units }) => ({ id, name, units })), ssm: null, target: { tx: "7E0", rx: "7E8", assumed: true }, link: sess.link };
  };

  // With Reconnect on, the preview drops the link every 40 ticks: five silent
  // ticks, lost, one failed re-open, reconnected (markers on the way).
  function linkStep(s) {
    const L = s.link;
    if (!L.reconnect) return { live: true, markers: [] };
    const k = s.n % 40;
    if (k >= 30 && k < 35) {
      L.state = "silent"; L.dead_ticks = k - 29; L.error = "timed out after 1000 ms";
      if (L.dead_ticks < 5) return { live: false, markers: [] };
      L.state = "lost"; L.drops += 1; L.attempt = 0; L.last_event = "link lost after 5 dead round(s): timed out after 1000 ms";
      return { live: false, markers: [marker("lost", L.last_event)] };
    }
    if (k === 35) {
      L.state = "reconnecting"; L.attempt = 1; L.retry_in_ms = 1000; L.error = "open: no such device";
      L.last_event = "reopen attempt 1 failed: open: no such device; retrying in 1.0 s";
      return { live: false, markers: [marker("reopen-failed", L.last_event)] };
    }
    if (k === 36) {
      Object.assign(L, { state: "up", dead_ticks: 0, attempt: 0, retry_in_ms: null, error: null, reconnects: L.reconnects + 1, downtime_ms: L.downtime_ms + 3400 });
      L.last_event = "reconnected after 3.4 s (attempt 2)";
      return { live: true, markers: [marker("reconnected", L.last_event)] };
    }
    if (k > 36 || k < 30) { L.state = "up"; L.dead_ticks = 0; L.error = null; }
    return { live: true, markers: [] };
  }
  F.log_tick = async () => {
    if (!sess) throw "no live log session — press Start";
    await wait(sess.status.mode === "fast-2c" ? 15 : 12 * sess.chans.length);
    sess.n += 1;
    const step = linkStep(sess);
    sess.status.ticks += 1;
    sess.status.tick_requests += sess.perTick;
    const el = (Date.now() - sess.t) / 1000;
    const down = sess.link.state === "lost" || sess.link.state === "reconnecting";
    const chans = down ? [] : step.live ? sess.chans.map(sess.ssm ? ssmSample : sample)
      : sess.chans.map((p) => ({ paramId: p.id, name: p.name, raw: null, value: null, units: p.units, note: "timed out after 1000 ms" }));
    const samples = step.markers[0] && step.markers[0].text === "reconnected" ? step.markers.concat(chans) : chans.concat(step.markers);
    return {
      ts: Date.now(), count: samples.length, samples, status: sess.status, running: true, ended: null,
      rate: { ticks: sess.status.ticks, elapsed_s: el, samples_per_s: el ? sess.status.ticks / el : 0, requests_per_sample: sess.perTick, channels: sess.chans.length },
      target: { tx: "7E0", rx: "7E8", assumed: true }, link: { ...sess.link },
    };
  };
  F.log_stop = () => {
    const s = sess;
    sess = null;
    return { running: false, status: s ? s.status : null, ended: null, rate: null, target: null, link: s ? s.link : null, events: [], ssm: null };
  };
})();
