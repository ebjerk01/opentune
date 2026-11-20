#!/usr/bin/env python3
"""Regenerate opentune.ico (the Windows exe icon) from the 512 px master.

    python3 native/packaging/windows/make-ico.py

Needs Pillow. The .ico is committed, so a normal build needs no image
tooling; rerun this only when native/packaging/opentune.png changes.
Every size is downscaled from the master with Lanczos (not chained), so
the small entries stay as sharp as the source allows.
"""
from pathlib import Path

from PIL import Image

SIZES = [16, 24, 32, 48, 64, 128, 256]

here = Path(__file__).resolve().parent
master = Image.open(here.parent / "opentune.png").convert("RGBA")
frames = [master.resize((s, s), Image.LANCZOS) for s in SIZES]
# Pillow writes the frame whose size matches each entry in `sizes`.
frames[-1].save(
    here / "opentune.ico",
    format="ICO",
    sizes=[(s, s) for s in SIZES],
    append_images=frames[:-1],
)
print(here / "opentune.ico")
