#!/usr/bin/env python3
"""Generate a SMALL SYNTHETIC EDC16-style demo flash image with a valid MPC56x
checksum directory, plus identity strings and one calibration map, so the
OpenTune apps/CLI have something real to open offline.

This is NOT a real ECU dump: every byte is fabricated. The checksum scheme
matches crates/opentune-checksum/src/edc16.rs (32-bit big-endian additive sum
of each directory block == 0xD01FE500, compensation word in the block's last
dword).
"""
import struct
import sys

LEN = 0x10000  # 64 KiB, power of two (required by the EDC16 offset mapping)
K = 0xD01FE500  # EDC16_MPC_SUM_CONST
MARK_CS = bytes([0xFA, 0xDE, 0xCA, 0xFE])
MARK_CP = bytes([0xCA, 0xFE, 0xAF, 0xFE])

BLK_START = 0x0100
BLK_END = 0x7FFF  # inclusive; (end+1) dword-aligned

# Map geometry (kept in sync with samples/opentune-demo-edc16.json)
X_ADDR, X_N = 0x1F00, 8          # air mass axis, u16 BE, *0.1 mg
Y_ADDR, Y_N = 0x1F20, 8          # engine speed axis, u16 BE, *1 rpm
DATA_ADDR = 0x2000               # 8x8 u16 BE cells, *0.01 mg/stroke

img = bytearray(LEN)

# --- identity header (scanned by opentune-scan; outside the summed block) ---
header = b"EPK=OPENTUNE-DEMO EDC16 SYNTHETIC\n03G906021AB 1037516722\n"
img[0xA0:0xA0 + len(header)] = header

# --- checksum directory record for the one protected block ---
rec = 0x40
img[rec:rec + 4] = struct.pack(">I", BLK_START)
img[rec + 4:rec + 8] = struct.pack(">I", BLK_END)
img[rec + 8:rec + 12] = MARK_CS
img[rec + 12:rec + 16] = MARK_CP

# --- axes ---
air_mass = [100, 200, 350, 500, 700, 900, 1100, 1300]      # *0.1 => 10..130 mg
rpm = [800, 1200, 1600, 2000, 2600, 3200, 3800, 4400]      # rpm
for k, v in enumerate(air_mass):
    struct.pack_into(">H", img, X_ADDR + 2 * k, v)
for k, v in enumerate(rpm):
    struct.pack_into(">H", img, Y_ADDR + 2 * k, v)

# --- 8x8 injection-quantity cells, raw*0.01 mg/stroke, rising & monotonic ---
for r in range(Y_N):          # row = rpm
    for c in range(X_N):      # col = air mass
        raw = 1400 + c * 450 + r * 120   # physical 14.0 .. ~52.7 mg
        struct.pack_into(">H", img, DATA_ADDR + 2 * (r * X_N + c), raw)

# --- compute the compensation word so the block sums to K (big-endian) ---
def sum32be(buf, start, end_inclusive):
    hi = min(end_inclusive + 1, len(buf))
    acc = 0
    i = start
    while i + 4 <= hi:
        acc = (acc + struct.unpack_from(">I", buf, i)[0]) & 0xFFFFFFFF
        i += 4
    return acc

store = BLK_END - 3                      # 0x7FFC
rest = sum32be(img, BLK_START, BLK_END - 4)  # sum excluding the last dword
want = (K - rest) & 0xFFFFFFFF
struct.pack_into(">I", img, store, want)

# --- sanity: block now sums to K, and exactly one directory record exists ---
assert sum32be(img, BLK_START, BLK_END) == K, "block does not sum to constant"
hits = 0
i = 0
while i + 8 <= LEN:
    if img[i:i + 4] == MARK_CS and img[i + 4:i + 8] == MARK_CP:
        hits += 1
    i += 4
assert hits == 1, f"expected 1 directory marker, found {hits}"

out = sys.argv[1]
with open(out, "wb") as f:
    f.write(img)
print(f"wrote {out} ({LEN} bytes)")
print(f"block [0x{BLK_START:04X}..0x{BLK_END:04X}] comp word @0x{store:04X} = 0x{want:08X}")
