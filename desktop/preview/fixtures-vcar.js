// Demo fixtures for the vehicle panels: Auto-scan, Coding, Adaptation, Service
// functions, Inspection and the Debug console. Preview only: every module answers
// from the `vcar` virtual car's own demo profile (crates/drivers/vcar/profiles/
// demo.json) and definition pack (packs/vcar-demo), both bundled by build.py.
// Every id, part number, DTC and coding byte is the virtual car's fiction.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const P = window.__PREVIEW__ || {};
  const VC = P.vcar || {};
  const FILES = VC.files || {};
  const PROFILE = VC.profile;
  if (!PROFILE) return;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const log = window.__PREVIEW_LOG__ || (() => {});
  const hx2 = (n) => (n & 255).toString(16).toUpperCase().padStart(2, "0");
  const hexs = (bytes) => bytes.map(hx2).join(" ");
  const parseHex = (s) => String(s || "").replace(/0x/gi, "").replace(/[^0-9a-f]/gi, "").match(/../g)?.map((h) => parseInt(h, 16)) || [];
  const canId = (v) => (typeof v === "number" ? v : parseInt(String(v).replace(/^0x/i, ""), 16));
  const idText = (n) => n.toString(16).toUpperCase().padStart(3, "0");
  const ascii = (s) => Array.from(String(s)).map((c) => c.charCodeAt(0));

  // ---- the simulated vehicle (mutable: clears, writes and resets stick) ----
  const car = {};
  PROFILE.modules.forEach((m) => {
    const dids = {};
    (m.dids || []).forEach((d) => { dids[d.did.toUpperCase()] = d.ascii != null ? ascii(d.ascii === "$VIN" ? PROFILE.vin : d.ascii) : parseHex(d.hex); });
    car[canId(m.req_id)] = {
      name: m.name, rx: canId(m.resp_id), dids, secured: !!m.require_security_for_write,
      dtcs: (m.dtcs || []).map((d) => ({ code: d.code, status: parseInt(d.status, 16) })),
      routines: m.routines || [], io: m.io || [],
    };
  });
  const engineTx = 0x7e0;

  // Generic SAE descriptions for the codes the virtual car stores (from the
  // app's bundled table, crates/opentune-dtc/data/dtc.tsv).
  const DTC = {
    P0101: "Mass or Volume Air Flow Circuit Range/Performance", P0420: "Catalyst System Efficiency Below Threshold Bank 1",
    P0171: "System Too Lean Bank 1", P0715: 'Input/Turbine Speed Sensor "A" Circuit', P0730: "Incorrect Gear Ratio",
    U0100: "Lost Communication With ECM/PCM “A”", U0155: "Lost Communication With Instrument Panel Cluster (IPC) Control Module",
    U0121: "Lost Communication With Anti-Lock Brake System (ABS) Control Module",
  };
  const describe = (code) => DTC[code] || "Manufacturer-specific code (no generic description)";
  window.__PREVIEW_DTC__ = DTC; // shared with the Utilities DTC lookup

  function readFile(path) {
    const f = FILES[path];
    if (!f) throw `read ${path}: No such file or directory (os error 2)`;
    return JSON.parse(JSON.stringify(f));
  }
  // The module a request goes to: explicit Target ids win, else the file's own.
  function target(args, def, what) {
    const tx = args.txId ? canId(args.txId) : def && def.req_id != null ? canId(def.req_id) : null;
    const rx = args.rxId ? canId(args.rxId) : def && def.resp_id != null ? canId(def.resp_id) : null;
    if (tx == null) {
      if (what) throw `${what} refused: no target module — set the Target ids (tx/rx) or use a definition that names req_id/resp_id`;
      return { tx: engineTx, rx: 0x7e8, assumed: true };
    }
    return { tx, rx: rx == null ? tx + 8 : rx, assumed: false };
  }
  const tjson = (t) => ({ tx: idText(t.tx), rx: idText(t.rx), assumed: t.assumed });
  function moduleAt(t, timeoutMs) {
    const m = car[t.tx];
    if (!m) throw `no response from ${idText(t.tx)}/${idText(t.rx)} within ${timeoutMs || 1000} ms (ISO-TP timeout) — module not fitted, or wrong bus`;
    return m;
  }

  // ---- Auto-scan ------------------------------------------------------------
  const ID_DIDS = [["F187", "part_number"], ["F189", "sw_version"], ["F191", "hw_number"], ["F195", "sw_version_number"]];
  const dtcJson = (d) => ({ code: d.code, description: describe(d.code), system: d.code[0], status: d.status == null ? null : "0x" + hx2(d.status) });
  function scanOne(m) {
    const t = { tx: m.req_id, rx: m.resp_id };
    // OBD requests go out on the functional id 7DF; the engine answers them.
    const mod = m.protocol === "obd" ? car[engineTx] : car[t.tx];
    const base = { name: m.name, target: { tx: idText(t.tx), rx: idText(t.rx), assumed: false } };
    if (!mod) return Object.assign(base, { present: false, identity: null, dtcs: [], dtcCount: 0, error: null });
    let identity, dtcs;
    if (m.protocol === "obd") {
      identity = { vin: PROFILE.vin, dids: [] };
      dtcs = mod.dtcs.filter((d) => d.status & 0x08 && d.code[0] === "P").map((d) => Object.assign(dtcJson(d), { status: null }));
    } else {
      const vin = mod.dids.F190 ? String.fromCharCode(...mod.dids.F190) : null;
      identity = {
        vin,
        dids: ID_DIDS.filter(([did]) => mod.dids[did]).map(([did, label]) => ({
          did: "0x" + did, label, text: String.fromCharCode(...mod.dids[did]), raw: hexs(mod.dids[did]),
        })),
      };
      dtcs = mod.dtcs.map(dtcJson);
    }
    return Object.assign(base, { present: true, identity, dtcs, dtcCount: dtcs.length, error: null });
  }
  F.autoscan_run = async ({ modulesPath }) => {
    const list = readFile(modulesPath);
    const mods = (list.modules || []).map((m) => Object.assign({}, m, { req_id: canId(m.req_id), resp_id: canId(m.resp_id) }));
    await wait(90 * mods.length);
    const out = mods.map(scanOne);
    return {
      vehicle: list.vehicle, moduleCount: out.length, presentCount: out.filter((m) => m.present).length,
      dtcTotal: out.reduce((a, m) => a + m.dtcCount, 0), modules: out,
    };
  };
  F.autoscan_clear = async ({ modulesPath }) => {
    const list = readFile(modulesPath);
    await wait(400);
    const results = (list.modules || []).map((m) => {
      const tx = canId(m.req_id);
      const mod = m.protocol === "obd" ? car[engineTx] : car[tx];
      const t = { tx: idText(tx), rx: idText(canId(m.resp_id)), assumed: false };
      if (!mod) return { name: m.name, cleared: false, error: null, target: t };
      mod.dtcs = [];
      log(`[${t.tx}] -> ${m.protocol === "obd" ? "04" : "14 FF FF FF"}  <- ${m.protocol === "obd" ? "44" : "54"}`);
      return { name: m.name, cleared: true, error: null, target: t };
    });
    return { vehicle: list.vehicle, clearedCount: results.filter((r) => r.cleared).length, results };
  };

  // ---- Coding -----------------------------------------------------------------
  const codingDid = (def) => ((def.addressing && def.addressing.dids) || [0x600])[0];
  const didKey = (did) => did.toString(16).toUpperCase().padStart(4, "0");
  function decodeParam(p, bytes) {
    const raw = bytes[p.byte] == null ? 0 : bytes[p.byte];
    const value = (raw >> (p.bit || 0)) & ((1 << p.len) - 1);
    const opt = (p.options || []).find((o) => o.value === value);
    const label = p.kind === "bool" ? (value ? "on" : "off") : opt ? opt.label : p.kind === "enum" ? `unknown (${value})` : String(value);
    return { name: p.name, value, label, kind: p.kind, note: p.note || "", options: (p.options || []).map((o) => ({ value: o.value, label: o.label })) };
  }
  function codingContext(args, what) {
    const def = readFile(args.defPath);
    const t = target(args, def, what);
    const mod = moduleAt(t, args.timeoutMs);
    const did = codingDid(def);
    const bytes = mod.dids[didKey(did)];
    if (!bytes) throw `negative response 0x31 (requestOutOfRange) reading DID 0x${didKey(did)}`;
    return { def, t, mod, did, bytes };
  }
  F.coding_read = async (args) => {
    await wait(250);
    const { def, t, bytes } = codingContext(args);
    return {
      def: { id: def.id, name: def.name, module: def.module, requiresSecurity: !!def.requires_security },
      params: def.params.map((p) => decodeParam(p, bytes)), target: tjson(t),
    };
  };
  function planEdit(c, name, value) {
    const p = c.def.params.find((x) => x.name === name);
    if (!p) throw `unknown parameter '${name}' (not in ${c.def.id})`;
    if (value >= 2 ** p.len) throw `value ${value} does not fit '${name}' (${p.len} bit${p.len > 1 ? "s" : ""})`;
    if (p.kind === "enum" && p.options && p.options.length && !p.options.some((o) => o.value === value)) {
      throw `value ${value} is not an option of '${name}' (${p.options.map((o) => o.value + "=" + o.label).join(", ")})`;
    }
    const nb = c.bytes.slice();
    const mask = ((1 << p.len) - 1) << (p.bit || 0);
    nb[p.byte] = (nb[p.byte] & ~mask) | ((value << (p.bit || 0)) & mask);
    const old = decodeParam(p, c.bytes), neu = decodeParam(p, nb);
    const changed = nb[p.byte] !== c.bytes[p.byte];
    return {
      nb, changed,
      report: {
        securityRequired: !!c.def.requires_security,
        backup: [{ did: "0x" + didKey(c.did), hex: hexs(c.bytes) }],
        written: [],
        byteChanges: changed ? [{ block: 0, did: "0x" + didKey(c.did), byte: p.byte, old: hx2(c.bytes[p.byte]), new: hx2(nb[p.byte]) }] : [],
        paramChanges: changed ? [{ name: p.name, old: { value: old.value, label: old.label }, new: { value: neu.value, label: neu.label } }] : [],
        target: tjson(c.t),
      },
    };
  }
  F.coding_set_preview = async (args) => {
    await wait(200);
    const e = planEdit(codingContext(args), args.name, args.value);
    return Object.assign(e.report, { outcome: { kind: e.changed ? "planned" : "no_change" } });
  };
  F.coding_write = async (args) => {
    if (args.ack !== true) throw "coding write refused: preview the change and confirm it (ack required)";
    await wait(400);
    const c = codingContext(args, "coding write");
    const e = planEdit(c, args.name, args.value);
    const did = "0x" + didKey(c.did);
    if (!e.changed) return Object.assign(e.report, { outcome: { kind: "no_change" } });
    if (c.mod.secured) {
      log(`[${idText(c.t.tx)}] -> 2E ${didKey(c.did).replace(/(..)(..)/, "$1 $2")} ${hexs(e.nb)}  <- 7F 2E 33`);
      return Object.assign(e.report, { outcome: { kind: "write_error", did, message: "negative response 0x33 (securityAccessDenied) — this module needs SecurityAccess (0x27) before a write" } });
    }
    c.mod.dids[didKey(c.did)] = e.nb;
    log(`[${idText(c.t.tx)}] -> 2E ${didKey(c.did).replace(/(..)(..)/, "$1 $2")} ${hexs(e.nb)}  <- 6E ${didKey(c.did).replace(/(..)(..)/, "$1 $2")}`);
    return Object.assign(e.report, { written: [did], outcome: { kind: "verified" } });
  };

  // ---- Adaptation ---------------------------------------------------------
  const KIND_KEYS = { value: ["did", "unit", "min", "max"], routine: ["id"], actuator: ["did"] };
  F.adaptation_list = async ({ defPath }) => {
    const def = readFile(defPath);
    const t = def.req_id != null ? { tx: canId(def.req_id), rx: canId(def.resp_id), assumed: false } : null;
    return {
      id: def.id, name: def.name, module: def.module, requires_security: !!def.requires_security, target: t && tjson(t),
      channels: def.channels.map((c) => {
        const o = { name: c.name, note: c.note || "", kind: c.kind };
        const src = Object.assign({}, c, { unit: c.field && c.field.scaling ? c.field.scaling.unit : "" });
        KIND_KEYS[c.kind].forEach((k) => (o[k] = src[k] == null ? null : src[k]));
        return o;
      }),
    };
  };
  function channel(args, kind, what) {
    const def = readFile(args.defPath);
    const ch = def.channels.find((c) => c.name === args.channel);
    if (!ch) throw `channel '${args.channel}' not in ${def.id}`;
    if (kind && ch.kind !== kind) throw `channel '${ch.name}' is a ${ch.kind} channel, not ${kind}`;
    const t = target(args, def, what);
    return { def, ch, t, mod: moduleAt(t, args.timeoutMs) };
  }
  const field = (bytes, f) => bytes.slice(f.offset, f.offset + f.width).reduce((a, b) => a * 256 + b, 0);
  F.adaptation_read = async (args) => {
    await wait(150);
    const { def, ch, t, mod } = channel(args, "value");
    const bytes = mod.dids[didKey(ch.did)];
    if (!bytes) throw `negative response 0x31 (requestOutOfRange) reading DID 0x${didKey(ch.did)}`;
    const raw = field(bytes, ch.field);
    const s = ch.field.scaling;
    return { channel: ch.name, raw, engineering: Math.round((raw * s.factor + s.offset) * 1000) / 1000, unit: s.unit, requires_security: !!def.requires_security, target: tjson(t) };
  };
  F.adaptation_routine = async (args) => {
    if (args.ack !== true) throw "routine refused: tick the confirmation (ack required)";
    await wait(350);
    const { def, ch, t } = channel(args, "routine", "routine");
    const sub = { start: 1, stop: 2, request_results: 3 }[args.op] || 1;
    const extra = parseHex(args.extra);
    const data = sub === 2 ? [] : [0x00];
    log(`[${idText(t.tx)}] -> 31 ${hx2(sub)} ${hx2(ch.id >> 8)} ${hx2(ch.id)}${extra.length ? " " + hexs(extra) : ""}  <- 71 ${hx2(sub)} ${hx2(ch.id >> 8)} ${hx2(ch.id)}${data.length ? " " + hexs(data) : ""}`);
    return { channel: ch.name, data: hexs(data), value: data.length ? 0 : null, unit: (ch.result && ch.result.scaling && ch.result.scaling.unit) || "", security_required: !!def.requires_security, target: tjson(t) };
  };
  F.adaptation_actuator = async (args) => {
    if (args.ack !== true) throw "actuator test refused: tick the confirmation (ack required)";
    await wait(600);
    const { def, ch, t } = channel(args, "actuator", "actuator test");
    const state = args.state ? parseHex(args.state) : ch.state || [1];
    const did = [ch.did >> 8, ch.did & 255];
    log(`[${idText(t.tx)}] -> 2F ${hexs(did)} 03 ${hexs(state)}  <- 6F ${hexs(did)} 03 ${hexs(state)}`);
    log(`[${idText(t.tx)}] -> 2F ${hexs(did)} 00  <- 6F ${hexs(did)} 00`);
    return {
      channel: ch.name, did: ch.did, applied: true, control_returned: true, response: `6F ${hexs(did)} 03 ${hexs(state)}`,
      status: state[0], status_unit: "", outcome: "completed", message: "", safe: true, security_required: !!def.requires_security, target: tjson(t),
    };
  };

  // ---- Service functions ----------------------------------------------------
  const w16 = (n) => [n >> 8, n & 255];
  function stepInfo(s) {
    switch (s.op) {
      case "diagnostic_session": return { op: `diagnostic_session(0x${hx2(s.subfunction)})`, req: [0x10, s.subfunction], resp: [0x50, s.subfunction, 0x00, 0x32, 0x01, 0xf4] };
      case "security_access": return { op: `security_access(level 0x${hx2(s.level)})`, req: [0x27, s.level], resp: null };
      case "routine_control": {
        const ct = { 1: "start", 2: "stop", 3: "results" }[s.control_type] || "?";
        return { op: `routine_control(${ct} 0x${didKey(s.routine_id)})`, req: [0x31, s.control_type, ...w16(s.routine_id), ...(s.data || [])], resp: [0x71, s.control_type, ...w16(s.routine_id), 0x00] };
      }
      case "io_control": return { op: `io_control(0x${didKey(s.did)} ctrl 0x${hx2(s.control)})`, req: [0x2f, ...w16(s.did), s.control, ...(s.data || [])], resp: [0x6f, ...w16(s.did), s.control, ...(s.data || [])] };
      case "write_data_by_identifier": return { op: `write_did(0x${didKey(s.did)})`, req: [0x2e, ...w16(s.did), ...(s.data || [])], resp: [0x6e, ...w16(s.did)] };
      case "read_data_by_identifier": return { op: `read_did(0x${didKey(s.did)})`, req: [0x22, ...w16(s.did)], resp: null };
      case "tester_present": return { op: "tester_present", req: [0x3e, 0x00], resp: [0x7e, 0x00] };
      case "wait": return { op: `wait(${s.ms} ms)`, req: null, resp: null };
      case "expect_positive": return { op: "expect_positive", req: null, resp: null };
      default: return { op: s.op, req: null, resp: null };
    }
  }
  const needsKey = (sc) => !!sc.requires_security || sc.steps.some((s) => s.op === "security_access");
  const missingKeyNote = (sc) => sc.seedkey
    ? `This script needs UDS SecurityAccess with seed/key algorithm '${sc.seedkey}', which is not available in this build. OpenTune ships no OEM seed/key provider; a plugin must supply it. The script can be planned here but not executed.`
    : "This script needs UDS SecurityAccess but names no seedkey algorithm. OpenTune ships no OEM seed/key provider, so it can be planned here but not executed.";
  F.service_plan = async ({ scriptPath }) => {
    const sc = readFile(scriptPath);
    const needs = needsKey(sc);
    const steps = sc.steps.map((s, i) => {
      const x = stepInfo(s);
      return { index: i, op: x.op, status: "planned", detail: needs ? "plan only — not sent (SecurityAccess required)" : "dry run — not sent", request: !needs && x.req ? hexs(x.req) : null, response: null };
    });
    return {
      mode: "plan", script_id: sc.id, name: sc.name, description: sc.description, dry_run: true, outcome: { kind: "planned" },
      security_unlocked: false, requires_security: !!sc.requires_security, needs_key_provider: needs, seedkey: sc.seedkey || null,
      seedkey_available: false, steps, note: needs ? missingKeyNote(sc) : undefined,
    };
  };
  F.service_run = async (args) => {
    if (args.ack !== true) throw "refusing to run: tick the confirmation box — Run sends real UDS writes/actions to the module";
    const sc = readFile(args.scriptPath);
    const t = target(args, sc, "service run");
    if (needsKey(sc)) throw missingKeyNote(sc);
    const mod = moduleAt(t, args.timeoutMs);
    const steps = [];
    let last = null;
    for (let i = 0; i < sc.steps.length; i++) {
      const s = sc.steps[i];
      const x = stepInfo(s);
      let resp = x.resp, detail = "ok";
      if (s.op === "wait") { await wait(Math.min(s.ms, 800)); detail = `waited ${s.ms} ms`; }
      else if (s.op === "expect_positive") { detail = last && last[0] !== 0x7f ? "previous response positive" : "previous response was not positive"; }
      else if (s.op === "read_data_by_identifier") {
        const v = mod.dids[didKey(s.did)];
        resp = v ? [0x62, ...w16(s.did), ...v] : [0x7f, 0x22, 0x31];
        detail = v ? `DID 0x${didKey(s.did)} = ${hexs(v)}` : "requestOutOfRange";
      } else { await wait(120); }
      if (resp) { last = resp; log(`[${idText(t.tx)}] -> ${hexs(x.req)}  <- ${hexs(resp)}`); }
      steps.push({ index: i, op: x.op, status: "ok", detail, request: x.req ? hexs(x.req) : null, response: resp ? hexs(resp) : null });
    }
    return { mode: "run", script_id: sc.id, name: sc.name, description: sc.description, dry_run: false, outcome: { kind: "completed" }, security_unlocked: false, steps, target: tjson(t) };
  };

  // ---- Debug console: raw requests to the virtual car, and the UDS sim ------
  function answer(mod, req, secured) {
    const [sid, a, b] = req;
    if (sid === 0x10) return [0x50, a, 0x00, 0x32, 0x01, 0xf4];
    if (sid === 0x3e) return [0x7e, 0x00];
    if (sid === 0x22) { const v = mod.dids[hx2(a) + hx2(b)]; return v ? [0x62, a, b, ...v] : [0x7f, 0x22, 0x31]; }
    if (sid === 0x2e) {
      if (secured) return [0x7f, 0x2e, 0x33];
      const k = hx2(a) + hx2(b);
      if (!mod.dids[k]) return [0x7f, 0x2e, 0x31];
      mod.dids[k] = req.slice(3);
      return [0x6e, a, b];
    }
    if (sid === 0x19 && a === 0x02) return [0x59, 0x02, 0xff, ...mod.dtcs.flatMap((d) => [...dtcBytes(d.code), d.status])];
    if (sid === 0x14) { mod.dtcs = []; return [0x54]; }
    if (sid === 0x27 && a % 2 === 1) return [0x67, a, 0x11, 0x22, 0x33, 0x44];
    return [0x7f, sid || 0, 0x11];
  }
  function dtcBytes(code) {
    const sys = "PCBU".indexOf(code[0]);
    const n = parseInt(code.slice(1), 16);
    return [(sys << 6) | (n >> 8), n & 255, 0x00];
  }
  F.debug_raw_request = async (args) => {
    const req = parseHex(args.request);
    if (!req.length) throw "enter a request as hex bytes, e.g. 22 F1 90";
    const t = target(args, null);
    await wait(60);
    const resp = answer(moduleAt(t, args.timeoutMs), req, false);
    log(`[${idText(t.tx)}] -> ${hexs(req)}  <- ${hexs(resp)}  (4 ms)`);
    return hexs(resp);
  };
  F.debug_simulate = ({ dids, requests, requireSecurityWrite }) => {
    const mod = { dids: {}, dtcs: [] };
    (dids || []).forEach((d) => {
      const [id, h] = String(d).split("=");
      if (h == null) throw `expected ID=HEX, got '${d}'`;
      mod.dids[id.trim().replace(/^0x/i, "").toUpperCase().padStart(4, "0")] = parseHex(h);
    });
    return (requests || []).map((r) => {
      const req = parseHex(r);
      const resp = answer(mod, req, !!requireSecurityWrite);
      log(`sim -> ${hexs(req)}  <- ${hexs(resp)}`);
      return hexs(resp);
    });
  };

  // ---- Inspection (OBD readiness / DTCs / freeze frame / mode 09) ----------
  const M = (key, name, continuous, state) => ({ key, name, continuous, state });
  const SPARK = (cat, egr) => [
    M("misfire", "Misfire", true, "complete"), M("fuel_system", "Fuel system", true, "complete"), M("components", "Comprehensive components", true, "complete"),
    M("catalyst", "Catalyst", false, cat), M("heated_catalyst", "Heated catalyst", false, "not_supported"), M("evap_system", "Evaporative system", false, "complete"),
    M("secondary_air", "Secondary air system", false, "not_supported"), M("ac_refrigerant", "A/C refrigerant", false, "not_supported"),
    M("o2_sensor", "Oxygen sensor", false, "complete"), M("o2_sensor_heater", "Oxygen sensor heater", false, "complete"), M("egr_vvt", "EGR / VVT system", false, egr),
  ];
  const entry = (code) => ({ code, description: describe(code), system: code[0] });
  const FV = (pid, key, name, unit, value, raw) => ({ pid, key, name, unit, value, error: null, raw });
  function inspectEngine(sections) {
    const mod = car[engineTx];
    const confirmed = mod.dtcs.filter((d) => d.status & 0x08 && d.code[0] === "P").map((d) => d.code);
    const pending = mod.dtcs.filter((d) => d.status & 0x04 && d.code[0] === "P").map((d) => d.code);
    const o = { responded: true, errors: {} };
    if (sections.readiness) {
      o.readiness = {
        status: { mil: confirmed.length > 0, dtc_count: confirmed.length, ignition: "spark", monitors: SPARK("incomplete", "complete") },
        this_cycle: { ignition: "spark", monitors: SPARK("incomplete", "incomplete") }, this_cycle_error: null,
      };
    }
    if (sections.dtcs) o.dtcs = { stored: confirmed.map(entry), pending: pending.map(entry), permanent: confirmed.filter((c) => c === "P0420").map(entry), errors: {} };
    if (sections.freeze) {
      o.freeze_frame = confirmed.length ? {
        frame: 0, stored: true, dtc: entry(confirmed[0]), pids: [0x04, 0x05, 0x06, 0x07, 0x0b, 0x0c, 0x0d, 0x0f, 0x10, 0x11],
        values: [
          FV(0x04, "engine_load", "Calculated engine load", "%", 23.5, "3C"), FV(0x05, "coolant_temp", "Coolant temperature", "°C", 88, "80"),
          FV(0x06, "stft_b1", "Short-term fuel trim bank 1", "%", 7.8, "8A"), FV(0x07, "ltft_b1", "Long-term fuel trim bank 1", "%", 12.5, "90"),
          FV(0x0b, "intake_map", "Intake manifold pressure", "kPa", 34, "22"), FV(0x0c, "rpm", "Engine speed", "rpm", 812, "0C B0"),
          FV(0x0d, "speed", "Vehicle speed", "km/h", 0, "00"), FV(0x0f, "intake_air_temp", "Intake air temperature", "°C", 21, "3D"),
          FV(0x10, "maf", "Air mass flow", "g/s", 3.2, "01 40"), FV(0x11, "throttle_pos", "Throttle position", "%", 14.5, "25"),
        ],
        errors: {},
      } : { frame: 0, stored: false, dtc: null, pids: [], values: [], errors: {} };
    }
    if (sections.info) {
      o.info = { supported: [0x02, 0x04, 0x06, 0x0a], vin: PROFILE.vin, calids: ["VCE-SW-1001-CAL0", "VCE-SW-1001-CAL1"], cvns: ["0000A5A5", "00005A5A"], ecu_name: "ECM-VirtualEngine", errors: {} };
    }
    return o;
  }
  function inspectGearbox(sections) {
    const mod = car[0x7e1];
    const confirmed = mod.dtcs.filter((d) => d.status & 0x08 && d.code[0] === "P").map((d) => d.code);
    const o = { responded: true, errors: {} };
    const mons = SPARK("not_supported", "not_supported").map((m) => Object.assign(m, { state: m.key === "components" ? "complete" : "not_supported" }));
    if (sections.readiness) o.readiness = { status: { mil: false, dtc_count: confirmed.length, ignition: "spark", monitors: mons }, this_cycle: null, this_cycle_error: "PID 41 not supported by this module" };
    if (sections.dtcs) o.dtcs = { stored: confirmed.map(entry), pending: [], permanent: [], errors: { permanent: "mode 0A not supported (negative response 0x11)" } };
    if (sections.freeze) o.freeze_frame = { frame: 0, stored: false, dtc: null, pids: [], values: [], errors: {} };
    if (sections.info) o.info = { supported: [0x04, 0x06, 0x0a], vin: null, calids: ["VCT-SW-2003-CAL0"], cvns: ["0000C3C3"], ecu_name: "TCM-VirtualGearbx", errors: {} };
    return o;
  }
  F.obd_inspect = async (args) => {
    const list = args.sections && args.sections.length ? args.sections : ["readiness", "dtcs", "freeze", "info"];
    const sections = Object.fromEntries(list.map((k) => [k, true]));
    await wait(args.allEcus ? 900 : 400);
    const one = (tx, assumed) => {
      const rx = tx + 8;
      const label = tx === 0x7e0 ? "engine" : tx === 0x7e1 ? "transmission" : "other";
      const body = tx === 0x7e0 ? inspectEngine(sections) : tx === 0x7e1 ? inspectGearbox(sections) : { responded: false, errors: { module: `no response from ${idText(tx)} — no OBD module at this address` } };
      return Object.assign(body, { addr: { tx: idText(tx), rx: idText(rx), assumed }, label });
    };
    if (args.txId) return { t_ms: Date.now(), ecus: [one(canId(args.txId), false)], silent: [] };
    if (!args.allEcus) return { t_ms: Date.now(), ecus: [one(0x7e0, true)], silent: [] };
    return { t_ms: Date.now(), ecus: [one(0x7e0, false), one(0x7e1, false)], silent: ["7E2", "7E3", "7E4", "7E5", "7E6", "7E7"] };
  };
})();
