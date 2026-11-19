#!/usr/bin/env python3
"""Bundle the desktop UI into ONE self-contained HTML page for a browser preview.

Inlines desktop/ui (index.html, styles.css, the feature scripts in load order)
plus preview/mock.js, which stands in for the Tauri backend with demo data. The
real UI files are used unchanged, so what the preview shows is what the app
ships. Output has no <html>/<head>/<body> wrapper (the artifact host adds one).

    python3 -I desktop/preview/build.py OUT.html [DESKTOP_DIR] [--guides DIR] [--help-json FILE]

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
  #app { grid-template-rows: auto minmax(0, 1fr); }
  #sidebar { display: flex; flex-direction: row; overflow-x: auto; overflow-y: hidden; padding: 8px max(12px, env(safe-area-inset-left, 0px)); }
  #sidebar .brand-text, #sidebar .nav-group-label, #sidebar .sidebar-foot { display: none; }
  #nav { display: flex; flex-direction: row; gap: 4px; flex: none; min-height: 0; overflow: visible; }
  #nav .nav-group[data-collapsed] > .nav-item { display: flex; }
  #nav .nav-item { white-space: nowrap; }
  #main { padding: 18px max(16px, env(safe-area-inset-right, 0px)) 60px max(16px, env(safe-area-inset-left, 0px)); min-width: 0; }
}
"""


def guides():
    out = []
    gdir = GUIDES or DESKTOP / "src-tauri" / "guide"
    if not gdir.is_dir():
        gdir = OWN_DESKTOP / "src-tauri" / "guide"
    for p in sorted(gdir.glob("*.md")):
        slug = p.stem.split("-", 1)[1] if "-" in p.stem else p.stem
        title = " ".join(w[:1].upper() + w[1:] for w in re.split(r"[-_]", slug))
        out.append({"id": p.stem, "title": title, "body": p.read_text()})
    return out


# Map metadata bundled per definition for the Maps / Compare / Log viewer demo:
# layout only (names, sizes, axes, scaling) — the preview fills the cells with
# synthetic values. Capped so the page stays small (some defs list 1000+ maps).
MAP_META_CAP = 24


def axis_meta(a):
    if not isinstance(a, dict):
        return None
    vals = a.get("values") or []
    return {"name": a.get("name", ""), "unit": a.get("unit", ""), "count": a.get("count"),
            "values": vals if len(vals) <= 64 else []}


def map_meta(maps):
    # Prefer real tables (2-D maps, then curves) over single values.
    ranked = sorted(maps, key=lambda m: 0 if m.get("rows", 1) > 1 and m.get("cols", 1) > 1
                    else 1 if m.get("rows", 1) * m.get("cols", 1) > 1 else 2)
    out = []
    for m in ranked[:MAP_META_CAP]:
        sc = m.get("scaling") or {}
        out.append({
            "name": m.get("name", ""), "rows": m.get("rows", 1), "cols": m.get("cols", 1),
            "unit": m.get("unit", ""), "address": m.get("address", 0),
            "data_type": m.get("data_type", "u16"), "endian": m.get("endian", "big"),
            "scaling": {"factor": sc.get("factor", 1.0), "offset": sc.get("offset", 0.0)},
            "min": m.get("min"), "max": m.get("max"),
            "x_axis": axis_meta(m.get("x_axis")), "y_axis": axis_meta(m.get("y_axis")),
            "notes": (m.get("notes") or "")[:240],
        })
    return out


def ecudefs():
    out = []
    for p in sorted((REPO / "ecudefs").glob("*.json")):
        try:
            d = json.loads(p.read_text())
        except ValueError:
            continue
        ident = d.get("identify") or {}
        out.append({
            "id": d.get("id", p.stem), "name": d.get("name", ""), "maker": d.get("maker", ""),
            "family": d.get("family", ""), "protocol": str(d.get("protocol", "")),
            "flash_kib": int(d.get("flash_size", 0)) // 1024, "checksum": d.get("checksum"),
            "seedkey": d.get("seedkey"), "maps": len(d.get("maps", [])), "notes": d.get("notes", ""),
            "identify": {"part_numbers": ident.get("part_numbers") or [], "sw_numbers": ident.get("sw_numbers") or []},
            "map_meta": map_meta(d.get("maps", [])),
        })
    return out


PACK_KINDS = ["ecudefs", "coding", "adaptation", "services", "loggers", "modules", "controllers"]


def packs():
    """Every shipped definition pack (as the app's packs_list sees it: `_` / `.`
    folders and files skipped), plus each definition file keyed by the
    repo-relative path the preview's fixtures read. Preview-side the only check
    is that a file parses; the app validates each definition too."""
    files, index = {}, []
    root = REPO / "packs"
    for manifest in sorted(root.glob("*/pack.json")):
        pdir = manifest.parent
        if pdir.name.startswith(("_", ".")):
            continue
        try:
            m = json.loads(manifest.read_text())
        except ValueError:
            continue
        files[manifest.relative_to(REPO).as_posix()] = m
        defs, notes = [], []
        for kind in PACK_KINDS:
            for p in sorted((pdir / kind).glob("*.json")):
                if p.name == "TEMPLATE.json" or p.name.startswith(("_", ".")):
                    continue
                rel = p.relative_to(REPO).as_posix()
                try:
                    d = json.loads(p.read_text())
                except ValueError as e:
                    notes.append({"file": f"{kind}/{p.name}", "kind": kind, "message": f"parse error: {e}"})
                    continue
                files[rel] = d
                defs.append({"kind": kind, "file": f"{kind}/{p.name}", "path": rel,
                             "id": d.get("id") or p.stem, "name": d.get("name") or d.get("vehicle") or ""})
        index.append({
            "id": m.get("id", pdir.name), "name": m.get("name", ""), "version": m.get("version", ""),
            "maker": m.get("maker", ""), "platform": m.get("platform", ""), "vehicles": m.get("vehicles", []),
            "author": m.get("author", ""), "license": m.get("license", ""), "description": m.get("description", ""),
            "dir": pdir.relative_to(REPO).as_posix(), "source": "installed",
            "loaded": len(defs), "valid": not notes, "notes": notes, "defs": defs,
        })
    return files, index


def ctrldefs():
    """The built-in drivetrain controller definitions, keyed by their
    repo-relative path (what the preview's Controllers panel lists)."""
    out = []
    for p in sorted((REPO / "ctrldefs").glob("*.json")):
        if p.name == "TEMPLATE.json" or p.name.startswith(("_", ".")):
            continue
        try:
            out.append({"path": p.relative_to(REPO).as_posix(), "def": json.loads(p.read_text())})
        except ValueError:
            continue
    return out


def vcar(pack_files):
    """The virtual car's demo profile plus the definition packs' files, keyed by
    the repo-relative paths the preview's file dialog and pack picker hand out."""
    prof = REPO / "crates" / "drivers" / "vcar" / "profiles" / "demo.json"
    profile = json.loads(prof.read_text()) if prof.is_file() else None
    return {"profile": profile, "files": pack_files}


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

    units = []
    for p in sorted((REPO / "database" / "units").glob("*.json")):
        try:
            units.append(json.loads(p.read_text()))
        except ValueError:
            pass
    pack_files, pack_index = packs()
    data = {"guides": guides(), "ecudefs": ecudefs(), "units": units, "vcar": vcar(pack_files), "packs": pack_index,
            "ctrldefs": ctrldefs()}
    hj = HELP_JSON or UI / "src" / "panel-help.json"
    help_data = json.loads(hj.read_text()) if hj.is_file() else None
    parts = [
        f"<title>{title}</title>",
        '<meta name="claude:viewport-fit-sides" content="cover">',
        "<style>\n" + css + PREVIEW_CSS + "</style>",
        body.strip(),
        '<div class="preview-ribbon">Preview · demo data</div>',
        "<script>window.__PREVIEW__ = " + safe_script(json.dumps(data)) + ";"
        + ("window.__PANEL_HELP__ = " + safe_script(json.dumps(help_data)) + ";" if help_data else "")
        + "</script>",
        "<script>\n" + safe_script((HERE / "mock.js").read_text()) + "\n</script>",
    ]
    for extra in sorted(HERE.glob("fixtures*.js")):
        parts.append("<script>\n" + safe_script(extra.read_text()) + "\n</script>")
    for name in scripts:
        parts.append(f"<script>/* {name} */\n" + safe_script((UI / "src" / name).read_text()) + "\n</script>")
    Path(out_path).write_text("\n".join(parts) + "\n")
    print(f"wrote {out_path}: {len(scripts)} app scripts, {len(data['guides'])} guide pages, "
          f"{len(data['ecudefs'])} ecudefs, {len(data['packs'])} packs")


GUIDES = None
HELP_JSON = None

if __name__ == "__main__":
    args = sys.argv[1:]
    for flag in ("--guides", "--help-json"):
        if flag in args:
            i = args.index(flag)
            val = Path(args[i + 1]).resolve()
            del args[i:i + 2]
            if flag == "--guides":
                GUIDES = val
            else:
                HELP_JSON = val
    if len(args) > 1:
        DESKTOP = Path(args[1]).resolve()
        UI = DESKTOP / "ui"
    main(args[0] if args else "opentune-ui-preview.html")
