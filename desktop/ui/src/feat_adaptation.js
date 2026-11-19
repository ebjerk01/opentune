// OpenTune — Adaptation panel (value channels + actuator/routine tests).
//
// Self-contained: injects its own nav item and panel into the existing shell, so
// index.html only needs a <script src="src/feat_adaptation.js"></script> after
// the app-*.js scripts. Drives the backend adaptation_* commands over the shared adapter
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

    const nav = $("nav");
    const main = $("main");
    if (!nav || !main) return;
    if ($("panel-adaptation")) return; // already injected

    // ---- nav item ----------------------------------------------------------
    window.OTNav.register({ id: "adaptation", label: "Adaptation", icon: "⟳", group: "vehicle", tier: "bench" });

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
        <div id="adpPackPick"></div>
        <div class="row">
          <button class="btn primary" id="adpList">List channels</button>
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

      <div class="card" id="adpKwpCard" hidden>
        <h2>KWP2000 (VAG, TP2.0 / K-line)</h2>
        <p class="muted small">Older VAG modules: measuring blocks (0x21), basic settings and adaptation channels (startRoutineByLocalIdentifier 0x31/0x32). TP2.0 needs a raw-CAN adapter (socketcan, slcan, gsusb); K-line a K-line adapter (kkl, kline-bridge, elm327). The VAG routine ids and record layouts are community-sourced and <b>unconfirmed</b> for most modules.</p>
        <div class="row tight">
          <label class="confirm"><input type="checkbox" id="adpKwpSim" /> Simulated module (synthetic)</label>
          <label>Module address</label><input class="inp sm mono" id="adpKwpAddr" placeholder="e.g. 0x01" />
          <label>Session</label><input class="inp sm mono" id="adpKwpSession" value="0x89" />
        </div>
        <div class="row tight">
          <label>Seed/key id</label><input class="inp mono" id="adpKwpSeedkey" placeholder="from a plugin — OpenTune ships none" />
          <label>Level</label><input class="inp sm mono" id="adpKwpLevel" value="0x03" />
        </div>
        <div class="row">
          <button class="btn primary" id="adpKwpRead">Read</button>
        </div>
        <div id="adpKwpWrite" hidden>
          <div class="banner bad">⚠ Basic settings and adaptation test / save <b>change the module</b>. They need SecurityAccess with a seed/key handler (none ships with OpenTune) and run only on a <b>safe, stationary vehicle</b>. A save is read back; a failed or interrupted write leaves the channel state unknown — read it again.</div>
          <div class="row tight" id="adpKwpAdaptRow">
            <label>Raw value</label><input class="inp sm mono" id="adpKwpValue" placeholder="0..65535 or 0x…" />
            <label>Workshop code (save)</label><input class="inp mono" id="adpKwpWsc" placeholder="6 bytes, 12 hex digits" />
          </div>
          <div class="row tight" id="adpKwpBasicRow">
            <label>Reads</label><input class="inp sm" id="adpKwpPolls" value="3" />
            <label>Interval ms</label><input class="inp sm" id="adpKwpInterval" value="500" />
          </div>
          <div class="row tight">
            <label class="confirm"><input type="checkbox" id="adpKwpAck" /> I confirm the vehicle is safe and stationary</label>
            <button class="btn" id="adpKwpPlan">Plan (send nothing)</button>
            <button class="btn warn" id="adpKwpTest">Test value</button>
            <button class="btn warn" id="adpKwpSave">Save value</button>
            <button class="btn warn" id="adpKwpBasic">Run basic settings</button>
          </div>
        </div>
        <div id="adpKwpResult" class="result"></div>
      </div>
    `;
    // place before the Debug panel if present, else append
    const debugPanel = $("panel-debug");
    if (debugPanel) main.insertBefore(sec, debugPanel);
    else main.appendChild(sec);

    // ---- capability gate (adaptation rides UDS over CAN) -------------------
    if (window.OT && typeof window.OT.gate === "function") {
      window.OT.gate(sec, ["diag"], "adaptation", { disable: ["#adpList", "#adpRead", "#adpRun"] });
    }

    // ---- state + helpers ---------------------------------------------------
    const stAdp = { defPath: null, channels: {} };
    const connArgs = () => (window.OT && window.OT.connArgs ? window.OT.connArgs() : { driver: "elm327obd", port: null, baud: null, bus: null });
    const timeoutMs = () => {
      const n = parseInt($("adpTimeout").value, 10);
      return Number.isFinite(n) && n > 0 ? n : 1000;
    };
    const errBanner = (e) => `<div class="banner bad">${esc(typeof e === "string" ? e : (e && e.message) || e)}</div>`;
    const kindChip = {
      value: "value",
      routine: "routine",
      actuator: "actuator",
      kwp_group: "KWP measuring block",
      kwp_basic_settings: "KWP basic settings",
      kwp_adaptation: "KWP adaptation",
    };
    const isKwp = (ch) => !!ch && String(ch.kind).startsWith("kwp_");
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
      const kwpCard = $("adpKwpCard");
      if (!ch) {
        kindEl.textContent = "—";
        noteEl.textContent = "";
        readCard.hidden = true;
        runCard.hidden = true;
        kwpCard.hidden = true;
        return;
      }
      kwpCard.hidden = !isKwp(ch);
      if (isKwp(ch)) {
        kindEl.textContent = kindChip[ch.kind] || ch.kind;
        let note = ch.note ? esc(ch.note) : "";
        if (ch.group != null) note += `${note ? " · " : ""}group ${esc(ch.group)}`;
        if (ch.channel != null) note += `${note ? " · " : ""}channel ${esc(ch.channel)}`;
        if (ch.unit) note += ` · unit: ${esc(ch.unit)}`;
        if (ch.min != null || ch.max != null) note += ` · range: ${ch.min != null ? ch.min : "—"}…${ch.max != null ? ch.max : "—"} (raw)`;
        noteEl.innerHTML = note;
        readCard.hidden = true;
        runCard.hidden = true;
        $("adpKwpRead").hidden = ch.kind === "kwp_basic_settings";
        $("adpKwpWrite").hidden = ch.kind === "kwp_group";
        $("adpKwpAdaptRow").hidden = ch.kind !== "kwp_adaptation";
        $("adpKwpTest").hidden = ch.kind !== "kwp_adaptation";
        $("adpKwpSave").hidden = ch.kind !== "kwp_adaptation";
        $("adpKwpBasicRow").hidden = ch.kind !== "kwp_basic_settings";
        $("adpKwpBasic").hidden = ch.kind !== "kwp_basic_settings";
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
    // The AdaptationDef comes from a definition pack (or a loose file).
    const packPick = window.OTPacks.picker({
      panel: "adaptation",
      kind: "adaptation",
      filter: { name: "Adaptation def", extensions: ["json"] },
      onChange: (sel) => {
        stAdp.defPath = sel.path;
        stAdp.channels = {};
        $("adpChan").innerHTML = "";
        $("adpDefInfo").innerHTML = "";
        refreshForChannel();
      },
    });
    $("adpPackPick").appendChild(packPick.el);

    $("adpList").addEventListener("click", async () => {
      if (!stAdp.defPath) {
        $("adpDefInfo").innerHTML = errBanner("Choose an adaptation definition (a pack and its definition, or a file) first.");
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
        const kwpOnly = (def.channels || []).length > 0 && (def.channels || []).every((c) => String(c.kind).startsWith("kwp_"));
        if (def.kwp_addr != null && !$("adpKwpAddr").value) $("adpKwpAddr").value = "0x" + Number(def.kwp_addr).toString(16).padStart(2, "0").toUpperCase();
        const tgt = def.target
          ? ` · module <span class="mono">${esc(def.target.tx)}/${esc(def.target.rx)}</span>`
          : kwpOnly
            ? (def.kwp_addr != null ? ` · KWP module <span class="mono">0x${esc(Number(def.kwp_addr).toString(16).toUpperCase())}</span>` : ` · <span class="chip">no KWP module address — set it in the KWP card</span>`)
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

    // ---- KWP2000 -----------------------------------------------------------
    const kwpArgs = () => ({
      ...connArgs(),
      sim: $("adpKwpSim").checked,
      kwpAddr: $("adpKwpAddr").value.trim() || null,
      defPath: stAdp.defPath,
      channel: (selectedChannel() || {}).name,
      timeoutMs: timeoutMs(),
      session: $("adpKwpSession").value.trim() || null,
      level: $("adpKwpLevel").value.trim() || null,
      seedkey: $("adpKwpSeedkey").value.trim() || null,
    });
    const fmtCells = (cells) =>
      (cells || [])
        .map((c) => `<span class="pill">[${esc(c.index)}] ${c.value != null ? esc(Number(c.value).toFixed(3)) + " " + esc(c.unit) : "raw f=0x" + esc(Number(c.formula).toString(16)) + " " + esc(c.a) + "/" + esc(c.b)}</span>`)
        .join("");
    const kwpTarget = (t) =>
      !t ? "" : t.dry_run ? `<div class="muted small">dry run — nothing was sent</div>` : t.sim ? `<div class="muted small">simulated module (synthetic values)</div>` : `<div class="muted small mono">module ${esc(t.addr)} over ${esc(t.transport)}</div>`;
    const planList = (plan) => (plan && plan.length ? `<pre class="mono small">${esc(plan.join("\n"))}</pre>` : "");
    const outcomeLine = (o) => {
      if (!o) return "";
      const bad = ["aborted", "verify_failed", "stop_failed"].includes(o.outcome);
      let s = `<span class="${bad ? "chip" : "pill"}">${esc(o.outcome)}</span>`;
      if (o.step) s += ` <span class="muted small">at ${esc(o.step)}</span>`;
      if (o.message) s += ` <span class="muted small">${esc(o.message)}</span>`;
      if (o.session_lost) s += ` <span class="chip">session lost — nothing more was sent</span>`;
      if (o.read_back != null) s += ` <span class="chip">read back ${esc(o.read_back)}</span>`;
      return `<div class="row tight">${s}</div>`;
    };
    const reading = (r) =>
      r ? `<span class="pill">raw ${esc(r.raw)}</span><span class="pill">eng ${esc(r.engineering)}${r.unit ? " " + esc(r.unit) : ""}</span>` : "";

    $("adpKwpRead").addEventListener("click", async () => {
      const out = $("adpKwpResult");
      if (!selectedChannel()) {
        out.innerHTML = errBanner("Select a channel.");
        return;
      }
      try {
        const v = await invoke("adaptation_kwp_read", kwpArgs());
        const r = v.reading || {};
        let html = "";
        if (v.kind === "kwp_group") html = `<div class="row tight">${fmtCells(r.cells)}</div>`;
        else {
          html = `<div class="row tight">${reading(r)}${r.measured ? fmtCells([r.measured]) : ""}</div>`;
        }
        if (r.reestablished) html += `<div class="muted small">The session was lost and re-established once before this read.</div>`;
        out.innerHTML = html + kwpTarget(v.target);
      } catch (e) {
        out.innerHTML = errBanner(e);
      }
    });

    async function kwpWrite(kind, dryRun) {
      const ch = selectedChannel();
      const out = $("adpKwpResult");
      if (!ch) {
        out.innerHTML = errBanner("Select a channel.");
        return;
      }
      if (!dryRun) {
        if (!$("adpKwpAck").checked) {
          out.innerHTML = errBanner("Tick the confirmation — this changes the module.");
          return;
        }
        $("adpKwpAck").checked = false; // a fresh confirmation per write
        const what = kind === "basic" ? `run basic settings “${ch.name}”` : `${kind} value ${$("adpKwpValue").value} on “${ch.name}”`;
        if (!window.confirm(`Really ${what} on the module?`)) {
          out.innerHTML = errBanner("Cancelled — nothing was sent.");
          return;
        }
      }
      try {
        let v;
        if (ch.kind === "kwp_basic_settings") {
          v = await invoke("adaptation_kwp_basic", {
            ...kwpArgs(),
            polls: parseInt($("adpKwpPolls").value, 10) || 3,
            intervalMs: parseInt($("adpKwpInterval").value, 10) || 0,
            ack: !dryRun,
            dryRun,
          });
          const samples = (v.samples || [])
            .map((s) => `<div class="row tight"><span class="muted small">read ${esc(s.index)} status 0x${esc(Number(s.status).toString(16))}</span>${fmtCells(s.cells)}</div>`)
            .join("");
          out.innerHTML = outcomeLine(v.outcome) + samples +
            (v.started && !v.stopped && v.outcome && v.outcome.outcome !== "planned" ? `<div class="banner bad">⚠ Basic settings may still be running on the module until its session ends.</div>` : "") +
            planList(v.plan) + kwpTarget(v.target);
        } else {
          v = await invoke("adaptation_kwp_adapt", {
            ...kwpArgs(),
            mode: kind === "save" || (dryRun && $("adpKwpWsc").value.trim()) ? "save" : "test",
            value: $("adpKwpValue").value.trim(),
            wsc: $("adpKwpWsc").value.trim() || null,
            ack: !dryRun,
            dryRun,
          });
          out.innerHTML = outcomeLine(v.outcome) +
            (v.before ? `<div class="row tight"><span class="muted small">before</span>${reading(v.before)}</div>` : "") +
            (v.after ? `<div class="row tight"><span class="muted small">after</span>${reading(v.after)}</div>` : "") +
            planList(v.plan) + kwpTarget(v.target);
        }
      } catch (e) {
        out.innerHTML = errBanner(e);
      }
    }
    $("adpKwpPlan").addEventListener("click", () => kwpWrite("plan", true));
    $("adpKwpTest").addEventListener("click", () => kwpWrite("test", false));
    $("adpKwpSave").addEventListener("click", () => kwpWrite("save", false));
    $("adpKwpBasic").addEventListener("click", () => kwpWrite("basic", false));

    refreshForChannel();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
