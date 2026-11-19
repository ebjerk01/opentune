// Demo fixtures for the "Check my car" wizard (feat_wizard.js). Preview only.
// The wizard uses existing commands, which the other fixture files already
// answer; this file only makes its Live check meaningful in the preview:
// - the demo engine also reports bank-1 fuel trims (PIDs 06 / 07);
// - a snapshot that does not ask for manifold pressure (PID 0B) — the
//   wizard's live check, never the Dashboard, which always polls MAP for
//   boost — sees the car parked with the engine idling, so the idle reading
//   has something to show. Synthetic values only.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F || !F.obd_read || !F.obd_supported) return;
  const j = (a) => (Math.random() - 0.5) * 2 * a;
  const TRIMS = [0x06, 0x07];

  const supported = F.obd_supported;
  F.obd_supported = async (args) => {
    const r = await supported(args);
    return Object.assign({}, r, { pids: Array.from(new Set((r.pids || []).concat(TRIMS))).sort((a, b) => a - b) });
  };

  const read = F.obd_read;
  F.obd_read = async (args) => {
    const r = await read(args);
    const pids = (args && args.pids) || [];
    const values = Object.assign({}, r.values, { stft_b1: 1.6 + j(1.2), ltft_b1: 3.1 });
    if (pids.length && !pids.includes(0x0b)) {
      Object.assign(values, { rpm: 815 + j(15), speed: 0, engine_load: 19 + j(1), coolant_temp: 88, module_voltage: 14.1 + j(0.03) });
    }
    return Object.assign({}, r, { values });
  };
})();
