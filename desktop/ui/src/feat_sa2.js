// OpenTune — SA2 script panel.
// Run your own VAG SA2 seed/key script offline: paste it (hex or assembler),
// load a file, or pick one of the SA2 candidates in your own flash container
// (FRF / ZIP / ODX / SGO; an encrypted FRF needs your FRF key file). Shows the
// disassembly with lints and computes a key from a seed, with an optional
// bounded step trace. OpenTune ships no SA2 script.
// Bench tier only: arm the script as seed/key id "sa2" for live sessions
// (service scripts naming "seedkey": "sa2", plugin commands).
// Backend: sa2_load, sa2_inspect, sa2_status, sa2_arm.
"use strict";
(function () {
  const invoke = (c, a) => window.__TAURI__.core.invoke(c, a);
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const bad = (e) => `<div class="banner bad">${esc(e)}</div>`;
  const pick = async (title, filters) => {
    const r = await window.__TAURI__.dialog.open({ title, multiple: false, directory: false, filters });
    return Array.isArray(r) ? r[0] : r;
  };

  // The candidate to preselect: the first that decodes as a script, else the first.
  function preferred(cands) {
    if (!cands || !cands.length) return -1;
    const i = cands.findIndex((c) => c.decodes);
    return i < 0 ? 0 : i;
  }

  const panel = document.createElement("section");
  panel.className = "panel wide";
  panel.id = "panel-sa2";
  panel.innerHTML = `
    <header class="panel-head">
      <h1>SA2 script</h1>
      <p>Run your own VAG SA2 seed/key script offline: disassemble it, check it and compute the key for a seed. OpenTune ships <b>no</b> script — it comes from your own flash container or your own notes.</p>
    </header>
    <div class="card">
      <h2>Script</h2>
      <div class="row">
        <button class="btn" id="sa2File" type="button">Load script file…</button>
        <button class="btn" id="sa2Cont" type="button">Open container…</button>
        <label>FRF key</label><input class="inp mono" id="sa2Key" placeholder="only for an encrypted .frf" />
        <button class="btn" id="sa2KeyPick" type="button">Choose…</button>
      </div>
      <textarea class="inp mono sa2-text" id="sa2Text" rows="5" spellcheck="false" placeholder="Hex (87 5A 5A 5A 5A 4C) or assembler, one instruction per line (eor 0x5A5A5A5A / finish)"></textarea>
      <div class="row">
        <button class="btn primary" id="sa2Dis" type="button">Disassemble</button>
        <span class="small muted" id="sa2Origin"></span>
      </div>
      <div id="sa2Cands" class="result"></div>
    </div>
    <div class="card">
      <h2>Disassembly</h2>
      <div id="sa2Disasm" class="result"><p class="muted small">Load or paste a script, then Disassemble.</p></div>
    </div>
    <div class="card">
      <h2>Seed → key</h2>
      <div class="row">
        <label>seed (4 bytes hex)</label><input class="inp mono" id="sa2Seed" placeholder="e.g. 1A 2B 3C 4D" />
        <label class="small"><input type="checkbox" id="sa2Trace" /> step trace</label>
        <label>rows</label><input class="inp sm mono" id="sa2Limit" value="500" />
        <button class="btn primary" id="sa2Run" type="button">Compute key</button>
      </div>
      <div id="sa2Result" class="result"></div>
    </div>
    <div class="card" id="sa2Live" hidden>
      <h2>Use for live sessions <span class="pill bad">Bench</span></h2>
      <p class="muted small">Arms the script above as seed/key id <span class="mono">sa2</span> until the app closes: a service script with <span class="mono">"seedkey": "sa2"</span> (and a plugin command asking for <span class="mono">sa2</span>) answers SecurityAccess with it. Not vehicle-tested. A wrong key counts against the module's attempt limit — check the key against a known seed first.</p>
      <div id="sa2Status" class="result"></div>
      <div class="row">
        <label>label</label><input class="inp" id="sa2Label" placeholder="where the script came from" />
        <label class="small"><input type="checkbox" id="sa2Ack" /> I understand this answers SecurityAccess on real modules</label>
      </div>
      <div class="row">
        <button class="btn primary" id="sa2Arm" type="button">Arm as sa2</button>
        <button class="btn" id="sa2Disarm" type="button">Disarm</button>
      </div>
    </div>`;
  document.getElementById("main").appendChild(panel);
  window.OTNav.register({ id: "sa2", label: "SA2 script", icon: "🔑", group: "reference", tier: "public" });

  const $ = (id) => panel.querySelector("#" + id);
  let cands = [];
  let origin = "";

  function renderCands(r) {
    cands = r.candidates || [];
    origin = r.origin || "";
    $("sa2Origin").textContent = origin ? "from " + origin + (r.container_kind ? " · " + r.container_kind : "") : "";
    if (r.source !== "container") { $("sa2Cands").innerHTML = ""; return; }
    if (!cands.length) { $("sa2Cands").innerHTML = `<div class="banner info">${esc(r.note || "No SA2 script in this container.")}</div>`; return; }
    $("sa2Cands").innerHTML = `<p class="small muted">${cands.length} SA2 candidate${cands.length === 1 ? "" : "s"} in the container. Pick the one that decodes as a script.</p>
      <table class="tbl"><thead><tr><th>#</th><th>ODX field</th><th>session</th><th>bytes</th><th>status</th><th></th></tr></thead><tbody>
      ${cands.map((c) => `<tr><td class="mono">${c.index}</td><td class="mono">${esc(c.field)}</td><td>${esc(c.session || "—")}</td><td class="mono">${c.len}</td>
        <td>${c.decodes ? `<span class="pill ok">decodes</span>${c.lints ? ` <span class="pill neutral">${c.lints} lint${c.lints === 1 ? "" : "s"}</span>` : ""}` : `<span class="pill bad" title="${esc(c.error || "")}">not a script</span>`}</td>
        <td><button class="btn" type="button" data-i="${c.index}"${c.truncated ? " disabled" : ""}>Use</button></td></tr>`).join("")}
      </tbody></table>`;
    $("sa2Cands").querySelectorAll("button[data-i]").forEach((b) => b.addEventListener("click", () => useCand(Number(b.dataset.i))));
  }

  function useCand(i) {
    const c = cands.find((x) => x.index === i);
    if (!c || c.truncated) return;
    $("sa2Text").value = c.hex;
    $("sa2Cands").querySelectorAll("button[data-i]").forEach((b) => b.classList.toggle("primary", Number(b.dataset.i) === i));
    $("sa2Origin").textContent = `from ${origin} · #${c.index} ${c.field}${c.session ? " · " + c.session : ""}`;
    $("sa2Label").value = `${origin} #${c.index} ${c.field}`;
    disassemble();
  }

  async function load(args) {
    $("sa2Cands").innerHTML = "";
    try {
      const r = await invoke("sa2_load", args);
      renderCands(r);
      if (r.source !== "container") {
        $("sa2Text").value = r.candidates[0].hex;
        $("sa2Label").value = origin;
        disassemble();
      } else if (cands.length) useCand(cands[preferred(cands)].index);
    } catch (e) { $("sa2Cands").innerHTML = bad(e); }
  }

  function disasmHtml(r) {
    const head = `<div class="banner ${r.decodes ? "ok" : "bad"}">${r.len} bytes · ${r.rows.filter((x) => x.bytes).length} instructions · read as ${esc(r.input)}${r.error ? " · disassembly stopped: " + esc(r.error) : ""}</div>`;
    const lints = (r.lints || []).length ? `<div class="banner info">${r.lints.map((l) => "lint: " + esc(l)).join("<br>")}</div>` : "";
    const rows = r.rows.map((x) => `<tr><td class="mono">${x.label ? esc(x.label) + ":" : ""}</td><td class="mono">${String(x.offset.toString(16)).toUpperCase().padStart(4, "0")}</td><td class="mono">${esc(x.bytes)}</td><td class="mono">${"&nbsp;&nbsp;".repeat(x.depth)}${esc(x.text)}</td><td class="mono">${x.target == null ? "" : "→ " + x.target.toString(16).toUpperCase().padStart(4, "0")}</td></tr>`).join("");
    return `${head}${lints}<div class="sa2-scroll"><table class="tbl"><thead><tr><th></th><th>offset</th><th>bytes</th><th>instruction</th><th>branch</th></tr></thead><tbody>${rows}</tbody></table></div>`;
  }

  async function disassemble() {
    try {
      const r = await invoke("sa2_inspect", { script: $("sa2Text").value });
      $("sa2Disasm").innerHTML = disasmHtml(r);
      return r;
    } catch (e) { $("sa2Disasm").innerHTML = bad(e); return null; }
  }

  function runHtml(run) {
    if (!run) return "";
    if (run.key == null) return bad(run.error || "no key") + traceHtml(run);
    return `<div class="banner ok">seed <b class="mono">${esc(run.seed)}</b> → key <b class="mono">${esc(run.key)}</b> <span class="small">(${run.steps} steps; at programming level 11 it is sent as <span class="mono">27 12 ${esc(run.key_bytes)}</span>)</span></div>
      <div class="note">Computed by OpenTune's SA2 interpreter running your script. It is only as right as the script: check it against a seed/key pair you know before using it on a module.</div>${traceHtml(run)}`;
  }

  function traceHtml(run) {
    if (!run.trace) return "";
    const more = run.trace_truncated ? `<p class="small muted">Showing the first ${run.trace.length} of ${run.steps} steps.</p>` : "";
    return `${more}<div class="sa2-scroll"><table class="tbl"><thead><tr><th>#</th><th>offset</th><th>instruction</th><th>register</th><th>carry</th><th>loops</th></tr></thead><tbody>
      ${run.trace.map((t, i) => `<tr><td class="mono">${i + 1}</td><td class="mono">${t.offset.toString(16).toUpperCase().padStart(4, "0")}</td><td class="mono">${esc(t.op)}</td><td class="mono">${esc(t.register)}</td><td class="mono">${t.carry ? 1 : 0}</td><td class="mono">${t.loops}</td></tr>`).join("")}
      </tbody></table></div>`;
  }

  async function compute() {
    try {
      const r = await invoke("sa2_inspect", {
        script: $("sa2Text").value, seed: $("sa2Seed").value, trace: !!$("sa2Trace").checked,
        traceLimit: parseInt($("sa2Limit").value, 10) || null,
      });
      $("sa2Disasm").innerHTML = disasmHtml(r);
      $("sa2Result").innerHTML = r.run ? runHtml(r.run) : bad("enter the 4-byte seed");
    } catch (e) { $("sa2Result").innerHTML = bad(e); }
  }

  function renderStatus(s) {
    if (!s) return;
    $("sa2Live").hidden = !s.live_allowed;
    const shadow = s.shadowed_by_plugin ? `<div class="banner info">A plugin in this build also provides <span class="mono">sa2</span>; it takes precedence over the script armed here.</div>` : "";
    const lints = (s.lints || []).length ? `<div class="banner info">${s.lints.map((l) => "lint: " + esc(l)).join("<br>")}</div>` : "";
    $("sa2Status").innerHTML = (s.armed
      ? `<div class="banner ok">Armed as <b class="mono">sa2</b>: ${s.len} bytes${s.label ? " · " + esc(s.label) : ""}</div>`
      : `<div class="banner info">No SA2 script armed — a service script naming <span class="mono">sa2</span> stays plan-only.</div>`) + shadow + lints;
  }

  async function status() {
    try { renderStatus(await invoke("sa2_status")); } catch (e) { $("sa2Status").innerHTML = bad(e); }
  }

  $("sa2File").addEventListener("click", async () => {
    const p = await pick("Open an SA2 script", [{ name: "SA2 script", extensions: ["sa2", "txt", "hex", "bin"] }, { name: "All files", extensions: ["*"] }]);
    if (p) load({ path: p, container: false });
  });
  $("sa2Cont").addEventListener("click", async () => {
    const p = await pick("Open your flash container", [{ name: "Flash container", extensions: ["frf", "zip", "odx", "sgo"] }, { name: "All files", extensions: ["*"] }]);
    if (p) load({ path: p, container: true, frfKey: $("sa2Key").value.trim() || null });
  });
  $("sa2KeyPick").addEventListener("click", async () => {
    const p = await pick("Your FRF key file", [{ name: "FRF key", extensions: ["key"] }, { name: "All files", extensions: ["*"] }]);
    if (p) $("sa2Key").value = p;
  });
  $("sa2Dis").addEventListener("click", () => { cands = []; $("sa2Cands").innerHTML = ""; $("sa2Origin").textContent = ""; disassemble(); });
  $("sa2Run").addEventListener("click", compute);
  $("sa2Arm").addEventListener("click", async () => {
    try {
      renderStatus(await invoke("sa2_arm", { script: $("sa2Text").value, label: $("sa2Label").value || null, ack: !!$("sa2Ack").checked }));
      if (window.toast) window.toast("SA2 script armed as sa2");
    } catch (e) { $("sa2Status").innerHTML = bad(e); }
  });
  $("sa2Disarm").addEventListener("click", async () => {
    try { renderStatus(await invoke("sa2_arm", { script: null, label: null, ack: false })); } catch (e) { $("sa2Status").innerHTML = bad(e); }
  });

  // The live card follows the build channel (Bench tier) and the backend's say.
  window.OTNav.onChannel(() => { if (window.OTNav.benchEnabled()) status(); });

  window.OTSa2 = { preferred, disasmHtml, runHtml };
})();
