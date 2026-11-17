#!/usr/bin/env python3
"""Generate a SMALL SYNTHETIC datalog for the desktop Log viewer.

Not recorded from any car: a made-up drive cycle (idle, two pulls through the
gears, cruise, lift-off, idle) computed from simple formulas. Channel names and
units match the demo map in opentune-demo-edc16.json (air mass on X, engine
speed on Y), so the Log viewer's map overlay works on a fresh clone:

    samples/opentune-demo-log.csv  +  samples/opentune-demo-edc16.json
                                   +  samples/opentune-demo-edc16.bin

Deterministic: re-running produces the same file.
"""
import math
import sys

HZ = 20
SECONDS = 60


def rpm_at(t):
    if t < 5:
        return 820.0
    if t < 15:  # first pull: 2nd gear 1200 -> 4300
        return 1200.0 + (t - 5) / 10 * 3100
    if t < 16:  # shift
        return 4300.0 - (t - 15) * 1500
    if t < 26:  # second pull: 3rd gear 2800 -> 4500 (runs off the 4400 axis end)
        return 2800.0 + (t - 16) / 10 * 1700
    if t < 45:  # cruise with small waves
        return 2200.0 + 120 * math.sin(t * 0.9)
    if t < 52:  # lift-off, overrun
        return 2200.0 - (t - 45) / 7 * 1300
    return 840.0 + 15 * math.sin(t * 3)


def load_at(t):
    """0..1 driver demand."""
    if t < 5 or t >= 52:
        return 0.05
    if t < 15 or 16 <= t < 26:
        return 1.0
    if t < 16:
        return 0.1
    if t < 45:
        return 0.35 + 0.05 * math.sin(t * 0.5)
    return 0.0


def main(out):
    lines = ["Time (s),engine speed [rpm],air mass [mg/stroke],injection quantity [mg/stroke],boost pressure [hPa],coolant temp [°C]"]
    n = HZ * SECONDS
    for i in range(n + 1):
        t = i / HZ
        rpm = rpm_at(t)
        load = load_at(t)
        boost = 1000 + load * min(1.0, max(0.0, (rpm - 1300) / 1200)) * 1250
        air = 9.0 + 0.085 * (boost - 1000) + load * 6 + 1.5 * math.sin(i * 0.7)
        iq = max(0.0, 3.5 + load * 48 * min(1.0, air / 90))
        if load == 0.0:
            iq = 0.0  # overrun fuel cut
        coolant = 78 + 12 * (1 - math.exp(-t / 25))
        lines.append(f"{t:.2f},{rpm:.0f},{air:.1f},{iq:.2f},{boost:.0f},{coolant:.1f}")
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "opentune-demo-log.csv")
