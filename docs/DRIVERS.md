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
NotGuaranteed). The log writers and stats live in `opentune-candump`.

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

## 4. Registration

Driver ids registered in the CLI today (`opentune drivers`; crates and hardware
are listed in [`ARCHITECTURE.md`](ARCHITECTURE.md#drivers--cratesdrivers)):
`mock`, `openport2`, `socketcan`, `elm327`, `kkl`, `kline-bridge`, `slcan`,
`j2534`, `gsusb`, `elm327net`, `elm327obd`. A doc-drift test fails when a new id
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
- [ ] CAN driver reuses `IsoTp` + `UdsClient` rather than re-rolling transport.
- [ ] `diag()` overridden if the driver owns a `DiagTransport`.
- [ ] `can_monitor()` overridden if the adapter can capture raw frames passively.
- [ ] Registered in the CLI registry (`crates/opentune-cli/src/registry.rs`) and,
      for a user-facing adapter, the desktop registry
      (`desktop/src-tauri/src/main.rs`).
- [ ] Progress reported; errors use the structured `Error` variants.
- [ ] No safety logic in the driver.
- [ ] Tests, ideally hardware-free. `cargo fmt` + `clippy` clean.
