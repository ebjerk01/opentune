# Flashing Notes — Bosch EDC16 / EDC17 (VAG), end to end

This is the real, step-by-step sequence an OpenTune driver runs to reflash a
Bosch EDC16 (KWP2000) or EDC17 (UDS) engine ECU on a VAG vehicle, mapped to the
concrete service clients in `opentune-core`:

- UDS (ISO 14229): `core::protocol::uds::UdsClient` — EDC17, MEDC17, Simos, most
  2008+ CAN ECUs.
- KWP2000 (ISO 14230): `core::protocol::kwp2000::Kwp2000Client` — EDC15/EDC16
  over K-line or VW TP2.0.

The flow below is cross-checked against the reference projects studied under
`/opt/nvme-fw/opentune-refs/` (read-only): **VW_Flash** (`lib/flash_uds.py` —
the end-to-end VAG UDS flash), **FastECU**, and **OpenVehicleDiag**. Nothing is
copied from them; they are used only to confirm the service order and the
concepts summarised here.

> **Scope — calibration only.** OpenTune reflashes the **calibration** area
> (maps, limiters, driveability). It does **not** ship, and this document does
> not describe, any emissions-defeat or immobiliser-defeat procedure, and it
> ships **no** OEM seed/key or signature material. See
> [SEEDKEY.md](SEEDKEY.md). Where a step below would be needed only to inject
> *unsigned code* (as opposed to a signed/valid calibration), that is called
> out and is out of scope.

---

## 0. Preconditions (enforced by `opentune-flash`)

A driver never flashes directly from the app. `opentune-flash::flash_calibration`
wraps every write with: **voltage gate → verified backup read → write →
read-back verify** (`crates/opentune-flash/src/lib.rs`). So before any of the
UDS/KWP services below run:

1. **Battery/supply voltage** is checked (`Interface::battery_millivolts`,
   default minimum 12.0 V). A flash that browns out mid-transfer bricks the ECU,
   so this gate is mandatory for interfaces that can measure voltage.
2. A **full backup** of the target region is read and length-verified. This is
   the original calibration you restore to if anything fails.

---

## 1. Session control — `0x10` DiagnosticSessionControl

UDS: `UdsClient::session(session)` / KWP: `Kwp2000Client::start_session(session)`.

1. **Extended session** `0x10 0x03` (`extendedDiagnosticSession`). Opens the
   richer service set (security access, routines, data-by-identifier). OpenTune's
   CAN drivers call this in `begin_session()` before a read.
2. **Programming session** `0x10 0x02` (`programmingSession`). Required before
   `requestDownload` on most ECUs; the ECU re-initialises its comms (often new
   CAN timing / faster baud) and arms the bootloader. On VAG UDS ECUs the tester
   must keep **TesterPresent `0x3E`** flowing (see §7) or the session drops back
   to default and security is lost.

EDC16/KWP ECUs use the same `0x10` SID with a KWP session byte (commonly `0x85`
programming / `0x86` or `0x87` developer, ECU-dependent).

> On EDC17/Simos the VW flow also runs a **programming-precondition** check
> (routineControl `0x31` routine `0x0203`) right after the extended session, and
> a **check-programming-dependencies** routine (`0x31` routine `0xFF01`) at the
> end. These are RoutineControl calls (§6) and are optional for a plain
> calibration write but recommended for correctness.

## 2. Security access — `0x27` SecurityAccess (seed/key)

Before `requestDownload` the ECU is **locked**. The unlock handshake:

1. **Request seed:** `0x27 <level>` with an **odd** level (EDC17/VAG programming
   uses `0x11`; EDC15/16 KWP commonly `0x01`). The ECU replies
   `0x67 <level> <seed…>`. → `UdsClient::request_seed(level)` /
   `Kwp2000Client::request_seed(level)` (returns just the seed bytes).
2. **Compute key** from the seed with the ECU's algorithm.
3. **Send key:** `0x27 <level+1> <key…>` (the matching **even** level). A
   positive `0x67 <level+1>` means unlocked; NRC `0x35 invalidKey` means wrong
   key, `0x36 exceededNumberOfAttempts` / `0x37 requiredTimeDelayNotExpired`
   means the ECU has locked you out for a cooldown. →
   `UdsClient::send_key(level+1, key)` (maps `0x35` to `Error::SecurityDenied`).

An **all-zero seed** conventionally means "already unlocked" — OpenTune's hook
detects this and skips sending a key.

> **OpenTune ships no OEM seed/key algorithms.** The compute step is delegated to
> a user-supplied `core::protocol::seedkey::SeedKey` implementation, wired into
> each driver's `write_memory`/read path. Full details and the driver hook are in
> [SEEDKEY.md](SEEDKEY.md). VW_Flash, by contrast, executes the **SA2** bytecode
> script (found in the car's ODX container) against the seed; that is the kind of
> algorithm a user would provide to OpenTune, not something OpenTune distributes.

## 3. Erase — `0x31` RoutineControl (eraseMemory)

UDS: `UdsClient::routine_control(kind, routine, data)` with
`kind = 0x01` (startRoutine). The erase routine identifier is **`0xFF00`**
(`eraseMemory`) on UDS; the data payload identifies the block/partition to
erase (VW_Flash sends `[0x01, blockIdentifier]`). KWP2000 uses `0x31`
startRoutine with an ECU-specific routine number.

Flash must be erased (set to `0xFF`) before new data is written to it. On a
block-structured ECU you erase and write one logical block at a time.

## 4. Request download — `0x34` RequestDownload

UDS: `UdsClient::request_download(dataFormatIdentifier, address, size)`.
Request layout (as built by the client):

```
0x34  <dataFormatIdentifier>  <addressAndLengthFormatIdentifier=0x44>
      <memoryAddress:4 BE>    <memorySize:4 BE>
```

- **`addressAndLengthFormatIdentifier = 0x44`**: 4-byte address + 4-byte size.
- **`dataFormatIdentifier` (DFI)**: high nibble = compression, low nibble =
  encryption. `0x00` = uncompressed + unencrypted — what OpenTune uses for a
  plain calibration write. See §8 for compression/encryption on newer ECUs.

The ECU answers `0x74 <lengthFormatIdentifier> <maxNumberOfBlockLength…>`.
`UdsClient::request_download` parses and returns **`maxNumberOfBlockLength`**:
the largest `transferData` message the ECU will accept, **including** the `0x36`
SID and the block-sequence-counter byte. OpenTune therefore uses a payload chunk
size of `maxNumberOfBlockLength − 2`.

KWP2000: `Kwp2000Client::request_download(address, size, format)` — 3-byte
address + 1-byte format + 3-byte size, no returned block length (K-line uses a
small fixed chunk).

NRC `0x70 uploadDownloadNotAccepted` or `0x33 securityAccessDenied` here means
the session/security in §1–§2 was not satisfied.

## 5. Transfer data — `0x36` TransferData (the block loop)

UDS: `UdsClient::transfer_data(blockSequenceCounter, block)`.

```
0x36  <blockSequenceCounter>  <data…>
```

- **Block sequence counter (BSC):** starts at **`0x01`**, increments for every
  block, wraps `0xFF → 0x00` (per ISO 14229 and VW_Flash's `next_counter`).
  OpenTune's loop uses `seq.wrapping_add(1)` from `1`. A mismatch is reported by
  the ECU as NRC `0x73 wrongBlockSequenceCounter`.
- **Max block length:** each block carries at most `maxNumberOfBlockLength − 2`
  data bytes (from §4). The driver walks the buffer in `chunks(chunk_size)`.
- The ECU echoes the counter in its positive `0x76` response.

KWP2000 `transferData` (`0x36`) has **no** per-block counter; OpenTune keeps the
K-line block size small (≤250 bytes) for reliability.

The loop reports `Phase::Write` progress after every block.

## 6. Checksum / verify routines — `0x31` RoutineControl

Many ECUs require a **checksum/validation routine** after the transfer and
before (or instead of) reset, so the ECU accepts the freshly written block:

- UDS: `UdsClient::routine_control(0x01, routineId, data)`. VW_Flash runs
  routine **`0x0202`** with the block identifier and the computed block
  checksum, and the dependency routine **`0xFF01`**
  (`checkProgrammingDependencies`).
- The **block/area checksum itself** is computed by `opentune-checksum`
  (e.g. the EDC16 checksum, `crates/opentune-checksum/src/edc16.rs`) and must be
  corrected in the calibration *image* **before** transfer, so the data written
  already carries a valid checksum. The routine then lets the ECU confirm it.

## 7. Transfer exit — `0x37` RequestTransferExit

UDS: `UdsClient::transfer_exit()` → `0x37`. KWP:
`Kwp2000Client::transfer_exit()`. Signals the end of this block's transfer; the
ECU finalises the write. Done once per `requestDownload`/transfer pair, i.e.
once per block.

Throughout §3–§7, **TesterPresent `0x3E`** (`UdsClient::tester_present`) is sent
periodically so the programming session does not time out between long
operations.

## 8. ECU reset — `0x11` ECUReset

UDS: `UdsClient::ecu_reset(kind)` with `kind = 0x01` (`hardReset`). KWP:
`0x11`. The ECU reboots into the normal application running the new
calibration. After reset the driver (via `opentune-flash`) re-reads the region
and compares it to what was written (**read-back verify**).

---

## Putting it together (UDS / EDC17, one calibration block)

```
0x10 0x03        extendedDiagnosticSession
0x31 .. 0x0203   programming precondition           (optional, recommended)
0x10 0x02        programmingSession
0x27 0x11        request seed        ─┐ security access
0x27 0x12 <key>  send key            ─┘ (user-supplied SeedKey)
0x31 .. 0xFF00   eraseMemory(block)
0x34 00 44 <addr> <size>             requestDownload (DFI 0x00)
0x36 01 <data…>  ┐
0x36 02 <data…>  │ transferData loop, BSC 1,2,3…wrap
   …             ┘
0x37             requestTransferExit
0x31 .. 0x0202   checksum/verify routine             (ECU-dependent)
0x31 .. 0xFF01   checkProgrammingDependencies        (optional, recommended)
0x11 0x01        hardReset
```

KWP2000 / EDC16 is the same shape with `Kwp2000Client`: `0x10` start session,
`0x27 0x01/0x02` seed/key, `0x34` requestDownload (3-byte addr), `0x36`
transferData (no BSC), `0x37` transferExit, `0x31` routine, `0x11` reset.

---

## Compression, encryption and RSA signatures — conceptual (newer ECUs)

Summarised **conceptually** from VW_Flash; no code or algorithm is reproduced.

- **Compression.** Factory flash containers (ODX/FRF) store blocks compressed
  (VAG Simos uses an LZSS variant). The DFI high nibble tells the ECU the block
  is compressed so it inflates on the fly during `transferData`. This is a
  transport-size optimisation. OpenTune writes **uncompressed** calibration
  (DFI `0x00`), which any of these ECUs also accept, so OpenTune does not need to
  implement the compressor.

- **Encryption.** The DFI low nibble marks a block as encrypted (VAG Simos uses
  an AES scheme); the ECU decrypts during transfer with a key held in its
  bootloader. Again OpenTune writes **plaintext** (DFI `0x00`) calibration, so no
  OEM crypto is required or shipped.

- **RSA signatures / trust chain (why newer ECUs need a "bypass").** EDC17/Simos
  and similar verify an **RSA signature** over each flashed block in the boot
  chain (SBOOT→CBOOT→ASW). Stock tooling only accepts blocks signed by the OEM
  private key, which tuners do not have. The reference projects defeat this with
  an **exploit chain** (e.g. a bootloader password-recovery exploit on the bench,
  then promoting a CBOOT with signature checking disabled) so the ECU will accept
  *unsigned* code. **This is firmware/trust-chain modification, explicitly out of
  OpenTune's scope.** OpenTune targets ECUs where:
  1. the **calibration area is not individually signature-gated** (common on
     EDC16 and many EDC17 cal partitions), or
  2. the user has *already* performed any unlock themselves on the bench.

  OpenTune does a straight signed/valid **calibration** write with corrected
  checksums; it never ships, builds, or automates a signature bypass.

## When a full read needs bench / boot access

`EcuDef.full_read` (`crates/opentune-core/src/ecudef.rs`) is `Obd` or
`BenchBoot`:

- **OBD full read.** On EDC16 and many EDC17s the whole flash (or at least the
  calibration) is readable over the diagnostic connector via `requestUpload`
  (`0x35`) + `transferData`, after security access. OpenTune's `read_memory`
  does exactly this.
- **Bench / boot read.** Later ECUs **block OBD reads of program flash** (the
  ECU only returns calibration, or nothing, over `0x35`/`0x23`). A *complete*
  image then requires **bench mode** (ECU on the bench, boot pins/pads driven, a
  bootloader uploaded over the manufacturer BSL) — e.g. the Tricore BSL used by
  the reference bench toolchains. This is hardware-specific, risks the ECU, and
  is **outside OpenTune's OBD-driver scope**; such defs are marked
  `full_read: bench_boot` so the app can tell the user an OBD read will be
  partial.
