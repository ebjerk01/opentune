// Demo fixtures for the Actuator tests panel (feat_actuator.js). Preview only:
// the channels come from the virtual car's definition pack (packs/vcar-demo,
// bundled by build.py) and the module is simulated here — every exchange is
// synthetic, nothing is sent anywhere. Mirrors the gates of
// opentune_adaptation::actuator and desktop feat_actuator.rs: declared
// channel and option, value range, duration cap, per-run ack, SecurityAccess
// (the virtual car's test-only key id "vcar-test"), and free-form DIDs only
// with the second confirmation.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const files = () => ((window.__PREVIEW__ || {}).vcar || {}).files || {};
  const log = window.__PREVIEW_LOG__ || (() => {});
  const hx = (n) => (n & 255).toString(16).toUpperCase().padStart(2, "0");
  const hexs = (b) => b.map(hx).join(" ");
  const parseHex = (s) => (String(s || "").replace(/0x/gi, "").replace(/[^0-9a-f]/gi, "").match(/../g) || []).map((h) => parseInt(h, 16));
  const canId = (v) => (typeof v === "number" ? v : parseInt(String(v).replace(/^0x/i, ""), 16));
  const idText = (n) => n.toString(16).toUpperCase().padStart(3, "0");
  const BYTE = { return_control_to_ecu: 0, reset_to_default: 1, freeze_current_state: 2, short_term_adjustment: 3 };
  const LABEL = { return_control_to_ecu: "returnControlToECU", reset_to_default: "resetToDefault", freeze_current_state: "freezeCurrentState", short_term_adjustment: "shortTermAdjustment" };
  const OPTS = Object.keys(BYTE);
  const PARSE = { return: "return_control_to_ecu", reset: "reset_to_default", freeze: "freeze_current_state", adjust: "short_term_adjustment", sta: "short_term_adjustment" };
  const FREEFORM_MAX = 5000;
  const TP_MS = 2000;

  function readDef(path) {
    const f = files()[path];
    if (!f) throw `read ${path}: No such file or directory (os error 2)`;
    return JSON.parse(JSON.stringify(f));
  }
  function info(def, c) {
    const declared = c.options && c.options.length ? c.options : ["short_term_adjustment"];
    const options = ["return_control_to_ecu", ...declared.filter((o) => o !== "return_control_to_ecu")];
    let value = null;
    if (c.value) {
      const s = c.value.scaling || {};
      const factor = s.factor == null ? 1 : s.factor;
      const offset = s.offset || 0;
      const minRaw = c.min == null ? 0 : c.min;
      const maxRaw = c.max == null ? 2 ** (8 * (c.value.width || 1)) - 1 : c.max;
      const a = minRaw * factor + offset, b = maxRaw * factor + offset;
      value = { unit: s.unit || "", factor, offset, width: c.value.width || 1, min_raw: minRaw, max_raw: maxRaw, min: Math.min(a, b), max: Math.max(a, b), _off: c.value.offset || 0 };
    }
    return {
      name: c.name, did: c.did, note: c.note || "", options, value, default_state: hexs(c.state || []),
      status_unit: (c.status && c.status.scaling && c.status.scaling.unit) || "",
      max_duration_ms: Math.min(c.max_duration_ms || 10000, 60000),
      requires_security: !!def.requires_security, security_level: def.security_level || 1,
    };
  }
  const strip = (i) => { if (i.value) delete i.value._off; return i; };

  F.actuator_list = ({ defPath }) => {
    const def = readDef(defPath);
    const acts = def.channels.filter((c) => c.kind === "actuator").map((c) => strip(info(def, c)));
    return {
      id: def.id, name: def.name, module: def.module, requires_security: !!def.requires_security, security_level: def.security_level || 1,
      target: def.req_id != null ? { tx: idText(canId(def.req_id)), rx: idText(canId(def.resp_id)), assumed: false } : null,
      channels: acts, other_channels: def.channels.length - acts.length, freeform_max_duration_ms: FREEFORM_MAX,
    };
  };

  function planOf(a) {
    const option = PARSE[a.option] || a.option;
    if (!OPTS.includes(option)) throw `unknown IO control option '${a.option}' (return, reset, freeze or adjust)`;
    let def = null, ch, freeform = false;
    if (a.freeformDid) {
      freeform = true;
      const did = parseInt(String(a.freeformDid).replace(/^0x/i, ""), 16);
      if (!(did >= 0 && did <= 0xffff)) throw `free-form DID '${a.freeformDid}': expected 4 hex digits`;
      ch = { name: `Free-form IO 0x${did.toString(16).toUpperCase().padStart(4, "0")}`, did, options: OPTS, value: null, default_state: "", max_duration_ms: FREEFORM_MAX, requires_security: !!a.freeformSecurity, security_level: 1 };
    } else {
      if (!a.defPath) throw "choose an adaptation definition first";
      def = readDef(a.defPath);
      const c = def.channels.find((x) => x.name === a.channel);
      if (!c) throw `unknown channel \`${a.channel}\``;
      if (c.kind !== "actuator") throw `channel \`${a.channel}\` is not a actuator channel`;
      if (a.state) throw "a raw control state is accepted only for a free-form test (alpha Debug); a declared channel uses its declared state and value range";
      ch = info(def, c);
    }
    const bad = (m) => `invalid value 0 for \`${ch.name}\`: ${m}`;
    if (!ch.options.includes(option)) throw bad(`${LABEL[option]} is not declared for this channel (declared: ${ch.options.map((o) => LABEL[o]).join(", ")})`);
    if (a.value != null && option !== "short_term_adjustment") throw bad(`a value applies only to shortTermAdjustment, not ${LABEL[option]}`);
    let record = [], raw = null, value = null, unit = "";
    if (option === "short_term_adjustment") {
      if (freeform) {
        record = parseHex(a.state);
        if (!record.length) throw bad("this channel declares no control state for shortTermAdjustment");
      } else if (ch.value) {
        const v = ch.value;
        if (a.value == null) throw bad(`enter a value (${v.min}..=${v.max} ${v.unit})`);
        raw = Math.round((a.value - v.offset) / v.factor);
        if (!(raw >= v.min_raw && raw <= v.max_raw)) throw `invalid value ${raw} for \`${ch.name}\`: outside the declared range ${v.min}..=${v.max} ${v.unit} (raw ${v.min_raw}..=${v.max_raw})`;
        record = parseHex(ch.default_state);
        for (let i = 0; i < v.width; i++) record[v._off + i] = Math.floor(raw / 256 ** (v.width - 1 - i)) & 255;
        value = raw * v.factor + v.offset;
        unit = v.unit;
      } else {
        if (a.value != null) throw bad("this channel declares no adjustable value");
        record = parseHex(ch.default_state);
        if (!record.length) throw bad("this channel declares no control state for shortTermAdjustment");
      }
    }
    let duration = 0;
    if (option !== "return_control_to_ecu") {
      duration = Number(a.durationMs) || 0;
      if (!duration) throw bad("set a test duration");
      if (duration > ch.max_duration_ms) throw bad(`duration ${duration} ms is above this channel's limit of ${ch.max_duration_ms} ms`);
    }
    const did = [ch.did >> 8, ch.did & 255];
    const sec = freeform ? ch.requires_security : !!(def && def.requires_security);
    const steps = [{ phase: "session", request: "10 03", note: "extended diagnostic session" }];
    if (sec) steps.push({ phase: "security", request: "27 01", note: "SecurityAccess requestSeed" }, { phase: "security", request: "27 02", note: "SecurityAccess sendKey <key from the seed/key handler>" });
    steps.push({ phase: "control", request: hexs([0x2f, ...did, BYTE[option], ...record]), note: LABEL[option] });
    if (duration) {
      steps.push({ phase: "hold", request: "3E 00", note: `tester present every ${TP_MS} ms while the output is held for ${duration} ms (STOP ends it early)` });
      steps.push({ phase: "return", request: hexs([0x2f, ...did, 0]), note: "returnControlToECU — always, also after an error or STOP (retried once)" });
    }
    steps.push({ phase: "leave", request: "10 01", note: "default session (ends any IO control still held)" });
    return {
      channel: ch.name, did: ch.did, option, record: hexs(record), raw, value, unit, duration_ms: duration,
      requires_security: sec, security_level: 1, steps, freeform, _def: def,
    };
  }
  const pub = (p) => { const o = Object.assign({}, p); delete o._def; return o; };
  F.actuator_plan = (a) => pub(planOf(a));

  let cur = null;
  const now = () => Date.now();
  function push(c, phase, request, response) {
    const ex = { t_ms: now() - c.t0, phase, request: hexs(request), response: hexs(response), error: null };
    c.ex.push(ex);
    log(`[actuator ${idText(c.tx)}] -> ${ex.request}  <- ${ex.response}`);
  }
  function finish(c, stopped) {
    const did = [c.plan.did >> 8, c.plan.did & 255];
    const held = Math.min(now() - c.t0, c.plan.duration_ms);
    if (c.plan.duration_ms) push(c, "return", [0x2f, ...did, 0], [0x6f, ...did, 0, 0]);
    push(c, "leave", [0x10, 0x01], [0x50, 0x01, 0x00, 0x32, 0x01, 0xf4]);
    c.running = false;
    c.report = {
      channel: c.plan.channel, did: c.plan.did, option: c.plan.option, record: c.plan.record, duration_ms: c.plan.duration_ms,
      held_ms: held, security_unlocked: c.plan.requires_security, applied: true, control_returned: true, return_attempts: c.plan.duration_ms ? 1 : 0,
      default_session: true, stopped, status: c.status, status_unit: "", response: c.plan.record, outcome: stopped ? "stopped" : "completed", safe: true, exchanges: c.ex,
    };
  }
  function tick(c) {
    const el = now() - c.t0;
    while ((c.tp + 1) * TP_MS < Math.min(el, c.plan.duration_ms)) {
      c.tp++;
      push(c, "hold", [0x3e, 0x00], [0x7e, 0x00]);
    }
    if (el >= c.plan.duration_ms) finish(c, false);
  }
  function status(c) {
    if (!c) return { running: false, exchanges: [], report: null };
    if (c.running) tick(c);
    const el = now() - c.t0;
    return {
      running: c.running, channel: c.plan.channel, did: c.plan.did, option: c.plan.option, duration_ms: c.plan.duration_ms, elapsed_ms: el,
      remaining_ms: c.running ? Math.max(0, c.plan.duration_ms - el) : 0, freeform: c.plan.freeform, target: c.target,
      exchanges: c.ex.slice(), report: c.report, error: null, stop_reason: c.stopReason,
    };
  }

  F.actuator_start = async (a) => {
    if (a.ack !== true) throw "actuator test refused: confirm this run — it drives a real output on the module (ack required)";
    const p = planOf(a);
    if (p.freeform && a.freeformAck !== true) throw "free-form IO control refused: no definition declares this DID, so nothing limits the state you send — tick the second (free-form) confirmation";
    const def = p._def;
    const tx = a.txId ? canId(a.txId) : def && def.req_id != null ? canId(def.req_id) : null;
    const rx = a.rxId ? canId(a.rxId) : def && def.resp_id != null ? canId(def.resp_id) : null;
    if (tx == null) throw "actuator test refused: the target module's CAN address is unknown. Enter the module's request/response ids in the Target fields (engine = 7E0 / 7E8) or add \"req_id\"/\"resp_id\" to the definition — OpenTune will not default a write to the engine ECU.";
    if (p.requires_security) {
      if (!a.seedkey) throw "this module needs SecurityAccess (level 0x01) before IO control: enter a seed/key id (from a plugin, or 'sa2' with your armed SA2 script). OpenTune ships no seed/key algorithm. (Preview: the virtual car's test-only id is vcar-test.)";
      if (a.seedkey !== "vcar-test") throw `no seed/key algorithm '${a.seedkey}' is available in this build — OpenTune ships none; it must come from a plugin`;
    }
    if (cur && cur.running) throw "an actuator test is already running — stop it first";
    const c = { plan: p, t0: now(), tp: 0, ex: [], running: true, report: null, stopReason: null, tx, target: { tx: idText(tx), rx: idText(rx), assumed: false }, status: null };
    cur = c;
    const did = [p.did >> 8, p.did & 255];
    const rec = parseHex(p.record);
    push(c, "session", [0x10, 0x03], [0x50, 0x03, 0x00, 0x32, 0x01, 0xf4]);
    if (p.requires_security) {
      push(c, "security", [0x27, 0x01], [0x67, 0x01, 0x41, 0x52, 0x63, 0x74]);
      push(c, "security", [0x27, 0x02, 0x1a, 0x2b, 0x3c, 0x4d], [0x67, 0x02]);
    }
    push(c, "control", [0x2f, ...did, BYTE[p.option], ...rec], [0x6f, ...did, BYTE[p.option], ...(rec.length ? rec : [0x00])]);
    c.status = rec.length ? rec[0] : 0;
    if (!p.duration_ms) finish(c, false);
    return { started: true, plan: pub(p), target: c.target };
  };
  F.actuator_status = () => status(cur);
  F.actuator_stop = (a) => {
    if (cur && cur.running) {
      cur.stopReason = (a && a.reason) || "STOP pressed";
      tick(cur);
      if (cur.running) finish(cur, true);
    }
    return status(cur);
  };
})();
