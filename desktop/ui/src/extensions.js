// OpenTune — extension loader. Loaded LAST by index.html.
// An embedding build (e.g. a private edition) replaces the list below with its
// own extra panel scripts; they load in order after every built-in panel and
// register themselves with window.OTNav like any other panel.
"use strict";
window.OT_EXTENSIONS = window.OT_EXTENSIONS || [];
(function load(list, i) {
  if (i >= list.length) return;
  const s = document.createElement("script");
  s.src = "./src/" + list[i];
  s.onload = s.onerror = () => load(list, i + 1);
  document.body.appendChild(s);
})(window.OT_EXTENSIONS, 0);
