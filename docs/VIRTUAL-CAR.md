# The virtual car (`vcar` driver)

`vcar` is an interface driver that simulates a **whole multi-module vehicle**
in memory. With it, every panel and engine — auto-scan and DTCs, coding,
adaptation, service functions, the OBD dashboard, logging and CAN capture — can
be used, demonstrated and tested end to end with no adapter and no car.

Everything about the virtual car (CAN ids, part numbers, DTCs, coding bytes,
the VIN) is **its own fiction**. It does not describe any real vehicle or
manufacturer, and nothing in it was taken from one.

## What it simulates

| Module | Request / response id | Notes |
|---|---|---|
| Engine | `7E0` / `7E8` | VIN (`F190`), OBD-II (physical and functional `7DF`) |
| Gearbox | `7E1` / `7E9` | |
| ABS / ESC | `713` / `77D` | |
| Airbag | `715` / `77F` | writes need SecurityAccess |
| Body / central | `70E` / `778` | |
| Instrument cluster | `714` / `77E` | |

These are the virtual car's addresses on its 11-bit CAN bus. They are not a
claim about where any real vehicle puts these modules.

Each module is an independent UDS server (`opentune-udssim`) behind its own
ISO-TP endpoint. The driver hands out the core's `IsoTp` transport, so every
request is framed as single, first, consecutive and flow-control frames, just
like on a real bus. Each module has:

- identity DIDs: `F187` part number, `F188` software number, `F189` software
  version, `F18A` supplier, `F191` hardware number, `F195` supplier software
  version and `F197` system name. `F190` (the VIN) is on the engine only.
- a few stored DTCs with ISO 14229 status bytes. `19 02` reads them, `14 FF FF
  FF` clears them, and so does OBD mode `04` on the engine. Mode `03` shows the
  confirmed P-codes and mode `07` the pending ones.
- a long-coding DID `0600` and two adaptation DIDs, `0610` and `0611`.
- one RoutineControl (`31`) routine and one InputOutputControl (`2F`) channel.
- SecurityAccess (`27 01` / `27 02`).

The engine also answers OBD-II: mode 01 (including the supported-PID bitmaps,
with rpm, speed, coolant, throttle, load and voltage from a small drive-cycle
model), mode 09 VIN, and modes 03, 07 and 04. The same model feeds the battery
voltage (about **14.1 V** with the engine running and **12.6 V** with it off).
It also feeds the synthetic CAN traffic the CAN capture panel records:

| id | rate | content |
|---|---|---|
| `0C9` | 100 Hz | rpm × 4, throttle, load, coolant, running flag, counter |
| `1F5` | 50 Hz | speed × 100, brake flag, counter |
| `3D1` | 10 Hz | fuel level, outside temperature, odometer |
| `4A0` | 5 Hz | ignition, doors, locks, battery voltage |

A request to an address that no module listens on gets **no answer** (an ISO-TP
timeout), so an auto-scan reports a missing module as absent rather than as an
error.

### Test-only security

SecurityAccess uses a deliberately trivial **test algorithm**: `key[i] = seed[i]
XOR 0x5A`. The mask is set in the profile, and the seed differs per module.
This algorithm exists only so that secured flows can be exercised offline. It
is not, and is not derived from, any manufacturer's algorithm. Tests use
`opentune_driver_vcar::test_security_key` as the key provider. OpenTune ships no
OEM key provider, so the desktop can *plan* the secured demo script but not run
it.

## Using it

### Desktop

Pick **Virtual car (offline simulator)** as the driver. The Port field selects
the car: leave it empty for the built-in demo. Then:

- **Auto-scan**: load `packs/vcar-demo/modules/vcar-demo.json`. It lists the
  six modules, the engine's OBD-II view on `7DF` and a parking-assist module
  that is "not fitted", so it shows as absent.
- **Coding / Adaptation / Service**: load a definition from
  `packs/vcar-demo/coding`, `adaptation` or `services`. Every definition carries
  its module's `req_id` / `resp_id`, so writes, routines and actuator tests go
  to the right module.
- **Dashboard**: use the engine address (the default) or `7DF` / `7E8`.
- **CAN capture**: start a capture with the `vcar` driver.

The car keeps its state (cleared codes, coding writes) for as long as the app
runs. Every panel and re-binding talks to the same car.

### CLI

```sh
opentune drivers                                   # lists vcar
opentune autoscan --driver vcar --modules packs/vcar-demo/modules/vcar-demo.json
opentune candump --driver vcar --out bus.log --duration 5   # synthetic traffic
```

`autoscan` binds the adapter to each module's own ids in turn. Use
`OPENTUNE_PORT=<profile>` to choose a different profile from the CLI, or
`candump --port <profile>`.

### Options (`ConnectConfig::options`)

| Option | Meaning |
|---|---|
| `profile` or `port` | `demo` (default) or the path to a profile JSON. Add `@name` to get an independent instance, e.g. `demo@bench`. |
| `engine` | `on` / `off`: start or stop the engine when opening |
| `reset` | `1` / `true`: rebuild the car from its profile, forgetting all writes and clears |

For the CAN monitor, `vcar_realtime=0` runs on a virtual clock (for tests) and
`vcar_frames=N` stops after N frames.

### In tests

```rust
use opentune_driver_vcar::{Profile, Vehicle, VcarDriver};
let car = std::sync::Arc::new(std::sync::Mutex::new(
    Vehicle::from_profile(&Profile::demo())?));
let mut drv = VcarDriver::with_vehicle(car.clone()); // a private car
drv.open(&ConnectConfig { tx_id: 0x7E1, rx_id: 0x7E9, ..Default::default() })?;
// drive any engine over CoreDiag::new(drv.diag().unwrap(), 100), then
// assert on car.lock()?.module(0x7E1).unwrap().server()
```

The crate's `tests/end_to_end.rs` drives auto-scan, coding, adaptation, service
scripts, OBD and CAN capture this way. The desktop's `vcar_e2e` test does the
same through the adapter hub.

## Writing a scenario

A vehicle is a profile JSON. Copy `crates/drivers/vcar/profiles/demo.json` and
edit it:

- `vin` (17 characters; use something obviously fictional),
  `engine_running`, and `security.xor_mask`.
- `modules[]`: `name`, `req_id` / `resp_id` (11-bit, unique, not `7DF`), `obd`,
  `seed`, `require_security_for_write`, plus:
  - `dids[]`: `{did, ascii}` or `{did, hex}`. `"ascii": "$VIN"` serves the
    profile VIN.
  - `dtcs[]`: `{code: "P0101", status: "2F", failure_type?}`.
  - `routines[]`: `{id, name, result}`.
  - `io[]`: `{did, name, default}`.

Point the driver at it with `profile=/path/to/car.json` (or the desktop Port
field). Add a matching definition pack (see
[`DEFINITION-PACKS.md`](DEFINITION-PACKS.md)) if the panels should have
definitions for it. `packs/vcar-demo` is the example. Keep scenarios fictional:
do not use real VINs, part numbers, calibration data or security algorithms.
