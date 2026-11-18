// Demo fixtures for the Utilities panel: VIN and part-number decoding, DTC
// lookup and the generic seed/key transforms. Preview only: small JS stand-ins
// for the app's decoders (a subset of their tables), enough to exercise the UI.
"use strict";
(function () {
  const F = window.__PREVIEW_FIXTURES__;
  if (!F) return;
  const hexs = (b) => b.map((x) => (x & 255).toString(16).toUpperCase().padStart(2, "0")).join(" ");
  const parseHex = (s) => {
    const t = String(s || "").replace(/0x/gi, "").replace(/[\s,:]/g, "");
    if (!t || /[^0-9a-f]/i.test(t) || t.length % 2) throw `'${s}' is not hex bytes (e.g. 1A 2B 3C 4D)`;
    return t.match(/../g).map((h) => parseInt(h, 16));
  };

  // ---- VIN ------------------------------------------------------------------
  const WMI = { WVW: "Volkswagen", WV1: "Volkswagen Commercial Vehicles", WV2: "Volkswagen Commercial Vehicles", WAU: "Audi", WBA: "BMW",
    WDB: "Mercedes-Benz", WDD: "Mercedes-Benz", TMB: "Škoda", VSS: "SEAT", VF1: "Renault", VF3: "Peugeot", ZFA: "Fiat", YV1: "Volvo",
    SAL: "Land Rover", JMZ: "Mazda", KNA: "Kia", "1G1": "Chevrolet", "1FA": "Ford" };
  const COUNTRY = [[/^[1-5]/, "United States / North America"], [/^J/, "Japan"], [/^K[L-R]/, "South Korea"], [/^S[A-M]/, "United Kingdom"],
    [/^T[J-P]/, "Czech Republic"], [/^V[F-R]/, "France"], [/^VS/, "Spain"], [/^W/, "Germany"], [/^Y[S-W]/, "Sweden"], [/^Z[A-R]/, "Italy"]];
  const YEAR = "ABCDEFGHJKLMNPRSTVWXY123456789";
  const VAL = (ch) => /\d/.test(ch) ? +ch : "ABCDEFGH".includes(ch) ? "ABCDEFGH".indexOf(ch) + 1 : "JKLMN".includes(ch) ? "JKLMN".indexOf(ch) + 1 : ch === "P" ? 7 : ch === "R" ? 9 : "STUVWXYZ".indexOf(ch) + 2;
  const WEIGHT = [8, 7, 6, 5, 4, 3, 2, 10, 0, 9, 8, 7, 6, 5, 4, 3, 2];
  F.vin_decode = ({ vin }) => {
    const v = String(vin || "").trim().toUpperCase();
    if (v.length !== 17) throw `a VIN has 17 characters (got ${v.length})`;
    if (/[IOQ]/.test(v) || /[^A-Z0-9]/.test(v)) throw "a VIN uses 0-9 and A-Z except I, O and Q";
    const wmi = v.slice(0, 3);
    const sum = Array.from(v).reduce((a, ch, i) => a + VAL(ch) * WEIGHT[i], 0) % 11;
    const na = /^[1-5]/.test(v);
    const yi = YEAR.indexOf(v[9]);
    const cand = yi < 0 ? [] : [1980 + yi, 2010 + yi].filter((y) => y <= new Date().getFullYear() + 1);
    const flags = [];
    if (/^0+$/.test(v.slice(11))) flags.push("serial is all zeros — looks like a placeholder VIN");
    if (yi < 0) flags.push(`position 10 '${v[9]}' is not a model-year code`);
    return {
      vin: v, wmi, vds: v.slice(3, 9), vis: v.slice(9), manufacturer: WMI[wmi] || null,
      country: (COUNTRY.find(([re]) => re.test(v)) || [null, null])[1],
      check_digit: v[8], check_digit_valid: na ? (sum === 10 ? "X" : String(sum)) === v[8] : null,
      model_year: { code: v[9], candidates: cand, resolved: cand.length === 1 ? cand[0] : null, ambiguous: cand.length !== 1 },
      plant_code: v[10], serial: v.slice(11), vds_descriptor: v.slice(3, 8), flags,
    };
  };

  // ---- part numbers -----------------------------------------------------------
  F.part_decode = ({ part }) => {
    const raw = String(part || "").trim();
    if (!raw) throw "enter a part number";
    const p = raw.toUpperCase().replace(/[\s.\-]/g, "");
    const out = { normalized: p, kind: "Unknown", supplier: null, vag_type_group: null, vag_main_group: null, vag_subgroup: null, vag_version: null, bosch_family: null, bosch_consecutive: null, flags: [] };
    let m;
    if ((m = /^(\d{4})(\d{6})$/.exec(p)) && /^02[68]/.test(p)) {
      Object.assign(out, { kind: "Bosch", supplier: "Bosch", bosch_family: m[1] === "0281" ? "0281 (diesel engine control units)" : m[1] === "0261" ? "0261 (petrol engine control units)" : m[1], bosch_consecutive: m[2] });
    } else if ((m = /^(\d{4})(\d{6})$/.exec(p)) && p.startsWith("1037")) {
      Object.assign(out, { kind: "BoschSoftware", supplier: "Bosch", bosch_family: "1037 (software number)", bosch_consecutive: m[2] });
    } else if ((m = /^([0-9A-Z]{3})(\d{3})(\d{3})([A-Z]{0,3})$/.exec(p))) {
      Object.assign(out, { kind: "Vag", vag_type_group: m[1], vag_main_group: m[2], vag_subgroup: m[3], vag_version: m[4] || null });
      if (/^0+$/.test(m[1] + m[3])) out.flags.push("placeholder-looking number");
    } else out.flags.push("no known numbering scheme matched (preview decoder knows VAG and Bosch only)");
    return out;
  };

  // ---- DTC lookup ---------------------------------------------------------------
  const SYS = { P: "Powertrain (engine / transmission / emissions)", C: "Chassis (ABS / steering / suspension)", B: "Body (airbags / lighting / HVAC / comfort)", U: "Network / communication (CAN bus / modules)" };
  const MORE = { P0299: "Turbo/Super Charger Underboost", P0401: "Exhaust Gas Recirculation Flow Insufficient Detected", P2002: "Particulate Trap Efficiency Below Threshold Bank 1",
    P0087: "Fuel Rail/System Pressure - Too Low", P0234: "Turbo/Super Charger Overboost Condition" };
  F.dtc_lookup = ({ code }) => {
    const c = String(code || "").trim().toUpperCase();
    if (!/^[PCBU][0-3][0-9A-F]{3}$/.test(c)) throw "not a recognisable DTC — it must start with P, C, B or U (e.g. P0101)";
    const d = Object.assign({}, MORE, window.__PREVIEW_DTC__ || {})[c];
    return { code: c, description: d || "Not in the preview's small code table (the app bundles the full list)", system: SYS[c[0]], system_letter: c[0] };
  };

  // ---- generic seed/key transforms (no OEM algorithms) -------------------------
  F.seedkey_algos = () => [
    { id: "identity", name: "Identity (key = seed)", params: [], desc: "Echoes the seed. Some access levels accept the seed unchanged." },
    { id: "xor-mask", name: "XOR mask", params: ["mask"], desc: "key[i] = seed[i] XOR mask[i] (mask repeats if shorter)." },
    { id: "add-const", name: "Add constant (per byte)", params: ["add"], desc: "key[i] = seed[i] + k (mod 256)." },
    { id: "xor-add", name: "XOR then add (demo shape)", params: ["mask", "add"], desc: "key[i] = (seed[i] XOR m) + k (mod 256), m = first mask byte." },
    { id: "byteswap", name: "Reverse byte order", params: [], desc: "key = seed reversed." },
  ];
  F.seedkey_compute = ({ seed, algo, mask, add }) => {
    const s = parseHex(seed);
    const m = mask ? parseHex(mask) : [];
    const k = add ? parseInt(String(add).replace(/^0x/i, ""), /^0x/i.test(add) ? 16 : 10) & 255 : 0;
    let key;
    switch (algo) {
      case "identity": key = s; break;
      case "xor-mask": if (!m.length) throw "xor-mask needs a mask"; key = s.map((b, i) => b ^ m[i % m.length]); break;
      case "add-const": key = s.map((b) => b + k); break;
      case "xor-add": key = s.map((b) => (b ^ (m[0] || 0)) + k); break;
      case "byteswap": key = s.slice().reverse(); break;
      default: throw `seed/key algorithm '${algo}' is not available in this build`;
    }
    return { key: hexs(key), algo, source: "generic", note: "Generic transform — NOT an OEM algorithm. The public edition bundles no vehicle-specific seed/key routines." };
  };
})();
