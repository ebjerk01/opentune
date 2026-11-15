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

## 4. Registration

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
- [ ] Progress reported; errors use the structured `Error` variants.
- [ ] No safety logic in the driver.
- [ ] Tests, ideally hardware-free. `cargo fmt` + `clippy` clean.
