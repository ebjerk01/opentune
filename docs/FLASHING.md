# Flashing a control module over UDS

OpenTune has two write paths:

| Path | What it does | Entry point |
|---|---|---|
| **Region flash** (default `opentune flash`) | Writes one memory region through the driver's own block write (`Interface::write_memory`), with backup → write → read-back verify, optionally chunked and resumable. | `opentune_flash::flash_calibration*` |
| **UDS module flash** (`opentune flash --uds-module`) | Programs a control module over ISO 14229 from a **flash container** (VBF, ODX/ODX-F, FRF, SGO) or a plain bin + layout: erase → RequestDownload → TransferData → RequestTransferExit → checksum routine per block, then dependency check and ECU reset. | `opentune_flash::module::flash_module` |

This page describes the second one. The per-service background (what each
UDS service means, how VAG engine ECUs use them) is in
[FLASHING-NOTES.md](FLASHING-NOTES.md).

> **Validation state: synthetic-tested.** The UDS module path is exercised end
> to end against the flashable-ECU model in `opentune-udssim` only. It has not
> programmed a real module. Real-vehicle flashing needs a proper CAN adapter —
> SocketCAN, gs_usb/candleLight, or a J2534 PassThru box — on a stable power
> supply. **Not an ELM327**: its single-frame / buffer limits make multi-frame
> TransferData unreliable and an interrupted block leaves the module without a
> valid application.

## The sequence

`flash_module` runs these steps over any `DiagTransport` (a registry driver's
`Interface::diag()`):

1. **Validate** (nothing sent). Every selected block must be plain, addressed,
   non-empty and inside its erase region; erase regions may not partially
   overlap; writable regions, checksum coverage and a target identity are
   checked against the `FlashSafetyPolicy`.
2. **Identify** — `22 F187 / F191` (part, hardware) and `22 F188 / F189`
   (software). The connected ECU must match the image's target identity (ODX
   box codes, or `--expect-part` / `--expect-sw`). A mismatch — or an ECU that
   reports nothing — is refused unless `--force-identity`, which is logged.
3. **Voltage gate** — the policy's gate, fail-closed under production (an
   adapter that cannot read voltage is refused; `--min-mv 0` disables it).
4. **Backup** — extended session (`10 03`), then `23` ReadMemoryByAddress of
   every erase region (what the erase will destroy). If the ECU refuses reads,
   no backup is possible: the flash is refused unless `--no-backup-ok`, and
   the override is recorded. The backup is saved next to the state file
   *before* anything is erased and archived to the vault after success.
5. **Programming session** (`10 02`) and **SecurityAccess** (`27 11` /
   `27 12 <key>`) through the caller-supplied `SeedKey` hook (`--seedkey`). A
   locked ECU and no hook is refused here — before anything is erased.
6. **Per erase region**: `31 01 FF00 <ALFID addr size>` (ISO eraseMemory),
   then per block `34 00 <ALFID addr size>` → `36 <counter> <data>` in chunks of
   `maxNumberOfBlockLength − 2` (the ECU's length counts the SID and the
   counter) → `37` → read-back verify with `23` (when the ECU allows reads) →
   checksum routine `31 01 0202 <ALFID addr size crc16>`. A failure stops here,
   **before** the dependency check and the reset.
7. `31 01 FF01` (ISO checkProgrammingDependencies), then `11 01` ECUReset.

Block counters start at `01` and wrap `FF → 00`. If a TransferData response is
lost, the same counter is sent once more — ISO 14229-1 defines a repeat of the
previous counter as a retransmit the ECU acknowledges without writing again.
`responsePending` (NRC 0x78) during a slow erase is drained by the transport;
no request is ever re-sent automatically otherwise.

Routine ids, the security level and the checksum record layout are
configurable (`ModuleProtocol`; CLI `--security-level`, `--erase-routine`,
`--check-routine`, `--dependency-routine`). The defaults follow ISO 14229-1
where it defines a value (sessions, 0xFF00, 0xFF01); the checksum routine
0x0202 and its `<ALFID addr size crc16>` record are common practice, not a
standard — set them per module.

## Containers and block encodings

Only **plain** blocks are sent, with dataFormatIdentifier `0x00`. A block the
container stores compressed or encrypted *for the wire* is refused by name,
because the ECU expects the encoded bytes with a matching dataFormatIdentifier
and that encoder is not implemented:

| Container | Blocks | Erase regions | Identity |
|---|---|---|---|
| **VBF** | plain; refused if the header's `data_format_identifier` ≠ 0; a block whose CRC-16 does not match is refused | header `erase = { { start, length }, … }` | none in the file — give `--expect-part` / `--expect-sw` |
| **ODX / ODX-F** (also inside an FRF) | method `00` only; other methods are refused as *compressed* (`A0`, LZSS10 / LZZ / …) or *encrypted* (`0A`, …) | the block's own span | box codes → part numbers |
| **SGO** | refused (a KWP2000-era container; its UDS wire encoding is not implemented) | — | — |
| **plain bin** (`--data --addr --layout`) | plain | each `--layout ADDR:LEN` region | `--expect-part` / `--expect-sw` |

Keys for FRF / encrypted ODX blocks are caller-supplied (`--frf-key`);
OpenTune ships none.

## Safety policy

The CLI runs the module path under `FlashSafetyPolicy::production()`:

- **identity** — a target identity is required and must match (or be forced);
- **voltage** — required and readable (`--min-mv`, `0` disables);
- **backup** — required; refused reads need `--no-backup-ok`;
- **writable regions** — every erase region must lie inside a declared
  writable region: `--writable ADDR:LEN`, else the container's / layout's
  erase regions (a block with no declared erase region is refused);
- **checksum coverage** — every block needs either the container's own CRC or
  an ECU checksum routine;
- **verify** — read-back when the ECU allows reads, else the ECU's checksum
  routine; with neither, the flash is refused.

## Resume

Progress is recorded in a state file (`--resume FILE`, default
`<vault>/uds-module.state.json`) written atomically before every step that
changes the ECU: per-block status, the bound ECU fingerprint, a hash of the
exact image and block selection, the original backup (sidecar
`<state>.backup.bin`) and a transcript of events.

Running the same command again after an interruption resumes: the state must
match the image and the connected ECU (otherwise it is refused), the backup
comes from the sidecar (never re-read from half-written memory), erase
regions that finished are **confirmed on the device** (read-back, or the
checksum routine) rather than trusted, and the region that failed is erased
and written again. The state file is removed after success.

## CLI

```sh
# Dry run: every block and every request, nothing sent, no adapter opened.
opentune flash --uds-module --container module.vbf --expect-part 5Q0907000 --plan

# Flash (the seed-key id comes from the embedding build; OpenTune ships none).
opentune flash --uds-module --container module.vbf --expect-part 5Q0907000 \
    --driver socketcan --port can0 --tx-id 7E0 --rx-id 7E8 --seedkey <id>

# A plain bin with an explicit layout, only the second block.
opentune flash --uds-module --data app.bin --addr 0x10000 \
    --layout 0x10000:0x400,0x20000:0x200 --block 1 --expect-sw 0001 ...
```

The plan assumes the ECU answers positively, is locked, allows reads and
reports `--max-block-len` (default `0x0402`); under those assumptions it is
exactly the run's request sequence (pinned by a test).

## Simulator

`opentune-udssim` models a flashable ECU (`UdsServer::enable_flash`): the
programming session needs the extended session and its preconditions,
programming needs the configured security level, download needs erased memory,
the memory persists across resets and power losses, and faults can be injected
— power loss mid-transfer, a lost TransferData response, erase refused,
corrupted write, failing checksum / dependency routines, NRC 0x78 on erase.
The tests in `crates/opentune-flash/tests/module.rs` drive the whole sequence
against it.
