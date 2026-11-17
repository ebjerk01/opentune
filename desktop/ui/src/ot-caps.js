// OpenTune — global adapter connection + capability framework.
// Loaded BEFORE app.js so window.OT exists at boot. Owns the current connection
// params, probes the adapter, updates the sidebar badge, and lets every panel
// gate its actions on what the adapter can actually do (e.g. an ELM327 cannot
// flash). Feature panels spread window.OT.connArgs() into their live invoke
// calls and call window.OT.gate(...) once.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);

  const OT = {
    // Current connection (shared by every panel). driver defaults to elm327obd.
    conn: { driver: "elm327obd", port: "", baud: "", bus: "hs" },
    adapter: null, // last probe_adapter result
    caps: null, // adapter.capabilities when connected, else null
    _subs: [],
    CAP_LABELS: {
      can: "CAN bus",
      can_fd: "CAN-FD",
      kline: "K-line",
      read_memory: "memory read",
      write_memory: "memory write / flashing",
      bench_boot: "bench / boot access",
      battery_voltage: "battery voltage",
      programming_voltage: "programming voltage",
    },
  };

  // Connection args for an invoke call. Empty strings become null.
  OT.connArgs = function () {
    const c = OT.conn;
    const nz = (s) => (s && String(s).trim() ? String(s) : null);
    return { driver: c.driver || "elm327obd", port: nz(c.port), baud: nz(c.baud), bus: nz(c.bus) };
  };

  OT.setConn = function (patch) {
    Object.assign(OT.conn, patch || {});
  };

  function fmtBadge(st) {
    if (!st || !st.connected) return "● no adapter";
    const fw = st.firmware ? " " + st.firmware : "";
    const v = st.battery_mv != null ? ` · ${(st.battery_mv / 1000).toFixed(1)}V` : "";
    return `● ${st.name || st.driver}${fw}${v}`;
  }

  // Probe the adapter with the current connection, update the badge + caps, and
  // notify subscribers. Fire-and-forget safe; never throws.
  OT.probe = async function () {
    const badge = document.getElementById("adapterBadge");
    try {
      const st = await invoke("probe_adapter", OT.connArgs());
      OT.adapter = st || null;
      OT.caps = st && st.connected ? st.capabilities || {} : null;
      if (badge) {
        badge.textContent = fmtBadge(st);
        badge.classList.toggle("connected", !!(st && st.connected));
      }
      OT._subs.forEach((fn) => { try { fn(OT.adapter, OT.caps); } catch (e) {} });
      return st;
    } catch (e) {
      OT.adapter = null; OT.caps = null;
      if (badge) { badge.textContent = "● no adapter"; badge.classList.remove("connected"); }
      OT._subs.forEach((fn) => { try { fn(null, null); } catch (e) {} });
      return null;
    }
  };

  OT.onCaps = function (fn) {
    OT._subs.push(fn);
    try { fn(OT.adapter, OT.caps); } catch (e) {}
  };

  OT.has = function (needs) {
    if (!OT.adapter || !OT.adapter.connected || !OT.caps) return false;
    return (needs || []).every((n) => !!OT.caps[n]);
  };

  // Gate a panel: inject/update a warning banner at the top of `sectionEl` and
  // disable the given action selectors when the current adapter can't do `label`.
  // Re-evaluates automatically whenever the adapter changes.
  OT.gate = function (sectionEl, needs, label, opts) {
    opts = opts || {};
    const disableSel = opts.disable || [];
    let banner = sectionEl.querySelector(".cap-banner");
    if (!banner) {
      banner = document.createElement("div");
      banner.className = "cap-banner";
      const head = sectionEl.querySelector(".panel-head");
      if (head && head.nextSibling) sectionEl.insertBefore(banner, head.nextSibling);
      else sectionEl.insertBefore(banner, sectionEl.firstChild);
    }
    function apply() {
      const ok = OT.has(needs);
      const connected = OT.adapter && OT.adapter.connected;
      if (ok) {
        banner.hidden = true;
      } else {
        banner.hidden = false;
        banner.className = "cap-banner warn";
        if (!connected) {
          banner.innerHTML = `⚠ No adapter connected — <b>${label}</b> needs a compatible interface. Set the connection and press “Detect”.`;
        } else {
          const missing = (needs || []).filter((n) => !OT.caps || !OT.caps[n]).map((n) => OT.CAP_LABELS[n] || n);
          const who = OT.adapter.name || OT.adapter.driver || "this adapter";
          banner.innerHTML = `⚠ <b>${who}</b> can’t do <b>${label}</b> — missing: ${missing.join(", ")}. Controls below are disabled.`;
        }
      }
      disableSel.forEach((sel) => {
        sectionEl.querySelectorAll(sel).forEach((btn) => {
          btn.disabled = !ok;
          btn.classList.toggle("cap-disabled", !ok);
        });
      });
    }
    OT.onCaps(apply);
    return { refresh: apply };
  };

  window.OT = OT;
})();
