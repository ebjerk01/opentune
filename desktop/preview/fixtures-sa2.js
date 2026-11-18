// Demo fixtures for the SA2 script panel: a small JS stand-in for the app's SA2
// interpreter (opentune-sa2) and container extraction, enough to exercise the
// UI. Preview only. The demo script and container are SYNTHETIC, written for
// OpenTune's tests (the same bytes as opentune-container's synthetic ODX
// fixture) — never an OEM script.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const hexs = (b) => b.map((x) => (x & 255).toString(16).toUpperCase().padStart(2, "0")).join(" ");
  const h8 = (v) => (v >>> 0).toString(16).toUpperCase().padStart(8, "0");
  const h4 = (v) => v.toString(16).toUpperCase().padStart(4, "0");
  const MAX_SCRIPT = 4096;
  const MAX_STEPS = 1000000;

  // loop 3 { rsl; bcc +5; eor 0x5A5A5A5A }; add 0x100; finish
  const SYNTH = [0x68, 0x03, 0x81, 0x4a, 0x05, 0x87, 0x5a, 0x5a, 0x5a, 0x5a, 0x49, 0x93, 0x00, 0x00, 0x01, 0x00, 0x4c];
  const FILES = {
    "demo/synthetic.sa2": { form: "assembler", bytes: SYNTH },
  };
  const CONTAINERS = {
    "demo/synthetic-sa2.odx": { kind: "Odx", scripts: [
      { field: "FW-SIGNATURE", session: "SES_SYNTHETIC", bytes: SYNTH },
      { field: "FW-CHECKSUM", session: "SES_SYNTHETIC", bytes: [0xde, 0xad, 0xbe, 0xef] },
    ] },
  };

  const OPS = { 0x81: ["rsl", 0], 0x82: ["rsr", 0], 0x93: ["add", 4], 0x84: ["sub", 4], 0x87: ["eor", 4], 0x68: ["loop", 1], 0x49: ["next", 0], 0x4a: ["bcc", 1], 0x6b: ["bra", 1], 0x4c: ["finish", 0] };
  const BY_NAME = Object.fromEntries(Object.entries(OPS).map(([k, v]) => [v[0], [Number(k), v[1]]]));

  function decode(b, at) {
    const op = OPS[b[at]];
    if (!op) throw `unknown opcode 0x${(b[at] & 255).toString(16).toUpperCase().padStart(2, "0")} at offset ${at}`;
    if (at + 1 + op[1] > b.length) throw `truncated ${op[0]} at offset ${at}`;
    const v = op[1] === 4 ? ((b[at + 1] << 24) | (b[at + 2] << 16) | (b[at + 3] << 8) | b[at + 4]) >>> 0 : b[at + 1];
    const text = op[1] === 4 ? `${op[0]} 0x${h8(v)}` : op[0] === "loop" ? `loop ${v}` : op[1] ? `${op[0]} +${v}` : op[0];
    return { name: op[0], len: 1 + op[1], v, text, target: op[0] === "bcc" || op[0] === "bra" ? at + 2 + v : null };
  }

  // The VM: one 32-bit register (seed in, key out), a carry flag, a loop stack.
  function exec(b, seed, budget, onStep) {
    let reg = seed >>> 0, carry = false, pc = 0, steps = 0;
    const loops = [];
    while (pc < b.length) {
      if (steps >= budget) throw `execution exceeded the step budget of ${budget} instructions`;
      steps++;
      const at = pc, op = decode(b, at);
      pc = at + op.len;
      let done = false;
      switch (op.name) {
        case "rsl": carry = (reg & 0x80000000) !== 0; reg = ((reg << 1) | (reg >>> 31)) >>> 0; break;
        case "rsr": carry = (reg & 1) !== 0; reg = ((reg >>> 1) | (reg << 31)) >>> 0; break;
        case "add": { const s = reg + op.v; carry = s > 0xffffffff; reg = s >>> 0; break; }
        case "sub": carry = op.v > reg; reg = (reg - op.v) >>> 0; break;
        case "eor": reg = (reg ^ op.v) >>> 0; break;
        case "loop":
          if (!op.v) throw `loop at offset ${at} has iteration count 0 (ambiguous; refused)`;
          if (loops.length >= 64) throw "loop nesting deeper than 64";
          loops.push([pc, op.v - 1]);
          break;
        case "next": {
          const top = loops[loops.length - 1];
          if (!top) throw `loop end (0x49) at offset ${at} without an open loop`;
          if (top[1] > 0) { top[1]--; pc = top[0]; } else loops.pop();
          break;
        }
        case "bcc": if (!carry) pc = op.target; break;
        case "bra": pc = op.target; break;
        default: done = true;
      }
      if (onStep) onStep({ offset: at, op: op.text, register: h8(reg), carry, loops: loops.length });
      if (done) break;
    }
    return { key: reg >>> 0, steps };
  }

  function assemble(src) {
    const items = [], labels = {};
    let off = 0;
    String(src).split(/\r?\n/).forEach((raw, i) => {
      let t = raw.split(/[;#]/)[0].trim();
      const m = t.match(/^([A-Za-z0-9_]+):(.*)$/);
      if (m) { labels[m[1]] = off; t = m[2].trim(); }
      if (!t) return;
      const [mn, ...args] = t.split(/\s+/);
      const op = BY_NAME[mn.toLowerCase()];
      if (!op) throw `assembler line ${i + 1}: unknown mnemonic "${mn}"`;
      items.push({ op, arg: args[0], off, line: i + 1 });
      off += 1 + op[1];
    });
    const out = [];
    for (const it of items) {
      out.push(it.op[0]);
      if (!it.op[1]) continue;
      const a = String(it.arg || "").replace(/^\+/, "");
      let v = /^[0-9]/.test(a) ? Number(a) : labels[a] != null ? labels[a] - it.off - 2 : NaN;
      if (!Number.isFinite(v) || v < 0) throw `assembler line ${it.line}: bad operand "${it.arg || ""}"`;
      if (it.op[1] === 4) out.push((v >>> 24) & 255, (v >>> 16) & 255, (v >>> 8) & 255, v & 255);
      else out.push(v & 255);
    }
    return out;
  }

  const MN = /(^|[\s,])(rsl|rsr|add|sub|eor|loop|next|bcc|bra|finish)(?=$|[\s,])/i;
  function fromText(text) {
    const t = String(text || "");
    if (!t.trim()) throw "enter a script: hex bytes (e.g. 87 5A 5A 5A 5A 4C) or SA2 assembler";
    const h = t.replace(/0x/gi, "").replace(/[\s,:]/g, "");
    if (!MN.test(t) && h.length % 2 === 0 && !/[^0-9a-f]/i.test(h)) return { bytes: h.match(/../g).map((x) => parseInt(x, 16)), form: "hex" };
    return { bytes: assemble(t), form: "assembler" };
  }

  function rows(b) {
    const ins = [];
    let error = null;
    for (let at = 0; at < b.length;) {
      try { const op = decode(b, at); ins.push([at, op]); at += op.len; } catch (e) { error = String(e); break; }
    }
    const bounds = new Set(ins.map(([o]) => o));
    if (!error) bounds.add(b.length);
    const labels = new Set(ins.map(([, op]) => op.target).filter((t) => t != null && bounds.has(t)));
    let depth = 0;
    const out = ins.map(([o, op]) => {
      if (op.name === "next") depth = Math.max(0, depth - 1);
      const r = { offset: o, bytes: hexs(b.slice(o, o + op.len)), text: labels.has(op.target) ? `${op.name} L${h4(op.target)}` : op.text,
        depth, label: labels.has(o) ? `L${h4(o)}` : null, target: op.target };
      if (op.name === "loop") depth++;
      return r;
    });
    if (labels.has(b.length)) out.push({ offset: b.length, bytes: "", text: "(end)", depth: 0, label: `L${h4(b.length)}`, target: null });
    return { rows: out, error, ins };
  }

  function lints(b, ins) {
    const out = [];
    let depth = 0;
    ins.forEach(([o, op]) => {
      if (op.name === "loop") { if (!op.v) out.push(`loop at ${h4(o)} has count 0 (refused when run)`); depth++; }
      if (op.name === "next") { if (!depth) out.push(`next at ${h4(o)} has no loop before it`); else depth--; }
      if (op.target != null && op.target > b.length) out.push(`branch at ${h4(o)} lands past the end (${h4(op.target)})`);
    });
    if (depth) out.push(`${depth} loop(s) never closed by next`);
    if (!ins.length || ins[ins.length - 1][1].name !== "finish") out.push("script does not end with finish (0x4C)");
    return out;
  }

  function candidate(i, field, session, b) {
    const r = rows(b);
    return { index: i, field, session, len: b.length, hex: hexs(b), truncated: false, decodes: !r.error, error: r.error, lints: r.error ? 0 : lints(b, r.ins).length };
  }

  F.sa2_load = ({ text, path, container, frfKey }) => {
    if (text && String(text).trim()) {
      const s = fromText(text);
      return { source: "text", origin: `typed in (${s.form})`, candidates: [candidate(0, s.form, null, s.bytes)] };
    }
    if (!path) throw "paste a script, choose a script file or open a container";
    const name = String(path).split(/[\\/]/).pop();
    if (!container) {
      const f = FILES[path];
      if (!f) throw `script file ${name}: not part of the preview's demo files`;
      return { source: "file", origin: `${name} (${f.form})`, candidates: [candidate(0, f.form, null, f.bytes)] };
    }
    const c = CONTAINERS[path];
    if (!c) throw `${name}: not a plain ZIP / ODX / SGO, so it is read as an encrypted FRF — choose your FRF key file${frfKey ? " (the preview has no FRF demo)" : ""}`;
    return { source: "container", origin: name, container_kind: c.kind, needs_frf_key: false,
      candidates: c.scripts.map((s, i) => candidate(i, s.field, s.session, s.bytes)), note: null };
  };

  F.sa2_inspect = ({ script, seed, trace, traceLimit, maxSteps }) => {
    const { bytes, form } = fromText(script);
    if (bytes.length > MAX_SCRIPT) throw `${bytes.length} bytes is longer than any SA2 script (${MAX_SCRIPT}-byte limit) — is this the right field?`;
    const r = rows(bytes);
    let run = null;
    if (seed && String(seed).trim()) {
      const sh = String(seed).replace(/0x/gi, "").replace(/[\s,:]/g, "");
      if (!/^[0-9a-f]{8}$/i.test(sh)) run = { error: "the seed must be exactly 4 bytes (the ECU's 67 11 <seed> reply)" };
      else {
        const limit = Math.min(Math.max(traceLimit || 500, 1), 5000);
        const tr = [];
        let seen = 0;
        const budget = Math.min(Math.max(maxSteps || MAX_STEPS, 1), 10 * MAX_STEPS);
        const onStep = trace ? (s) => { seen++; if (tr.length < limit) tr.push(s); } : null;
        run = { seed: sh.toUpperCase(), trace: trace ? tr : null, trace_limit: limit };
        try {
          const o = exec(bytes, parseInt(sh, 16), budget, onStep);
          Object.assign(run, { key: h8(o.key), key_bytes: hexs([o.key >>> 24, o.key >>> 16, o.key >>> 8, o.key]), steps: o.steps, error: null });
        } catch (e) { Object.assign(run, { key: null, key_bytes: null, steps: seen, error: String(e) }); }
        run.trace_truncated = !!trace && seen > tr.length;
      }
    }
    return { len: bytes.length, hex: hexs(bytes), input: form, decodes: !r.error, error: r.error, rows: r.rows,
      lints: r.error ? [] : lints(bytes, r.ins), listing: null, source: null, run };
  };

  // Arming is process memory in the app; the preview keeps it for the page.
  let armed = null;
  const status = () => ({ armed: !!armed, id: "sa2", len: armed ? armed.bytes.length : null, hex: armed ? hexs(armed.bytes) : null,
    label: armed ? armed.label : null, live_allowed: true, shadowed_by_plugin: false });
  F.sa2_status = () => status();
  F.sa2_arm = ({ script, label, ack }) => {
    if (!script || !String(script).trim()) { armed = null; return status(); }
    if (!ack) throw "tick the confirmation box: the armed script answers SecurityAccess on real modules, and wrong keys count against their attempt limit";
    const { bytes } = fromText(script);
    const r = rows(bytes);
    if (r.error) throw `refusing to arm: ${r.error}`;
    for (const s of [0, 0xffffffff, 0x12345678]) exec(bytes, s, MAX_STEPS, null);
    armed = { bytes, label: String(label || "").trim() };
    return Object.assign(status(), { lints: lints(bytes, r.ins) });
  };

  window.__PREVIEW_SA2__ = { exec, assemble, fromText, SYNTH };
})();
