// OpenTune — sidebar navigation registry.
// Loaded right after ot-caps.js, before the app-*.js scripts and every feature script.
//
// Panels register their sidebar entry with OTNav.register({ id, label, icon,
// group, tier }). Groups are fixed containers in index.html, top to bottom:
//   calibration · vehicle · reference · help · bench
// tier "public" = tested, shown in every build, in its own group.
// tier "bench"  = untested or not public: always placed in the Bench group at the
// bottom, and only shown when the build channel is "alpha" or "bench". A tool
// graduates by changing its tier to "public" once it has passed a real-car test.
// Empty groups hide themselves.
//
// Groups collapse and expand from their header button (aria-expanded, chevron).
// The user's choice is remembered in localStorage ("ot.nav.collapsed"). A
// collapsed group still shows the active panel's entry (CSS), and navigating to
// a panel inside a collapsed group expands that group for the session without
// changing the remembered choice. app-core.js's showPanel reports each switch
// through OTNav.activated(id).
"use strict";
(function () {
  const GROUPS = ["calibration", "vehicle", "reference", "help", "bench"];
  const STORE_KEY = "ot.nav.collapsed";
  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  const groupEl = (g) => document.querySelector('#nav .nav-group[data-group="' + g + '"]');

  let benchOn = false;
  const listeners = [];

  // ---- collapsible groups ---------------------------------------------------
  function loadCollapsed() {
    try {
      const v = JSON.parse(window.localStorage.getItem(STORE_KEY) || "[]");
      return new Set(Array.isArray(v) ? v.filter((g) => GROUPS.includes(g)) : []);
    } catch (e) {
      return new Set();
    }
  }
  const collapsed = loadCollapsed(); // the user's remembered choice
  const opened = new Set(); // collapsed groups expanded by navigation (not stored)
  function save() {
    try { window.localStorage.setItem(STORE_KEY, JSON.stringify(Array.from(collapsed))); } catch (e) {}
  }
  const isCollapsed = (g) => collapsed.has(g) && !opened.has(g);

  // The group's header button, upgrading a plain label <div> if needed.
  function header(el) {
    let h = el.querySelector(".nav-group-label");
    if (!h) return null;
    if (h.tagName !== "BUTTON") {
      const b = document.createElement("button");
      b.className = h.className;
      b.innerHTML = h.innerHTML;
      el.insertBefore(b, h);
      h.remove();
      h = b;
    }
    if (!h.dataset.navToggle) {
      h.dataset.navToggle = "1";
      h.setAttribute("type", "button");
      if (!h.querySelector(".ng-chev")) {
        const chev = document.createElement("span");
        chev.className = "ng-chev";
        chev.setAttribute("aria-hidden", "true");
        h.appendChild(chev);
      }
      h.addEventListener("click", () => toggle(el.dataset.group));
    }
    return h;
  }
  function applyGroup(g) {
    const el = groupEl(g);
    if (!el) return;
    const c = isCollapsed(g);
    if (c) el.dataset.collapsed = "1";
    else delete el.dataset.collapsed;
    const h = header(el);
    if (h) h.setAttribute("aria-expanded", c ? "false" : "true");
  }
  function setCollapsed(g, on) {
    if (!GROUPS.includes(g)) return;
    opened.delete(g);
    if (on) collapsed.add(g);
    else collapsed.delete(g);
    save();
    applyGroup(g);
  }
  function toggle(g) { setCollapsed(g, !isCollapsed(g)); }

  // The panel shown at first paint (index.html marks it active): no expand at boot.
  const first = document.querySelector("#nav .nav-item.active");
  let activeId = first ? first.dataset.panel : null;
  function activated(id) {
    if (id === activeId) return;
    activeId = id;
    const btn = document.querySelector('#nav .nav-item[data-panel="' + id + '"]');
    const g = btn && btn.parentElement && btn.parentElement.dataset ? btn.parentElement.dataset.group : null;
    if (g && isCollapsed(g)) {
      opened.add(g);
      applyGroup(g);
    }
  }

  function refresh() {
    GROUPS.forEach(applyGroup);
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

    // Collapsible groups.
    isCollapsed,
    setCollapsed,
    toggle,
    // Called by showPanel on every switch: expands a collapsed group that holds
    // the newly active entry (for this session only).
    activated,
  };
  window.OTNav = OTNav;
  GROUPS.forEach(applyGroup);

  (async function resolveChannel() {
    try { OTNav.channel = await window.__TAURI__.core.invoke("build_channel"); } catch (e) {}
    benchOn = OTNav.channel === "alpha" || OTNav.channel === "bench";
    OTNav._resolved = true;
    refresh();
    listeners.splice(0).forEach((fn) => { try { fn(OTNav.channel); } catch (e) {} });
  })();
})();
