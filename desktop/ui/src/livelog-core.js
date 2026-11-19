// OpenTune — Live log panel logic (pure: no DOM, no Tauri).
// What the Live log panel (feat_logging.js) shows about the link and the SSM
// module lives here so the node tests can check it: the link pill from a
// tick's `link` view and its marker samples (`paramId` "link"), what the
// Drive view is fed for a tick, the transport options sent to `log_start`,
// the capability a transport needs, and the SSM identification summary.
"use strict";
(function (root) {
  const LINK_ID = "link"; // opentune_logdef::LINK_PARAM_ID
  const DOWN = ["silent", "lost", "reconnecting", "gave-up"];

  // ---- link markers -----------------------------------------------------------
  const isMarker = (s) => !!s && s.paramId === LINK_ID;
  // The link-event marker samples of a tick, in order.
  function linkMarkers(samples) {
    return (samples || []).filter(isMarker);
  }
  // The channel samples of a tick (markers left out).
  function channelSamples(samples) {
    return (samples || []).filter((s) => s && !isMarker(s));
  }

  const secs = (ms) => (Number(ms) / 1000).toFixed(1) + " s";

  // The link pill for a tick: { cls, text, title }, or null while the link is
  // up and nothing happened. `link` is the tick's link view (session ticks
  // and `log_poll_once` both send one), `markers` its marker samples; a
  // "reconnected"/"recovered" marker shows even though the link is up again.
  function linkBadge(link, markers) {
    const last = (markers || []).length ? markers[markers.length - 1] : null;
    const ev = last ? String(last.text || "") : "";
    const st = link && link.state ? link.state : "up";
    if (st === "up" || st === "reconnected") {
      if (ev === "reconnected" || ev === "recovered" || st === "reconnected") {
        return { cls: "ok", text: "reconnected", title: (last && last.note) || (link && link.last_event) || "the link is back" };
      }
      return null;
    }
    const err = link.error ? String(link.error) : "";
    if (st === "silent") {
      const k = link.dead_ticks ? link.dead_ticks + (link.dead_after ? "/" + link.dead_after : "") : "";
      return { cls: "accent", text: "no answer" + (k ? " " + k : ""), title: err || "nothing answered this tick" };
    }
    if (st === "lost") {
      return {
        cls: "bad",
        text: link.reconnect ? "link lost — reconnecting…" : "link lost",
        title: (last && last.note) || err || "the link is down",
      };
    }
    if (st === "reconnecting") {
      const retry = link.retry_in_ms ? "; next try in " + secs(link.retry_in_ms) : "";
      return { cls: "bad", text: "reconnecting (attempt " + (link.attempt || 1) + ")", title: (err || "re-opening the adapter") + retry };
    }
    if (st === "gave-up") return { cls: "bad", text: "gave up reconnecting", title: (last && last.note) || err };
    return { cls: "neutral", text: st, title: err };
  }

  // One status line for the link, for the panel's status pill and the Drive
  // view: "" while the link is up.
  function linkText(link, markers) {
    const b = linkBadge(link, markers);
    if (!b || b.cls === "ok") return "";
    return b.text + (b.title ? ": " + b.title : "");
  }

  // What a tick publishes to the Drive view (window.OTDrive.feed): a frame of
  // the channel samples while the link answers, an error (the link state)
  // while it does not — so the Drive view greys out instead of showing a
  // dead link as live. `frameOf(samples)` builds the frame's values.
  function driveEvent(res, frameOf) {
    const link = res && res.link;
    const samples = (res && res.samples) || [];
    if (link && DOWN.includes(link.state)) {
      return { type: "error", source: "livelog", message: linkText(link, linkMarkers(samples)) || "link down", busy: false };
    }
    return { type: "frame", source: "livelog", tMs: (res && res.ts) || Date.now(), values: frameOf(channelSamples(samples)) };
  }

  // ---- transport ----------------------------------------------------------------
  const TRANSPORTS = [
    { id: "uds", label: "UDS / OBD-II" },
    { id: "ssm", label: "Subaru SSM (K-line)" },
    { id: "ssm-can", label: "Subaru SSM (CAN)" },
  ];
  const SSM_SIM = "ssmsim";
  const isSsm = (t) => t === "ssm" || t === "ssm-can";

  // The `log_start` transport arguments for the panel's choices. SSM always
  // batches (no Fast 0x2C); `sim` swaps the driver for the offline simulator.
  function startArgs(o) {
    const t = o && isSsm(o.transport) ? o.transport : "uds";
    const a = { transport: t, reconnect: !!(o && o.reconnect) };
    if (isSsm(t)) {
      a.ssmTarget = o.target === "tcu" ? "tcu" : "ecu";
      a.fast = false;
      if (o.sim) a.driver = SSM_SIM;
    }
    return a;
  }
  // Whether Start opens a live session (log_start) rather than stateless
  // polls: Fast, SSM and Reconnect all need one.
  function usesSession(o) {
    return !!(o && (o.fast || isSsm(o.transport) || o.reconnect));
  }
  // The adapter capability the transport needs (the panel's gate): nothing
  // for the simulator, a raw K-line for SSM over K-line, else a diagnostic
  // link.
  function needsFor(transport, sim) {
    if (isSsm(transport) && sim) return [];
    return transport === "ssm" ? ["kline"] : ["diag"];
  }

  // ---- SSM identification -----------------------------------------------------------
  // The panel's SSM summary from log_start's `ssm`: { head, logged, notes,
  // notLogged } (notLogged "" when every requested id is logged), or null.
  function ssmSummary(ssm) {
    if (!ssm) return null;
    const who = ssm.target === "tcu" ? "TCU" : "ECU";
    const wire = ssm.wire === "can" ? "CAN" : "K-line";
    const nl = ssm.not_logged || [];
    return {
      head: `${who} over ${wire} · SSM ID ${ssm.ssm_id} · ROM ID ${ssm.rom_id} · ${ssm.capability_bytes} capability byte(s)`,
      logged: `${ssm.logged} parameter(s) logged`,
      notes: (ssm.notes || []).slice(),
      notLogged: nl.length ? "Selected but not logged (no supported SSM parameter by that id): " + nl.join(", ") : "",
    };
  }

  // The channels to show in the table: what the session reports it logs, else
  // the definition's parameters (filtered by the selection).
  function tableParams(sessionParams, defParams, ids) {
    if (sessionParams && sessionParams.length) return sessionParams.slice();
    const all = defParams || [];
    return ids && ids.length ? all.filter((p) => ids.includes(p.id)) : all.slice();
  }

  const OTLiveLog = {
    LINK_ID, linkMarkers, channelSamples, linkBadge, linkText, driveEvent,
    TRANSPORTS, SSM_SIM, isSsm, startArgs, usesSession, needsFor, ssmSummary, tableParams,
  };
  root.OTLiveLog = OTLiveLog;
  if (typeof module !== "undefined" && module.exports) module.exports = OTLiveLog;
})(typeof globalThis !== "undefined" ? globalThis : this);
