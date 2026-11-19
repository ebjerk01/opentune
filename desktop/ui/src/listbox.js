// OpenTune — a small custom listbox that stands in for a native <select>.
//
// Why: WebKitGTK opens a native <select> as a GTK popup menu and places it
// itself. For the sidebar's adapter pickers (pinned at the bottom of the
// window) it lands past the screen edge in fullscreen. This listbox draws its
// own popup in the page: down when it fits, else up, clamped to the viewport,
// scrolling inside when the list is taller than the room there.
//
// Contract: enhance(select) keeps the <select> in the DOM (hidden) as the
// source of truth, so callers keep using it unchanged — `select.value` get/set
// (the button follows), `change` events, filling it with <option>s (picked up
// by a MutationObserver, or call refresh()), `select.disabled`, and
// querySelector('option[value=…]'). The popup is fixed-position on <body>, so
// no overflow:hidden ancestor (the sidebar) clips it.
//
// The pure parts (place, keyAction, typeahead) have no DOM and are exported for
// the node tests; enhance() needs a document.
"use strict";
(function (root) {
  // ---- positioning (pure) ---------------------------------------------------
  // anchor: the button's client rect {left, top, right?, bottom, width, height}.
  // viewport: {width, height}. size: the list's natural {width, height}.
  // Down when it fits below; else up when it fits above; else the roomier side
  // with maxHeight = the room there (the list scrolls inside). The popup is at
  // least as wide as the anchor, never wider than the viewport, and kept
  // `margin` px inside every edge.
  function place(anchor, viewport, size, opts) {
    const o = opts || {};
    const margin = o.margin == null ? 6 : o.margin;
    const gap = o.gap == null ? 2 : o.gap;
    const vw = Math.max(0, viewport.width), vh = Math.max(0, viewport.height);
    const below = Math.max(0, vh - margin - (anchor.bottom + gap));
    const above = Math.max(0, anchor.top - gap - margin);
    const natural = Math.max(0, size.height || 0);
    let dir;
    if (natural <= below) dir = "down";
    else if (natural <= above) dir = "up";
    else dir = below >= above ? "down" : "up";
    const room = Math.max(0, vh - 2 * margin);
    const maxHeight = Math.floor(Math.min(dir === "down" ? below : above, room));
    const height = Math.min(natural, maxHeight);
    let top = dir === "down" ? anchor.bottom + gap : anchor.top - gap - height;
    top = clamp(top, margin, vh - margin - height);
    const width = Math.min(Math.max(anchor.width || 0, size.width || 0), Math.max(0, vw - 2 * margin));
    const left = clamp(anchor.left, margin, vw - margin - width);
    return { dir, top: Math.round(top), left: Math.round(left), width: Math.ceil(width), maxHeight, height };
  }
  function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi < lo ? lo : hi, v)); }

  // ---- keyboard (pure) ------------------------------------------------------
  // Next enabled index from `from` moving by `step` (clamped, never wraps);
  // -1 when every option is disabled.
  function step(from, by, n, disabled) {
    const dis = (i) => !!(disabled && disabled[i]);
    if (n <= 0) return -1;
    const dir = by > 0 ? 1 : -1;
    let i = from < 0 || from >= n ? (dir > 0 ? -1 : n) : from;
    let left = Math.abs(by), best = from >= 0 && from < n && !dis(from) ? from : -1;
    while (left > 0) {
      i += dir;
      if (i < 0 || i >= n) break;
      if (!dis(i)) { best = i; left--; }
    }
    if (best >= 0) return best;
    return edge(dir > 0 ? "first" : "last", n, disabled);
  }
  function edge(which, n, disabled) {
    for (let k = 0; k < n; k++) {
      const i = which === "first" ? k : n - 1 - k;
      if (!(disabled && disabled[i])) return i;
    }
    return -1;
  }

  // Type-ahead: the next option whose label starts with `query` (case-
  // insensitive). A single character, or the same character repeated, cycles
  // from the option after `from`; a longer query starts at `from` itself so
  // typing "elm3" stays on "elm327" once "e" found it. -1 when none matches.
  function typeahead(labels, query, from, disabled) {
    const q = String(query || "").toLowerCase();
    const n = labels.length;
    if (!q || !n) return -1;
    const same = q.split("").every((c) => c === q[0]);
    const needle = same ? q[0] : q;
    const start = same ? from + 1 : Math.max(0, from);
    for (let k = 0; k < n; k++) {
      const i = (((start + k) % n) + n) % n;
      if (disabled && disabled[i]) continue;
      if (String(labels[i]).toLowerCase().startsWith(needle)) return i;
    }
    return -1;
  }

  // What a keydown does. st: {open, active, selected, n, disabled, alt, typing}.
  // Returns {type, index?, prevent}: "open" (with the option to make active),
  // "move", "select" (commit `index` and close), "close" (no change), "type"
  // (feed the key to type-ahead; opens when closed) or "none".
  const PAGE = 10;
  function keyAction(key, st) {
    const n = st.n, dis = st.disabled;
    const cur = st.open ? st.active : st.selected;
    if (!st.open) {
      switch (key) {
        case "ArrowDown": case "ArrowUp": case "Enter": case " ":
          return { type: "open", index: cur >= 0 && !(dis && dis[cur]) ? cur : edge("first", n, dis), prevent: true };
        case "Home": return { type: "open", index: edge("first", n, dis), prevent: true };
        case "End": return { type: "open", index: edge("last", n, dis), prevent: true };
        default: return isPrintable(key) ? { type: "type", prevent: true } : { type: "none", prevent: false };
      }
    }
    switch (key) {
      case "ArrowDown": return { type: "move", index: step(cur, 1, n, dis), prevent: true };
      case "ArrowUp":
        if (st.alt) return { type: "select", index: cur, prevent: true };
        return { type: "move", index: step(cur, -1, n, dis), prevent: true };
      case "PageDown": return { type: "move", index: step(cur, PAGE, n, dis), prevent: true };
      case "PageUp": return { type: "move", index: step(cur, -PAGE, n, dis), prevent: true };
      case "Home": return { type: "move", index: edge("first", n, dis), prevent: true };
      case "End": return { type: "move", index: edge("last", n, dis), prevent: true };
      case "Enter": return { type: "select", index: cur, prevent: true };
      case " ": return st.typing ? { type: "type", prevent: true } : { type: "select", index: cur, prevent: true };
      case "Escape": return { type: "close", prevent: true };
      // Tab closes without changing the value (a driver switch should be a
      // deliberate pick) and lets focus move on.
      case "Tab": return { type: "close", prevent: false };
      default: return isPrintable(key) ? { type: "type", prevent: true } : { type: "none", prevent: false };
    }
  }
  function isPrintable(key) { return typeof key === "string" && key.length === 1 && key !== " "; }

  // ---- the component ----------------------------------------------------------
  const live = new Set(); // open listboxes (closeAll)
  let seq = 0;

  // A data property on the test DOM stub, an accessor on HTMLSelectElement in
  // a browser: wrap either so assigning it also runs `after`.
  function hookProp(obj, key, after) {
    const own = Object.getOwnPropertyDescriptor(obj, key);
    if (own && "value" in own) {
      let v = own.value;
      Object.defineProperty(obj, key, { configurable: true, enumerable: true, get: () => v, set: (x) => { v = x; after(x); } });
      return;
    }
    let p = Object.getPrototypeOf(obj), d = null;
    while (p && !(d = Object.getOwnPropertyDescriptor(p, key))) p = Object.getPrototypeOf(p);
    if (!d || !d.set) return;
    Object.defineProperty(obj, key, {
      configurable: true, enumerable: true,
      get() { return d.get.call(this); },
      set(x) { d.set.call(this, x); after(x); },
    });
  }

  function enhance(select, opts) {
    if (!select || select._otListbox) return select && select._otListbox;
    const o = opts || {};
    const doc = select.ownerDocument || root.document;
    const win = root;
    const uid = (select.id || "ot-lb") + "-lb" + ++seq;
    const btn = doc.createElement("button");
    btn.type = "button";
    btn.className = ((select.className || "") + " ot-lb").trim();
    btn.setAttribute("role", "combobox");
    btn.setAttribute("aria-haspopup", "listbox");
    btn.setAttribute("aria-expanded", "false");
    btn.setAttribute("aria-controls", uid);
    const name = select.getAttribute("aria-label") || select.getAttribute("title") || o.label || "";
    if (name) { btn.setAttribute("aria-label", name); btn.title = select.getAttribute("title") || name; }
    btn.innerHTML = '<span class="ot-lb-val"></span><span class="ot-lb-chev" aria-hidden="true"></span>';
    const valEl = btn.querySelector(".ot-lb-val") || btn;
    select.insertAdjacentElement("afterend", btn);
    select.hidden = true;
    select.tabIndex = -1;
    select.setAttribute("aria-hidden", "true");
    select.classList.add("ot-lb-native");

    let pop = null, open = false, active = -1, items = [], buf = "", bufAt = 0;
    const now = () => (win.performance && win.performance.now ? win.performance.now() : Date.now());

    const options = () => Array.from(select.querySelectorAll("option"));
    const labelOf = (op) => (op.textContent || op.label || op.value || "").trim() || String(op.value);
    function selectedIndex() { const v = select.value; return options().findIndex((op) => op.value === v); }
    function syncLabel() {
      const ops = options(), i = selectedIndex();
      valEl.textContent = i >= 0 ? labelOf(ops[i]) : (select.value || "");
      btn.disabled = !!select.disabled;
      btn.setAttribute("aria-disabled", select.disabled ? "true" : "false");
    }

    function render() {
      const ops = options(), sel = selectedIndex();
      items = ops.map((op, i) => ({ value: op.value, label: labelOf(op), disabled: !!op.disabled, id: uid + "-o" + i }));
      pop.innerHTML = items.map((it, i) =>
        `<div role="option" id="${it.id}" class="ot-lb-opt" data-i="${i}" aria-selected="${i === sel}"${it.disabled ? ' aria-disabled="true"' : ""}></div>`).join("");
      pop.querySelectorAll(".ot-lb-opt").forEach((n, i) => { n.textContent = items[i].label; });
    }
    function setActive(i) {
      active = i;
      if (!pop) return;
      pop.querySelectorAll(".ot-lb-opt").forEach((n, k) => n.classList.toggle("active", k === i));
      if (i >= 0 && items[i]) {
        btn.setAttribute("aria-activedescendant", items[i].id);
        const n = pop.querySelector(`#${items[i].id}`);
        if (n && n.offsetHeight) {
          if (n.offsetTop < pop.scrollTop) pop.scrollTop = n.offsetTop;
          else if (n.offsetTop + n.offsetHeight > pop.scrollTop + pop.clientHeight) pop.scrollTop = n.offsetTop + n.offsetHeight - pop.clientHeight;
        }
      } else btn.removeAttribute("aria-activedescendant");
    }
    function position() {
      if (!pop) return;
      // Measure the natural size first, then place it.
      Object.assign(pop.style, { visibility: "hidden", maxHeight: "none", top: "0px", left: "0px", width: "auto", minWidth: "0" });
      const a = btn.getBoundingClientRect();
      const vp = { width: win.innerWidth || (doc.documentElement && doc.documentElement.clientWidth) || 0, height: win.innerHeight || (doc.documentElement && doc.documentElement.clientHeight) || 0 };
      const p = place(a, vp, { width: pop.scrollWidth || pop.offsetWidth || 0, height: pop.scrollHeight || pop.offsetHeight || 0 });
      Object.assign(pop.style, { top: p.top + "px", left: p.left + "px", width: p.width + "px", maxHeight: p.maxHeight + "px", visibility: "" });
      pop.dataset.dir = p.dir;
    }

    function onDocDown(e) { if (open && !btn.contains(e.target) && !(pop && pop.contains(e.target))) close(); }
    function onScroll(e) { if (open && !(pop && e && pop.contains(e.target))) close(); }
    function onResize() { close(); }

    function openList(i) {
      if (open || select.disabled) return;
      live.forEach((c) => c !== ctrl && c.close());
      pop = doc.createElement("div");
      pop.id = uid;
      pop.className = "ot-lb-pop";
      pop.setAttribute("role", "listbox");
      if (name) pop.setAttribute("aria-label", name);
      pop.tabIndex = -1;
      // Keep focus on the button: a mousedown in the list (option or its
      // scrollbar) must not blur it.
      pop.addEventListener("mousedown", (e) => e.preventDefault());
      pop.addEventListener("click", (e) => {
        const n = e.target && e.target.closest ? e.target.closest(".ot-lb-opt") : null;
        if (n) commit(parseInt(n.dataset.i, 10));
      });
      pop.addEventListener("mousemove", (e) => {
        const n = e.target && e.target.closest ? e.target.closest(".ot-lb-opt") : null;
        if (n) { const k = parseInt(n.dataset.i, 10); if (k !== active && !items[k].disabled) setActive(k); }
      });
      doc.body.appendChild(pop);
      render();
      open = true;
      live.add(ctrl);
      btn.setAttribute("aria-expanded", "true");
      btn.classList.add("open");
      position();
      setActive(i == null ? selectedIndex() : i);
      doc.addEventListener("pointerdown", onDocDown, true);
      doc.addEventListener("scroll", onScroll, true);
      win.addEventListener("resize", onResize);
      win.addEventListener("blur", onResize);
    }
    function close() {
      if (!open) return;
      open = false;
      live.delete(ctrl);
      doc.removeEventListener("pointerdown", onDocDown, true);
      doc.removeEventListener("scroll", onScroll, true);
      win.removeEventListener("resize", onResize);
      win.removeEventListener("blur", onResize);
      if (pop) pop.remove();
      pop = null;
      active = -1;
      buf = "";
      btn.setAttribute("aria-expanded", "false");
      btn.removeAttribute("aria-activedescendant");
      btn.classList.remove("open");
    }
    function commit(i) {
      const it = items[i];
      close();
      if (!it || it.disabled) return;
      if (select.value !== it.value) {
        select.value = it.value;
        fireChange();
      }
    }
    function fireChange() {
      let ev;
      try { ev = new win.Event("change", { bubbles: true }); } catch (e) { ev = { type: "change", bubbles: true }; }
      select.dispatchEvent(ev);
    }

    function onKey(e) {
      if (select.disabled) return;
      if (!open) items = options().map((op) => ({ label: labelOf(op), disabled: !!op.disabled, value: op.value }));
      const typing = !!buf && now() - bufAt < 600;
      const act = keyAction(e.key, {
        open, active, selected: selectedIndex(), n: items.length,
        disabled: items.map((it) => it.disabled), alt: !!e.altKey, typing,
      });
      if (act.prevent) { e.preventDefault(); if (e.stopPropagation) e.stopPropagation(); }
      switch (act.type) {
        case "open": openList(act.index); break;
        case "move": if (act.index >= 0) setActive(act.index); break;
        case "select": commit(act.index); break;
        case "close": close(); break;
        case "type": {
          buf = (typing ? buf : "") + e.key;
          bufAt = now();
          const from = open ? active : selectedIndex();
          const hit = typeahead(items.map((it) => it.label), buf, from, items.map((it) => it.disabled));
          if (!open) openList(hit >= 0 ? hit : undefined);
          else if (hit >= 0) setActive(hit);
          break;
        }
        default: break;
      }
    }

    btn.addEventListener("click", () => {
      if (open) { close(); return; }
      // WebKit does not always focus a clicked button; the keyboard needs it.
      try { btn.focus(); } catch (e) { /* ignore */ }
      openList();
    });
    btn.addEventListener("keydown", onKey);
    btn.addEventListener("blur", () => close());

    const ctrl = {
      button: btn, select,
      get open() { return open; },
      get active() { return active; },
      get popup() { return pop; },
      openList, close, refresh() { syncLabel(); if (open) { render(); position(); setActive(selectedIndex()); } },
      key: onKey,
    };
    hookProp(select, "value", () => { syncLabel(); if (open) ctrl.refresh(); });
    hookProp(select, "disabled", () => { syncLabel(); if (select.disabled) close(); });
    select.focus = () => btn.focus();
    try {
      const MO = win.MutationObserver;
      if (MO) new MO(() => ctrl.refresh()).observe(select, { childList: true, subtree: true, characterData: true, attributes: true });
    } catch (e) { /* refresh() still works */ }
    select._otListbox = ctrl;
    syncLabel();
    return ctrl;
  }

  // Close every open listbox (panel switch).
  function closeAll() { Array.from(live).forEach((c) => c.close()); }
  const of = (select) => (select && select._otListbox) || null;

  const OTListbox = { place, keyAction, typeahead, step, enhance, closeAll, of };
  root.OTListbox = OTListbox;
  if (typeof module !== "undefined" && module.exports) module.exports = OTListbox;
})(typeof window !== "undefined" ? window : globalThis);
