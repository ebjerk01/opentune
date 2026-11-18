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
handled in the ELM reply parser with the P2* budget.

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
matches the identification-only definition `siemens-pcr21-03l907425c`. Public
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
A unit test pins it (`every_open_resets_with_atz_and_never_sends_bare_atcra`).
Any future in-session retargeting must send `ATAR` (or `ATZ`) before the new
filter, never a bare `ATCRA`.

Not yet validated against a real ECU. The first car session (read-only, see
[`FIRST-CAR-TEST.md`](FIRST-CAR-TEST.md)) should start with
`plan_read_memory` and a small region.

## 3e. `elm327`: UDS block read on CAN

`crates/drivers/kline-elm327` (crate `opentune-driver-elm327`, id `elm327`)
has the same single-frame transmit limit on CAN. Its UDS `read_memory` plans
the request format before anything is sent, security access included, and
uses the compact `addressAndLengthFormatIdentifier`s:

- `read_mode=upload` (default): `RequestUpload 0x35` with format `0x22`
  (16-bit address) or `0x13` (24-bit address, 255-byte windows, one
  request/transfer/exit cycle per window). A 32-bit address is refused with a
  pointer to `read_mode=rmba`.
- `read_mode=rmba`: `ReadMemoryByAddress 0x23` with format `0x14` (32-bit
  address, 1-byte size), `read_chunk` bytes per request (default 32, 1..=255;
  this driver does not pace the ECU's consecutive frames).

K-line reads (KWP2000, and UDS over K-line) are unchanged. Writes on CAN still
use the 4-byte format and are refused by the 7-byte check; use `elm327obd`'s
bench path or a raw-CAN interface. Like `elm327obd`, this is unit-tested
against scripted ELM replies only. It has no dry run (`plan_read_memory` is
`None`).

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
  lands inside a telegram. An unanswered keep-alive marks the channel lost and
  the next request fails with that reason (re-open the driver: the module's
  KWP session went with the channel).
- Data frames honour the module's block size (we wait for its ACK at each
  block end), ACK the module's blocks, accept the "last frame, no ACK" (`0x3`)
  and "ACK, not ready" (`0x9`) opcodes, and answer a module-initiated `A3`.
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

## 4. Registration

Driver ids registered in the CLI today (`opentune drivers`; crates and hardware
are listed in [`ARCHITECTURE.md`](ARCHITECTURE.md#drivers--cratesdrivers)):
`mock`, `openport2`, `socketcan`, `elm327`, `kkl`, `kline-bridge`, `slcan`,
`j2534`, `gsusb`, `elm327net`, `elm327obd`, `efp02c`, `vcar`, `udssim`. A
doc-drift test fails when a new id
`j2534`, `gsusb`, `elm327net`, `elm327obd`, `vcar`, `udssim`, `isotp-ble`. A doc-drift test fails when a new id
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
