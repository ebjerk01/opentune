// OpenTune — "Coding" feature panel (self-contained, no shared-file edits).
//
// Read, decode and write module coding over the connected adapter. Injects its
// own nav item + panel, gates on the "can" (CAN bus) capability, and drives the
// coding_read / coding_set_preview / coding_write Tauri commands. Load this
// AFTER ot-caps.js and the app-*.js scripts (needs window.OT and window.showPanel), e.g.
//   <script src="src/feat_coding.js"></script>
"use strict";
(function () {
  const invoke = window.__TAURI__.core.invoke;
  const $ = (id) => document.getElementById(id);
  const dlgOpen = (o) => window.__TAURI__.dialog.open(o);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );

  // --- Nav item --------------------------------------------------------------
  window.OTNav.register({ id: "coding", label: "Coding", icon: "⚙", group: "vehicle", tier: "bench" });

  // --- Panel -----------------------------------------------------------------
  const sec = document.createElement("section");
  sec.className = "panel";
  sec.id = "panel-coding";
  sec.innerHTML = `
    <header class="panel-head">
      <h1>Coding</h1>
      <p>Read, decode and write module coding over the connected adapter.</p>
    </header>
    <div class="card">
      <div class="row">
        <button class="btn" id="codPick">Pick coding def…</button>
        <span class="mono small muted" id="codDefPath">no definition loaded</span>
      </div>
      <div class="row" style="margin-top:8px">
        <button class="btn primary" id="codRead">Read coding</button>
        <span class="small muted" id="codDefInfo"></span>
      </div>
      <p class="small muted" style="margin:6px 0 0">
        ⚠ The dongle's HS/MS-CAN hardware switch must match the target module's bus.
        Set the connection's bus (hs/ms) to the same side before reading or writing.
      </p>
      <div id="codTableWrap" style="margin-top:10px"></div>
      <div class="row" style="margin-top:10px">
        <input class="inp" id="codName" placeholder="Parameter name (click a row)" style="min-width:220px" />
        <input class="inp" id="codValue" placeholder="New value (e.g. 1 or 0x02)" style="min-width:160px" />
        <button class="btn" id="codPreview">Preview</button>
      </div>
      <div id="codResult" class="result" style="margin-top:10px"></div>
      <div class="row tight" id="codConfirmRow" hidden style="margin-top:8px">
        <label class="confirm"><input type="checkbox" id="codAck" /> <span id="codAckText">I confirm writing this change to the module</span></label>
        <button class="btn warn" id="codWrite" disabled>Write</button>
      </div>
      <p class="small muted" style="margin:6px 0 0">Write unlocks after a Preview of the same parameter and value, and needs the confirmation each time.</p>
    </div>`;
  document.getElementById("main").appendChild(sec);

  // --- Capability gate (needs CAN) ------------------------------------------
  window.OT.gate(sec, ["can"], "module coding", {
    disable: ["#codRead", "#codPreview"],
  });

  // --- State -----------------------------------------------------------------
  let defPath = null;
  // The last successful preview with changes: { name, value }. Write is only
  // allowed for exactly that edit, after the confirm box is ticked.
  let previewed = null;

  function syncWrite() {
    const capsOk = window.OT && window.OT.has ? window.OT.has(["can"]) : true;
    const same = previewed && previewed.name === $("codName").value.trim() &&
      previewed.value === parseValue($("codValue").value);
    $("codConfirmRow").hidden = !same;
    $("codWrite").disabled = !(capsOk && same && $("codAck").checked);
  }
  function invalidatePreview() {
    previewed = null;
    $("codAck").checked = false;
    syncWrite();
  }

  const result = () => $("codResult");
  function showErr(msg) {
    result().innerHTML = `<div class="chip">error</div> <span class="mono">${esc(msg)}</span>`;
  }
  function showNote(msg) {
    result().innerHTML = `<span class="muted">${esc(msg)}</span>`;
  }

  function parseValue(raw) {
    const s = String(raw || "").trim();
    if (!s) return null;
    const n = /^0x[0-9a-f]+$/i.test(s) ? parseInt(s, 16) : /^\d+$/.test(s) ? parseInt(s, 10) : NaN;
    return Number.isFinite(n) ? n : null;
  }

  // --- Render the decoded parameter table -----------------------------------
  function renderParams(data) {
    const def = (data && data.def) || {};
    const params = (data && data.params) || [];
    $("codDefInfo").textContent =
      `${def.name || def.id || "def"}${def.module ? " · " + def.module : ""}` +
      (def.requiresSecurity ? " · requires SecurityAccess (0x27)" : "");
    if (!params.length) {
      $("codTableWrap").innerHTML = `<span class="muted small">No parameters in this definition.</span>`;
      return;
    }
    const rows = params
      .map((p) => {
        const opts = (p.options || [])
          .map((o) => `<span class="pill">${esc(o.value)}=${esc(o.label)}</span>`)
          .join(" ");
        const note = p.note ? `<div class="small muted">${esc(p.note)}</div>` : "";
        return `<tr data-name="${esc(p.name)}" style="cursor:pointer">
          <td>${esc(p.name)} <span class="chip">${esc(p.kind)}</span>${note}</td>
          <td class="mono">${esc(p.value)}</td>
          <td class="mono">${esc(p.label)}</td>
          <td class="small">${opts}</td>
        </tr>`;
      })
      .join("");
    $("codTableWrap").innerHTML = `<table style="width:100%;border-collapse:collapse">
      <thead><tr><th style="text-align:left">Parameter</th><th style="text-align:left">Value</th>
      <th style="text-align:left">Decoded</th><th style="text-align:left">Options</th></tr></thead>
      <tbody>${rows}</tbody></table>`;
    $("codTableWrap").querySelectorAll("tr[data-name]").forEach((tr) => {
      tr.addEventListener("click", () => {
        $("codName").value = tr.getAttribute("data-name");
        syncWrite();
        $("codValue").focus();
      });
    });
  }

  // --- Render a write/preview report ----------------------------------------
  function outcomeLabel(o) {
    o = o || {};
    switch (o.kind) {
      case "planned": return "planned (dry run — nothing written)";
      case "no_change": return "no change (edit matches live value)";
      case "verified": return "written & verified";
      case "unverified": return "written (verification skipped)";
      case "verify_failed": return `VERIFY FAILED on ${o.did}`;
      case "write_error": return `WRITE ERROR on ${o.did}: ${o.message || ""}`;
      default: return o.kind || "unknown";
    }
  }

  function renderReport(rep, title) {
    rep = rep || {};
    const pc = (rep.paramChanges || [])
      .map(
        (p) =>
          `<tr><td>${esc(p.name)}</td><td class="mono">${esc(p.old.value)} (${esc(p.old.label)})</td>
           <td class="mono">→ ${esc(p.new.value)} (${esc(p.new.label)})</td></tr>`
      )
      .join("");
    const bc = (rep.byteChanges || [])
      .map(
        (c) =>
          `<tr><td class="mono">${esc(c.did)}</td><td class="mono">byte ${esc(c.byte)}</td>
           <td class="mono">${esc(c.old)} → ${esc(c.new)}</td></tr>`
      )
      .join("");
    const sec = rep.securityRequired
      ? `<div class="small muted">⚠ This module requires SecurityAccess (0x27) to be unlocked before a write will be accepted.</div>`
      : "";
    const changes = pc
      ? `<div class="small muted" style="margin-top:6px">Parameter changes</div>
         <table style="width:100%;border-collapse:collapse"><tbody>${pc}</tbody></table>`
      : `<div class="small muted" style="margin-top:6px">No parameter changes.</div>`;
    const bytes = bc
      ? `<div class="small muted" style="margin-top:6px">Byte changes</div>
         <table style="width:100%;border-collapse:collapse"><tbody>${bc}</tbody></table>`
      : "";
    const written = (rep.written || []).length
      ? `<div class="small muted" style="margin-top:6px">DIDs written: <span class="mono">${rep.written.map(esc).join(", ")}</span></div>`
      : "";
    // Pre-write backup: what to restore if the write/verify went wrong.
    const kind = rep.outcome && rep.outcome.kind;
    const failed = kind === "verify_failed" || kind === "write_error";
    const bk = (rep.backup || [])
      .map((b) => `<tr><td class="mono">${esc(b.did)}</td><td class="mono">${esc(b.hex)}</td></tr>`)
      .join("");
    const backup = bk && kind !== "planned"
      ? `<details ${failed ? "open" : ""} style="margin-top:6px"><summary class="small${failed ? "" : " muted"}">` +
        `${failed ? "⚠ Backup before write — restore these bytes (2E) to recover the module" : "Backup before write"}</summary>` +
        `<table style="width:100%;border-collapse:collapse"><tbody>${bk}</tbody></table></details>`
      : "";
    const target = rep.target && window.OT && window.OT.targetText
      ? `<div class="small muted mono">module ${esc(window.OT.targetText(rep.target))}</div>`
      : "";
    result().innerHTML =
      `<div><span class="chip">${esc(title)}</span> <b>${esc(outcomeLabel(rep.outcome))}</b></div>` +
      target + sec + changes + bytes + written + backup;
  }

  // --- Actions ---------------------------------------------------------------
  async function pickDef() {
    try {
      const p = await dlgOpen({ filters: [{ name: "Coding def", extensions: ["json"] }] });
      if (!p) return;
      defPath = p;
      $("codDefPath").textContent = p;
      showNote("Definition loaded — press “Read coding”.");
    } catch (e) {
      showErr(String(e));
    }
  }

  function needDef() {
    if (!defPath) {
      showErr("Pick a coding definition JSON file first.");
      return false;
    }
    return true;
  }

  async function doRead() {
    if (!needDef()) return;
    showNote("Reading coding…");
    try {
      const data = await invoke("coding_read", {
        ...window.OT.connArgs(),
        defPath,
        timeoutMs: 1000,
      });
      renderParams(data);
      showNote("Read OK.");
    } catch (e) {
      showErr(String(e));
    }
  }

  async function doEdit(cmd, title) {
    if (!needDef()) return;
    const name = $("codName").value.trim();
    if (!name) return showErr("Enter (or click) a parameter name.");
    const value = parseValue($("codValue").value);
    if (value == null) return showErr("Enter a numeric value, e.g. 1 or 0x02.");
    const isWrite = cmd === "coding_write";
    if (isWrite) {
      if (!previewed || previewed.name !== name || previewed.value !== value) {
        invalidatePreview();
        return showErr("Preview this exact change first.");
      }
      if (!$("codAck").checked) return showErr("Tick the confirmation — this writes to the module.");
    }
    // A write consumes the confirmation and the preview (its baseline is now stale).
    if (isWrite) invalidatePreview();
    showNote(title + "…");
    try {
      const args = { ...window.OT.connArgs(), defPath, timeoutMs: 1000, name, value };
      if (isWrite) args.ack = true;
      const rep = await invoke(cmd, args);
      renderReport(rep, title);
      if (!isWrite) {
        const changes = (rep.paramChanges || []).length + (rep.byteChanges || []).length;
        previewed = changes && rep.outcome && rep.outcome.kind === "planned" ? { name, value } : null;
        $("codAck").checked = false;
        $("codAckText").textContent = `I confirm writing ${name} = ${value} to the module`;
        syncWrite();
      }
    } catch (e) {
      if (!isWrite) invalidatePreview();
      showErr(String(e));
    }
  }

  $("codPick").addEventListener("click", () => { invalidatePreview(); pickDef(); });
  $("codRead").addEventListener("click", doRead);
  $("codPreview").addEventListener("click", () => doEdit("coding_set_preview", "Preview"));
  $("codWrite").addEventListener("click", () => doEdit("coding_write", "Write"));
  $("codAck").addEventListener("change", syncWrite);
  $("codName").addEventListener("input", syncWrite);
  $("codValue").addEventListener("input", syncWrite);
  if (window.OT && window.OT.onCaps) window.OT.onCaps(syncWrite);
})();
