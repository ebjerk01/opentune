#!/usr/bin/env python3
# Run as:  python3 -I scripts/harvest_opendbc.py <opendbc_repo> <out_dir>
#
# Harvest comma.ai opendbc ECU firmware-version fingerprints into OpenTune
# `CatalogEntry` JSON files (one per car brand/port).
#
# SAFETY: opendbc is treated as untrusted data. This script *parses* its Python
# sources as text with the `ast` module. It never imports or executes any
# opendbc module. Always run it with `python3 -I` (isolated mode: no cwd/user
# site-packages on sys.path, env ignored) so a planted `json.py` etc. cannot be
# imported.
#
# Source data: opendbc `car/<brand>/fingerprints.py` (`FW_VERSIONS` dict) and
# `car/<brand>/values.py` (`class CAR(Platforms)` with per-platform `CarDocs`).
# opendbc is MIT licensed (comma.ai); OpenTune is GPL-3.0 and MIT is
# reuse-compatible. See docs/IDENTIFICATION.md for attribution.

import ast
import json
import os
import re
import sys
from collections import Counter

# opendbc brands whose make string isn't just the capitalised port name, or
# where a port has no CarDocs to derive a make from. Used only as a fallback.
BRAND_MAKE_FALLBACK = {
    "volkswagen": "Volkswagen",
    "gm": "GM",
    "toyota": "Toyota",
    "ford": "Ford",
    "honda": "Honda",
    "hyundai": "Hyundai",
    "chrysler": "Chrysler",
    "mazda": "Mazda",
    "subaru": "Subaru",
    "nissan": "Nissan",
    "tesla": "Tesla",
    "rivian": "Rivian",
    "psa": "PSA",
    "byd": "BYD",
    "mg": "MG",
    "body": "comma",
}

YEAR_RE = re.compile(r"\b((?:19|20)\d{2})(?:-(\d{4}|\d{2}))?\s*$")
# A firmware "part number": a run of [A-Za-z0-9-], length >= 6, with a digit.
PARTNO_RE = re.compile(rb"[A-Za-z0-9][A-Za-z0-9-]{5,}")


def decode_fw(b: bytes) -> str:
    """Decode a firmware byte string to a stable, readable form: printable
    ASCII kept verbatim, every other byte shown as \\xNN."""
    out = []
    for c in b:
        if 0x20 <= c <= 0x7E:
            out.append(chr(c))
        else:
            out.append("\\x%02x" % c)
    return "".join(out)


def extract_part_numbers(b: bytes) -> list[str]:
    pns = []
    for m in PARTNO_RE.findall(b):
        s = m.decode("ascii")
        if any(ch.isdigit() for ch in s):
            pns.append(s)
    return pns


def parse_doc_name(name: str):
    """'Volkswagen Golf GTI 2015-21' -> ('Volkswagen', 'Golf GTI', (2015, 2021))."""
    years = None
    m = YEAR_RE.search(name)
    label = name
    if m:
        label = name[: m.start()].strip()
        start = int(m.group(1))
        if m.group(2):
            end_raw = m.group(2)
            end = int(end_raw) if len(end_raw) == 4 else (start // 100) * 100 + int(end_raw)
        else:
            end = start
        years = (start, end)
    parts = label.split()
    make = parts[0] if parts else ""
    model = " ".join(parts[1:]) if len(parts) > 1 else label
    return make, model, years


def load_platform_docs(values_path: str) -> dict:
    """Parse values.py -> {PLATFORM_NAME: {'makes': [...], 'models': [...],
    'years': (min,max) | None}} using ast (no execution)."""
    with open(values_path, "r", encoding="utf-8") as f:
        tree = ast.parse(f.read(), filename=values_path)

    car_cls = None
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == "CAR":
            car_cls = node
            break
    if car_cls is None:
        return {}

    def doc_calls(arg):
        """Yield Call nodes that look like CarDocs(...) from a PlatformConfig
        first positional arg (a list of calls, or a single call)."""
        if isinstance(arg, ast.List):
            for e in arg.elts:
                if isinstance(e, ast.Call):
                    yield e
        elif isinstance(arg, ast.Call):
            yield arg

    platforms = {}
    for node in car_cls.body:
        if not isinstance(node, ast.Assign):
            continue
        if len(node.targets) != 1 or not isinstance(node.targets[0], ast.Name):
            continue
        if not isinstance(node.value, ast.Call) or not node.value.args:
            continue
        pname = node.targets[0].id
        makes, models, year_spans = [], [], []
        for call in doc_calls(node.value.args[0]):
            if not call.args:
                continue
            first = call.args[0]
            if not (isinstance(first, ast.Constant) and isinstance(first.value, str)):
                continue
            make, model, years = parse_doc_name(first.value)
            if make:
                makes.append(make)
            full = (make + " " + model).strip()
            if full:
                models.append(full)
            if years:
                year_spans.append(years)
        yr = None
        if year_spans:
            lo = min(s for s, _ in year_spans)
            hi = max(e for _, e in year_spans)
            yr = str(lo) if lo == hi else f"{lo}-{hi}"
        platforms[pname] = {"makes": makes, "models": models, "years": yr}
    return platforms


def load_fw_versions(fp_path: str) -> dict:
    """Parse fingerprints.py -> {PLATFORM_NAME: {ecu_type: {'addresses': set,
    'fw': [str], 'part_numbers': set}}} using ast (no execution)."""
    with open(fp_path, "r", encoding="utf-8") as f:
        tree = ast.parse(f.read(), filename=fp_path)

    fw_node = None
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name) and t.id == "FW_VERSIONS":
                    fw_node = node.value
    if fw_node is None or not isinstance(fw_node, ast.Dict):
        return {}

    result = {}
    for pkey, pval in zip(fw_node.keys, fw_node.values):
        if not (isinstance(pkey, ast.Attribute) and isinstance(pkey.value, ast.Name)
                and pkey.value.id == "CAR"):
            continue
        if not isinstance(pval, ast.Dict):
            continue
        pname = pkey.attr
        ecus = {}
        for ekey, elist in zip(pval.keys, pval.values):
            if not (isinstance(ekey, ast.Tuple) and ekey.elts):
                continue
            ecu_el = ekey.elts[0]
            if not (isinstance(ecu_el, ast.Attribute)):
                continue
            ecu_type = ecu_el.attr
            addr = None
            if len(ekey.elts) > 1 and isinstance(ekey.elts[1], ast.Constant) \
                    and isinstance(ekey.elts[1].value, int):
                addr = ekey.elts[1].value
            if not isinstance(elist, ast.List):
                continue
            slot = ecus.setdefault(ecu_type, {"addresses": set(), "fw": [], "part_numbers": set()})
            if addr is not None:
                slot["addresses"].add("0x%x" % addr)
            for item in elist.elts:
                if isinstance(item, ast.Constant) and isinstance(item.value, bytes):
                    b = item.value
                    slot["fw"].append(decode_fw(b))
                    slot["part_numbers"].update(extract_part_numbers(b))
        if ecus:
            result[pname] = ecus
    return result


def build_entries(brand: str, car_dir: str) -> list[dict]:
    fp_path = os.path.join(car_dir, "fingerprints.py")
    values_path = os.path.join(car_dir, "values.py")
    if not os.path.exists(fp_path):
        return []
    fw = load_fw_versions(fp_path)
    docs = load_platform_docs(values_path) if os.path.exists(values_path) else {}
    fallback_make = BRAND_MAKE_FALLBACK.get(brand, brand.capitalize())

    entries = []
    for pname in sorted(fw):
        meta = docs.get(pname, {})
        makes = meta.get("makes") or []
        make = Counter(makes).most_common(1)[0][0] if makes else fallback_make
        # models = human car names from CarDocs; fall back to the raw opendbc
        # platform token when a platform has no CarDocs, so no fingerprint group
        # loses its identifier.
        models = meta.get("models") or [pname]
        years = meta.get("years")
        for ecu_type in sorted(fw[pname]):
            slot = fw[pname][ecu_type]
            fw_versions = sorted(set(slot["fw"]))
            if not fw_versions:
                continue
            entry = {
                "make": make,
                "models": models,
                "ecu": ecu_type,
                "addresses": sorted(slot["addresses"]),
                "fw_versions": fw_versions,
                "part_numbers": sorted(slot["part_numbers"]),
                "family": None,
                "ecudef_id": None,
                "source": "opendbc",
            }
            if years:
                entry["years"] = years
            entries.append(entry)
    return entries


def main():
    if len(sys.argv) != 3:
        print("usage: python3 -I scripts/harvest_opendbc.py <opendbc_repo> <out_dir>",
              file=sys.stderr)
        return 2
    repo, out_dir = sys.argv[1], sys.argv[2]
    car_root = os.path.join(repo, "opendbc", "car")
    if not os.path.isdir(car_root):
        print(f"error: {car_root} not found (is this an opendbc checkout?)", file=sys.stderr)
        return 1
    os.makedirs(out_dir, exist_ok=True)

    total_entries = 0
    total_files = 0
    summary = []
    for brand in sorted(os.listdir(car_root)):
        car_dir = os.path.join(car_root, brand)
        if not os.path.isdir(car_dir):
            continue
        entries = build_entries(brand, car_dir)
        if not entries:
            continue
        out_path = os.path.join(out_dir, f"opendbc-{brand}.json")
        with open(out_path, "w", encoding="utf-8") as f:
            json.dump(entries, f, ensure_ascii=False, indent=2, sort_keys=True)
            f.write("\n")
        total_entries += len(entries)
        total_files += 1
        summary.append((brand, len(entries)))

    for brand, n in summary:
        print(f"  {brand:12s} {n:5d} entries")
    print(f"TOTAL: {total_entries} entries across {total_files} files -> {out_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
