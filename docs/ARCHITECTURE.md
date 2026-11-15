# Architecture

OpenTune is layered so that a new app or a new piece of hardware is an additive
change, never a rewrite. The rule is one-directional: **apps depend on the core,
the core defines one driver trait, and drivers implement that trait.** Nothing
above the core talks to hardware, and no driver can bypass the safety rules.

```
          ┌───────────────────────────────────────────────┐
  apps    │   CLI        Desktop (Tauri)        Android     │
          │  opentune-cli   desktop/          android/      │
          └───────────────────────┬───────────────────────┘
                                  │  trait Interface (+ Registry)
          ┌───────────────────────┴───────────────────────┐
  core    │   opentune-core                                │
          │     Interface trait · Registry · types         │
          │     protocol: ISO-TP · UDS · KWP2000 · seed/key │
          │     ECU definitions (ecudef)                    │
          │                                                 │
          │   opentune-checksum   opentune-flash            │
          │     ChecksumAlgo        safety orchestrator     │
          └───────────────────────┬───────────────────────┘
                                  │  implements Interface (and/or CanLink)
   ┌───────────────┬─────────────┼───────────────┬──────────────────┐
 mock/file     OpenPort 2.0    SocketCAN     K-line (ELM327)   …future drivers
 (offline)     (CDC-ACM)       (Linux CAN)   + own firmware
```

## Layers

### Apps
Thin shells that wire drivers into a `Registry` and drive core operations.

- **`opentune-cli`** — the reference app. Offline-capable operations (identify,
  read, diff, checksum) plus a safety-gated `flash`. With the `mock` driver
  everything works against local files.
- **`desktop/`** — a Tauri app (lives outside the workspace, builds on its own).
- **`android/`** — planned Kotlin/Compose shell over the same core via UniFFI
  (see [`android.md`](android.md)).

### Core — `opentune-core`
The abstraction every app talks to and every driver implements.

- `interface` — the `Interface` trait: `open`/`close`, `read_ecu_id`,
  `read_memory`, `write_memory`, `battery_millivolts`, plus `info` and
  `capabilities`. This is the whole surface an app uses.
- `registry` — `Registry`/`DriverEntry`: enumerate and construct drivers by id,
  so apps don't hard-code which drivers exist.
- `capabilities` — `Capabilities`: what a driver supports (CAN/CAN-FD, K-line,
  read/write memory, bench/boot, battery/programming voltage), so apps can grey
  out actions.
- `types` — shared values: `Protocol`, `DiagLayer`, `MemoryRegion`, `EcuId`,
  `ConnectConfig`, `Progress`/`Phase`.
- `error` — `Error`/`Result`, including structured `NegativeResponse` (NRC).
- `protocol` — the diagnostic stack reused by every CAN/K-line driver:
  - `isotp` — `CanLink` (raw-frame send/recv, provided by a driver) and `IsoTp`
    (ISO 15765-2 transport that implements `DiagTransport`).
  - `diag` — the `DiagTransport` request/response trait and NRC helpers.
  - `uds` — `UdsClient` (ISO 14229 services over a `DiagTransport`).
  - `kwp2000` — `Kwp2000Client`.
  - `seedkey` — the `SeedKey` plugin interface for security access.
- `ecudef` — `EcuDef` and `load_dir`: data describing a known ECU (protocol,
  memory map, checksum id, seed/key id), loaded from JSON in
  [`ecudefs/`](../ecudefs/). See [`ECU-DEFS.md`](ECU-DEFS.md).

### Core helpers — `opentune-checksum`, `opentune-flash`
Kept as separate crates so apps and drivers can pull in only what they need.

- **`opentune-checksum`** — the `ChecksumAlgo` trait (`verify`/`correct`) with a
  per-family lookup, a generic CRC32-tail example and the EDC16 module. The id
  used here is what an ECU definition names in its `checksum` field.
- **`opentune-flash`** — the flash orchestrator. `flash_calibration` enforces the
  safety sequence **voltage gate → verified backup → write → read-back verify**.
  These rules live here, not in drivers, so no driver can skip them.

### Drivers — `crates/drivers/*`
Each implements `Interface`. CAN drivers typically implement the lighter
`CanLink` trait and let the core's `IsoTp` + `UdsClient` do the protocol work.

| Crate | Driver id | Hardware |
|---|---|---|
| `opentune-driver-mock` | `mock` | none — local `.bin` files |
| `opentune-driver-openport2` | `openport2` | Tactrix OpenPort 2.0 (CDC-ACM) |
| `opentune-driver-socketcan` | `socketcan` | any Linux CAN interface |
| `opentune-driver-elm327` | `elm327` | ELM327 / OpenTune's own K-line firmware |

Writing a new one is covered in [`DRIVERS.md`](DRIVERS.md).

## Data flow of a flash

1. The app picks a driver from the `Registry` and `open`s it with a
   `ConnectConfig` derived from the chosen `EcuDef`.
2. `read_ecu_id` confirms identity; the app loads the matching `EcuDef`.
3. For a calibration change, the app calls `opentune_flash::flash_calibration`
   with the target `MemoryRegion` and new bytes.
4. The orchestrator reads battery voltage (gate), takes a verified backup via
   `read_memory`, writes via `write_memory`, then reads back and compares.
5. `opentune-checksum` corrects the family checksums on the new image before it
   is written, using the algorithm named in the `EcuDef`.

Every long operation reports `Progress { phase, done, total }` so any app can
show a meaningful progress bar without knowing driver internals.
