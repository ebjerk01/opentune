// OpenTune — Adaptation panel (value channels + actuator/routine tests).
//
// Self-contained: injects its own nav item and panel into the existing shell, so
// index.html only needs a <script src="src/feat_adaptation.js"></script> after
// app.js. Drives the backend adaptation_* commands over the shared adapter
// connection (window.OT.connArgs) and gates its actions on the adapter having a
// CAN bus (window.OT.gate). No bundler, no framework.
"use strict";
(function () {
  function init() {
    const T = window.__TAURI__ || {};
    const invoke = (cmd, args) => T.core.invoke(cmd, args);
    const $ = (id) => document.getElementById(id);
    const esc = (s) =>
      String(s == null ? "" : s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
    const baseName = (p) => (p ? String(p).split(/[\\/]/).pop() : "—");

    const nav = $("nav");
    const main = $("main");
    if (!nav || !main) return;
    if ($("panel-adaptation")) return; // already injected

    // ---- nav item ----------------------------------------------------------
    const divider = document.createElement("div");
    divider.className = "nav-divider";
    const label = document.createElement("div");
    label.className = "nav-group-label";
    label.textContent = "Diagnostics";
    const btn = document.createElement("button");
    btn.className = "nav-item";
    btn.dataset.panel = "adaptation";
    btn.innerHTML = '<span class="ni-ico">⟳</span> Adaptation';
    btn.addEventListener("click", () => {
      if (typeof window.showPanel === "function") window.showPanel("adaptation");
    });
    const anchor = $("debugNavDiv");
    if (anchor) {
      nav.insertBefore(divider, anchor);
      nav.insertBefore(label, anchor);
      nav.insertBefore(btn, anchor);
    } else {
      nav.appendChild(divider);
      nav.appendChild(label);
      nav.appendChild(btn);
    }

    // ---- panel -------------------------------------------------------------
    const sec = document.createElement("section");
    sec.className = "panel";
    sec.id = "panel-adaptation";
    sec.innerHTML = `
      <header class="panel-head">
        <h1>Adaptation</h1>
        <p>Read and apply module adaptation value channels, run basic-settings routines, and run actuator / output tests — all driven by a data-defined channel set (an AdaptationDef). Live, over the current adapter.</p>
      </header>

      <div class="card">
        <div class="row">
          <button class="btn primary" id="adpPick">Adaptation def…</button>
          <span id="adpDefName" class="muted small mono">—</span>
          <button class="btn" id="adpList">List channels</button>
        </div>
        <div class="row tight">
          <label>Timeout ms</label><input class="inp sm" id="adpTimeout" value="1000" />
        </div>
        <div id="adpDefInfo" class="result"></div>
      </div>

      <div class="card">
        <h2>Channel</h2>
        <div class="row">
          <label>Channel</label>
          <select class="inp wide" id="adpChan"></select>
          <span id="adpChanKind" class="pill muted">—</span>
        </div>
        <div id="adpChanNote" class="muted small"></div>
      </div>

      <div class="card" id="adpReadCard">
        <h2>Read value</h2>
        <p class="muted small">ReadDataByIdentifier (0x22) on a value channel: shows the raw field, the scaled engineering value and its unit.</p>
        <button class="btn primary" id="adpRead">Read</button>
        <div id="adpReadResult" class="result"></div>
      </div>

      <div class="card warn-card" id="adpRunCard">
        <h2>Actuator / Routine test</h2>
        <div class="banner bad">⚠ Actuator and routine tests <b>command the ECU</b> and can move real outputs. Run only on a <b>safe, stationary vehicle</b> (parking brake on, clear of the component). The dongle's <b>HS/MS-CAN switch must match the module's bus</b> — set the Bus select to match before running.</div>
        <div class="row tight" id="adpRoutineRow" hidden>
          <label>Routine op</label>
          <select class="inp" id="adpOp">
            <option value="start">start</option>
            <option value="stop">stop</option>
            <option value="request_results">request results</option>
          </select>
          <label>extra (hex)</label><input class="inp mono" id="adpExtra" placeholder="optional data override" />
        </div>
        <div class="row tight" id="adpActuatorRow" hidden>
          <label>state (hex)</label><input class="inp mono" id="adpState" placeholder="optional — overrides channel default control state" />
        </div>
        <div class="row tight">
          <label class="confirm"><input type="checkbox" id="adpAck" /> I confirm the vehicle is safe and stationary</label>
          <button class="btn warn" id="adpRun">Run test</button>
        </div>
        <div id="adpRunResult" class="result"></div>
      </div>
    `;
    // place before the Debug panel if present, else append
    const debugPanel = $("panel-debug");
    if (debugPanel) main.insertBefore(sec, debugPanel);
    else main.appendChild(sec);

    // ---- capability gate (adaptation rides UDS over CAN) -------------------
    if (window.OT && typeof window.OT.gate === "function") {
      window.OT.gate(sec, ["can"], "adaptation", { disable: ["#adpList", "#adpRead", "#adpRun"] });
    }

    // ---- state + helpers ---------------------------------------------------
    const stAdp = { defPath: null, channels: {} };
    const connArgs = () => (window.OT && window.OT.connArgs ? window.OT.connArgs() : { driver: "elm327obd", port: null, baud: null, bus: null });
    const timeoutMs = () => {
      const n = parseInt($("adpTimeout").value, 10);
      return Number.isFinite(n) && n > 0 ? n : 1000;
    };
    const errBanner = (e) => `<div class="banner bad">${esc(typeof e === "string" ? e : (e && e.message) || e)}</div>`;
    const kindChip = { value: "value", routine: "routine", actuator: "actuator" };
    const targetLine = (r) =>
      r && r.target && window.OT && window.OT.targetText
        ? `<div class="muted small mono">module ${esc(window.OT.targetText(r.target))}</div>`
        : "";

    function selectedChannel() {
      const name = $("adpChan").value;
      return stAdp.channels[name] || null;
    }

    function refreshForChannel() {
      const ch = selectedChannel();
      const kindEl = $("adpChanKind");
      const noteEl = $("adpChanNote");
      const readCard = $("adpReadCard");
      const runCard = $("adpRunCard");
      const routineRow = $("adpRoutineRow");
      const actRow = $("adpActuatorRow");
      if (!ch) {
        kindEl.textContent = "—";
        noteEl.textContent = "";
        readCard.hidden = true;
        runCard.hidden = true;
        return;
      }
      kindEl.textContent = kindChip[ch.kind] || ch.kind;
      let note = ch.note ? esc(ch.note) : "";
      if (ch.kind === "value" && ch.unit) note += `${note ? " · " : ""}unit: ${esc(ch.unit)}`;
      if (ch.kind === "value" && (ch.min != null || ch.max != null)) note += `${note ? " · " : ""}range: ${ch.min != null ? ch.min : "—"}…${ch.max != null ? ch.max : "—"} (raw)`;
      noteEl.innerHTML = note;
      readCard.hidden = ch.kind !== "value";
      runCard.hidden = ch.kind === "value";
      routineRow.hidden = ch.kind !== "routine";
      actRow.hidden = ch.kind !== "actuator";
    }

    // ---- actions -----------------------------------------------------------
    $("adpPick").addEventListener("click", async () => {
      try {
        const r = await T.dialog.open({ filters: [{ name: "Adaptation def", extensions: ["json"] }] });
        const path = Array.isArray(r) ? r[0] : r;
        if (!path) return;
        stAdp.defPath = path;
        $("adpDefName").textContent = baseName(path);
      } catch (e) {
        $("adpDefInfo").innerHTML = errBanner(e);
      }
    });

    $("adpList").addEventListener("click", async () => {
      if (!stAdp.defPath) {
        $("adpDefInfo").innerHTML = errBanner("Pick an AdaptationDef JSON first.");
        return;
      }
      try {
        const def = await invoke("adaptation_list", { defPath: stAdp.defPath });
        stAdp.channels = {};
        const sel = $("adpChan");
        sel.innerHTML = "";
        (def.channels || []).forEach((c) => {
          stAdp.channels[c.name] = c;
          const opt = document.createElement("option");
          opt.value = c.name;
          opt.textContent = `${c.name}  (${c.kind})`;
          sel.appendChild(opt);
        });
        const sec = def.requires_security ? ` · <span class="chip">SecurityAccess required</span>` : "";
        const tgt = def.target
          ? ` · module <span class="mono">${esc(def.target.tx)}/${esc(def.target.rx)}</span>`
          : ` · <span class="chip">no module address — set Target ids before running</span>`;
        $("adpDefInfo").innerHTML =
          `<div><b>${esc(def.name || def.id)}</b> <span class="muted small">${esc(def.module || "")}</span></div>` +
          `<div class="muted small">${(def.channels || []).length} channel(s)${sec}${tgt}</div>`;
        refreshForChannel();
      } catch (e) {
        $("adpDefInfo").innerHTML = errBanner(e);
      }
    });

    $("adpChan").addEventListener("change", refreshForChannel);

    $("adpRead").addEventListener("click", async () => {
      const ch = selectedChannel();
      if (!ch) {
        $("adpReadResult").innerHTML = errBanner("Select a channel.");
        return;
      }
      try {
        const r = await invoke("adaptation_read", {
          ...connArgs(),
          defPath: stAdp.defPath,
          channel: ch.name,
          timeoutMs: timeoutMs(),
        });
        const eng = r.engineering != null ? r.engineering : "—";
        $("adpReadResult").innerHTML =
          `<div class="row tight">` +
          `<span class="pill">raw ${esc(r.raw)}</span>` +
          `<span class="pill">eng ${esc(eng)}${r.unit ? " " + esc(r.unit) : ""}</span>` +
          (r.requires_security ? `<span class="chip">SecurityAccess required</span>` : "") +
          `</div>` + targetLine(r);
      } catch (e) {
        $("adpReadResult").innerHTML = errBanner(e);
      }
    });

    $("adpRun").addEventListener("click", async () => {
      const ch = selectedChannel();
      const out = $("adpRunResult");
      if (!ch) {
        out.innerHTML = errBanner("Select a channel.");
        return;
      }
      if (!$("adpAck").checked) {
        out.innerHTML = errBanner("Tick the confirmation — this commands the ECU.");
        return;
      }
      // Each run that commands the ECU needs a fresh confirmation.
      $("adpAck").checked = false;
      try {
        if (ch.kind === "routine") {
          const op = $("adpOp").value;
          if (op !== "request_results" &&
              !window.confirm(`Really ${op} routine “${ch.name}” on the module? A routine can start a regeneration / relearn on the ECU.`)) {
            out.innerHTML = errBanner("Cancelled — nothing was sent.");
            return;
          }
          const r = await invoke("adaptation_routine", {
            ...connArgs(),
            defPath: stAdp.defPath,
            channel: ch.name,
            timeoutMs: timeoutMs(),
            op,
            extra: $("adpExtra").value || null,
            ack: true,
          });
          const val = r.value != null ? `<span class="pill">result ${esc(r.value)}${r.unit ? " " + esc(r.unit) : ""}</span>` : "";
          out.innerHTML =
            `<div class="row tight"><span class="pill">routine ok</span>${val}` +
            (r.security_required ? `<span class="chip">SecurityAccess required</span>` : "") + `</div>` +
            `<div class="muted small mono">data: ${esc(r.data || "—")}</div>` + targetLine(r);
        } else if (ch.kind === "actuator") {
          const r = await invoke("adaptation_actuator", {
            ...connArgs(),
            defPath: stAdp.defPath,
            channel: ch.name,
            timeoutMs: timeoutMs(),
            ack: true,
            state: $("adpState").value || null,
          });
          const cls = r.safe ? "pill" : "chip";
          const status = r.status != null ? `<span class="pill">status ${esc(r.status)}${r.status_unit ? " " + esc(r.status_unit) : ""}</span>` : "";
          out.innerHTML =
            `<div class="row tight">` +
            `<span class="${cls}">${esc(r.outcome)}</span>` +
            `<span class="pill">applied ${r.applied ? "yes" : "no"}</span>` +
            `<span class="${r.control_returned ? "pill" : "chip"}">control returned ${r.control_returned ? "yes" : "no"}</span>` +
            status + `</div>` +
            (r.message ? `<div class="muted small">${esc(r.message)}</div>` : "") +
            (!r.control_returned ? `<div class="banner bad">⚠ Control was NOT returned to the ECU — an output may be left forced. Recover the module before driving.</div>` : "") +
            `<div class="muted small mono">response: ${esc(r.response || "—")}</div>` + targetLine(r);
        } else {
          out.innerHTML = errBanner("This channel is a value channel — use Read above.");
        }
      } catch (e) {
        out.innerHTML = errBanner(e);
      }
    });

    refreshForChannel();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
