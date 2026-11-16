# OpenTune sample fixtures

Small, **100% synthetic** files so the apps and CLI have something real to open
on a fresh clone — fully offline, no hardware, no real ECU data.

> These are fabricated. They are **not** ECU dumps, contain no proprietary data,
> and must never be flashed to anything. They exist only to demonstrate the
> offline workflow and to anchor documentation/screenshots.

## `opentune-demo-edc16.bin` + `opentune-demo-edc16.json`

A 64 KiB toy image in the Bosch **EDC16 (MPC56x)** shape, paired with a matching
ECU definition.

What is inside the image (all made up):

- **A valid checksum directory.** One protected block `[0x0100‥0x7FFF]` with the
  `0xFADECAFE`/`0xCAFEAFFE` directory record at `0x40`. The block's 32-bit
  big-endian word sum equals the Bosch constant `0xD01FE500`, exactly as the
  real EDC16 engine (`crates/opentune-checksum/src/edc16.rs`) expects. The last
  dword of the block is the compensation word.
- **Identity strings** an offline scan can pick up: an `EPK` descriptor marker,
  a VAG-shaped part number `03G906021AB`, and a software number `1037516722`.
  These match the definition's `identify` keys (`03G906`, `1037`).
- **One calibration map**, an 8×8 injection-quantity limiter at `0x2000`
  (`u16` big-endian, `×0.01 mg/stroke`) with stored air-mass (X, `0x1F00`) and
  engine-speed (Y, `0x1F20`) axes. It decodes to a monotonic ~14‥53 mg/stroke
  grid.

### Try it

```sh
# Verify the checksum directory (expect: OK, computed == 0xD01FE500)
opentune checksum verify --algo edc16 samples/opentune-demo-edc16.bin

# Corrupt-then-fix round trip (on a copy):
cp samples/opentune-demo-edc16.bin /tmp/demo.bin
printf '\xFF' | dd of=/tmp/demo.bin bs=1 seek=8208 count=1 conv=notrunc 2>/dev/null
opentune checksum verify --algo edc16 /tmp/demo.bin   # BAD
opentune checksum fix    --algo edc16 /tmp/demo.bin   # rewrites the comp word
opentune checksum verify --algo edc16 /tmp/demo.bin   # OK

# Load the definition and decode the demo map through it:
opentune ecudefs list samples
opentune compare --ecudefs samples --def opentune-demo-edc16 \
    samples/opentune-demo-edc16.bin samples/opentune-demo-edc16.bin --map injection
```

In the desktop and Android apps, open the `.bin` and point the definition at
`opentune-demo-edc16.json` to exercise the **Analyze / Identify / Checksum /
Maps** screens.

### Regenerating

The image is produced deterministically by `samples/generate.py`:

```sh
python3 samples/generate.py samples/opentune-demo-edc16.bin
```

If you change the geometry, keep the `.json` addresses in sync with the script,
then re-run `checksum verify` to confirm the block still sums to `0xD01FE500`.
