# Adaptation, basic settings and actuator tests

`opentune-adaptation` reads and changes a module's adaptation channels, runs
basic settings and actuator tests. What a module offers is **data**: an
`AdaptationDef` JSON file (usually in a definition pack, `adaptation/*.json`,
see [DEFINITION-PACKS.md](DEFINITION-PACKS.md)). Two protocol families are
supported:

| Kind | Protocol | Requests | Writes? |
|---|---|---|---|
| `value` | UDS | `22` read / `2E` write a DID field (read fresh, splice, write, read back) | write |
| `routine` | UDS | `31 01/02/03` RoutineControl start / stop / results | start / stop |
| `actuator` | UDS | `2F <did> 03` shortTermAdjustment, then always `2F <did> 00`; timed tests below | yes |
| `kwp_group` | KWP2000 | `21 <group>` measuring block | no |
| `kwp_basic_settings` | KWP2000 | start / read in basic settings / stop (below) | start |
| `kwp_adaptation` | KWP2000 | read / test / save (below) | test, save |

Definitions written before the KWP kinds existed parse unchanged.

## Definition format

```json
{
  "id": "example",
  "name": "Example module",
  "module": "free text",
  "requires_security": false,
  "kwp_addr": "0x01",
  "channels": [
    { "name": "Idle", "kind": "value", "did": 8193,
      "field": { "offset": 0, "width": 2, "scaling": { "factor": 0.25, "unit": "rpm" } },
      "min": 2000, "max": 4000 },
    { "name": "Group 1", "kind": "kwp_group", "group": 1 },
    { "name": "Throttle basic settings", "kind": "kwp_basic_settings", "group": 60 },
    { "name": "Idle adaptation", "kind": "kwp_adaptation", "channel": 1,
      "scaling": { "factor": 1, "unit": "" }, "min": 0, "max": 4096,
      "kwp": { "enumerate": false } }
  ]
}
```

* `kwp_addr` (optional, top level) is the module's TP2.0 logical address or
  K-line address; the desktop app uses it when the panel's address field is
  empty. A real KWP operation never assumes an address.
* `kwp` (optional, per KWP channel) is the **KWP addressing form**: which
  routine local identifiers and selectors the module uses. Every field defaults
  to the community VAG values below, so `{}` or no `kwp` means the defaults:

| Field | Default | Meaning |
|---|---|---|
| `enter` | `0xB8` | start adaptation / basic settings (its `32` stop exits) |
| `select` | `0xB9` | select an adaptation channel; with a 2-byte value: the test value |
| `read` | `0xBA` | read the current adaptation / basic-settings record |
| `save` | `0xBB` | save an adaptation value |
| `adaptation_selector` | `[1, 3]` | sub-selector sent with every adaptation routine (1..=4 bytes) |
| `basic_settings_selector` | `0` | first byte of the basic-settings selector (`<this> <group>`) |
| `enumerate` | `true` | send `<enter> 00 00` before entering |

`AdaptationDef::validate` checks unique names, field widths, non-inverted
ranges and that the four routine ids are distinct.

## Timed actuator tests (UDS 0x2F)

`opentune_adaptation::actuator` runs an output test for a set time. Only an
`actuator` channel the definition **declares** can be driven, and only with
what it declares:

```json
{ "name": "Radiator fan", "kind": "actuator", "did": 1280, "state": [1],
  "options": ["short_term_adjustment", "reset_to_default", "freeze_current_state"],
  "value": { "offset": 0, "width": 1, "scaling": { "factor": 1, "unit": "%" } },
  "min": 0, "max": 100, "max_duration_ms": 10000 }
```

| Field | Default | Meaning |
|---|---|---|
| `options` | `["short_term_adjustment"]` | control options a test may use; `return_control_to_ecu` is always allowed |
| `value` | none | where the shortTermAdjustment value goes in the `state` record (big-endian field + scaling); without it the declared `state` is sent as is |
| `min` / `max` | the field's full range | raw range of `value` (needs `value`) |
| `max_duration_ms` | 10000 | longest hold; never more than 60000 |
| `security_level` (top level) | `0x01` | UDS SecurityAccess requestSeed level used when `requires_security` is set |

`AdaptationDef::validate` checks that `value` lies inside `state`, that the
range fits the field and is not inverted, and the duration cap.

A test (`plan_test`, then `run_test`) sends:

| Step | Request |
|---|---|
| session | `10 03` |
| SecurityAccess (definitions with `requires_security`) | `27 <level>` -> seed, `27 <level+1> <key>` |
| control | `2F <did> <option> [state]` |
| hold | `3E 00` every 2 s until the duration ends or a stop is requested |
| return | `2F <did> 00` (returnControlToECU), retried once |
| leave | `10 01` |

returnControlToECU is sent after **every** control request: at the end of the
duration, on a stop (desktop STOP, panel left, adapter disconnected, window
closed; Ctrl-C in the CLI), after a refused control request and after a link
failure during the hold. If it is not acknowledged the report says
`control_return_failed` and `safe: false`; the output may still be forced, so
recover the module (ignition off and on) before driving. Returning to the
default session also ends IO control on an ISO 14229 module. A failed session
or SecurityAccess step drives nothing (`aborted`). The run is refused before
any I/O when it is not confirmed, or when SecurityAccess is required and no
seed/key handler is given (OpenTune ships none).

`returnControlToECU` on its own is a one-shot test (no hold), for handing an
output back after an interrupted session.

**Free-form DIDs.** The engine can build a one-channel definition for an
undeclared IO DID (`freeform_def`: every option, a raw state from the caller,
at most 5 s). Only the desktop's Debug card (alpha / bench builds) offers it,
with a second confirmation and explicit Target ids. The CLI has no free-form
path.

### CLI

Timed tests live under `opentune adaptation` because they run on the same
`AdaptationDef` files, `--def` / `--driver` / `--timeout-ms` flags and engine
as the other adaptation actions.

```sh
D=packs/vcar-demo/adaptation/engine.json   # the virtual car's engine module
opentune adaptation --def $D actuator list
opentune adaptation --def $D actuator run "Radiator fan" --value 40 --duration-ms 3000
#   prints the plan; nothing is sent
opentune adaptation --def $D --driver vcar actuator run "Radiator fan" \
    --value 40 --duration-ms 3000 --ack --port demo
# real hardware: --driver elm327obd --port /dev/ttyUSB0 --bus hs [--tx-id 7E0 --rx-id 7E8]
```

`--op adjust|reset|freeze|return`, `--value` (engineering) or `--raw`,
`--seedkey ID` for secured modules, `--json`. The module address comes from
`--tx-id` / `--rx-id` or the definition's `req_id` / `resp_id`; a test never
defaults to the engine ECU. The command exits non-zero unless the test
completed or was stopped with control returned.

### Desktop

The **Actuator tests** panel (Bench tier) lists the declared channels of the
chosen definition, plans a test (sends nothing, every build) and, on alpha /
bench builds, runs it with a per-run confirmation, a live exchange log, a
progress bar and a STOP button. Commands: `actuator_list`, `actuator_plan`,
`actuator_start`, `actuator_status`, `actuator_stop`. A test holds the adapter
for its whole duration (other panels see "adapter busy"); the engine ends it
at the end of the duration even if the window goes away.

## KWP2000 (VAG, TP2.0 / K-line)

The KWP path (`opentune_adaptation::kwp`) runs over the same request/response
seam as the UDS engine, so the transport is whatever the driver opened: a VW
TP2.0 channel on a raw-CAN adapter (`socketcan`, `slcan`, `gsusb`; the ELM327
drivers refuse TP2.0) or KWP2000 on a K-line adapter (`kkl`, `kline-bridge`,
`elm327`).

| Operation | Requests (defaults) |
|---|---|
| session | `10 89` |
| SecurityAccess | `27 03` -> seed, `27 04 <key>` |
| measuring block | `21 <group>` -> `61 <group> (formula a b)*` |
| basic settings | `31 B8 00 00`, `31 B8 00 <group>`, then `31 BA 00 <group>` -> `71 BA 00 <group> <status> (formula a b)*` per read, `32 B8 00 <group>` |
| adaptation read | `31 B8 00 00`, `31 B8 01 03`, `31 B9 01 03 <channel>`, `31 BA 01 03` -> `71 BA 01 03 <status> <fmt> <hi> <lo> [04 f a b]`, `32 B8 01 03` |
| adaptation test | ... select, `31 B9 01 03 <hi> <lo>`, read back, exit (not stored) |
| adaptation save | ... select, test value, `31 BB 01 03 <hi> <lo> <workshop code: 6 bytes>`, read back, exit |

Cells are scaled with the shared VAG formula table
(`opentune_core::protocol::vagmeas`); unknown formulas stay raw.

**Provenance and confidence.** The service ids are ISO 14230-3. The routine
local identifiers, selectors, record layouts and the SecurityAccess level come
from public open-source work (see [PRIOR-ART.md](PRIOR-ART.md#vag-kwp2000-adaptation-and-basic-settings))
observed on one module family (EDC16 over K-line), corroborated by a public
TP2.0 simulator. They are **unconfirmed** for every other module, which is why
they are data defaults a channel can override. The status bytes and the byte
before the adaptation value are recorded, not interpreted. Test semantics
(the module discards a test value on exit) follow KWP1281 and are unconfirmed
for KWP2000. Nothing here has run on a car yet.

### Login and session loss

Every KWP flow starts its own session and, when a handler is given,
SecurityAccess (basic settings: only when the module asks for it, see below). The key comes from a caller-supplied `SeedKey`: **OpenTune
ships no VAG KWP login algorithm.** An embedding build (Bench) supplies one
through its seed/key resolvers; tests and `--sim` use the simulator's
test-only `kwpsim-test` handler, which unlocks only the simulator.

A TP2.0 channel that broke (failed exchange, module `A8`) is set up again on
the next request **without** the module's KWP session. So:

* a read that fails with a session-loss symptom (transport failure, NRC
  `0x80`, `0x7E`, `0x7F`) re-establishes the session and security once and
  retries; the result says so (`reestablished`);
* a write never retries: the first failure after the first mutating request
  ends it as `aborted` (with `session_lost`), nothing more is sent after a
  session-loss symptom, and after a plain negative response only a
  best-effort exit is sent. The channel state is then unknown: read it again.

### Basic settings without login

> **Untested on a real car.** Only the synthetic module has run this.

Many VAG modules run basic settings (and answer measuring blocks) without a
login. The public kw1281test tool reflects that: `BasicSetting GROUP` and
`GroupRead GROUP` take no login, while `AdaptationRead/Test/Save` take an
optional one (see [PRIOR-ART.md](PRIOR-ART.md)). So basic settings are
attempted **without SecurityAccess by default**, even when a seed/key handler
is given:

1. `10 89`, enumerate, `31 B8 00 <group>` with no login.
2. If the module refuses that start with NRC **`0x33` securityAccessDenied**,
   the run reports "this module needs a login for basic settings":
   * with a seed/key handler it runs SecurityAccess (`27 03`, `27 04 <key>`)
     in the still-open session, sends the enumerate and start again, and
     carries on (`login_required: true` in the report);
   * without one it stops with a `LoginRequired` error. Nothing was started.

Only `0x33` counts as "login needed". It is the one ISO 14230-3 code that
means the security condition is not met. `0x22`
(conditionsNotCorrect / requestSequenceError) is reported as the module's
refusal and **not** retried with a login: it covers engine state, speed,
temperature and request order, and a pointless login would use up one of the
module's limited key attempts. `0x35`/`0x36`/`0x37` answer SecurityAccess
itself, and `0x80`/`0x7E`/`0x7F` are session loss.

The confirmation gate is unchanged (`--ack` / the panel's tick and second
confirmation, plus an explicit session), and the desktop panel stays in the
Bench tier. Adaptation **test / save still require a seed/key handler up
front**. Only basic settings changed.

### Safety gates

Basic-settings start and adaptation test / save are refused **before any
I/O** unless the operation is confirmed (`--ack` / the panel's confirmation)
and a session is given. Adaptation test / save also need a seed/key handler
before anything is sent; basic settings need one only when the module asks
for a login (above). A save also needs the module's 6-byte workshop code. Values are range-checked against the channel's
`min`/`max`; a test or save is read back (`verified`, `tested` or
`verify_failed`). A dry run returns the request plan and sends nothing.
Basic settings are always stopped after the reads, unless the session was
lost (the module leaves basic settings with it). A start the module refused
(negative response) is an error: nothing started. A start whose answer was
lost or malformed is reported as `aborted` at step `start`, because the
module may have started.

## CLI

```sh
D=crates/opentune-adaptation/tests/fixtures/kwp-sim.json   # synthetic module map
opentune adaptation --def $D list
opentune adaptation --def $D kwp-read "Group 1" --sim
opentune adaptation --def $D kwp-read "Adaptation 1" --sim
opentune adaptation --def $D kwp-save "Adaptation 1" --value 0x300 \
    --wsc 000001234567 --ack --sim --seedkey kwpsim-test
opentune adaptation --def $D kwp-basic "Basic settings 60" --polls 3 \
    --ack --sim --seedkey kwpsim-test   # the synthetic module wants a login
opentune adaptation --def $D kwp-basic "Basic settings 60" --ack --sim
#   -> "login required: this module needs a login for basic settings ..."
opentune adaptation --def $D kwp-test "Adaptation 1" --value 7 --dry-run
# real hardware: --driver slcan --port /dev/ttyACM0 --tp20-addr 0x01
#                --driver kkl --port /dev/ttyUSB0 --kline-addr 0x01
```

`--session` (default `0x89`, `none` skips), `--level` (default `0x03`),
`--seedkey ID`, `--opt KEY=VALUE` and `--json` apply to every `kwp-*` action.

## Desktop

The **Adaptation** panel (Bench tier) shows a KWP2000 card for the KWP kinds:
simulated-module switch, module address, session, seed/key id and level; Read
for measuring blocks and adaptation channels; a Plan button (dry run) and
confirmation-gated Test / Save / Run basic settings. Commands:
`adaptation_kwp_read`, `adaptation_kwp_basic`, `adaptation_kwp_adapt`. The
adapter is closed after each KWP operation, so the TP2.0 channel is torn down.

## Tests

`crates/opentune-adaptation/tests/actuator.rs` runs timed tests against the
`opentune-udssim` server (IO control allowed in the extended session only):
the hold with tester present, stop, a refused control request, a link failure
during the hold, a lost link (control not returned, reported loudly),
SecurityAccess with the simulator's test key and with a wrong key. The desktop
(`feat_actuator.rs`) and CLI (`tests/actuator.rs`) tests drive the virtual
car.

`crates/opentune-adaptation/tests/kwp.rs` drives the KWP path against the
synthetic module (`opentune_adaptation::sim`) over the core TP2.0 stack
(`Tp20Channel` on the `Tp20Ecu` simulator, whose module session resets on
every channel setup) and over a K-line-style transport: reads, session loss
after a re-open (read retried; write aborted with nothing more sent), negative
responses, security refusal, the confirmation gate, dry runs, verify failure
and basic settings, including basic settings without a login (a module that
allows it, one that refuses with `0x33` until login, that refusal with no
handler, `0x22` not taken as a login request, and the confirmation still
required).
