// Demo fixtures for the Haldex panel. Preview only: a simulated OpenHaldex, the
// Gen4 TP2.0 refusal / simulated read, and a synthetic drivetrain-CAN log.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const t0 = Date.now();

  const MODES = [
    ["stock", null, "Stock (OEM behaviour)"], ["fwd", 0, "FWD (no lock)"], ["5050", 100, "50:50 (100 % lock)"],
    ["6040", 40, "60:40 (40 % lock)"], ["7525", 25, "75:25 (25 % lock)"], ["expert", null, "Expert (unit's own map)"],
  ].map(([mode, lock_pct, label], index) => ({ mode, index, lock_pct, label, can_command: index + 1 }));
  F.haldex_modes = () => MODES;

  const unit = { mode: "stock", commanded: 0 };
  const lock = () => ({ commanded_pct: unit.commanded, engagement_raw: 0x80 + Math.round(unit.commanded * 1.25), mode_index: MODES.find((m) => m.mode === unit.mode).index, mode: unit.mode });
  F.haldex_status = async ({ sim }) => {
    await wait(80);
    if (!sim) throw "Preview: no adapter here — tick Simulated unit.";
    const s = (Date.now() - t0) / 1000;
    return { lock: lock(), slip: { fl: Math.round(2 * Math.sin(s)), fr: Math.round(2 * Math.cos(s)), rl: null, rr: null }, slip_error: null, sim: true, target: { tx: "70F", rx: "779", assumed: false } };
  };
  F.haldex_set_mode = async ({ mode, confirm, sim }) => {
    await wait(200);
    if (!confirm) throw "refusing to change the AWD mode without confirmation";
    if (!sim) throw "Preview: no adapter here — tick Simulated unit.";
    const m = MODES.find((x) => x.mode === mode);
    unit.mode = m.mode; unit.commanded = m.lock_pct || 0;
    return { requested: mode, lock: lock(), mode: unit.mode, sim: true, request: `2E FD A2 ${String(m.index).padStart(2, "0")}`, target: { tx: "70F", rx: "779", assumed: false } };
  };

  F.haldex_gen4_read = async ({ driver, sim }) => {
    await wait(400);
    if (!sim) throw `driver '${driver}' cannot open a VW TP2.0 channel, which the Haldex Gen4 needs (KWP2000 over TP2.0 at logical address 0x0A). No OpenTune driver does yet: the ELM327 drivers refuse TP2.0 and socketcan / slcan / gs_usb only build ISO-TP. Use the simulated unit to see the read, or capture the drivetrain CAN (CAN capture) and decode it below.`;
    const v = (formula, value, unit, label) => ({ formula, a: 0, b: 0, value, unit, label });
    return {
      sim: true, responded: true,
      report: {
        session_error: null, identity_error: null, dtc_error: null, dtcs: [],
        identity: { part_number: "0AY907554C", sw_version: "9999", component: "Haldex SIMULATED", extra_hex: "", generation: "gen4", raw_hex: "" },
        groups: [
          { group: 1, raw_hex: "", error: null, values: [v(0x1a, 45, "°C", "oil temperature"), v(0x1a, 52, "°C", "clutch plate temperature"), v(0x06, 13.8, "V", "supply voltage")] },
          { group: 3, raw_hex: "", error: null, values: [v(0x0e, 4, "bar", "oil pressure"), v(0x5e, 254, "Nm", "estimated torque"), v(0x21, 21, "%", "clutch valve duty"), v(0x18, 0.86, "A", "clutch valve current")] },
          { group: 125, raw_hex: "250001", error: null, values: [{ formula: 0x25, a: 0, b: 1, value: null, unit: "", label: null }] },
          { group: 126, raw_hex: "", error: "negative response 0x31", values: [] },
        ],
      },
    };
  };

  // A synthetic launch: request ramps to full lock, the coupling follows, temperature rises.
  F.haldex_decode_log = async ({ path, bus }) => {
    await wait(300);
    const t = [], req = [], act = [], clutch = [], temp = [], delta = [];
    for (let i = 0; i <= 300; i++) {
      const s = i * 0.05;
      const r = s < 2 ? 0 : s < 4 ? (s - 2) * 50 : s < 9 ? 100 : Math.max(0, 100 - (s - 9) * 40);
      t.push(s); req.push(r);
      act.push(Math.max(0, r - 8 + Math.sin(s * 3) * 2));
      clutch.push(-381 + 3 * (128 + r * 1.25));
      temp.push(31 + s * 0.9);
      delta.push(s > 2 && s < 5 ? 3 * Math.sin((s - 2) * 1.05) : 0.1 * Math.sin(s * 5));
    }
    const ser = (key, name, units, v) => ({ key, name, units, t, v });
    const range = (v) => ({ min: Math.min(...v), max: Math.max(...v), last: v[v.length - 1], samples: v.length });
    return {
      path, bus: bus || "auto", bad_lines: 2, rows_total: 1204,
      summary: {
        frames: 18432, decoded: 1204, per_kind: { lock_request_pq: 301, status_pq: 301, wheel_speeds: 301, engine_pq: 301 },
        lock_request_pct: range(req), lock_actual_pct: range(act), gen4_temp_c: range(temp),
        max_front_rear_delta_kmh: 3, max_wheel_speed_kmh: 42, flags_seen: ["coupling-open"], mqb_fault_reaction: false,
        fault_stored_seen: false, openhaldex_modes: [], openhaldex_commands: [],
      },
      series: [ser("lock_request", "Lock request", "%", req), ser("lock_actual", "Lock actual", "%", act),
        ser("clutch_actual", "Clutch actual (DBC scale)", "Nm/min", clutch), ser("gen4_temp", "Gen4 temperature", "°C", temp),
        ser("wheel_delta", "Front - rear wheel speed", "km/h", delta)],
      rows: [0, 1, 2, 3, 4, 5].map((i) => ({ t: i * 0.0125, id: ["2A0", "2C0", "4A0", "280"][i % 4],
        decoded: [{ frame: "lock_request_pq", raw: 127, center_nm_per_min: 0, lock_pct: 0, rear_pct: 0 },
          { frame: "status_pq", flags: { coupling_open: true }, actual_raw: 128, actual_nm_per_min: 3, engagement_pct: 0, fault_stored: false, byte4_raw: 91, gen4_temp_c: 31, counter: 3 },
          { frame: "wheel_speeds", fl: 0, fr: 0, rl: 0, rr: 0 }, { frame: "engine_pq", rpm: 880, pedal_pct: 0 }][i % 4] })),
    };
  };
  F.haldex_export_csv = async ({ out }) => { await wait(150); return { out, rows: 1505 }; };
})();
