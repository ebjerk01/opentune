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
