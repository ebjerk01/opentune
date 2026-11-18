# C167 / ST10 bootstrap-mode read (EDC15)

> **Status: 🔒 offline only.** The planner, protocol engine and simulator are
> built and tested against a simulated C167 BSL. The read has **not** been run
> on a real ECU. `opentune bsl read` refuses to run without `--i-understand`.
> **Read only:** nothing here writes or erases flash or EEPROM.

Crate: `opentune-c167bsl`. CLI: `opentune bsl presets | plan | read`.
Sources and licences: [PRIOR-ART.md](PRIOR-ART.md#c166--c167-bootstrap-loader-edc15-bootstrap-mode-read).

## Why

Bosch EDC15 boards (VAG EDC15VM+/P+, Mercedes EDC15C6) use a C167-family
CPU. Its mask-ROM bootstrap loader (BSL) runs before any ECU software, so a
bench read works when the KWP2000 programming session is locked or the
software no longer boots. This is ROADMAP follow-up (2) under EDC15C6.

## The sequence

Every step below is what `opentune bsl plan` prints and what `read`
executes, in the same order (the engine runs the plan's steps).

| # | Host sends | ECU answers | Notes |
|---|---|---|---|
| 1 | `00` (zero byte) | ID byte | The BSL times start bit + 8 zero bits with Timer 6 and sets its baud. ID: `55` 8xC166, `A5` early C167, `B5` early C165, `C5` C167 derivatives, `D5` devices with ID registers. EDC15 presets accept `A5 C5 D5`. `AA` instead = a monitor kernel is already running → steps 2–3 skipped. |
| 2 | 32-byte primary loader | `01` | The BSL stores exactly 32 bytes at `00'FA40h` and jumps there. Fewer bytes = it waits forever. |
| 3 | monitor kernel | `03` | Pulled in by the loader (MiniMon's loader takes 394 bytes to `00'FA60h`). |
| 4 | `93` | `AA EA` | Test communication. |
| 5 | per register: `82` → `AA`; `addr24 word` → `EA`; `CD` → `AA`; `addr24` → `lo hi EA` | | Bus / chip-select setup, each value read back and compared. Volatile SFRs only. `--no-setup` skips it. |
| 6 | per block: `85` → `AA`; `addr24 len16` → `len` bytes + `EA`; `33` → `AA chk EA` | | `chk` = XOR of the block, verified. Blocks of up to `0x200`. |

All multi-byte values are little-endian; addresses are 24-bit. On a K-line
every byte the host sends comes back as an echo; the engine reads exactly
that many bytes and compares them before reading the reply (`--no-echo` for a
full-duplex TTL/RS232 link straight to RxD0/TxD0). Any mismatch, wrong ID,
missing acknowledge, wrong read-back, bad block checksum or timeout stops the
read with the step number; `--transcript FILE` keeps every send/echo/reply.
A reply of `FD` is a NAK; NefMotoOpenSource reports it with CH340 cables and
marginal bauds (try 38400 or an FTDI cable).

## Loader and kernel: user-supplied

OpenTune ships **no** loader. The pair the public tools use is Infineon
MiniMon's K-line loader + kernel (`LOADK.bin` / `MINIMONK.bin`), Infineon
freeware distributed without source and not under the GPL. Pass them with
`--loader` and `--kernel`. `plan` recognises them by SHA-256 and then checks
that the kernel is the 394 bytes that loader expects. Any loader/kernel that
speaks the protocol above works. A loader must be exactly 32 bytes.

## Presets

`opentune bsl presets` prints them with decoded register values.

| Preset | Region | CPU address | Default length | Setup |
|---|---|---|---|---|
| `edc15vm-p` | `ext-flash` | `0x400000` (1 MiB CS3 window) | 512 KiB | `SYSCON E204h`, `BUSCON0 04ADh`, `ADDRSEL3 4008h`, `BUSCON1 040Dh`, `BUSCON3 848Eh`, other windows/buses off |
| `edc15c6` | `ext-flash` | `0x400000` | 1 MiB | same as above (assumed for C6) |
| all three | `int-rom` | `0x010000` (32 KiB) | 32 KiB | `SYSCON F604h` (`ROMS1=1`, `ROMEN=1`), buses off |

Register values are C167BootTool's (GPL-3.0): its EDC15 values, in the order
NefMotoOpenSource (GPL-3.0) programs them for an EDC15 external-flash read,
and C167BootTool's internal-ROM read. In BSL mode data reads of the internal address space come
from the user ROM/flash (code fetches come from the Boot-ROM), which is what
makes the internal read possible. On a ROMless (`-LM`) part the internal read
returns undefined data. None of the regions is hardware-verified.

## Bench setup (public material only)

* **Power** the ECU on the bench as for a K-line session: battery +12 V,
  ground, ignition (terminal 15). Use the board's public pinout.
* **Bootstrap strap.** Pull `P0L.4` low through a hardware reset (power-up):
  the CPU samples it at the end of reset. The application note's typical
  pull-down is ≈ 8 kΩ to ground; `P0L.2`, `P0L.3` and `P0L.5` must stay high.
  Release the strap after the BSL has started (public tools: 2–3 s after
  power-up); leaving it on can disturb the bus. Where the `P0L.4` net is
  reachable on an EDC15 board (pad, flash data/address pin) is board-specific:
  OpenTune documents no pad location it has not verified — use a public
  pinout for your board. A software reset ignores the strap; a hardware reset
  without it leaves BSL mode.
* **Link.** The BSL talks on ASC0 (`RxD0` = `P3.11`, `TxD0` = `P3.10`). On
  EDC15 these reach the K-line through the board's transceiver, so a KKL
  cable (FTDI preferred) on the diagnostic K-line pin works, at K-line (12 V)
  levels, half-duplex with echo. Never put RS232 (±12 V) or K-line levels on
  the CPU pins; a direct tap needs 5 V TTL and `--no-echo`.
* **Baud.** Pick one the BSL can reproduce: `B = fCPU / (32·(S0BRL+1))` with
  `S0BRL = ⌊⌊9·fCPU/(4·Bhost)⌋/72⌋ − 1` must be within ±2.5 % of the host
  baud. `plan` prints the deviation for `--fcpu-mhz`, or for 16/20/24/32 MHz
  when the clock is unknown; `read` refuses a baud that is out of tolerance
  when `--fcpu-mhz` is given. Example: 57600 is 0.16 % off at 24 MHz but
  7.8 % off at 20 MHz. Public tools use 57600/38400 on KKL cables.
* The application note warns that the BSL itself is not meant for
  half-duplex links (its own ID byte could be taken as the first loader
  byte). The public tools use the K-line regardless; this is part of what is
  unverified here.

## CLI

```text
opentune bsl presets
opentune bsl plan --preset edc15c6 [--region ext-flash|int-rom] [--offset N] [--len N]
                  [--loader LOADK.bin --kernel MINIMONK.bin] [--baud 38400] [--fcpu-mhz 20]
                  [--no-echo] [--no-setup] [--all-blocks]
opentune bsl read --preset edc15c6 --port /dev/ttyUSB0 --baud 38400
                  --loader LOADK.bin --kernel MINIMONK.bin --out edc15c6.bin
                  [--transcript bsl.log] [--timeout-ms 1500] --i-understand
opentune bsl read --preset edc15c6 --port sim --len 0x1000 --out sim.bin --i-understand
```

`plan` opens no port and needs no files (missing payloads are shown as
placeholders); the middle block reads are elided unless `--all-blocks`.
`--port sim` runs the read against the simulated C167 with a synthetic
pattern (a synthetic loader/kernel is used when none is given).

## Open

* First hardware run on an EDC15 bench (ID byte, loader echo, setup
  read-back, then a short `--len` read compared against a known KWP read).
* EDC15C6 bus setup is assumed equal to EDC15VM+/P+.
* The EEPROM (24C0x over I2C on EDC15) and any write/erase are out of scope.
* An own, GPL loader + kernel (assembly source + bytes) would remove the
  MiniMon dependency; it needs a C166 assembler and hardware to verify.
