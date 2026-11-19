// OpenTune — "Check my car" wizard logic (pure: no DOM, no Tauri).
// The guided wizard (feat_wizard.js) walks a non-expert through Connect →
// Identify → Scan → Explain → Live check → Report over commands the app
// already has. Everything that decides what a step SAYS lives here so the node
// tests can check it: the step state machine (run / done / failed / skipped /
// retry), plain-language errors with the raw detail kept, the battery-voltage
// rating, the scan summary, the explanations (only what the data says — never
// a cause), the live-snapshot rating and the HTML / Markdown report.
"use strict";
(function (root) {
  // ---- steps -----------------------------------------------------------------
  // io: the step talks to the car (it waits for the user to press Run).
  const STEPS = [
    { id: "connect", title: "Connect", io: true },
    { id: "identify", title: "Identify", io: true },
    { id: "scan", title: "Scan", io: true },
    { id: "explain", title: "Explain", io: false },
    { id: "live", title: "Live check", io: true },
    { id: "report", title: "Report", io: false },
  ];
  const stepIndex = (id) => STEPS.findIndex((s) => s.id === id);
  const RESOLVED = ["done", "skipped"];
  // A step whose result feeds another: re-running or skipping it makes the
  // other one stale, so it is redone on the next visit.
  const DEPENDENTS = { scan: ["explain"] };
  function stale(run, id) {
    (DEPENDENTS[id] || []).forEach((d) => {
      const s = run.steps[d];
      if (s.status !== "running") Object.assign(s, { status: "pending", data: null, error: null });
    });
  }

  function newRun(now) {
    const steps = {};
    STEPS.forEach((s) => { steps[s.id] = { status: "pending", data: null, error: null, attempts: 0 }; });
    return { startedAt: now == null ? Date.now() : now, current: 0, steps };
  }
  const cur = (run) => STEPS[run.current].id;
  function begin(run, id) {
    const s = run.steps[id];
    s.status = "running";
    s.error = null;
    s.attempts += 1;
    return run;
  }
  function succeed(run, id, data) {
    const s = run.steps[id];
    s.status = "done";
    s.data = data == null ? {} : data;
    s.error = null;
    stale(run, id);
    return run;
  }
  function failStep(run, id, err) {
    const s = run.steps[id];
    s.status = "failed";
    s.error = errorInfo(err);
    stale(run, id);
    return run;
  }
  // Skipping keeps nothing from an earlier attempt: a skipped step reports nothing.
  function skip(run, id) {
    const s = run.steps[id];
    if (s.status === "running") return run;
    s.status = "skipped";
    s.data = null;
    s.error = null;
    stale(run, id);
    return run;
  }
  const busy = (run) => STEPS.some((s) => run.steps[s.id].status === "running");
  const canNext = (run) => !busy(run) && RESOLVED.includes(run.steps[cur(run)].status) && run.current < STEPS.length - 1;
  function next(run) {
    if (canNext(run)) run.current += 1;
    return run;
  }
  function back(run) {
    if (!busy(run) && run.current > 0) run.current -= 1;
    return run;
  }
  // Any step up to the first unresolved one can be opened from the step bar.
  function reachable(run, i) {
    for (let k = 0; k < i; k++) if (!RESOLVED.includes(run.steps[STEPS[k].id].status)) return false;
    return i >= 0 && i < STEPS.length;
  }
  function goTo(run, i) {
    if (!busy(run) && reachable(run, i)) run.current = i;
    return run;
  }

  // ---- errors ----------------------------------------------------------------
  // Plain language first; the backend's own text stays available as `raw`.
  const ERRORS = [
    [/adapter busy/i, "Another panel is using the adapter right now (for example the Dashboard or a live log). Stop it there, then press Retry."],
    [/no (adapter|interface)|not found|could not open|cannot open|permission denied|no such file|os error 2|access is denied|\bport\b/i,
      "The adapter could not be opened. Check that it is plugged in, that the port in the sidebar Adapter box is right and that no other program is using it."],
    [/unable to connect|bus init|can error|bus ?off|stopped/i,
      "The adapter is there but could not talk to the car. Check that the ignition is on; on a dongle with an HS/MS switch, set it to HS."],
    [/time ?out|timed out|no response|no data|did not answer|silent/i,
      "The car did not answer. Check that the ignition is on and the adapter is firmly in the OBD socket; on a dongle with an HS/MS switch, set it to HS."],
    [/module list|parse|read .*json/i, "The vehicle module list could not be read. Choose another module list, or skip the full module scan."],
    [/cannot carry diagnostics|no diagnostic link/i, "This adapter cannot carry diagnostic requests (it needs CAN or DoIP). Choose another adapter in the sidebar."],
  ];
  function errorInfo(e) {
    const raw = e == null ? "" : typeof e === "string" ? e : e.message ? String(e.message) : (() => { try { return JSON.stringify(e); } catch (_) { return String(e); } })();
    const hit = ERRORS.find(([re]) => re.test(raw));
    return { text: hit ? hit[1] : "Something went wrong while talking to the car. The technical detail below says what the app received.", raw };
  }

  // ---- battery voltage ---------------------------------------------------------
  // OpenTune had no shared voltage rating for reads, so the wizard uses these
  // simple bands (millivolts, measured at the OBD socket by the adapter):
  //   < 11.0 V        bad   — the Auto-debug panel's "low / no car power" limit
  //   11.0 – 12.0 V   warn  — below 12.0 V, the minimum OpenTune's flash policy
  //                           requires before any write
  //   12.0 – 13.0 V   ok    — the usual range with the engine off
  //   13.0 – 15.0 V   ok    — the usual charging range with the engine running
  //   > 15.0 V        warn  — above the usual charging range
  // A rating describes the reading only; it never names a cause.
  const VOLT = { NO_POWER: 11000, MIN_WRITE: 12000, RUNNING: 13000, HIGH: 15000 };
  const volts = (mv) => (mv / 1000).toFixed(1) + " V";
  function voltageVerdict(mv) {
    if (mv == null || !Number.isFinite(Number(mv))) return { level: "unknown", mv: null, text: "This adapter does not report the battery voltage." };
    mv = Number(mv);
    const v = volts(mv);
    if (mv < VOLT.NO_POWER) return { level: "bad", mv, text: `${v} — below ${volts(VOLT.NO_POWER)}, which OpenTune treats as low or no car power. Check that the ignition is on.` };
    if (mv < VOLT.MIN_WRITE) return { level: "warn", mv, text: `${v} — below ${volts(VOLT.MIN_WRITE)}, the minimum OpenTune requires before writing to a control unit. Reading is still possible.` };
    if (mv < VOLT.RUNNING) return { level: "ok", mv, text: `${v} — in the usual range with the engine off (${volts(VOLT.MIN_WRITE)}–${volts(VOLT.RUNNING)}).` };
    if (mv <= VOLT.HIGH) return { level: "ok", mv, text: `${v} — in the usual charging range with the engine running (${volts(VOLT.RUNNING)}–${volts(VOLT.HIGH)}).` };
    return { level: "warn", mv, text: `${v} — above ${volts(VOLT.HIGH)}, higher than the usual charging range.` };
  }

  // ---- connect -------------------------------------------------------------------
  const SIM_DRIVERS = { obdsim: "OBD simulator (built in, offline)" };
  // Interpret probe_adapter's answer. Throws (a plain string) when the step fails.
  function connectResult(probe, conn) {
    const driver = (conn && conn.driver) || "";
    if (SIM_DRIVERS[driver]) {
      return { sim: true, driver, name: SIM_DRIVERS[driver], firmware: null, battery_mv: null, caps: null,
        voltage: { level: "unknown", mv: null, text: "The simulator has no adapter to measure; the live check reads its voltage." } };
    }
    if (!probe) throw "no adapter answered the probe";
    if (probe.busy) throw probe.error || "adapter busy";
    if (!probe.connected) throw probe.error || "no adapter found";
    const caps = probe.capabilities || {};
    if (!(caps.can || caps.doip)) throw `${probe.name || driver} cannot carry diagnostics: no diagnostic link (CAN or DoIP)`;
    return {
      sim: false, driver: probe.driver || driver, name: probe.name || driver, firmware: probe.firmware || null,
      battery_mv: probe.battery_mv == null ? null : probe.battery_mv, caps,
      voltage: caps.battery_voltage === false && probe.battery_mv == null
        ? voltageVerdict(null)
        : voltageVerdict(probe.battery_mv),
    };
  }

  // ---- identify ------------------------------------------------------------------
  function identity(inspect, supported, decoded) {
    const e = (inspect && inspect.ecus && inspect.ecus[0]) || null;
    const info = (e && e.info) || {};
    let vin = info.vin || null;
    let vinSource = vin ? "OBD mode 09" : null;
    if (!vin && supported && supported.vin) { vin = supported.vin; vinSource = "OBD mode 09 / UDS 22 F190"; }
    const errors = Object.assign({}, info.errors || {}, (e && e.errors) || {});
    return {
      vin, vinSource,
      ecuName: info.ecu_name || null,
      calids: info.calids || [],
      cvns: info.cvns || [],
      target: e && e.addr ? `${e.addr.tx}/${e.addr.rx}` : null,
      responded: !!(e && e.responded),
      decoded: decoded ? {
        manufacturer: decoded.manufacturer || null,
        country: decoded.country || null,
        modelYear: decoded.model_year ? (decoded.model_year.resolved || (decoded.model_year.candidates || []).join(" or ") || null) : null,
        flags: decoded.flags || [],
      } : null,
      errors,
    };
  }

  // ---- scan summary ----------------------------------------------------------------
  const LABEL = { engine: "Engine", transmission: "Transmission", other: "Module" };
  const KIND_ORDER = ["stored", "pending", "permanent", "module"];
  const UNKNOWN_DESC = /manufacturer-specific|unknown code|not in the/i;
  function obdSource(e) { return `${LABEL[e.label] || "Module"} (OBD ${e.addr ? e.addr.tx : "?"})`; }
  function scanSummary(scan) {
    scan = scan || {};
    const obd = scan.obd || null;
    const ecus = (obd && obd.ecus) || [];
    const byCode = new Map();
    const add = (d, seen) => {
      const code = String(d.code || "").toUpperCase();
      if (!code) return;
      if (!byCode.has(code)) byCode.set(code, { code, description: d.description || "", system: d.system || code[0], seenAs: [] });
      const x = byCode.get(code);
      if (!x.description && d.description) x.description = d.description;
      if (!x.seenAs.some((s) => s.source === seen.source && s.kind === seen.kind)) x.seenAs.push(seen);
    };
    const milBy = [];
    let milKnown = false;
    const notes = [];
    ecus.forEach((e) => {
      if (!e.responded) return;
      const st = e.readiness && e.readiness.status;
      if (st) { milKnown = true; if (st.mil) milBy.push(obdSource(e)); }
      const d = e.dtcs;
      if (d) {
        ["stored", "pending", "permanent"].forEach((k) => (d[k] || []).forEach((x) => add(x, { source: obdSource(e), kind: k, status: null })));
        Object.keys(d.errors || {}).forEach((k) => notes.push(`${obdSource(e)}: ${k} codes not read (${d.errors[k]})`));
      }
    });
    // Readiness: the engine's view (it owns the emissions monitors).
    const eng = ecus.find((e) => e.responded && e.label === "engine" && e.readiness) || ecus.find((e) => e.responded && e.readiness) || null;
    let readiness = null;
    if (eng) {
      const st = eng.readiness.status;
      const pick = (s) => (st.monitors || []).filter((m) => m.state === s);
      readiness = {
        source: obdSource(eng), ignition: st.ignition || null, dtcCount: st.dtc_count == null ? null : st.dtc_count,
        complete: pick("complete"), incomplete: pick("incomplete"), unsupported: pick("not_supported"),
      };
    }
    const mods = scan.modules || null;
    let modules = null;
    if (mods) {
      const list = mods.modules || [];
      list.forEach((m) => (m.dtcs || []).forEach((x) => add(x, { source: m.name, kind: "module", status: x.status == null ? null : x.status })));
      modules = {
        vehicle: mods.vehicle || null, total: list.length, present: list.filter((m) => m.present).map((m) => m.name),
        absent: list.filter((m) => !m.present).map((m) => m.name),
        errors: list.filter((m) => m.present && m.error).map((m) => ({ name: m.name, error: m.error })),
        identities: list.filter((m) => m.present && m.identity).map((m) => ({ name: m.name, vin: m.identity.vin || null, dids: m.identity.dids || [] })),
      };
    }
    const dtcs = Array.from(byCode.values()).map((x) => Object.assign(x, {
      seenAs: x.seenAs.sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind)),
    })).sort((a, b) => (a.code < b.code ? -1 : a.code > b.code ? 1 : 0));
    return {
      mil: milKnown ? milBy.length > 0 : null, milBy, readiness, dtcs, modules,
      obdModules: ecus.filter((e) => e.responded).map(obdSource), silent: (obd && obd.silent) || [], notes,
    };
  }

  // ---- explain ---------------------------------------------------------------------
  // ISO 14229-1 DTC status bits a reader can act on, in plain words.
  const UDS_STATUS = [
    [0x01, "failing at the moment of the scan"],
    [0x04, "pending (seen in the current or last drive cycle, not confirmed yet)"],
    [0x08, "confirmed and stored"],
    [0x20, "has failed since the codes were last cleared"],
    [0x80, "the module asks for a warning lamp"],
  ];
  function udsStatusText(status) {
    if (status == null || status === "") return [];
    const n = typeof status === "number" ? status : parseInt(String(status).replace(/^0x/i, ""), 16);
    if (!Number.isFinite(n)) return [];
    return UDS_STATUS.filter(([bit]) => n & bit).map(([, t]) => t);
  }
  const KIND_TEXT = {
    stored: "stored (confirmed) over OBD: the fault was detected often enough for the control unit to store it",
    pending: "pending over OBD: the control unit saw it in the current or last drive cycle but has not confirmed it",
    permanent: "permanent over OBD: it stays until the control unit itself sees the system pass again — clearing codes does not remove it",
    module: "stored in the module's fault memory",
  };
  const SYSTEM_TEXT = {
    P: "Powertrain (engine / transmission / emissions)", C: "Chassis (ABS / steering / suspension)",
    B: "Body (airbags / lighting / HVAC / comfort)", U: "Network / communication (CAN bus / modules)",
  };
  const INCOMPLETE_TEXT = "An incomplete monitor means the engine computer has not finished that self-test since its fault memory was last cleared or the battery was disconnected. It is not a fault by itself. Monitors finish on their own during normal driving; some need particular driving conditions. Some emissions inspections require them to be complete.";
  const CODE_TEXT = "A trouble code names the test that failed. It does not by itself say which part is at fault.";

  // Plain-language items for the Explain step. `lookups` maps a code to the
  // DTC database's answer (dtc_lookup); its description wins over the scan's.
  function explain(summary, lookups, opts) {
    const items = [];
    if (!summary) {
      items.push({ kind: "nodata", level: "info", title: "Nothing to explain", text: "The scan was skipped, so there are no results to explain. Go back to the Scan step to run it." });
      return items;
    }
    lookups = lookups || {};
    if (summary.mil === true) {
      items.push({ kind: "mil", level: "bad", title: "Check-engine lamp (MIL): ON",
        text: `${summary.milBy.join(", ")} reports that it is commanding the check-engine lamp on.` });
    } else if (summary.mil === false) {
      items.push({ kind: "mil", level: "ok", title: "Check-engine lamp (MIL): off", text: "No module that answered over OBD reports the check-engine lamp on." });
    } else {
      items.push({ kind: "mil", level: "info", title: "Check-engine lamp (MIL): not read", text: "No module reported its readiness status, so the lamp state is unknown." });
    }

    if (!summary.dtcs.length) {
      const where = summary.obdModules.length + (summary.modules ? summary.modules.present.length : 0);
      items.push({ kind: "dtc", level: where ? "ok" : "info", title: "No trouble codes",
        text: where ? "None of the modules that answered reported a trouble code." : "No module answered, so no trouble codes were read." });
    }
    summary.dtcs.forEach((d) => {
      const db = lookups[d.code] || null;
      const desc = (db && db.description) || d.description || "";
      const unknown = !desc || UNKNOWN_DESC.test(desc);
      const sys = (db && db.system) || SYSTEM_TEXT[String(d.code)[0]] || null;
      const seen = d.seenAs.map((s) => {
        const flags = s.kind === "module" ? udsStatusText(s.status) : [];
        return `${s.source}: ${KIND_TEXT[s.kind] || s.kind}` + (flags.length ? ` — status ${s.status}: ${flags.join(", ")}` : s.status ? ` — status ${s.status}` : "");
      });
      const pendingOnly = d.seenAs.every((s) => s.kind === "pending");
      items.push({
        kind: "dtc", level: pendingOnly ? "warn" : "bad", code: d.code,
        title: `${d.code} — ${unknown ? "no description in OpenTune's code table" : desc}`,
        text: (unknown ? "OpenTune's code table has no description for this code (it is manufacturer-specific or unknown). " : "") + CODE_TEXT,
        system: sys, seen,
      });
    });

    const r = summary.readiness;
    if (r) {
      if (r.incomplete.length) {
        items.push({ kind: "readiness", level: "warn", title: `Readiness: ${r.incomplete.length} monitor(s) not finished`,
          text: INCOMPLETE_TEXT, list: r.incomplete.map((m) => m.name), extra: r.complete.length ? `Finished: ${r.complete.map((m) => m.name).join(", ")}.` : "" });
      } else {
        items.push({ kind: "readiness", level: "ok", title: "Readiness: every supported monitor has finished",
          text: r.complete.length ? `Finished: ${r.complete.map((m) => m.name).join(", ")}.` : "The engine computer reports no supported monitors." });
      }
      if (r.unsupported.length) items.push({ kind: "readiness", level: "info", title: "Monitors this car does not run", text: `Not supported by this engine computer, so they are not checked: ${r.unsupported.map((m) => m.name).join(", ")}.` });
    } else {
      items.push({ kind: "readiness", level: "info", title: "Readiness: not read", text: "No module reported readiness monitors." });
    }

    const m = summary.modules;
    if (m) {
      items.push({ kind: "modules", level: m.absent.length ? "info" : "ok", title: `Modules: ${m.present.length} of ${m.total} answered`,
        text: m.absent.length ? `Did not answer: ${m.absent.join(", ")}. A module that is not fitted, or is on the other CAN bus, does not answer.` : "Every module in the list answered." });
    } else if (opts && opts.modulesNote) {
      items.push({ kind: "modules", level: "info", title: "Full module scan: not run", text: opts.modulesNote });
    }
    summary.notes.forEach((n) => items.push({ kind: "note", level: "info", title: "Not read", text: n }));
    return items;
  }

  // ---- live snapshot ---------------------------------------------------------------
  // Key mode-01 values; ratings use only the channel table's own warn / crit.
  const LIVE_KEYS = ["coolant_temp", "rpm", "speed", "stft_b1", "ltft_b1", "module_voltage"];
  const LIVE_SAMPLES = 6;
  const LIVE_GAP_MS = 500;
  function livePids(table, supportedPids) {
    const sup = supportedPids ? new Set(supportedPids) : null;
    const pids = new Set();
    LIVE_KEYS.forEach((k) => {
      const t = (table || []).find((r) => r.key === k);
      if (t && t.pid != null && !t.derived && (!sup || sup.has(t.pid))) pids.add(t.pid);
    });
    return Array.from(pids).sort((a, b) => a - b);
  }
  const num = (v) => (typeof v === "number" && Number.isFinite(v) ? v : Array.isArray(v) ? v.find((x) => typeof x === "number") ?? null : null);
  function rate(t, v) {
    if (v == null || !t || (t.warn == null && t.crit == null)) return "";
    if (t.crit != null && v >= t.crit) return "crit";
    if (t.warn != null && v >= t.warn) return "warn";
    return "ok";
  }
  const WORSE = { "": 0, ok: 1, warn: 2, crit: 3 };
  function fmtVal(v, unit) {
    if (v == null) return "—";
    const dp = unit === "V" ? 2 : unit === "%" ? 1 : 0;
    return v.toFixed(dp);
  }
  // samples: obd_read answers. Returns rows + battery rating + the idle note.
  function liveSummary(samples, table) {
    samples = samples || [];
    const rows = [];
    LIVE_KEYS.forEach((k) => {
      const t = (table || []).find((r) => r.key === k);
      if (!t) return;
      const vals = samples.map((s) => num(s && s.values ? s.values[k] : null)).filter((v) => v != null);
      const errs = samples.map((s) => s && s.errors && s.errors[k]).filter(Boolean);
      const requested = samples.some((s) => s && s.values && k in s.values);
      if (!requested) return;
      const row = { key: k, name: t.name, unit: t.unit || "", warn: t.warn, crit: t.crit, n: vals.length };
      if (!vals.length) {
        Object.assign(row, { last: null, min: null, max: null, level: "", note: errs[0] ? `Not reported: ${errs[0]}.` : "Not reported by this ECU." });
      } else {
        const level = vals.map((v) => rate(t, v)).reduce((a, b) => (WORSE[b] > WORSE[a] ? b : a), "");
        Object.assign(row, { last: vals[vals.length - 1], min: Math.min(...vals), max: Math.max(...vals), level });
        if (t.warn == null && t.crit == null) row.note = "OpenTune has no threshold for this value, so it is shown without a rating.";
        else if (level === "crit") row.note = `Reached ${fmtVal(row.max, row.unit)} ${row.unit}, at or above the critical mark of ${t.crit} ${row.unit}.`;
        else if (level === "warn") row.note = `Reached ${fmtVal(row.max, row.unit)} ${row.unit}, at or above the warning mark of ${t.warn} ${row.unit}.`;
        else row.note = `Below the warning mark of ${t.warn != null ? t.warn : t.crit} ${row.unit}.`;
      }
      rows.push(row);
    });
    const mvs = samples.map((s) => (s && s.battery_mv != null ? Number(s.battery_mv) : null)).filter((v) => v != null);
    const battery = mvs.length ? voltageVerdict(mvs[mvs.length - 1]) : voltageVerdict(null);
    const rpm = rows.find((r) => r.key === "rpm");
    const speed = rows.find((r) => r.key === "speed");
    let idle;
    if (!rpm || rpm.last == null) idle = { state: "unknown", text: "Engine speed was not reported, so there is no idle reading." };
    else if (rpm.max === 0) idle = { state: "off", text: "The engine was not running (0 rpm). Start the engine and run the live check again for an idle reading." };
    else if (speed && speed.max != null && speed.max > 0) idle = { state: "moving", text: `The car was moving (up to ${fmtVal(speed.max, "km/h")} km/h), so this is not an idle reading.` };
    else idle = { state: "idle", text: `Idle speed ${fmtVal(rpm.min, "rpm")}–${fmtVal(rpm.max, "rpm")} rpm over ${rpm.n} reading(s). OpenTune has no idle threshold, so it is shown without a rating.` };
    return { samples: samples.length, rows, battery, idle };
  }

  // ---- report ----------------------------------------------------------------------
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const mdEsc = (s) => String(s == null ? "" : s).replace(/([\\`*_[\]|<>])/g, "\\$1");
  const STATUS_TEXT = { done: "done", skipped: "skipped", failed: "failed", pending: "not run", running: "running" };
  const DISCLAIMER = "Read-only check: nothing was written to the car and no codes were cleared. OpenTune reports what the car's control units returned; it does not diagnose causes.";
  const pad2 = (n) => String(n).padStart(2, "0");
  function stamp(ms) {
    const d = new Date(ms);
    return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  }

  // One neutral model both renderers walk: [{ title, status, error, blocks }]
  // with blocks = { p } | { kv: [[k, v]] } | { list: [..] } | { table: { head, rows } }.
  function reportModel(run, meta) {
    meta = meta || {};
    const S = run.steps;
    const sec = (id, blocks) => ({ id, title: STEPS[stepIndex(id)].title, status: S[id].status, error: S[id].error, blocks: S[id].status === "done" ? blocks() : [] });
    const c = S.connect.data, i = S.identify.data, sc = S.scan.data, ex = S.explain.data, lv = S.live.data;
    return {
      title: "OpenTune car check",
      when: stamp(meta.now == null ? Date.now() : meta.now),
      app: meta.app || "OpenTune",
      vehicle: (i && i.vin) || null,
      disclaimer: DISCLAIMER,
      sections: [
        sec("connect", () => [{ kv: [["Adapter", `${c.name}${c.firmware ? " " + c.firmware : ""}`], ["Battery", c.voltage.text]] }]),
        sec("identify", () => {
          const kv = [["VIN", i.vin ? `${i.vin} (${i.vinSource})` : "not reported"]];
          if (i.decoded && i.decoded.manufacturer) kv.push(["Manufacturer (from the VIN)", i.decoded.manufacturer]);
          if (i.decoded && i.decoded.modelYear) kv.push(["Model year (from the VIN)", String(i.decoded.modelYear)]);
          if (i.ecuName) kv.push(["Engine computer name", i.ecuName]);
          if (i.calids.length) kv.push(["Calibration IDs", i.calids.join(", ")]);
          if (i.cvns.length) kv.push(["Calibration verification numbers", i.cvns.join(", ")]);
          if (i.target) kv.push(["Asked", i.target]);
          return [{ kv }];
        }),
        sec("scan", () => {
          const sm = sc.summary;
          const b = [{ kv: [
            ["Check-engine lamp (MIL)", sm.mil == null ? "not read" : sm.mil ? `ON (${sm.milBy.join(", ")})` : "off"],
            ["Trouble codes", String(sm.dtcs.length)],
            ["OBD modules that answered", sm.obdModules.join(", ") || "none"],
          ].concat(sm.modules ? [["Module list", `${sm.modules.vehicle || ""} — ${sm.modules.present.length} of ${sm.modules.total} answered`]] : sc.modulesNote ? [["Module list", sc.modulesNote]] : []) }];
          if (sm.dtcs.length) b.push({ table: { head: ["Code", "Description", "Seen as"], rows: sm.dtcs.map((d) => [d.code, d.description, d.seenAs.map((s) => `${s.source}: ${s.kind}${s.status ? " " + s.status : ""}`).join("; ")]) } });
          if (sm.readiness) b.push({ kv: [["Monitors finished", sm.readiness.complete.map((m) => m.name).join(", ") || "none"], ["Monitors not finished", sm.readiness.incomplete.map((m) => m.name).join(", ") || "none"]] });
          (sc.problems || []).forEach((p) => b.push({ p: `Not read: ${p.text} (${p.raw})` }));
          return b;
        }),
        sec("explain", () => (ex.items || []).map((it) => ({ list: [`${it.title}. ${it.text}`].concat(it.system ? [`System: ${it.system}`] : [], it.seen || [], it.list ? [`Not finished: ${it.list.join(", ")}`] : [], it.extra ? [it.extra] : []) }))),
        sec("live", () => [
          { table: { head: ["Value", "Last", "Min – max", "Rating", "Note"], rows: lv.summary.rows.map((r) => [r.name, r.last == null ? "—" : `${fmtVal(r.last, r.unit)} ${r.unit}`, r.last == null ? "—" : `${fmtVal(r.min, r.unit)} – ${fmtVal(r.max, r.unit)} ${r.unit}`, r.level || "—", r.note || ""]) } },
          { kv: [["Battery", lv.summary.battery.text], ["Idle", lv.summary.idle.text], ["Readings", String(lv.summary.samples)]] },
        ]),
      ],
    };
  }

  function reportMarkdown(run, meta) {
    const m = reportModel(run, meta);
    const out = [`# ${m.title}`, "", `${m.when} · ${m.app}${m.vehicle ? " · VIN " + m.vehicle : ""}`, "", `> ${m.disclaimer}`, ""];
    m.sections.forEach((s) => {
      out.push(`## ${s.title} — ${STATUS_TEXT[s.status] || s.status}`, "");
      if (s.status === "failed" && s.error) out.push(`${mdEsc(s.error.text)}`, "", `Detail: \`${String(s.error.raw).replace(/`/g, "'")}\``, "");
      if (s.status === "skipped") out.push("This step was skipped.", "");
      s.blocks.forEach((b) => {
        if (b.p) out.push(mdEsc(b.p), "");
        if (b.kv) { b.kv.forEach(([k, v]) => out.push(`- **${mdEsc(k)}:** ${mdEsc(v)}`)); out.push(""); }
        if (b.list) { b.list.forEach((x) => out.push(`- ${mdEsc(x)}`)); out.push(""); }
        if (b.table) {
          out.push("| " + b.table.head.map(mdEsc).join(" | ") + " |", "|" + b.table.head.map(() => " --- |").join(""));
          b.table.rows.forEach((r) => out.push("| " + r.map(mdEsc).join(" | ") + " |"));
          out.push("");
        }
      });
    });
    return out.join("\n");
  }

  const REPORT_CSS = "body{font:14px/1.45 system-ui,sans-serif;color:#1d232b;background:#fff;max-width:880px;margin:24px auto;padding:0 16px}" +
    "h1{font-size:22px;margin:0 0 4px}h2{font-size:16px;margin:22px 0 8px;border-bottom:1px solid #d9dde3;padding-bottom:4px}" +
    ".meta{color:#5b6573}.note{background:#f3f5f8;border-left:3px solid #8a96a8;padding:8px 12px;margin:12px 0}" +
    ".st{font-size:12px;font-weight:600;padding:1px 8px;border-radius:10px;background:#e9edf2;margin-left:6px}" +
    ".st.done{background:#e3f4e8;color:#1c6b37}.st.failed{background:#fbe5e5;color:#9b1c1c}" +
    "table{border-collapse:collapse;width:100%;margin:6px 0 10px}th,td{border:1px solid #d9dde3;padding:4px 8px;text-align:left;vertical-align:top}" +
    "dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 14px;margin:6px 0 10px}dt{color:#5b6573}dd{margin:0}code{font-size:12px}";

  // The report body (also shown in the panel); `full` wraps it in a document.
  function reportHtml(run, meta, full) {
    const m = reportModel(run, meta);
    let h = `<h1>${esc(m.title)}</h1><div class="meta">${esc(m.when)} · ${esc(m.app)}${m.vehicle ? " · VIN " + esc(m.vehicle) : ""}</div>` +
      `<div class="note">${esc(m.disclaimer)}</div>`;
    m.sections.forEach((s) => {
      h += `<h2>${esc(s.title)}<span class="st ${esc(s.status)}">${esc(STATUS_TEXT[s.status] || s.status)}</span></h2>`;
      if (s.status === "failed" && s.error) h += `<p>${esc(s.error.text)}</p><p class="meta">Detail: <code>${esc(s.error.raw)}</code></p>`;
      if (s.status === "skipped") h += `<p class="meta">This step was skipped.</p>`;
      if (s.status === "pending") h += `<p class="meta">This step was not run.</p>`;
      s.blocks.forEach((b) => {
        if (b.p) h += `<p>${esc(b.p)}</p>`;
        if (b.kv) h += `<dl>${b.kv.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join("")}</dl>`;
        if (b.list) h += `<ul>${b.list.map((x) => `<li>${esc(x)}</li>`).join("")}</ul>`;
        if (b.table) h += `<table><thead><tr>${b.table.head.map((x) => `<th>${esc(x)}</th>`).join("")}</tr></thead><tbody>` +
          b.table.rows.map((r) => `<tr>${r.map((x) => `<td>${esc(x)}</td>`).join("")}</tr>`).join("") + `</tbody></table>`;
      });
    });
    if (!full) return h;
    return `<!doctype html>\n<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">` +
      `<title>${esc(m.title)}${m.vehicle ? " — " + esc(m.vehicle) : ""}</title><style>${REPORT_CSS}</style></head><body>${h}</body></html>\n`;
  }
  function reportFileName(run, ext, now) {
    const d = new Date(now == null ? Date.now() : now);
    const vin = run.steps.identify.data && run.steps.identify.data.vin;
    const tail = vin ? "-" + String(vin).replace(/[^A-Za-z0-9]/g, "").slice(-6) : "";
    return `opentune-car-check-${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}${tail}.${ext}`;
  }

  const OTWizardCore = {
    STEPS, stepIndex, newRun, begin, succeed, failStep, skip, busy, canNext, next, back, reachable, goTo,
    errorInfo, VOLT, voltageVerdict, SIM_DRIVERS, connectResult, identity, scanSummary, udsStatusText, explain,
    INCOMPLETE_TEXT, CODE_TEXT, LIVE_KEYS, LIVE_SAMPLES, LIVE_GAP_MS, livePids, liveSummary,
    DISCLAIMER, reportModel, reportMarkdown, reportHtml, reportFileName,
  };
  root.OTWizardCore = OTWizardCore;
  if (typeof module !== "undefined" && module.exports) module.exports = OTWizardCore;
})(typeof globalThis !== "undefined" ? globalThis : this);
