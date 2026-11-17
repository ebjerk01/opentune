#!/usr/bin/env python3
"""Bundle the desktop UI into ONE self-contained HTML page for a browser preview.

Inlines desktop/ui (index.html, styles.css, the feature scripts in load order)
plus preview/mock.js, which stands in for the Tauri backend with demo data. The
real UI files are used unchanged, so what the preview shows is what the app
ships. Output has no <html>/<head>/<body> wrapper (the artifact host adds one).

    python3 -I desktop/preview/build.py OUT.html [DESKTOP_DIR]

DESKTOP_DIR defaults to this repo's desktop/; pass another app's desktop/ dir
(same ui/ layout) to preview it. Guide pages come from DESKTOP_DIR/src-tauri/guide
when present, else from this repo's.
"""
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OWN_DESKTOP = HERE.parent
REPO = OWN_DESKTOP.parent
DESKTOP = OWN_DESKTOP
UI = DESKTOP / "ui"


def theme_aware(css: str) -> str:
    """Make the app's prefers-color-scheme tokens honour an explicit theme toggle."""
    m = re.search(r"@media \(prefers-color-scheme: dark\) \{\s*:root \{(.*?)\n  \}\n\}", css, re.S)
    if not m:
        return css
    dark = m.group(1)
    guarded = (
        "@media (prefers-color-scheme: dark) {\n  :root:not([data-theme=\"light\"]) {"
        + dark + "\n    color-scheme: dark;\n  }\n}\n"
        ":root[data-theme=\"dark\"] {" + dark + "\n  color-scheme: dark;\n}\n"
        ":root[data-theme=\"light\"] { color-scheme: light; }"
    )
    return css[: m.start()] + guarded + css[m.end():]


# Preview-only layout fixes: fill the artifact frame instead of 100vh, and keep
# navigation at phone width (the app hides the sidebar below 760px).
PREVIEW_CSS = """
#app { height: 100%; }
.preview-ribbon { position: fixed; right: max(10px, env(safe-area-inset-right, 0px));
  bottom: max(10px, env(safe-area-inset-bottom, 0px)); z-index: 50; font: 600 11px/1 system-ui, sans-serif;
  letter-spacing: .06em; text-transform: uppercase; padding: 6px 9px; border-radius: 6px;
  background: var(--accent); color: #fff; opacity: .9; pointer-events: none; }
@media (max-width: 760px) {
  #app { grid-template-rows: auto 1fr; }
  #sidebar { display: flex; flex-direction: row; overflow-x: auto; padding: 8px max(12px, env(safe-area-inset-left, 0px)); }
  #sidebar .brand-text, #sidebar .nav-group-label, #sidebar .sidebar-foot { display: none; }
  #nav { display: flex; flex-direction: row; gap: 4px; }
  #nav .nav-item { white-space: nowrap; }
  #main { padding: 18px max(16px, env(safe-area-inset-right, 0px)) 60px max(16px, env(safe-area-inset-left, 0px)); min-width: 0; }
}
"""


def guides():
    out = []
    gdir = DESKTOP / "src-tauri" / "guide"
    if not gdir.is_dir():
        gdir = OWN_DESKTOP / "src-tauri" / "guide"
    for p in sorted(gdir.glob("*.md")):
        slug = p.stem.split("-", 1)[1] if "-" in p.stem else p.stem
        title = " ".join(w[:1].upper() + w[1:] for w in re.split(r"[-_]", slug))
        out.append({"id": p.stem, "title": title, "body": p.read_text()})
    return out


def ecudefs():
    out = []
    for p in sorted((REPO / "ecudefs").glob("*.json")):
        try:
            d = json.loads(p.read_text())
        except ValueError:
            continue
        out.append({
            "id": d.get("id", p.stem), "name": d.get("name", ""), "maker": d.get("maker", ""),
            "family": d.get("family", ""), "protocol": str(d.get("protocol", "")),
            "flash_kib": int(d.get("flash_size", 0)) // 1024, "checksum": d.get("checksum"),
            "seedkey": d.get("seedkey"), "maps": len(d.get("maps", [])), "notes": d.get("notes", ""),
        })
    return out


def safe_script(js: str) -> str:
    return js.replace("</script", "<\\/script")


def main(out_path: str) -> None:
    index = (UI / "index.html").read_text()
    m = re.search(r"<title>(.*?)</title>", index)
    title = (m.group(1) if m else "OpenTune") + " UI Preview"
    body = re.search(r"<body>(.*)</body>", index, re.S).group(1)
    scripts = re.findall(r'<script src="\./src/([^"]+)"></script>', body)
    body = re.sub(r'\s*<script src="[^"]+"></script>', "", body)
    css = theme_aware((UI / "src" / "styles.css").read_text())

    data = {"guides": guides(), "ecudefs": ecudefs()}
    parts = [
        f"<title>{title}</title>",
        '<meta name="claude:viewport-fit-sides" content="cover">',
        "<style>\n" + css + PREVIEW_CSS + "</style>",
        body.strip(),
        '<div class="preview-ribbon">Preview · demo data</div>',
        "<script>window.__PREVIEW__ = " + safe_script(json.dumps(data)) + ";</script>",
        "<script>\n" + safe_script((HERE / "mock.js").read_text()) + "\n</script>",
    ]
    for extra in sorted(HERE.glob("fixtures*.js")):
        parts.append("<script>\n" + safe_script(extra.read_text()) + "\n</script>")
    for name in scripts:
        parts.append(f"<script>/* {name} */\n" + safe_script((UI / "src" / name).read_text()) + "\n</script>")
    Path(out_path).write_text("\n".join(parts) + "\n")
    print(f"wrote {out_path}: {len(scripts)} app scripts, {len(data['guides'])} guide pages, {len(data['ecudefs'])} ecudefs")


if __name__ == "__main__":
    if len(sys.argv) > 2:
        DESKTOP = Path(sys.argv[2]).resolve()
        UI = DESKTOP / "ui"
    main(sys.argv[1] if len(sys.argv) > 1 else "opentune-ui-preview.html")
