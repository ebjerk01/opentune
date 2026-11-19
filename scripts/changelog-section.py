#!/usr/bin/env python3
"""Print one version's section of CHANGELOG.md (Keep a Changelog layout).

    scripts/changelog-section.py 0.6.0 [CHANGELOG.md]

Prints the body under ``## [0.6.0] ...`` up to the next ``## [`` heading,
without the heading itself and without trailing link definitions. Exits 1 with
a message on stderr when the version has no section (the release workflow then
falls back to the generated notes only). A leading ``v`` is accepted.
"""
import re
import sys
from pathlib import Path


def section(text: str, version: str) -> str | None:
    version = version.removeprefix("v")
    head = re.compile(r"^## \[" + re.escape(version) + r"\]")
    out, inside = [], False
    for line in text.splitlines():
        if line.startswith("## ["):
            if inside:
                break
            inside = bool(head.match(line))
            continue
        if inside:
            if re.match(r"^\[[^\]]+\]: ", line):
                continue  # link reference definitions at the file end
            out.append(line)
    if not inside and not out:
        return None
    body = "\n".join(out).strip()
    return body or None


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(__doc__, file=sys.stderr)
        return 2
    path = Path(sys.argv[2] if len(sys.argv) == 3 else "CHANGELOG.md")
    body = section(path.read_text(encoding="utf-8"), sys.argv[1])
    if body is None:
        print(f"no section for {sys.argv[1]} in {path}", file=sys.stderr)
        return 1
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
