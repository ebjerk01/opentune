// OpenTune — sidebar navigation registry.
// Loaded right after ot-caps.js, before app.js and every feature script.
//
// Panels register their sidebar entry with OTNav.register({ id, label, icon,
// group, tier }). Groups are fixed containers in index.html, top to bottom:
//   calibration · vehicle · reference · help · bench
// tier "public" = tested, shown in every build, in its own group.
// tier "bench"  = untested or not public: always placed in the Bench group at the
// bottom, and only shown when the build channel is "alpha" or "bench". A tool
// graduates by changing its tier to "public" once it has passed a real-car test.
// Empty groups hide themselves.
"use strict";
(function () {
  const GROUPS = ["calibration", "vehicle", "reference", "help", "bench"];
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const groupEl = (g) => document.querySelector('#nav .nav-group[data-group="' + g + '"]');

  let benchOn = false;
  const listeners = [];

  function refresh() {
    document.querySelectorAll('#nav .nav-item[data-tier="bench"]').forEach((b) => { b.hidden = !benchOn; });
    GROUPS.forEach((g) => {
      const el = groupEl(g);
      if (el) el.hidden = !Array.from(el.querySelectorAll(".nav-item")).some((b) => !b.hidden);
    });
    // Never leave a hidden panel on screen.
    const active = document.querySelector("#nav .nav-item.active");
    if (active && active.hidden && window.showPanel) {
      const first = Array.from(document.querySelectorAll("#nav .nav-item")).find((b) => !b.hidden);
      if (first) window.showPanel(first.dataset.panel);
    }
  }

  const OTNav = {
    channel: "stable",
    benchEnabled: () => benchOn,

    // Register (or re-tier) a sidebar entry. Returns the nav button.
    register(o) {
      const tier = o.tier === "bench" ? "bench" : "public";
      const target = groupEl(tier === "bench" ? "bench" : o.group || "vehicle") || document.getElementById("nav");
      let btn = document.querySelector('#nav .nav-item[data-panel="' + o.id + '"]');
      if (!btn) {
        btn = document.createElement("button");
        btn.className = "nav-item";
        btn.dataset.panel = o.id;
        btn.innerHTML = '<span class="ni-ico">' + esc(o.icon || "•") + "</span> " + esc(o.label);
        btn.addEventListener("click", () => window.showPanel && window.showPanel(o.id));
      }
      if (btn.parentElement !== target) target.appendChild(btn);
      btn.dataset.tier = tier;
      if (o.title) btn.title = o.title;
      refresh();
      return btn;
    },

    // Run fn(channel) once the build channel is known (immediately if it is).
    onChannel(fn) {
      if (OTNav._resolved) fn(OTNav.channel);
      else listeners.push(fn);
    },
    refresh,
  };
  window.OTNav = OTNav;

  (async function resolveChannel() {
    try { OTNav.channel = await window.__TAURI__.core.invoke("build_channel"); } catch (e) {}
    benchOn = OTNav.channel === "alpha" || OTNav.channel === "bench";
    OTNav._resolved = true;
    refresh();
    listeners.splice(0).forEach((fn) => { try { fn(OTNav.channel); } catch (e) {} });
  })();
})();
