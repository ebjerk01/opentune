// OpenTune — per-panel help strip.
// Adds a short "what this does / what you need / how to use it" strip under each
// panel's header, with a button that opens the panel's full page in the Guide.
// Data: panel-help.json (one entry per panel id: title, summary, needs, risk,
// guide). Short step lists live in STEPS below. Extensions can add entries with
// window.OTHelp.add(id, entry). Panels added later are decorated automatically.
"use strict";
(function () {
  // The strip's markup (risk pill, needs, steps) is built by OTUtil.helpStripHtml (util.js).
  const { helpStripHtml } = window.OTUtil;

  // Three short steps per panel; the guide page has the detail.
  const STEPS = {
    image: ["Open a dump with Open image…", "Check the size and page through the hex", "Continue in Identify or Checksum"],
    analyze: ["Choose a .bin file", "Read the family, identity strings and matches", "Open a match in Identify or Maps"],
    identify: ["Open an image, or type a part / software number", "Pick the matching definition", "Use its maps and checksum in the other panels"],
    checksum: ["Open an image and pick the checksum family", "Verify: every block should be OK", "After editing, Fix → Save as a new file"],
    maps: ["Load a definition, XDF or A2L", "Pick a map and edit cells in the grid", "Write back: a backup and checksum fix are automatic"],
    compare: ["Choose a stock and a tuned image", "Pick the definition both share", "Read changed cells per map in real units"],
    flash: ["Choose the target file and the data file", "Set the address to write at", "Write: vault backup → write → read-back verify"],
    vault: ["Choose the backup folder", "Verify recomputes each CRC-32", "Restore by copying the .bin you need"],
    project: ["Edit maps, then Add to project", "Save the .otproj next to the image", "Reopen it to continue later"],
    sa2: ["Paste a script, load a file, or open your own container and pick a candidate", "Read the disassembly and lints", "Enter a seed you know the key for and Compute key"],
    database: ["Search by name, part number or car", "Open a unit for read methods and checksum", "Check the pinout confidence before wiring a bench"],
    scan: ["Detect the adapter (HS/MS switch on the module bus)", "Choose the vehicle's module list and Scan", "Clear codes per module only after fixing the cause"],
    dashboard: ["Detect the adapter, engine running", "Pick a preset or your own values", "Start: only values the ECU reports are shown"],
    obd: ["Detect the adapter", "Start with the ignition on", "Values read — when the ECU does not report them"],
    inspection: ["Detect the adapter, ignition on", "Read all (tick All OBD modules for every ECU)", "Check MIL, monitors not ready, codes and CVNs"],
    logging: ["Load a logger definition", "Pick parameters and Start", "Save the log as CSV"],
    logview: ["Open a CSV log and tick channels to plot", "Zoom with the wheel, drag to pan, read values at the cursor", "Map overlay: definition, map, image, X/Y channels → Overlay"],
    haldex: ["Tick Simulated unit to try it, or connect to the drivetrain CAN", "Read status; change the mode only when stationary (confirm + read-back)", "Decode a CAN capture offline: lock, clutch and temperature charts"],
    ts: ["Load the INI for the ECU's firmware (and a .msq, or read the ECU)", "Edit tables and settings: values outside the INI's limits are refused", "Connect (port or sim), Plan write, then Write: backup first, verify, burn"],
    candump: ["Detect a CAN-capable adapter on the right bus", "Choose format and file, then Start", "Stop to close the file; watch Dropped"],
    coding: ["Load a coding definition and set the target module", "Read, change a value, preview the diff", "Confirm and Write: backup first, read-back verify after"],
    adaptation: ["Load an adaptation definition and set the target", "Read channels before changing anything", "Routines and tests ask for confirmation"],
    service: ["Load a service script", "Plan shows every step without sending", "Run only with the car stationary and charged"],
    autodebug: ["Detect the adapter and fill in the car", "Follow the prompts in the car", "Save the report"],
    debug: ["Pick a driver and target ids", "Send a raw request or use the simulator", "Save the session log for analysis"],
  };

  const help = Object.assign({}, window.__PANEL_HELP__ || {});
  const OTHelp = {
    add(id, entry) { help[id] = Object.assign({}, help[id] || {}, entry); decorate(); },
    steps(id, list) { STEPS[id] = list; decorate(); },
  };
  window.OTHelp = OTHelp;

  function openGuide(id) {
    if (window.showPanel) window.showPanel("guide");
    if (typeof window.loadGuideSection === "function") window.loadGuideSection(id);
  }

  function strip(id, h) {
    const div = document.createElement("div");
    div.className = "ph";
    div.dataset.for = id;
    div.innerHTML = helpStripHtml(h, STEPS[id]);
    const b = div.querySelector(".ph-guide");
    if (b) b.addEventListener("click", () => openGuide(h.guide));
    return div;
  }

  function decorate() {
    document.querySelectorAll("section.panel[id^='panel-']").forEach((sec) => {
      const id = sec.id.slice(6);
      const h = help[id];
      if (!h || id === "guide") return;
      const head = sec.querySelector(".panel-head");
      if (!head) return;
      const old = sec.querySelector(".ph");
      if (old) old.remove();
      head.insertAdjacentElement("afterend", strip(id, h));
    });
  }

  async function init() {
    if (!window.__PANEL_HELP__) {
      try { Object.assign(help, await (await fetch("./src/panel-help.json")).json()); } catch (e) {}
    }
    decorate();
    const main = document.getElementById("main");
    if (main) {
      new MutationObserver((muts) => {
        if (muts.some((m) => Array.from(m.addedNodes).some((n) => n.classList && n.classList.contains("panel")))) decorate();
      }).observe(main, { childList: true });
    }
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
