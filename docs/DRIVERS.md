# Writing an interface driver

An **interface driver** is the only part of OpenTune that touches hardware.
Everything above it (CLI, desktop, Android) talks to the `Interface` trait, so a
new reader/flasher is a new crate that implements that trait — nothing else
changes.

This guide walks through the two ways to write one:

1. Implement `Interface` directly (for transports OpenTune has no stack for).
2. Implement the lighter `CanLink` and reuse the core's `IsoTp` + `UdsClient`
   (the recommended path for anything speaking UDS/KWP over CAN).

The **mock/file driver** (`crates/drivers/mock`) is the reference template:
a complete, hardware-free `Interface` you can copy and adapt.

## 1. Crate layout

Add a crate under `crates/drivers/<name>` and list it in the workspace
`members`. Minimal `Cargo.toml`:

```toml
[package]
name = "opentune-driver-<name>"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
opentune-core = { path = "../../opentune-core" }
# transport crates as needed, e.g. serialport, socketcan
```

## 2. The `Interface` trait

```rust
pub trait Interface {
    fn info(&self) -> InterfaceInfo;
    fn capabilities(&self) -> Capabilities;

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()>;
    fn close(&mut self) -> Result<()>;

    fn battery_millivolts(&mut self) -> Result<Option<u32>> { Ok(None) }

    fn read_ecu_id(&mut self) -> Result<EcuId>;
    fn read_memory(&mut self, region: MemoryRegion,
                   progress: &mut dyn FnMut(Progress)) -> Result<Vec<u8>>;
    fn write_memory(&mut self, region: MemoryRegion, data: &[u8],
                    progress: &mut dyn FnMut(Progress)) -> Result<()>;

    fn plan_read_memory(&mut self, region: MemoryRegion)
        -> Option<Result<Vec<String>>> { None }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> { None }
    fn can_monitor(&mut self) -> Option<&mut dyn CanMonitor> { None }
}
```

Contract:

- **`info`** — stable `id` (the string apps use to construct the driver),
  a human `display_name`, and optional firmware/serial.
- **`capabilities`** — be honest. Apps grey out actions from this, and the flash
  orchestrator uses `battery_voltage` to decide whether a missing reading is an
  error. OBD drivers must leave `bench_boot` false.
- **`open`** — read what you need from `ConnectConfig` (`protocol`, `diag_layer`,
  `bitrate`, `tx_id`/`rx_id`, and the free-form `options` map) and establish the
  session. Fail with `Error::Config` for bad options.
- **`read_memory` / `write_memory`** — call `progress` at least at start and end
  with `Progress { phase, done, total }`; report real progress for long reads.
  Validate lengths; return `Error::NotConnected` if used before `open`.
- **`plan_read_memory`** — optional dry run of `read_memory` on the opened
  driver: the exact lines the read would write to the adapter, in order, with
  **nothing sent**. Return `Some(Err(_))` where the real read would be refused
  before reaching the bus. The default `None` means "this driver cannot plan";
  callers (`opentune read --plan`, `CliContext::plan_read`) then refuse with
  "driver cannot plan" instead of reading for real. `elm327obd` plans its full
  read; `mock` plans the file slice it would copy.
- **`diag`** — if the driver owns a request/response `DiagTransport` (an
  `IsoTp` over its `CanLink`, a KWP2000/OBD session, …), **override this** to
  return it once `open` has succeeded. The default `None` means "no generic
  transport", and every service engine (coding, adaptation, service functions,
  auto-scan, logging) then refuses with "no diagnostic transport". Leave the
  default only for drivers that genuinely offer just the block read/write
  pipeline.
- Surface protocol failures as the structured error types
  (`Error::NegativeResponse`, `Error::Timeout`, `Error::Protocol`) rather than
  ad-hoc strings, so apps can react.

Do **not** implement safety logic here. The voltage gate, backup and read-back
verify live in `opentune-flash`; a driver that re-implemented them could skip
them.

## 3. The easy path: implement `CanLink`, reuse the stack

If your hardware moves raw CAN frames, you only need:

```rust
pub trait CanLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()>;
    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)>;
}
```

Then the core gives you ISO-TP and UDS for free:

```rust
use opentune_core::protocol::{CanLink, IsoTp, UdsClient};
use opentune_core::protocol::uds::sid;

// `link` is your CanLink implementation.
let mut isotp = IsoTp::new(Box::new(link), cfg.tx_id, cfg.rx_id);
let mut uds = UdsClient::new(&mut isotp);

uds.session(0x03)?;                     // extended diagnostic session
let id = uds.read_data_by_id(0xF190)?;  // e.g. VIN
```

`IsoTp` implements `DiagTransport`, so `UdsClient` (or `Kwp2000Client`) runs
straight on top of it. Your `read_memory`/`write_memory` then build on the UDS
upload/download services (`request_download`/`transfer_data`/`transfer_exit`),
and `read_ecu_id` on `read_data_by_id`. Security access is handled through the
`SeedKey` plugin interface named by the ECU definition's `seedkey` field.

For K-line, implement `DiagTransport` directly over your init + framing and feed
it to `Kwp2000Client` the same way.

## 3b. Optional: raw bus capture (`can_monitor`)

A CAN adapter that can see every frame on the bus can offer a full CAN dump
(`opentune candump`, the desktop CAN capture panel) by overriding
`can_monitor()` to return a `CanMonitor` (`opentune_core::monitor`):
`start(&MonitorOpts)` → `next_frame(timeout)` → `stop()`. Rules:

- `start` opens the hardware **itself** — the app never calls `open` first,
  because a diagnostic session may transmit.
- Never transmit while monitoring. Use the adapter's silent / listen-only mode
  where it has one and report the achieved level honestly via `listen_only()`
  (`Hardware`, `Passive`, `NotGuaranteed`).
- `next_frame` returns `Ok(None)` for a quiet interval; count adapter overflows
  in `dropped()` instead of failing.

Implemented today: `mock` (synthetic traffic), `socketcan` and `gsusb` (kernel
`listen-only on` = Hardware, else Passive), `slcan` (`L` listen-only open),
`elm327obd` (`ATMA` with `ATCSM1` silent monitoring; clones that refuse it are
NotGuaranteed), `efp02c` (no silent mode known: Passive, 1 Mbit/s only). The
log writers and stats live in `opentune-candump`.

## 3c. The virtual car (`vcar`) — a test peer for everything above

`crates/drivers/vcar` simulates a whole six-module vehicle. Each module is an
`opentune-udssim` UDS server behind a server-side ISO-TP endpoint on a virtual
11-bit CAN bus. The engine also answers functional OBD-II on `0x7DF`. The
driver is a good reference for the patterns above:

- `diag()` returns the core `IsoTp` over a `CanLink` (`VcarLink`). It is bound
  to `ConnectConfig::tx_id` / `rx_id` (the engine by default), and a request to
  an id with no module behind it times out, as on a real bus.
- `can_monitor()` produces synthetic traffic (`ListenOnly::Simulated`).
- `capabilities()` is honest: CAN and battery voltage, but no memory
  read/write.
- The vehicle lives in a process-wide table keyed by the profile spec. Apps
  re-open a driver for every module (the per-module auto-scan, the desktop
  hub), and the table means the car keeps its state across those re-opens.

Use it to test an engine or a panel end to end without hardware. See
[`VIRTUAL-CAR.md`](VIRTUAL-CAR.md).

### The UDS simulator (`udssim`) — one flashable ECU, in process

`opentune_udssim::UdssimDriver` (registry id `udssim`, "UDS simulator
(offline)") puts a single simulated ECU behind the `Interface` trait, with no
CAN framing in between. Where `vcar` is a whole vehicle for the diagnostic
engines, `udssim` is the peer for **memory read / write and flashing**:

- `diag()` returns the simulator transport directly, so the UDS module flasher
  (`opentune flash --uds-module --driver udssim`) and every engine run on it.
- `read_memory` is `23` ReadMemoryByAddress in 0x80-byte chunks; `plan_read_memory`
  lists those requests (`read --plan`).
- `write_memory` programs over UDS: extended → programming session,
  SecurityAccess 0x11/0x12, eraseMemory, `34`/`36`/`37`, checksum routine,
  ECUReset. It needs a seed-key handler: the simulator's **test-only**
  `udssim-test` (`key = seed XOR 0xA5`, `opentune_udssim::TestSeedKey`) — the
  public CLI resolves that id (`--seedkey udssim-test`), and it unlocks only
  the simulator.
- The ECU has one 64 KiB flash region at `0x00000000` (initial contents: byte
  `i` = `i & 0xFF`), identity DIDs, and the flash model's persistent memory, so
  write → read-back round trips work.
- Like `vcar`, the ECU lives in a process-wide table keyed by name, so it keeps
  its state across re-opens. Options: `ecu` (or `port`) = instance name,
  `reset=1` rebuilds it, `readable=0` refuses flash reads (to exercise the
  no-backup paths).

```sh
opentune read  --driver udssim --addr 0x0 --len 0x400 --out stock.bin
opentune flash --driver udssim --seedkey udssim-test --addr 0x0 --data new.bin
```

## 3d. `elm327obd`: block read over a USB ELM327

`crates/drivers/elm327` (crate `opentune-driver-elm327-obd`) drives a USB
ELM327 on CAN with `ATCAF1`, so the adapter does the ISO-TP framing. That has
one hard limit that shapes everything below: **an ELM327 can only transmit a
single ISO-TP frame**, so every request must fit in 7 bytes
(`protocol::elm::MAX_CAN_REQUEST`). Replies can be multi-frame. The driver
checks every request format against this limit before it sends anything, and
refuses with a clear error when a format does not fit. It does not truncate.

**Read: recommended.** `read_memory` runs:

1. Flow-control pacing: `ATFCSD 30 00 <STmin>` asks the ECU to wait between
   consecutive frames. The default STmin comes from the serial baud: 6 ms at
   38400. Without the pacing, long replies overrun the clone's UART and the
   adapter prints `BUFFER FULL`. The init flow control (`30 00 00`) is put back
   after the read.
2. A session (`0x10`). Default: extended (`03`). Set it with option
   `session=none|default|extended|programming|<byte>`.
3. SecurityAccess (`0x27`). It runs when a handler was given through
   `set_seed_key`. The handler works through the trait too, so a
   registry-opened `Box<dyn Interface>` accepts one. The driver makes exactly
   one attempt:
   - An all-zero seed means the ECU is already unlocked, and no key is sent.
   - NRC 0x35 (invalid key) and 0x36 (too many attempts) stop with
     `SecurityDenied`. Nothing more is sent.
   - NRC 0x37 (delay not expired) is reported with an explanation and is never
     retried.
   - If the config marks the ECU as needing security (`seedkey=<id>` or
     `security=required`) and no handler is set, the read is refused before
     anything goes on the bus.
   - Option `seedkey_level` sets the level (odd, default 0x11).
4. The transfer:
   - `read_mode=rmba` (default): `ReadMemoryByAddress 0x23` with format `0x14`
     (4-byte address, 1-byte size). It reads 128 bytes per request, or 32 if
     the adapter refused user flow control. Change the size with
     `read_chunk`; the 1-byte size field caps it at 255.
   - `read_mode=upload`: `RequestUpload 0x35` + `TransferData 0x36` (counter
     from 01, wrapping FF→00) + `RequestTransferExit 0x37`. After `3x <dfi>
     <alfid>` only 4 bytes remain for address + size, so the format is `0x22`
     (16-bit address) or `0x13` (24-bit address, 255-byte windows, one
     request/exit cycle per window). **A 32-bit address cannot be uploaded
     through an ELM327.** Use `rmba` for those.
   - Option `alfid` overrides the format, and option `data_format` sets the
     `dataFormatIdentifier`.

Progress is reported for each step:

- `Connect` before the session.
- `SecurityAccess` before and after the unlock.
- `Read` at 0 and after every chunk or block, with the region length as the
  total.

Errors from a timeout or an adapter error line (`NO DATA`, `BUFFER FULL`, ...)
name the address and the bytes read so far. Some negative responses get a hint:

- 0x33 (security denied) when no handler was set: "supply a seed/key handler".
- 0x11/0x7F on 0x23: "try `read_mode=upload`".
- 0x12/0x22/0x7E on the session request: "check ignition, or pick another
  session".

Each memory request waits 3 s (option `timeout_ms`), longer than the 2 s UDS
default, because the ELM adds its own latency. `responsePending` (0x78) is
handled in the ELM reply parser with the P2* budget, and only when it echoes
the request's service (see "ELM reply parser" below).

**Dry run.** `Elm327Obd::plan_read_memory(region)` lists, without sending
anything, every line the read would write, in order: the `AT` pacing and each
hex request, each with its purpose. A send-key appears as `2712<key>`, and an
upload's blocks appear as a counter range. The dry run fails where the real
read would fail before reaching the bus. `memory::dry_run_read` is the same
planner without an opened adapter. The lines (without the purposes) are also
offered through `Interface::plan_read_memory`, so a registry-opened driver and
the CLI can plan:

```sh
opentune read --driver elm327obd --port /dev/ttyUSB0 --bus hs \
    --tx-id 7E0 --rx-id 7E8 --opt read_mode=upload \
    --addr 0x10000 --len 0x400 --plan
```

`--plan` opens the adapter (the `AT` init only), prints each line on stdout
and sends no diagnostic request. `dump --plan` does the same without reading
the identity or archiving anything.

**Write: bench-only, discouraged.** `capabilities().write_memory` is `false`.
The ELM cannot send multi-frame ISO-TP, so `write_memory` does the following:

- It refuses unless option `elm_write=single-frame` is set.
- It then uses `RequestDownload 0x34` with the same 16/24-bit formats as upload.
- It sends `TransferData` in blocks of **5 bytes** (7 minus the service id and
  the counter), with the counter wrapping FF→00, and ends with
  `RequestTransferExit`.

Every block costs one serial round trip, and a dropped USB link in the middle
of a download can leave an ECU unbootable. Flashing over an ELM327 is **not
recommended**. Use it only on a bench ECU, behind the flash-safety policy that
callers enforce. Use slcan, gs_usb, SocketCAN or J2534 for real flashing.

**Clone quirks** were measured on a v1.5 clone (PIC18F25K80 + CH340, HS/MS
switch) at 38400 baud, with only `AT` commands and no vehicle attached:

- `ATI` reports `ELM327 v1.5`, `AT@1` reports `OBDII to RS232 Interpreter`,
  and `ATRV` reads `0.0V` with no car attached.
- The reply to the first `ATE0` after `ATZ` still carries the echo
  (`ATE0`/`OK`).
- Accepted: `ATCAF0/1`, `ATAL`, `ATSH 7E0`, `ATCRA 7E8`, `ATCF`/`ATCM`,
  `ATCFC1`, `ATFCSH`, `ATFCSD 30 00 00`, `ATFCSD 30 00 06`, `ATFCSD 30 08 06`,
  `ATFCSM 0/1`, `ATST 64`/`FF`, `ATAT0/1/2`, `ATR0/1`, `ATCEA`, `ATTA`,
  `ATD0/1`, `ATV0/1`, `ATH0/1`, `ATPC` and `ATWS`. User flow control works,
  so block reads are paced.
- `ATSPB` + `ATPB C1 04` reports `USER1 (CAN 11/125)`. This confirms the
  MS-CAN divisor formula.
- **Rejected (`?`):**
  - `AT@2`.
  - `ATCRA` with no argument. A filter cannot be reset that way; the driver
    always passes an id.
  - `ATCSM1`/`ATCSM0`. With no silent monitoring, `ATMA` capture is
    `NotGuaranteed` passive.
  - A 4-byte `ATSH 18DA10F1`. 29-bit addressing is out of scope here anyway.
- `ATSP` is stored in the adapter's EEPROM. The probe left it at protocol 6,
  which is where it started.

**On a car (2026-11-18):** with the same clone on HS-CAN (`ATSP6`, 7E0/7E8),
`opentune info` / `identify --driver elm327obd` read the battery voltage, ECU
name, part number and VIN from a VW 1.6 TDI CR (EA189) engine ECU on the first
try: the first real-ECU exchange of this driver. Not yet exercised on a car: DTC
read/clear, block reads and anything that needs security access. The reported
identity (ECU name `R4 1.6l TDI`, hardware/part number `03L907425C`) now
matches the identification-only definition `siemens-pcr21-vag16tdi-ident`. Public
parts listings cross-reference that number to Continental Simos PCR2.1 (not
Bosch EDC17 as first noted here); the family is unconfirmed until an image of
this unit is read.

**Receive filter sticks until `ATZ`** (isolated on a car with the same clone,
using a separate script, 2026-11-18):

- Toggling only the request header is clean: `ATSH 7E1` then `ATSH 7DF`
  gives normal functional replies again.
- Once `ATCRA 7E9` is set, functional requests keep returning `NO DATA` for
  the engine even after `ATSH 7DF`. A bare `ATCRA` (the documented filter
  reset) is rejected with `?` and does not clear it.
- `ATAR` (automatic receive), `ATD` (defaults) and `ATZ` with a full re-init
  each clear a stuck filter. `ATAR` is the cheapest reset between ECUs.
- A functional `0100` returned both 7E8 and 7E9 on a car with an engine ECU
  and a DSG, and only 7E8 on a single-ECU car.

The driver is built around this. It resets with `ATZ` on every open, sets one
physical `ATSH`/`ATCRA` pair per session and never sends a bare `ATCRA`.
Scans re-bind per module by opening again (CLI and the desktop adapter hub).
A unit test pins it (`every_open_resets_with_atz_and_never_sends_bare_atcra`)
in each of the three ELM drivers (`elm327obd`, `elm327`, `elm327net`).
Any future in-session retargeting must send `ATAR` (or `ATZ`) before the new
filter, never a bare `ATCRA`.

**ELM reply parser** (`opentune_core::protocol::elm`, shared by `elm327obd`,
`elm327` and `elm327net`). What it does with what a clone prints on a car:

- **responsePending is echo-checked.** A `7F <sid> 78` is waited on (P2*,
  5 s per pending, at most 64) only when `<sid>` is the service of the request
  that was sent, like `diag::is_response_pending` on every other driver. A
  `7F xx 78` for another service is handed back as the reply at once, and the
  UDS/KWP client reports it as mis-correlated.
- **Several ECUs answering** (a functional request, headers off): responses
  are never concatenated. A positive response for the request's service wins
  over another ECU's negative one; otherwise the first response is the reply.
  A single frame from another ECU inside a multi-frame reply is an error,
  never spliced data.
- **Adapter error lines** (`NO DATA`, `STOPPED`, `BUFFER FULL`, `CAN ERROR`,
  `BUS BUSY`, `?`, `ERRxx`, ...) end the read at once with a protocol error;
  in the middle of a multi-frame reply too, so a short image is never
  returned. The request is not retried automatically.
- `SEARCHING...`, `OK` and the K-line `BUS INIT: ...OK` banner are skipped.
- **A stray `>`**: before any text it is ignored; in the middle of a line it is
  dropped when more text follows straight away, and taken as the prompt when
  the adapter then goes idle (a clone that skips the final line break).
- **Power cycle mid-session.** The clone's UART can emit a burst of
  non-printable bytes, then its `ELM327 v1.5` banner. Lines of only
  non-printable bytes are skipped; the banner anywhere in a reply (even after
  an error line, or inside a multi-frame reply) is `Error::AdapterReset`,
  returned at once. If the banner was missed, the next request comes back
  echoed (echo is on again after the reboot), which is `AdapterReset` too,
  never parsed as data. The caller re-opens the interface: every ELM driver's
  `open` releases the previous port or socket first (a serial port is opened
  exclusively) and runs the full `ATZ` init again. `elm327net` also drains a
  TCP socket before each request, so a late `NO DATA` or banner from an
  earlier exchange is not read as the next answer.

Not yet validated against a real ECU. The first car session (read-only, see
[`FIRST-CAR-TEST.md`](FIRST-CAR-TEST.md)) should start with
`plan_read_memory` and a small region.

## 3e. `elm327`: UDS block read on CAN

`crates/drivers/kline-elm327` (crate `opentune-driver-elm327`, id `elm327`)
has the same single-frame transmit limit on CAN. Its UDS `read_memory` plans
the request format before anything is sent, security access included, and
uses the compact `addressAndLengthFormatIdentifier`s (the planning helpers in
`opentune_core::protocol::elm::compact` are shared with `elm327obd`):

- `read_mode=upload` (default): `RequestUpload 0x35` with format `0x22`
  (16-bit address) or `0x13` (24-bit address, 255-byte windows, one
  request/transfer/exit cycle per window). A 32-bit address is refused with a
  pointer to `read_mode=rmba`.
- `read_mode=rmba`: `ReadMemoryByAddress 0x23` with format `0x14` (32-bit
  address, 1-byte size), `read_chunk` bytes per request (default 32, at most
  what the size field holds; this driver does not pace the ECU's consecutive
  frames).
- `alfid=<hex>` picks another compact format, e.g. `12` (16-bit address,
  8-bit size) or `23` (24-bit address, 16-bit size, rmba). It is checked
  before anything is sent: address + size must fit 5 bytes for 0x23 and 4 for
  0x35 (else `Unsupported`), and the region's highest address must fit the
  address field (else `Config`).

**Dry run.** `Interface::plan_read_memory` lists the hex requests the read
would write (`2711`, `2712<key>` when a seed/key handler is set, each 0x23
chunk, or each `35`/`3601..`/`37` upload window) and sends nothing. It fails
where the read would fail before the bus (a format that does not fit,
security required without a handler). It is `None` on K-line and for KWP2000,
so `opentune read --driver elm327 --plan` works for UDS on CAN.

**Writes on CAN are refused before anything is sent** (`Unsupported`, no
security access, no session): `RequestDownload` with its 4-byte format and
the `TransferData` blocks do not fit one frame. Use a raw-CAN interface, or
`elm327obd`'s bench-only single-frame download. K-line reads and writes
(KWP2000, and UDS over K-line) are unchanged. Like `elm327obd`, this is
unit-tested against scripted ELM replies only.

## 3f. VW TP2.0 on the raw-CAN drivers (`socketcan`, `slcan`, `gsusb`)

Older VAG modules (the PQ-era Haldex Gen4 at logical address `0x0A`, EDC15/16
and ME7 engines, other pre-UDS modules) speak KWP2000 over **VW TP2.0**, not
ISO-TP. The three raw-CAN drivers can open a TP2.0 channel instead of their
default ISO-TP transport. Nothing changes unless you ask for it: without the
options below they build ISO-TP exactly as before.

Select it with driver options (CLI `--opt KEY=VALUE`; parsed by
`opentune_core::protocol::tp20channel::Tp20Options`):

| Option | Meaning | Default |
|---|---|---|
| `transport` | `isotp` or `tp20` | `isotp` (`tp20` when the config's protocol is `vwtp20`) |
| `tp20_addr` | logical address of the module, e.g. `0x0A` Haldex, `0x01` engine, `0x02` gearbox | required (or `tx_id` when the protocol is `vwtp20`) |
| `tp20_rx_id` | CAN id the module is asked to transmit on (11-bit, outside `0x200..=0x2FF`) | `0x300` (or `rx_id` with protocol `vwtp20`) |
| `tp20_keepalive_ms` | send a channel test (`A3`) after this long idle; `0` = off | `1000` |
| `tp20_timeout_ms` | wait per setup / parameter / keep-alive step | `1000` |
| `tp20_block_size` | block size requested in our `A0`, 1..=15 | `15` |
| `tp20_ack_retries` | times a block whose ACK does not arrive is re-sent (after an `A4` break), 0..=5 | `2` |

A `tp20_*` option without `transport=tp20` is refused (`Error::Config`), as is
an unknown transport, before the adapter is touched.

What the driver does:

- **`open`** sets up the channel: the broadcast to `0x200`, the module's
  answer on `0x200 + addr` with the CAN id it assigns the tester, then the
  `A0`/`A1` parameter exchange. If the module does not answer, `open` fails
  naming the step and the ids, e.g. *"TP2.0 channel setup (step 1 of 2):
  logical address 0x0A did not answer on CAN id 0x20A within 1000 ms (setup
  request sent on 0x200, asking the module to transmit on 0x300)"*.
- **`diag()`** returns the TP2.0 channel, so `Kwp2000Client` and every engine
  running on `RawDiag` work over it. The driver's own UDS paths
  (`read_ecu_id`, `read_memory`, `write_memory`) refuse on a TP2.0 channel
  with `Error::Unsupported`: they are UDS over ISO-TP.
- While the channel is idle a background thread sends `A3` and expects the
  module's `A1`; requests and keep-alives share one lock, so an `A3` never
  lands inside a telegram. A keep-alive is retried once; two unanswered in a
  row mark the channel lost and requests fail with that reason until the
  channel is re-opened (`Tp20Channel::reopen`, or re-open the driver: the
  module's KWP session went with the channel).
- Data frames honour the module's block size (we wait for its ACK at each
  block end; 0 and values above 15 count as 15), ACK the module's blocks,
  accept the "last frame, no ACK" (`0x3`) and "ACK, not ready" (`0x9`)
  opcodes, and answer a module-initiated `A3`.
- Lost frames are recovered per the TP2.0 break rule: a block whose ACK does
  not come within the module's `T1` (at least 100 ms) is broken (`A4`) and
  re-sent, up to `tp20_ack_retries` times; a module's `A4` rolls our receive
  back to our last ACK. When a request still fails (or the module sends
  `A8`), the channel is torn down and the **next request sets up a new one**
  automatically; restart the KWP session then. Details and the exact rules:
  [PROTOCOL-NOTES.md](PROTOCOL-NOTES.md#tp20-hardening-loss-recovery-bounds-fuzzing).
- **`close`** stops the keep-alive and sends the disconnect (`A8`).
- `capabilities().vwtp20` is `true` for these three drivers; `info()` names
  the open channel (e.g. `slcan USB-CAN (CANable/CANtact), VW TP2.0 to 0x0A
  (tester 0x764 -> module, module 0x300 -> tester)`).

Other modules: the same options reach any TP2.0 module by its logical address
(e.g. `tp20_addr=0x01` engine, `0x02` gearbox) and the KWP2000 engines run on
`diag()`. Which modules on a given car answer TP2.0 (for example whether a PQ
02E DSG TCU does) is to be confirmed on the car.

Per driver: `slcan` sends the frames as `t…` lines over its serial link
(`--port /dev/ttyACM0`); `socketcan` and `gsusb` use the SocketCAN interface
(option `iface`, or `port` when it is an interface name, default `can0`; bring
it up at the bus bitrate first). For tests and other frame sources,
`SlcanDriver::open_with_port` and `SocketCanDriver`/`GsUsbDriver::open_with_link`
run the same selection over a scripted adapter or any `Send` `CanLink`.

**Timing caveats.** TP2.0 is timing-sensitive in ways ISO-TP is not: the
module expects the tester's block ACK within T1 (we request 100 ms), and an
idle channel is dropped by the module after a time no public source pins down
(hence the keep-alive after 1 s idle by default; tune it with
`tp20_keepalive_ms`). All of this runs on the host, so USB latency counts: native SocketCAN
and gs_usb adapters are the safe choice; slcan over a USB-CDC CANable is
fine in principle; a CH340-based slcan clone at 115200 baud is slow (about
2 ms per frame on the serial line) and may struggle with long multi-frame
replies. We ask for no separation time (`T3 = 0`) in the `A0`. The ELM327
drivers cannot do TP2.0 at all (no raw frames, per-block ACKs or keep-alives)
and refuse it.

**Validation state: synthetic-tested.** Covered against a simulated TP2.0
module (`opentune_core::protocol::tp20sim`): channel setup and parameter
negotiation, multi-frame KWP replies with ACK windows, keep-alive through a
pause longer than the module's idle timeout, disconnect, a silent module and a
busy bus; slcan through its real ASCII line protocol, socketcan and gsusb
through their frame layer. Not yet bench- or vehicle-tested.

## 3g. `efp02c`: EFI Technology EFP02C USB-CAN (unverified)

`crates/drivers/efp02c` (crate `opentune-driver-efp02c`, id `efp02c`) drives
the EFP02C, a small USB 1.1 CAN box sold with EFI Technology's tuning
software, as a **generic CAN adapter**: the opentune-core ISO-TP + UDS stack
runs on its `CanLink`, `diag()` exposes the transport and `can_monitor()`
captures the bus. It talks USB directly through the pure-Rust
[`nusb`](https://crates.io/crates/nusb) crate (Apache-2.0 OR MIT; no libusb,
no vendor DLL), behind a small `Efp02cTransport` trait so the tests replay
captured transfers. No EFI ECU protocol is implemented, and the box's vendor
licensing exchange is never sent.

**Validation state: unverified (Bench tier).** Only what the provenance note
below supports is enabled:

| Item | State |
|---|---|
| 1 Mbit/s, accept-all config | confirmed on the owner's bench; **every other bitrate is refused** |
| 11-bit data frames, TX and RX | confirmed; **29-bit ids are refused** (driver and CLI) |
| Termination on (`04 09 01`), read-back (`04 0A`) | confirmed; the "off" value is not, so `termination=off` just skips the command |
| Poll `0B`: seq (byte 7) echoed in the trailer | confirmed; wait (bytes 2..3, LE) and max records (byte 4) inferred from two captured values |
| RX bytes 3..4 ("timestamp"), byte 15 | uninterpreted: the values cycle rather than count, so capture timestamps are host time |
| Identification block (`09` → four `0A` chunks, sum checksum) | confirmed; diagnostic text only (`identify=on`), serial numbers are not decoded |
| ECU writes | **disabled** (`write_memory` refuses, capability not declared) until verified |
| Listen-only mode | none known: the monitor reports `Passive` (the controller ACKs) |

Options: `bitrate` (optional; must be `1000000` if given — when nothing is
specified the driver uses 1 Mbit/s, an explicit other rate is refused), `termination`
(`on`/`off`, default `on` — mind an already-terminated bus), `identify`
(`on`/`off`, default `off`), `usb_index` (which attached EFP02C, default 0).
The monitor (`opentune candump --driver efp02c`) uses 1 Mbit/s when no
bitrate or `--bus` is given and refuses anything else.

**Host setup.** Windows: bind the WinUSB driver to the device (USB
`2323:0001`) once, e.g. with [Zadig](https://zadig.akeo.ie/); the vendor's own
driver must not hold it. Linux: grant your user access with a udev rule, for
example `/etc/udev/rules.d/70-efp02c.rules`:

```
SUBSYSTEM=="usb", ATTRS{idVendor}=="2323", ATTRS{idProduct}=="0001", MODE="0660", TAG+="uaccess"
```

then `sudo udevadm control --reload && sudo udevadm trigger` and replug.
macOS needs nothing extra.

**Provenance.** The USB record layout was first identified during
interoperability analysis of the vendor's Windows software, then implemented
in this driver from captures of the project owner's own EFP02C on a 1 Mbit/s
bench bus, driven from Linux (pyusb) without the vendor software. No vendor
code or data is included; the test fixtures are those own-hardware USB
transfers (bench CAN traffic, no serial numbers) and synthetic records. The
layout is to be re-confirmed from a black-box (USB) capture of the vendor
software before the driver leaves the Bench tier; items marked *inferred* in
`src/proto.rs` are the ones to check first.

## 3h. `isotp-ble`: a bridge that does ISO-TP itself

The `isotp-ble` driver (`crates/drivers/isotp-ble-bridge`) drives an ESP32
running the open-source esp32-isotp-ble-bridge firmware. Unlike the raw-CAN
drivers it does **not** implement `CanLink`: the device runs ISO-TP, so the
driver exchanges whole ISO-TP payloads in the firmware's framed protocol and
implements `DiagTransport` directly, which `UdsClient` runs on. That is the
pattern for any adapter with on-board ISO-TP.

Layers: a dependency-free `codec` (frames, BLE split/reassembly, settings), a
`Link` byte-transport trait (USB serial built in; BLE via `btleplug` behind
the optional `btleplug` cargo feature, off by default), a `Bridge` session
(answer matching, responsePending, stale-answer skipping) and a `mock` of the
firmware for tests. `IsoTpBleDriver::open_with_link` opens over any `Link`,
which is also how a front end with its own BLE stack (the Android app) can
plug in.

Limits come from the firmware: CAN at 500 kbit/s, 11-bit ids, and only the
id pairs 7E0/7E8, 7E1/7E9, 7E5/7ED and 700/7E8 (others are refused at `open`
unless `any_ids=1`). No TP2.0, K-line or raw capture. Options and the wire
format: [`ISOTP-BLE-BRIDGE.md`](ISOTP-BLE-BRIDGE.md).

**Validation state: unverified (Bench tier).** Synthetic-tested only:
codec round trips at many MTUs and payload sizes, and UDS through the
simulated firmware over BLE (MTU 23 and 247) and serial — multi-frame DIDs,
4095-byte upload/download blocks, responsePending, settings, persist frames —
against the `opentune-udssim` server. The BLE link compiles but has never run.

## 3i. `doip`: UDS over Ethernet (ISO 13400)

The `doip` driver (`crates/drivers/doip`, crate `opentune-driver-doip`) needs
no adapter: the host's Ethernet port talks to the vehicle's DoIP entity (an
Ethernet-capable gateway or ECU). Like `isotp-ble` it does not implement
`CanLink`; it wraps the core's `DoIpTransport` (TCP data socket + routing
activation) and `DoIpUdpClient` (UDP vehicle identification) behind
`Interface`, and `diag()` hands out the activated transport. Every engine —
coding, adaptation, auto-scan, service functions, live logging, OBD
inspection — and the UDS module flasher run over it unchanged.

**Addressing the entity.** `ip` (or `port`, which the CLI's `--port` /
`OPENTUNE_PORT` and the desktop's port field fill) is
`HOST[:TCP_PORT][?key=value&…]`: an IPv4 / IPv6 address
(`[fe80::10%3]:13400` for a scoped one) or host name, and any other option
after `?`. One port string is a complete connection, which matters for the
engine commands: `coding`, `adaptation`, `autoscan`, `service` and `log` take
only `--driver` and read the address from `OPENTUNE_PORT`:

```sh
opentune info --driver doip --port 192.0.2.10
OPENTUNE_PORT='192.0.2.10?target=0x0010' \
    opentune coding decode --def packs/vcar-demo/coding/engine.json --driver doip
OPENTUNE_PORT='discover?vin=WVWZZZ00000000000' opentune adaptation … --driver doip
```

Options given separately (`--opt key=value`) win over the port string's.
Unknown keys in the port string are refused as typos.

| Option | Default | Meaning |
|---|---|---|
| `ip` / `port` | — | the entity, as above; empty or `discover` = find it by UDP |
| `discover` | off | `1`: send a vehicle identification request first (to `ip` if given, else the IPv4 broadcast, or `ff02::1` with `ipv6=1`) |
| `vin` / `eid` | — | identify only the entity with this VIN (17 chars) or EID (6 hex bytes); implies discovery |
| `udp_port` / `tcp_port` | 13400 | discovery and data ports |
| `scope_id` | — | IPv6 interface index for a link-local / multicast address |
| `ipv6` | off | broadcast discovery on `ff02::1` instead of IPv4 |
| `tester` | `0x0E00` | tester (source) logical address |
| `target` | entity | ECU logical address: hex, or `tx_id` = the connection's request id (per-module maps). Default: the entity's own address (announced, or from routing activation) |
| `activation` | default | routing activation type: `default`, `wwh-obd`, `central` or a hex byte. A discovered entity announcing further action `0x10` gets central security by default |
| `timeout_ms` | 2000 | TCP connect and routing activation, each |
| `discover_timeout_ms` | 2000 | how long discovery waits (`A_DoIP_Ctrl`) |
| `session` | `extended` | session before a block read: `none`, `default`, `programming`, `extended` or a hex byte |
| `seedkey_level` | `0x11` | SecurityAccess request-seed level for a block read |
| `security` / `seedkey` | — | `security=required` (or any `seedkey=`) refuses a block read without a seed/key handler |
| `read_chunk` | 256 | bytes per ReadMemoryByAddress (max 4095) |

When discovery finds several entities the driver refuses and lists them
(address, logical address, EID, VIN); narrow it with `ip=`, `eid=`, `vin=` or a
`target` only one of them announces. `opentune doip discover` shows the same
list without connecting.

**Per-module engines.** Auto-scan and `obd inspect --all-ecus` re-open the
driver per module with that module's request id. DoIP has no CAN ids, so a
DoIP module map puts each ECU's logical address in `req_id` and the
connection sets `target=tx_id`. Without it every module would go to the
entity's own address. A logical address nobody serves is refused by the entity
(diagnostic NACK “unknown target address”) and the module reads as absent.

**Capabilities.** `doip` (new flag) and `read_memory`; not `can` — there is no
raw bus, so `candump` and `can_monitor` are not offered. No battery voltage
(DoIP does not carry it): the flash safety policy sees no reading, so
measure the supply separately before programming. `read_memory` is
ReadMemoryByAddress `0x23` with format `0x44` after the configured session and,
when a handler is attached through `set_seed_key`, one SecurityAccess attempt
(all-zero seed = already unlocked). `plan_read_memory` lists those requests
without sending them. `write_memory` is refused: program a module through the
UDS module flasher (`opentune flash --container …`), which runs on `diag()`.

The desktop shows `doip` in alpha builds (Bench tier). Choosing it adds a
**Discover** button under the sidebar port field; it lists the entities that
answer (VIN, logical address, EID, IP) and a click fills the port field with
`IP?target=0xADDR`.

**Validation state: loopback only (Bench tier).** Tested against the
`opentune-udssim` DoIP entity (UDP + TCP) on `127.0.0.1`: open by address and
by unicast discovery (all / VIN / EID), explicit and `tx_id` targets, block
reads with and without SecurityAccess, and the coding, adaptation, auto-scan,
service and logging engines through the driver crate's
`tests/loopback.rs` and the CLI's `tests/doip_driver.rs`. 🔒 Untested on a
vehicle: broadcast discovery, real gateway routing to ECUs behind it, activation
lines, central-security activation and entity inactivity timeouts (the
desktop re-opens after an error).

## 3j. `esp32-opentune`: OpenTune's own ESP32-S3 interface (unverified)

The `esp32-opentune` driver (`crates/drivers/esp32-opentune`, crate
`opentune-driver-esp32`) speaks the framed USB-CDC protocol of the reference
firmware in `hardware/firmware/esp32-s3` (protocol and layers:
[`hardware.md`](hardware.md#host-driver)). Like `isotp-ble` the device runs
ISO-TP itself, so the CAN path is a `DiagTransport` for `UdsClient`; the K-line
path frames ISO-14230 for `Kwp2000Client`. `diag()` exposes whichever path
`open` selected. Options: `port` (required), `init=5baud` for a 5-baud K-line
wake-up, and the `seedkey` / `security` / `seedkey_level` keys of the
SecurityAccess hook.

**Capabilities.** `can`, `kline`, `read_memory`, `write_memory` and
`battery_voltage` (the board's ADC divider): the protocol carries all of them.
No TP2.0 (refused at `open`), no CAN-FD, no bench boot. A block upload that
stops before the requested length is an error, never a short image.

**Validation state: unverified (Bench tier).** The firmware is compile-only
(CI builds it on a pinned ESP-IDF) and has never been flashed, so the driver
is tested against a mocked serial link only. The desktop lists it in alpha
builds.

## 3k. Optional: raw K-line (`kline`) for SSM2

Some K-line protocols frame their own packets: Subaru SSM2 has its own
`80 <dst> <src> <len>` header and checksum, runs at 4800 baud and needs no
wake-up, so the KWP2000 `DiagTransport` a K-line driver normally exposes
cannot carry it. A driver that can hand out the bus unframed overrides
`kline()` to return a `KLineRaw` (`opentune_core::protocol::kline`):

- `set_baud`, `clear_input`, `write(bytes)` (sent as is, nothing added) and
  `read(buf, timeout)` (at least one byte, or `Error::Timeout`);
- `echoes()`: `true` when `read` also returns the tester's own bytes (the
  half-duplex echo of a plain cable; the protocol layer reads it back and
  compares it), `false` when the adapter strips it;
- `max_write()`: the largest burst the adapter accepts, if limited.

`kline()` is available after `open`; open with `options["init"] = "none"` to
skip the fast/5-baud wake-up. The default returns `None`. The SSM transport
(`opentune_core::protocol::ssm::SsmKLine`) runs on any `KLineRaw` and is
itself a payload-level `DiagTransport` (`A8 …` in, `E8 …` out), the same
shape as an ISO-TP transport carrying SSM over CAN. See
[LIVE-LOGGING.md](LIVE-LOGGING.md#subaru-ssm2).

| Driver | `kline()` | Echo | Notes |
|---|---|---|---|
| `kkl` | the serial port | included | `init=none` skips the wake-up; baud set per call |
| `kline-bridge` | the firmware's `0x10 transceive` | removed on the device | 80-byte limit per transceive; the reply ends after a 60 ms quiet gap, so a module slower than that to start answering is missed (unverified for SSM) |
| `elm327`, `elm327obd` | none | — | not wired: the chip frames every ISO 9141 message itself and opens with a 5-baud wake-up that SSM2 does not use; whether that can carry SSM needs a bench check |

For tests and demos, `opentune log --driver ssmsim --transport ssm|ssm-can`
runs against `opentune_core::protocol::ssmsim::SsmSim`, a synthetic module
that is both a `KLineRaw` bus (with an optional echo) and a CAN payload
transport. `ssmsim` is handled by the `log` command and the desktop's
adapter opener (Live log, SSM transports), not the registry, like `obdsim`
for `opentune obd`; `SsmSim` implements `Interface` for that.

## 3l. `openport2`: Tactrix OpenPort 2.0 (capture validated; K-line and transmit pending)

`crates/drivers/j2534-openport` (crate `opentune-driver-openport2`, id
`openport2`) talks to an OpenPort 2.0 (OP2) or clone through its USB CDC-ACM
interface (`/dev/ttyACM*` on Linux, a COM port on Windows): no vendor DLL, no
kernel driver. It opens the cable's **raw CAN channel** (J2534 protocol 5) and
implements `CanLink`, so ISO-TP, UDS, identify and the block read/write come
from `opentune-core` like every raw-CAN driver. It does **not** use the
cable's own ISO-TP channel (6). On request it opens the **K-line** channels
instead (ISO 14230 channel 4, ISO 9141 channel 3; read path only, see
[K-line](#k-line-iso-14230--iso-9141-read-only) below), and `opentune candump
--driver openport2` captures raw CAN **receive-only** (see
[Capture](#receive-only-capture-candump)).

**Validation state (2026-11-25, a genuine OP2, firmware 1.17.4877):**

- **Receive-only CAN capture: validated on hardware.** `opentune candump
  --driver openport2` at 500 kbit/s on a live ECU captured every frame with
  none dropped, matching an independent wire-level reference capture.
- **K-line: K-line init not yet answered by a real ECU.** On a bench ECU the
  cable ran both the five-baud and the fast init and reported that the ECU
  did not answer (`are 7`), with the cable itself healthy. The cause (ECU
  address, wiring, or the cable protocol) is not known yet, so every K-line
  record shape below is still unproven.
- **CAN transmit, ISO-TP and UDS over the cable: hardware validation
  pending.** Tested against the offline fake only.

Until the checklist at the end of this section has been worked through on a
cable, treat every unvalidated byte of it as unproven.

### The session as implemented

| Step | Host sends | Device answers |
|---|---|---|
| resync | `\r\n\r\n` (ends any half line a previous session left) | nothing |
| identify | `ati\r\n` (no sequence number) | `ari main code version : 1.17.4877` |
| reset | `atz <seq>`, then `ata <seq>` | `aro <seq>` each |
| open | `ato5 <flags> <baud> 0 <seq>` (flags **before** baud; 4th field ignored; flags `256` = 29-bit ids) | `aro <seq>` |
| filter | `atf5 1 <txflags> 4 <seq>` + mask + pattern (4 bytes each, big-endian): PASS on the response id | `arf5 <filter id> <seq>` |
| transmit | `att5 <4+n> <txflags> <budget µs> <seq>` + CAN id (4 bytes BE) + data, **one write** | `aro <seq>`, then a `0x10` transmit-indication record |
| receive | — | `a r '5' <len> <status> <timestamp BE32> <id BE32> <data>`, `len` = 9 + data length |
| battery | `atr 16 <seq>` | `arr 16 <mV> <seq>` |
| close | `atc5 <seq>`, `ata <seq>` | `aro <seq>` each |

Every line ends in `\r\n`; the protocol digit is glued to the verb. Replies
are matched on their trailing sequence number (1..0xFFFF, wrapping); stale
replies are dropped and counted. Receive records interleave with replies, so
frames that arrive while a command waits are queued, not lost. Records with
status `0x10` (transmit indication), `0x20` (loopback) or a bare `0x80` (START)
are indications, not data, and are dropped. `are <code> … <seq>` becomes an
error naming the J2534 code (`ERR_TIMEOUT` on a transmit means nothing on the
bus acknowledged the frame).

Options: `port` (default `/dev/ttyACM0`), `bitrate` (overrides the config;
one of 50000, 83333, 100000, 125000, 250000, 500000, 1000000 — the cable does
not check the rate and a wrong one on a live bus can hang it, so other values
are refused), `tx_timeout_ms` (per-frame transmit budget, 1..=5000, default
100; a zero budget never transmits). `programming_voltage` is **not**
advertised: the cable can drive a pin (`atv`) but the driver sends no `atv`.

### Audit, 2026-11-19

Sources (facts only, no code copied; licences in
[PRIOR-ART.md](PRIOR-ART.md#openport-20-and-j2534-interface-protocol)):
**[B]** bisak/openport-j2534 `docs/PROTOCOL.md` (cable-verified, with black-box
traces of the vendor DLL), **[M]** Mackanized/op2j2534 `PROTOCOL.md`,
**[R]** roffe/libj2534_openport2 `RE_NOTES.md`, **[J]** SAE J2534-1 (04.04).
The vendor DLL and EcuFlash were not opened.

| # | Before the audit | Now | Source |
|---|---|---|---|
| 1 | Lines ended in a bare `\r` | `\r\n` (a bare CR is ignored by the cable, so no command was ever executed) | [M] §2, [B] §2 |
| 2 | `ato 5 …` (spaced digit) | `ato5 …`, the form all three codebases send ([M] §2 says the space is skipped, [B] §2 that "spacing matters"; glued works either way) | [B] §2, [M] §2 |
| 3 | `ato <proto> <flags> <baud> <seq>`: the seq landed in the ignored 4th field and the command went unnumbered | `ato5 <flags> <baud> 0 <seq>` | [B] §3–4, [R] §4 |
| 4 | `atc`/`att`/`atf` addressed a "channel handle" read from the `aro` | addressed by protocol id (`atc5`) | [M] §3 Channels, [B] §5 |
| 5 | `att <ch> 0 <µs> ` + `<len><id><data>` + ` <seq>\r` inline, `len` = data length | `att5 <4+n> <txflags> <µs> <seq>\r\n` + id + data in one write | [M] §4 Transmit, [R] §4, [B] §2 |
| 6 | `atf <ch> 1 0 <mask hex> <pattern hex>` in ASCII | binary payload, `atf5 1 <txflags> 4` | [B] §4 (see note) |
| 7 | RX `len` read as the data length after the id, so every frame swallowed 9 bytes of the next message | `len` counts status + timestamp + id + data | [B] §7.1, [M] §4, [R] §4 |
| 8 | Transmit indications / echoes delivered to ISO-TP as received frames | dropped by status bit | [B] §7.2, §7.5, §7.7 |
| 9 | Frames that arrived while a command awaited its ack were thrown away | queued | [B] §2 "they interleave" |
| 10 | Replies not matched; `are` read as `are <seq> <code>` | `are <code> [<detail>] <seq>`; matched on the trailing seq; stale ones dropped | [R] §4, [M] §2, [B] §3, §9.2 |
| 11 | 8-bit sequence counter | 1..0xFFFF | [R] §4 |
| 12 | `atr 16` reply expected as `aro <seq> <mV>` | `arr 16 <mV> <seq>` | [M] §3, [B] §3, [R] §4 |
| 13 | `ati` numbered, reply kept raw | unnumbered (as the vendor DLL sends it), version parsed | [B] §4, [R] §4 |
| 14 | No resync; `atz` failure ignored; a failed open left the cable claimed | `\r\n\r\n`, `ati`, `atz`, `ata`; a failed open sends `ata` | [B] §4, §9.1 |
| 15 | One byte per read with a per-byte timeout | buffered parser with an overall deadline: split, merged and partial reads, resync on `ar` | [B] §2 |
| 16 | Any bit rate sent | allow-list (above) | [B] §1, §5 |
| 17 | DTR/RTS untouched | asserted (best effort) | [M] §1 |
| 18 | `programming_voltage: true` with no implementation | `false` | DRIVERS.md §2 contract |
| 19 | 50 ms budget, 1 s ack wait | `tx_timeout_ms` (100 ms default), ack wait budget + 1.5 s (no-ACK `are 9` measured at ~1.3 s for a 1 s budget) | [M] §4, [B] §4, §10 |
| 20 | `j2534` crate: ISO 15765 flow-control filter without `ISO15765_FRAME_PAD` | set (flow-control frames padded) | [J], [B] §4 DLL trace `atf6 3 64 4`, §11 |
| 21 | `j2534` crate (Windows FFI): any `PassThruReadMsgs` error with no message reported as a timeout | only `ERR_BUFFER_EMPTY`/`ERR_TIMEOUT` are timeouts | [J] |

Note on `atf` (item 6): [M] and [R] read its 2nd and 3rd fields as
`<mask_len> <pattern_len>` and send `4 4`; [B] reads `<txflags> <len>` and
shows the vendor DLL sending `atf6 3 64 4` with 12 bytes (TxFlags 0x40, three
4-byte messages), which only that reading explains. The driver follows [B]
(`atf5 1 0 4`); `4 4` also parses as "TxFlags 4, length 4", which is why both
projects work. [M]'s PROTOCOL.md table also lists `ato` as `<baud> <flags>`,
contradicted by its own code and the other two sources. Both are on the
checklist.

Not done here: the cable's own ISO-TP channel 6 with flow-control filters,
`atg` read-back, `atv` programming voltage, periodic messages, VW TP2.0 over
this raw-CAN link, and any K-line write path. (K-line itself was added on
2026-11-25, below.)

### K-line (ISO 14230 / ISO 9141), read-only

**Validation state: offline only.** Built from [B] §3–5, §7.9, §8.1 (with
its vendor-DLL traces), [M], [R] and [J]; tested against the offline fake's
*synthetic* KWP2000 ECU. Upstream itself has no cable capture of a received
K-line frame or of a successful `ary`/`arw` ([B] §7.9, §12), so the receive
path is written to accept every reading the sources allow (below).

Selected by the `bus` option (`--bus kline`, `k` or `iso14230` → channel 4;
`--bus iso9141` → channel 3) or by a connection whose protocol is
`iso14230_kline` / `iso9141` (an ECU definition). Any other `bus` value, or
none, keeps the CAN path exactly as before; `bus=can` with a K-line protocol
is refused.

| Step | Host sends | Device answers |
|---|---|---|
| resync, identify, reset | as on CAN | as on CAN |
| open | `ato4 <flags> 10400 0 <seq>`: flags `0` (the cable appends/checks the checksum: J2534 default and the DLL trace) or `512` (`ISO9141_NO_CHECKSUM`, `checksum=host`) | `aro <seq>` |
| timing | `ats4 <param> <value> <seq>`, only for options given | `aro <seq>` (`are 1`: not supported) |
| filter | `atf4 1 0 1 <seq>` + `00` + `00`: pass-all PASS (the shape measured on a cable, [B] §7.9); addressing is checked by the driver | `arf4 <id> <seq>` |
| fast init | `aty4 <n> 0 <seq>` + StartCommunication `81 <ecu> <tester> 81` (+ checksum in host mode) | `ary4 <n> <seq>\r\n` + the ECU's answer (`C1 kb1 kb2`), or `are 7` (no ECU) |
| five-baud | `atw4 <addr, decimal> <seq>`; the cable does sync, KB1/KB2, inverted KB2 and inverted address | `arw4 <kb1> <kb2> [<seq>]` (number optional: [B] saw one unnumbered) |
| request | `att4 <n> 0 <budget µs> <seq>` + the KWP2000 frame (no CAN id; budget 1 s default) | `aro <seq>`; no transmit indication |
| answer | — | `ar4 <len> <status> <bytes>`: data **without** timestamp; a START/END record whose body is exactly 4 bytes is a timestamp-only marker |
| close | `atc4 <seq>`, `ata <seq>` | `aro <seq>` |

Receive: records with `0x20` (loopback) or `0x10` are dropped; START/END
markers delimit a message; a data record outside START…END is a message on its
own; a frame split over two messages is joined; two frames in one are split.
In cable-checksum mode a trailing byte is taken as the checksum only if it
matches (whether the cable passes it through is unobserved). Frames addressed
to another tester are skipped; responsePending (`7F xx 78`) is waited
through (up to 40). Before each request stale messages are cleared.

Read-only: the K-line `DiagTransport` refuses, **before anything is sent**,
every service outside an allow-list (`kline::READ_SERVICES`: OBD read modes
01/02/03/06/07/09/0A; 10 (not 0x85/0x02 programming sessions), 12, 17, 18, 19,
1A, 21, 22, 23, 27, 3E; 81/82). It mirrors the desktop's `READ_SERVICES`
(`native/src/ops.rs`, which also wraps every read path's `diag()` in
`ReadOnlyDiag`); the K-line link is reached only through `diag()`, so both
apply. `write_memory` is refused on K-line and `capabilities().write_memory`
is false while K-line is open. `read_memory` uses KWP2000
`readMemoryByAddress` (0x23) in 250-byte chunks, after a KWP security access
only when a seed/key handler is set or required.

Identify (`read_ecu_id`, KWP2000 layer): `1A` options 80 (identification
table; ECU name when 97 is absent), 90 (VIN), 91 (hardware number), 94
(software number), 97 (system name), 9B (VAG record; software number when 94
is absent). Options the ECU refuses are skipped.

Options (same names as `kkl` where `kkl` has them):

| Option | Default | Meaning |
|---|---|---|
| `tx_id` (`--tx-id`) | — | ECU address, one byte (required: a CAN-sized id is refused). `--rx-id` is ignored on K-line (pass `F1`) |
| `init` | `fast` (ISO 9141: `5baud`) | `fast`, `5baud`/`slow`, `none`; ISO 9141 has no fast init |
| `init_addr` | `tx_id` | five-baud address byte, hex |
| `tester` | `F1` | tester address, hex |
| `baud` (`--baud`) | 10400 | only 10400: [M]/[R] report ISO 14230 at 10400 only, [B] §5 an ISO 9141 open at another rate reading back 10400 |
| `checksum` | `cable` | `cable` (flags 0) or `host` (`ISO9141_NO_CHECKSUM`) |
| `layer` | `kwp2000` | `kwp2000` or `uds`; an ECU definition's layer is used when its protocol selected K-line |
| `tx_timeout_ms` | 1000 | per-message transmit budget, 1..=5000 |
| `p1_max_ms` 1..=50, `p3_min_ms` 0..=5000, `p4_min_ms` 0..=20 | cable (20/55/5) | sent as `ats` P1_MAX/P3_MIN/P4_MIN in 0.5 ms units ([J]) |
| `w0_ms` 2..=1000, `w1_ms` 20..=1000, `w2_ms` 1..=1000, `w3_ms` 0..=1000, `w4_ms` 1..=1000, `w5_ms` 0..=10000 | cable (300/300/20/20/50/300) | W0–W5, 1 ms units |
| `tidle_ms` 0..=10000, `tinil_ms` 1..=100, `twup_ms` 2..=200 | cable (300/25/50) | TIDLE/TINIL/TWUP; `twup_ms` must exceed `tinil_ms` |
| `five_baud_mod` | cable (0) | J2534 `FIVE_BAUD_MOD` 0..=3 |

Out-of-range or malformed values are refused before the port is opened.

Bench identify of a K-line-only ECU (read-only; the EDC15C6 profile and
definition give address `01`):

```sh
opentune identify --driver openport2 --port /dev/ttyACM0 --bus kline \
    --tx-id 01 --rx-id F1 --opt init=5baud    # or --opt init=fast
```

**Raw K-line (design note, not implemented).** The C167 bootstrap-loader
route (`opentune bsl --preset edc15c6`, a serial port at 38400 today) needs
unframed bytes, no init and no checksum. On the OP2 that would be channel 3
opened `ISO9141_NO_CHECKSUM | ISO9141_K_LINE_ONLY` (`ato3 4608 …`, the form
[B] §7.9 measured), `init=none`, a pass-all filter, each `att3` carrying
arbitrary bytes and every data record read back as bytes (a `KLineRaw` whose
`echoes()` is false unless `LOOPBACK` is set). The blockers: the rate is fixed
at 10400 (`DATA_RATE` is read-only on the K channels and no source shows
another rate working), so the BSL would have to autobaud at 10400 and every
later stage stay there (a 1 MiB read at ~1 KB/s, ~17 min); the cable delimits
received bytes into messages by its own P1/P4 timing, which a byte-stream
reader must tolerate; and the `bsl` command takes a serial path, not an
`Interface`. Feasible, but not trivial and not verified, so not built.

### Receive-only capture (`candump`)

`Interface::can_monitor` returns a receive-only monitor: the session above up
to `ato5 <flags> <rate> 0`, then a **pass-all** filter (`atf5 1 <txflags> 4` +
zero mask + zero pattern) instead of the response-id filter, then only reads.
It has no send method, and its session refuses `att`, `atm`, `aty`, `atw`,
`atv`, `atx` and `atp` before writing a byte (tested: no `att*` ever reaches
the fake). Transmit indications, loopback echoes and bare START records are
dropped; status bit `0x02` (or an id above 0x7FF) marks a 29-bit frame. The
cable still acknowledges frames on the bus (no listen-only mode, [B] §10), so
the listen-only level is reported `not-guaranteed` and `listen_only=require`
is refused. Options: `--port`, the rate (`--bitrate`, else `--bus hs|ms`;
one of the CAN rates above), `--opt can_ids=11|29|both` (connect flags 0,
`CAN_29BIT_ID`, or J2534 `CAN_ID_BOTH`; `both` is unverified on a cable).
The cable's wire-level CAN receive at 500 kbit/s has been checked on the
bench (integrator's check, 2026-11); whether a flags-0 channel also delivers
29-bit frames is open (use `can_ids=29` for a 29-bit bus).

```sh
opentune candump --driver openport2 --port /dev/ttyACM0 --bitrate 500000 \
    --out bus.log --duration 60
```

### The offline fake

`opentune_driver_openport2::fake` is a scripted OP2 that speaks the protocol
above and simulates one CAN bus with an ISO-TP ECU behind it (the tests put
the independent `opentune-udssim` server there). It enforces what the notes
report: `\n`-terminated lines, glued protocol digits, sequence-number echo
(unnumbered `att` gets no reply), payload lengths (a wrong one is refused
without consuming the payload, so the stream desynchronises as on the cable),
mandatory filters with PASS/BLOCK semantics, non-zero transmit budgets and
transmit indications. Faults: `are` replies per command, silence, stale
replies, ECU frames ahead of the `aro`, no other node on the bus (`are 9`),
background traffic, leading garbage, loopback echoes, and an ECU flow-control
block size. `FakePort` feeds it to the driver with whole, byte-wise or
patterned read chunking; `serve` runs it behind a pty.

Tests: `crates/drivers/j2534-openport/src/proto.rs` (codec, parser under
every split), `tests/fake_e2e.rs` (identify, block read and write through the
real driver, filters, flow control, 29-bit ids, error and timeout paths), and
`crates/opentune-cli/tests/openport2_fake.rs` (`opentune info`, `identify` and
`read` through the real binary and the real serial open path, against the
fake on a pty). The fake was written from the same notes as the driver, so it
cannot catch a misreading the two share: only the cable can.

### Hardware-validation checklist (when the clone arrives)

Do not run any firmware "update" (EcuFlash or the Tactrix updater) on a clone:
clone serial numbers are blacklisted and the update can wipe the device. The
driver never sends `atx` (bootloader); do not send it by hand either. Remove
the microSD card (with a card in, the cable enumerates as storage). Record
results here with dates; do not commit serial numbers or VINs.

1. **Enumeration.** `lsusb` shows `0403:cc4d` (note the product string; clones
   vary); `/dev/ttyACM*` appears via `cdc_acm` (`dmesg`), and
   `/dev/serial/by-id/` has an entry. The user is in `dialout`/`uucp`.
2. **Version.** `opentune info --driver openport2 --port /dev/ttyACM0` with
   no car: it opens, prints the firmware text, closes cleanly; `ati` by hand
   (115200 8N1, `\r\n`) answers `ari …`. Record the clone's string.
3. **Battery.** On USB power alone `atr 16` reads ~0.1 V; on a 12 V supply or
   the car it matches a multimeter within ~0.2 V.
4. **CAN 500k loopback on the bench.** OP2 plus a reference node (CANable /
   candleLight with `candump`), terminated, 500 kbit/s. Each transmit answers
   `aro` and the reference sees the exact id and data; frames from the
   reference reach the driver; 29-bit ids work both ways (status bit `0x02`).
   With the reference unplugged, a transmit fails with `ERR_TIMEOUT` after
   roughly the budget (+ up to ~1.3 s), never hangs.
5. **Filters.** Frames on other ids never reach the host; with only the PASS
   filter, the reference's 0x7E8 traffic does. Try `atf5 1 0 4` and
   `atf5 1 4 4` by hand: both should answer `arf5`, settling the `atf` field
   reading. Read back `DATA_RATE` (`atg5 1 <seq>` → `arg5 1 500000 <seq>`) to
   confirm the `ato` argument order.
6. **Status bits.** Capture the raw stream (e.g. `strace -e read,write -x` or
   a USB capture) during a transmit and a received frame: transmit indication
   `0x10` + id after the `aro`, received frames status `0x00`/`0x02`,
   `len` = 9 + data length.
7. **Timing.** `aro` latency per frame (expect a few ms); an ISO-TP read of
   4 KB blocks (`read --len 0x4000` on a bench ECU that allows upload) without
   consecutive-frame sequence errors; the ECU's STmin honoured on writes.
8. **ISO 15765 identify on a car.** `opentune identify --driver openport2
   --port /dev/ttyACM0` against the engine ECU (7E0/7E8), ignition on; compare
   the VIN / part numbers with another adapter's result.
9. **K-line (bench, read-only).** ECU on a current-limited supply, K-line to
   OBD pin 7 of the cable, ground shared, terminal 15 on. Nothing here writes;
   the driver refuses non-read services before sending.
   a. `opentune info --driver openport2 --port /dev/ttyACM0 --bus kline
      --tx-id 01 --rx-id F1 --opt init=5baud`: expect `atw4 1` to answer
      `arw4 <kb1> <kb2>` after ~2.5 s; note whether the reply carried the
      sequence number and the key bytes (shown in the firmware line).
   b. The same with `--opt init=fast`: `aty4 4 0` + `81 01 F1 81` should
      answer `ary4 <n>` + bytes; record whether those bytes end in a
      checksum (cable-checksum mode).
   c. `opentune identify` with the same flags: capture the raw stream
      (`strace -e read,write -x`) and record the receive records: START/END
      markers with 4-byte bodies? data in one record or several? checksum
      byte present? `0x10` records? If identify fails, retry with
      `--opt checksum=host` (flags 512) and compare.
   d. With nothing on pin 7, `are 7` on both inits must surface as "no answer
      from the ECU", and the cable must be released (`ata`).
   e. Optional: `atg4 7 <seq>` by hand to learn the P1_MAX unit (expect 40 =
      20 ms in 0.5 ms units per [J]; 20 would mean ms) before setting any
      `p*_ms` option.
   Record results here with dates (no serials, no VINs).
10. **Error paths.** Unplug USB mid-read (clean I/O error, no hang); a second
    program holding the cable (`ERR_CHANNEL_IN_USE`); a refused bit rate.
    If the cable stops answering after a wrong-rate close, remove USB **and**
    the OBD connector together to recover ([B] §1).
11. **Clone quirks to watch.** Different USB ids or product strings; a
    different `ati` text; missing transmit indications or `0x02` bits; other
    `atf`/`ato` field readings; CR/LF handling; packetisation (64-byte bulk,
    one record per packet); battery reading scale; K-line transceiver
    missing; behaviour after `atz`.

Only when steps 1–8 pass on the clone does the status here move from
"hardware validation pending" to "validated (clone)", naming the firmware text.

## 4. Registration

Driver ids registered in the CLI today (`opentune drivers`; crates and hardware
are listed in [`ARCHITECTURE.md`](ARCHITECTURE.md#drivers--cratesdrivers)):
`mock`, `openport2`, `socketcan`, `elm327`, `kkl`, `kline-bridge`, `slcan`,
`j2534`, `gsusb`, `elm327net`, `elm327obd`, `efp02c`, `vcar`, `udssim`,
`isotp-ble`, `doip`, `esp32-opentune`. The desktop registers `isotp-ble`,
`doip` and `esp32-opentune` in alpha builds only (Bench tier). A doc-drift test fails when a new id
is registered without being added here.

Drivers are constructed through a `Registry`, so an app can enumerate and build
them by id without hard-coding:

```rust
let mut reg = Registry::new();
reg.register("mock", "Mock / file driver", || Box::new(MockDriver::new()));
// reg.make("mock") -> Box<dyn Interface>
```

## 5. Testing

- Make as much as possible hardware-free. The mock driver shows how to exercise
  the full read/verify/write pipeline against a file.
- For CAN drivers, a fake `CanLink` that replays canned frames lets you unit-test
  ISO-TP framing and UDS flows with no hardware.
- Keep `cargo clippy --all-targets` clean and add tests for new behaviour; CI
  denies warnings.

## Checklist

- [ ] New crate under `crates/drivers/`, added to workspace `members`.
- [ ] `Interface` implemented; `capabilities` honest; `bench_boot` only if true.
- [ ] CAN driver reuses `IsoTp` + `UdsClient` rather than re-rolling transport
      (a raw-CAN driver gets TP2.0 for free through `CanDiag::open`, §3f).
- [ ] `diag()` overridden if the driver owns a `DiagTransport`.
- [ ] `can_monitor()` overridden if the adapter can capture raw frames passively.
- [ ] Registered in the CLI registry (`crates/opentune-cli/src/registry.rs`) and,
      for a user-facing adapter, the desktop registry
      (`desktop/src-tauri/src/main.rs`).
- [ ] Progress reported; errors use the structured `Error` variants.
- [ ] No safety logic in the driver.
- [ ] Tests, ideally hardware-free. `cargo fmt` + `clippy` clean.
